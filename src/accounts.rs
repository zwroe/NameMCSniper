use std::collections::{HashMap, hash_map::Entry};
use std::fs::File;
use std::future::Future;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Error, Result, ensure};
use futures_util::stream::{FuturesUnordered, StreamExt};
use reqwest::Client;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::minecraft::{Minecraft, Profile, ReadRateLimit, bearer_value};

const MAX_FILE_BYTES: u64 = 128 * 1024;
pub const MAX_TOKEN_ENTRIES: usize = 64;
const MAX_TOKEN_BYTES: usize = 8192;
const BATCH_SIZE: usize = 4;

pub struct BatchCheck {
    pub accounts: Vec<CheckedAccount>,
    pub failures: usize,
    pub rate_limit: Option<ReadRateLimit>,
}

impl BatchCheck {
    pub fn into_eligible_accounts(self, target: &str) -> Result<Vec<CheckedAccount>> {
        if let Some(rate_limit) = self.rate_limit {
            return Err(rate_limit.into());
        }
        let accounts: Vec<_> = self
            .accounts
            .into_iter()
            .filter(|account| {
                account.eligible && !account.profile.name.eq_ignore_ascii_case(target)
            })
            .collect();
        ensure!(
            !accounts.is_empty(),
            "The token list has no eligible Java profiles for this name change."
        );
        Ok(accounts)
    }
}

pub struct CheckedAccount {
    pub minecraft: Minecraft,
    pub profile: Profile,
    pub id: Uuid,
    pub eligible: bool,
}

struct Row<'a> {
    line: usize,
    token: &'a str,
    outcome: Option<Outcome>,
}

enum Outcome {
    Checked(CheckedAccount),
    DuplicateToken(usize),
    DuplicateProfile(usize),
    Failed(Error),
}

pub async fn check_file(path: &Path, client: Client) -> Result<BatchCheck> {
    let metadata = path
        .metadata()
        .with_context(|| format!("Could not open the token list at {}", path.display()))?;
    ensure!(metadata.is_file(), "The token list must be a regular file.");
    ensure!(
        metadata.len() <= MAX_FILE_BYTES,
        "The token list may be at most 128 KiB."
    );

    let mut contents = Zeroizing::new(String::new());
    File::open(path)
        .context("Could not open the token list.")?
        .take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut contents)
        .context("Could not read the token list as UTF-8.")?;
    ensure!(
        contents.len() as u64 <= MAX_FILE_BYTES,
        "The token list may be at most 128 KiB."
    );
    let rows = parse_rows(&contents)?;
    println!("Checking {} token entries.", rows.len());
    Ok(check_rows(rows, |bearer| {
        Minecraft::new(client.clone(), bearer.to_owned())
    })
    .await)
}

fn parse_rows(contents: &str) -> Result<Vec<Row<'_>>> {
    let mut seen = HashMap::new();
    let mut rows = Vec::new();
    for (index, entry) in contents.trim_start_matches('\u{feff}').lines().enumerate() {
        let line = index + 1;
        let entry = entry.trim_start();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        ensure!(
            rows.len() < MAX_TOKEN_ENTRIES,
            "The token list may contain at most {MAX_TOKEN_ENTRIES} entries."
        );
        let bearer = bearer_value(entry);
        let outcome = match seen.entry(bearer) {
            Entry::Occupied(first) => Some(Outcome::DuplicateToken(*first.get())),
            Entry::Vacant(first) => {
                first.insert(line);
                None
            }
        };
        rows.push(Row {
            line,
            token: entry,
            outcome,
        });
    }
    ensure!(!rows.is_empty(), "The token list contains no entries.");
    Ok(rows)
}

async fn check_rows(
    mut rows: Vec<Row<'_>>,
    minecraft_for: impl Fn(&str) -> Result<Minecraft>,
) -> BatchCheck {
    let mut profiles_seen = HashMap::new();
    let mut candidates = rows.iter_mut().filter(|row| row.outcome.is_none());
    let rate_limit = loop {
        let mut batch: Vec<_> = candidates.by_ref().take(BATCH_SIZE).collect();
        if batch.is_empty() {
            break None;
        }

        let (mut profiles, rate_limit) =
            until_limited(batch.iter().enumerate().map(|(index, row)| {
                let token = row.token;
                let minecraft_for = &minecraft_for;
                async move {
                    let result = async {
                        ensure!(
                            bearer_value(token).len() <= MAX_TOKEN_BYTES,
                            "The bearer token is too long."
                        );
                        let minecraft = minecraft_for(token)?;
                        let profile = minecraft.profile().await?;
                        let id = Uuid::parse_str(&profile.id)
                            .context("Minecraft returned an invalid profile UUID.")?;
                        Ok((minecraft, profile, id))
                    }
                    .await;
                    (index, result)
                }
            }))
            .await;
        profiles.sort_unstable_by_key(|(index, _)| *index);
        let mut unique = Vec::new();
        for (index, result) in profiles {
            let row = &mut batch[index];
            match result {
                Ok((minecraft, profile, id)) if rate_limit.is_none() => {
                    match profiles_seen.entry(id) {
                        Entry::Occupied(first) => {
                            row.outcome = Some(Outcome::DuplicateProfile(*first.get()));
                        }
                        Entry::Vacant(first) => {
                            first.insert(row.line);
                            unique.push((index, minecraft, profile, id));
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => row.outcome = Some(Outcome::Failed(error)),
            }
        }
        if rate_limit.is_some() {
            break rate_limit;
        }

        let (eligibility, rate_limit) = until_limited(unique.into_iter().map(
            |(index, minecraft, profile, id)| async move {
                let result = minecraft.eligible().await.map(|eligible| CheckedAccount {
                    minecraft,
                    profile,
                    id,
                    eligible,
                });
                (index, result)
            },
        ))
        .await;
        for (index, result) in eligibility {
            batch[index].outcome = Some(match result {
                Ok(account) => Outcome::Checked(account),
                Err(error) => Outcome::Failed(error),
            });
        }
        if rate_limit.is_some() {
            break rate_limit;
        }
    };

    report(rows, rate_limit)
}

async fn until_limited<T>(
    pending: impl IntoIterator<Item = impl Future<Output = (usize, Result<T>)>>,
) -> (Vec<(usize, Result<T>)>, Option<ReadRateLimit>) {
    let mut requests: FuturesUnordered<_> = pending.into_iter().collect();
    let mut results = Vec::new();
    while let Some((index, result)) = requests.next().await {
        let rate_limit = result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<ReadRateLimit>())
            .copied();
        results.push((index, result));
        if rate_limit.is_some() {
            return (results, rate_limit);
        }
    }
    (results, None)
}

fn report(rows: Vec<Row<'_>>, rate_limit: Option<ReadRateLimit>) -> BatchCheck {
    let mut accounts = Vec::new();
    let mut failures = 0;
    let mut duplicates = 0;
    let mut skipped = 0;
    for row in rows {
        let line = row.line;
        match row.outcome {
            Some(Outcome::Checked(account)) => {
                let status = if account.eligible {
                    "eligible"
                } else {
                    "rename unavailable"
                };
                println!(
                    "Line {line}  {} ({})  {status}",
                    account.profile.name, account.id
                );
                accounts.push(account);
            }
            Some(Outcome::DuplicateToken(first)) => {
                println!("Line {line}  duplicate token from line {first}");
                duplicates += 1;
            }
            Some(Outcome::DuplicateProfile(first)) => {
                println!("Line {line}  duplicate Java profile from line {first}");
                duplicates += 1;
            }
            Some(Outcome::Failed(error)) => {
                println!("Line {line}  {error}");
                failures += 1;
            }
            None => {
                println!("Line {line}  skipped after rate limit");
                skipped += 1;
            }
        }
    }

    let eligible = accounts.iter().filter(|account| account.eligible).count();
    println!(
        "{} checked, {eligible} eligible, {failures} failed, {duplicates} duplicates, {skipped} skipped.",
        accounts.len()
    );
    BatchCheck {
        accounts,
        failures,
        rate_limit,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::{BufRead, BufReader, ErrorKind, Write};
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use super::*;

    const FIRST: &str = r#"{"id":"853c80ef3c3749fdaa49938b674adae6","name":"First"}"#;
    const SAME: &str = r#"{"id":"853C80EF-3C37-49FD-AA49-938B674ADAE6","name":"First"}"#;
    const OTHER: &str = r#"{"id":"363c80ef3c3749fdaa49938b674adae6","name":"Other"}"#;

    fn localhost(
        expected_requests: usize,
        respond: impl Fn(&str) -> (&'static str, &'static str, &'static str) + Send + 'static,
    ) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(4);
            let mut requests = Vec::new();
            while requests.len() < expected_requests {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "fixture did not receive expected requests"
                        );
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                let request = request.to_ascii_lowercase();
                let (status, headers, body) = respond(&request);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                requests.push(request);
            }
            requests
        });
        (origin, server)
    }

    fn test_client() -> Client {
        Client::builder()
            .timeout(Duration::from_secs(2))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap()
    }

    #[test]
    fn keeps_line_numbers_and_normalizes_the_bearer_prefix() {
        let rows = parse_rows("\u{feff}# comment\n \n demo \n  bEaReR demo\nBearer \n").unwrap();
        assert_eq!(
            rows.iter().map(|row| row.line).collect::<Vec<_>>(),
            [3, 4, 5]
        );
        assert_eq!(bearer_value(rows[0].token), "demo");
        assert!(matches!(rows[1].outcome, Some(Outcome::DuplicateToken(3))));
        assert!(bearer_value(rows[2].token).is_empty());
        assert!(parse_rows("\n# empty").is_err());
        assert!(parse_rows(&"x\n".repeat(MAX_TOKEN_ENTRIES + 1)).is_err());
    }

    #[tokio::test]
    async fn deduplicates_uuids_before_eligibility_and_retains_cooldown_accounts() {
        let (origin, server) = localhost(5, |request| {
            let token_is =
                |token| request.contains(&format!("\r\nauthorization: bearer {token}\r\n"));
            let profile = request.starts_with("get /minecraft/profile http/1.1\r\n");
            let body = if profile {
                if token_is("alpha") {
                    FIRST
                } else if token_is("alternate") {
                    SAME
                } else {
                    OTHER
                }
            } else {
                assert!(request.starts_with("get /minecraft/profile/namechange http/1.1\r\n"));
                assert!(!token_is("alternate"));
                if token_is("alpha") {
                    r#"{"nameChangeAllowed":true}"#
                } else {
                    r#"{"nameChangeAllowed":false}"#
                }
            };
            ("200 OK", "", body)
        });
        let client = test_client();
        let rows =
            parse_rows("# fake tokens\nalpha\nBearer alpha\nalternate\nother\nbroken token\n")
                .unwrap();
        let result = check_rows(rows, |bearer| {
            Minecraft::with_test_origin(client.clone(), bearer.to_owned(), origin.clone())
        })
        .await;
        assert!(result.rate_limit.is_none());
        assert_eq!(result.failures, 1);
        assert_eq!(
            result
                .accounts
                .iter()
                .map(|account| (account.profile.name.as_str(), account.eligible))
                .collect::<Vec<_>>(),
            [("First", true), ("Other", false)]
        );
        assert_eq!(server.join().unwrap().len(), 5);
        let eligible = result.into_eligible_accounts("TargetName").unwrap();
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].profile.name, "First");
    }

    #[tokio::test]
    async fn a_read_429_stops_before_the_next_batch() {
        let (origin, server) = localhost(1, |request| {
            assert!(request.starts_with("get /minecraft/profile http/1.1\r\n"));
            ("429 Too Many Requests", "Retry-After: 20\r\n", "")
        });
        let client = test_client();
        let created = Cell::new(0);
        let rows = parse_rows("alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\n").unwrap();
        let result = check_rows(rows, |bearer| {
            created.set(created.get() + 1);
            Minecraft::with_test_origin(client.clone(), bearer.to_owned(), origin.clone())
        })
        .await;
        assert!(result.failures >= 1);
        assert!(result.accounts.is_empty());
        assert_eq!(created.get(), BATCH_SIZE);
        assert_eq!(server.join().unwrap().len(), 1);
        let error = match result.into_eligible_accounts("TargetName") {
            Err(error) => error,
            Ok(_) => panic!("A rate-limited scan must stop live account selection"),
        };
        assert!(error.is::<ReadRateLimit>());
        assert!(error.to_string().contains("Retry-After is 20 seconds"));
        assert_eq!(
            crate::cli_exit_code(Err(error.context("Live validation stopped"))),
            3
        );
    }
}
