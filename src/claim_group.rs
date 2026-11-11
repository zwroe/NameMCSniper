use std::{future::Future, pin::pin};

use anyhow::{Result, bail, ensure};
use futures_util::future::join_all;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use uuid::Uuid;

use crate::accounts::MAX_TOKEN_ENTRIES;
use crate::minecraft::{ClaimOutcome, Minecraft, PreparedClaim, ReadRateLimit};

pub(crate) const READ_CONCURRENCY: usize = 4;

pub struct LiveAccount {
    pub id: Uuid,
    pub minecraft: Minecraft,
}

pub struct ArmedAccount<'a> {
    account: &'a LiveAccount,
    target: &'a str,
    profile_name: Option<String>,
    requests: std::vec::IntoIter<PreparedClaim<'a>>,
}

pub struct BatchOutcome {
    pub outcome: ClaimOutcome,
    pub details: Vec<AccountOutcome>,
}

pub struct AccountOutcome {
    pub id: Uuid,
    pub name: String,
    pub outcome: ClaimOutcome,
}

pub fn prepare<'a>(
    accounts: &'a [LiveAccount],
    name: &'a str,
    slots: usize,
) -> Result<Vec<ArmedAccount<'a>>> {
    ensure!(
        (1..=MAX_TOKEN_ENTRIES).contains(&accounts.len()),
        "Choose one to {MAX_TOKEN_ENTRIES} Java profiles."
    );
    ensure!(
        (1..=3).contains(&slots),
        "Choose one to three attempt slots."
    );
    ensure!(
        accounts.len() == 1 || slots == 1,
        "A group claim uses exactly one attempt slot."
    );

    let mut armed = Vec::with_capacity(accounts.len());
    for (index, account) in accounts.iter().enumerate() {
        ensure!(
            accounts[..index].iter().all(|other| other.id != account.id),
            "Profile {} appears more than once.",
            account.id
        );
        let requests = (0..slots)
            .map(|_| account.minecraft.prepare_claim(name, account.id))
            .collect::<Result<Vec<_>>>()?
            .into_iter();
        armed.push(ArmedAccount {
            account,
            target: name,
            profile_name: None,
            requests,
        });
    }
    Ok(armed)
}

pub async fn preflight<'a>(
    accounts: Vec<ArmedAccount<'a>>,
    name: &str,
) -> Result<(Vec<ArmedAccount<'a>>, Vec<String>)> {
    ensure!(
        accounts.iter().all(|armed| armed.target == name),
        "The target changed after preparing the requests."
    );
    let group = accounts.len() > 1;
    let profiles = read_for_preflight(
        accounts
            .iter()
            .map(|armed| armed.account.minecraft.profile()),
    )
    .await?;
    let mut matching = Vec::with_capacity(accounts.len());
    let mut skipped = Vec::new();
    for (mut armed, profile) in accounts.into_iter().zip(profiles) {
        let profile = match profile {
            Ok(profile) => profile,
            Err(error) if !group => return Err(error),
            Err(error) => {
                skipped.push(format!("Skipped profile {}. {error:#}", armed.account.id));
                continue;
            }
        };
        let id = armed.account.id;
        ensure!(
            Uuid::parse_str(&profile.id)? == id,
            "The token belongs to {} ({}). Expected profile {id}.",
            profile.name,
            profile.id
        );
        ensure!(
            !profile.name.eq_ignore_ascii_case(name),
            "This account already has the requested name."
        );
        armed.profile_name = Some(profile.name);
        matching.push(armed);
    }

    let eligibility = read_for_preflight(
        matching
            .iter()
            .map(|armed| armed.account.minecraft.eligible()),
    )
    .await?;
    let mut ready = Vec::with_capacity(matching.len());
    for (armed, eligible) in matching.into_iter().zip(eligibility) {
        match eligible {
            Ok(true) => ready.push(armed),
            Ok(false) if !group => {
                bail!("Minecraft reports that this profile cannot change its name yet.")
            }
            Ok(false) => skipped.push(format!(
                "Skipped profile {}. Minecraft reports that it cannot change its name yet.",
                armed.account.id
            )),
            Err(error) if !group => return Err(error),
            Err(error) => skipped.push(format!("Skipped profile {}. {error:#}", armed.account.id)),
        }
    }
    ensure!(
        !ready.is_empty(),
        "No eligible Java profiles for this run. {}",
        skipped.join(" ")
    );
    Ok((ready, skipped))
}

async fn read_for_preflight<T>(
    pending: impl IntoIterator<Item = impl Future<Output = Result<T>>>,
) -> Result<Vec<Result<T>>> {
    let mut results: Vec<_> = stream::iter(pending.into_iter().enumerate())
        .map(|(index, request)| async move {
            match request.await {
                Err(error) if error.is::<ReadRateLimit>() => Err(error),
                result => Ok((index, result)),
            }
        })
        .buffer_unordered(READ_CONCURRENCY)
        .try_collect()
        .await?;
    results.sort_unstable_by_key(|(index, _)| *index);
    Ok(results.into_iter().map(|(_, result)| result).collect())
}

pub async fn dispatch(accounts: &mut [ArmedAccount<'_>], name: &str) -> BatchOutcome {
    if accounts.is_empty()
        || accounts.iter().any(|armed| {
            armed.target != name || armed.profile_name.is_none() || armed.requests.len() == 0
        })
    {
        return BatchOutcome {
            outcome: ClaimOutcome::Rejected("No prepared claim batch for this slot.".into()),
            details: Vec::new(),
        };
    }

    let mut details = join_all(accounts.iter_mut().map(|armed| async move {
        let request = armed.requests.next().expect("checked prepared requests");
        let outcome = armed.account.minecraft.claim(request).await;
        AccountOutcome {
            id: armed.account.id,
            name: armed.profile_name.clone().expect("checked preflight"),
            outcome,
        }
    }))
    .await;

    if !details
        .iter()
        .any(|result| matches!(result.outcome, ClaimOutcome::RateLimited(_)))
    {
        let requests = accounts
            .iter()
            .zip(&mut details)
            .filter(|(_, result)| matches!(result.outcome, ClaimOutcome::Uncertain(_)))
            .map(|(armed, result)| async move {
                let profile = armed.account.minecraft.profile().await?;
                if Uuid::parse_str(&profile.id).ok() == Some(result.id)
                    && profile.name.eq_ignore_ascii_case(name)
                {
                    result.outcome = ClaimOutcome::Claimed;
                }
                Ok::<_, anyhow::Error>(())
            });
        let mut reads = pin!(stream::iter(requests).buffer_unordered(READ_CONCURRENCY));
        while let Some(result) = reads.next().await {
            if result.is_err_and(|error| error.is::<ReadRateLimit>()) {
                break;
            }
        }
    }

    BatchOutcome {
        outcome: aggregate(&details),
        details,
    }
}

fn aggregate(details: &[AccountOutcome]) -> ClaimOutcome {
    details
        .iter()
        .map(|result| &result.outcome)
        .max_by_key(|outcome| match outcome {
            ClaimOutcome::Claimed => (4, None),
            ClaimOutcome::Uncertain(_) => (3, None),
            ClaimOutcome::RateLimited(delay) => (2, *delay),
            ClaimOutcome::Rejected(_) => (1, None),
            ClaimOutcome::Unavailable => (0, None),
        })
        .cloned()
        .expect("batch has at least one result")
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, ErrorKind, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    const IDS: [Uuid; 6] = [
        Uuid::from_u128(1),
        Uuid::from_u128(2),
        Uuid::from_u128(3),
        Uuid::from_u128(4),
        Uuid::from_u128(5),
        Uuid::from_u128(6),
    ];
    const NAMES: [&str; IDS.len()] = [
        "AccountOne",
        "AccountTwo",
        "AccountThree",
        "AccountFour",
        "AccountFive",
        "AccountSix",
    ];
    const TOKENS: [&str; IDS.len()] = [
        "fixture-one",
        "fixture-two",
        "fixture-three",
        "fixture-four",
        "fixture-five",
        "fixture-six",
    ];

    #[test]
    fn aggregates_success_uncertainty_and_the_longest_rate_limit() {
        let details = |outcomes: Vec<ClaimOutcome>| {
            outcomes
                .into_iter()
                .map(|outcome| AccountOutcome {
                    id: IDS[0],
                    name: NAMES[0].to_owned(),
                    outcome,
                })
                .collect::<Vec<_>>()
        };
        let rate_limit = ClaimOutcome::RateLimited(Some(Duration::from_secs(10)));
        let uncertain = ClaimOutcome::Uncertain("fixture timeout".into());
        assert_eq!(
            aggregate(&details(vec![
                rate_limit.clone(),
                uncertain.clone(),
                ClaimOutcome::Claimed
            ])),
            ClaimOutcome::Claimed
        );
        assert_eq!(
            aggregate(&details(vec![rate_limit.clone(), uncertain.clone()])),
            uncertain
        );
        assert_eq!(
            aggregate(&details(vec![
                rate_limit.clone(),
                ClaimOutcome::RateLimited(None),
                ClaimOutcome::RateLimited(Some(Duration::from_secs(1))),
            ])),
            rate_limit
        );
        assert_eq!(
            aggregate(&details(vec![ClaimOutcome::Unavailable])),
            ClaimOutcome::Unavailable
        );
    }

    #[tokio::test]
    async fn group_dispatch_sends_every_put_before_any_reply_and_skips_reconciliation_on_429() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (finished, wait_finished) = mpsc::channel();
        let server = thread::spawn(move || {
            reply_to_preflight(&listener);

            let claims = IDS.map(|_| receive(&listener));
            let mut seen = [false; IDS.len()];
            for (stream, request, account) in claims {
                assert!(!seen[account]);
                seen[account] = true;
                assert!(request.starts_with("PUT /minecraft/profile/name/TargetName HTTP/1.1\r\n"));
                for line in request.to_ascii_lowercase().lines() {
                    assert!(!line.starts_with("transfer-encoding:"));
                    if let Some(length) = line.strip_prefix("content-length:") {
                        assert_eq!(length.trim(), "0");
                    }
                }
                if account == 0 {
                    reply(stream, "429 Too Many Requests", "Retry-After: 20\r\n", "");
                } else {
                    reply(stream, "500 Internal Server Error", "", "");
                }
            }
            wait_finished.recv_timeout(Duration::from_secs(4)).unwrap();
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == ErrorKind::WouldBlock)
            );
        });

        let accounts = fixture_accounts(&origin);
        assert!(prepare(&accounts, "TargetName", 2).is_err());
        let prepared = prepare(&accounts, "TargetName", 1).unwrap();
        let (mut ready, skipped) = preflight(prepared, "TargetName").await.unwrap();
        assert!(skipped.is_empty());
        let batch = dispatch(&mut ready, "TargetName").await;
        finished.send(()).unwrap();
        server.join().unwrap();

        assert_eq!(batch.details.len(), IDS.len());
        for (index, detail) in batch.details.iter().enumerate() {
            assert_eq!(detail.id, IDS[index]);
            assert_eq!(detail.name, NAMES[index]);
        }
        assert_eq!(
            batch.details[0].outcome,
            ClaimOutcome::RateLimited(Some(Duration::from_secs(20)))
        );
        assert!(
            batch.details[1..]
                .iter()
                .all(|detail| matches!(detail.outcome, ClaimOutcome::Uncertain(_)))
        );
        assert_eq!(batch.outcome, batch.details[1].outcome);
        assert!(matches!(
            dispatch(&mut ready, "TargetName").await.outcome,
            ClaimOutcome::Rejected(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn full_group_preflight_has_time_for_two_slow_read_phases() {
        use chrono::TimeDelta;

        use crate::{HTTP_REQUEST_TIMEOUT, Timing, schedule::Clock, wait_for_preflight};

        let clock = Clock::anchored(TimeDelta::zero());
        let slot = clock.now() + TimeDelta::minutes(3);
        wait_for_preflight(slot, MAX_TOKEN_ENTRIES, Timing::Relative(clock), &mut None)
            .await
            .unwrap();
        let started = clock.now();

        for _ in 0..2 {
            let replies = read_for_preflight((0..MAX_TOKEN_ENTRIES).map(|_| async {
                tokio::time::sleep(HTTP_REQUEST_TIMEOUT).await;
                Ok(())
            }))
            .await
            .unwrap();
            assert_eq!(replies.len(), MAX_TOKEN_ENTRIES);
        }

        let finished = clock.now();
        assert!(finished - started > TimeDelta::seconds(20));
        assert!(finished < slot);
    }

    #[tokio::test]
    async fn namemc_retries_the_group_at_the_latest_bound_with_a_fresh_preflight() {
        use std::cell::Cell;

        use chrono::TimeDelta;

        use crate::{
            Timing,
            drops::DropWindow,
            execute_live_plan,
            schedule::{Clock, Plan},
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let accounts = fixture_accounts(&origin);
        let clock = Clock::anchored(TimeDelta::zero());
        let now = clock.now();
        let first = now + TimeDelta::milliseconds(300);
        let latest = first + TimeDelta::milliseconds(500);
        let window = DropWindow::manual(first, Some(first + TimeDelta::days(2))).unwrap();
        let mut plan = Plan::from_namemc(window, now).unwrap();
        // Shorten the fixture's wait after building the two-batch NameMC plan.
        plan.slots[1] = latest;

        let (finished, wait_finished) = mpsc::channel();
        let server = thread::spawn(move || {
            for slot in [first, latest] {
                reply_to_preflight(&listener);
                let mut seen = [false; IDS.len()];
                for _ in IDS {
                    let (stream, request, account) = receive(&listener);
                    assert!(
                        request.starts_with("PUT /minecraft/profile/name/TargetName HTTP/1.1\r\n")
                    );
                    assert!(clock.now() >= slot, "PUT arrived before its scheduled slot");
                    assert!(!seen[account]);
                    seen[account] = true;
                    if slot == latest && account == 0 {
                        let body = format!(r#"{{"id":"{}","name":"TargetName"}}"#, IDS[account]);
                        reply(stream, "200 OK", "", &body);
                    } else {
                        reply(
                            stream,
                            "400 Bad Request",
                            "",
                            r#"{"details":{"status":"DUPLICATE"}}"#,
                        );
                    }
                }
            }
            wait_finished.recv_timeout(Duration::from_secs(4)).unwrap();
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == ErrorKind::WouldBlock)
            );
        });
        let status = execute_live_plan(
            &accounts,
            "TargetName",
            &plan,
            0,
            Timing::Relative(clock),
            &Cell::new(false),
            &mut None,
        )
        .await;
        let _ = finished.send(());
        server.join().unwrap();
        assert_eq!(status.unwrap(), 0);
    }

    #[tokio::test]
    async fn preflight_stops_before_starting_more_reads_when_one_is_rate_limited() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (finished, wait_finished) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut in_flight = Vec::new();
            for _ in 0..READ_CONCURRENCY {
                let (stream, request, _) = receive(&listener);
                assert!(request.starts_with("GET /minecraft/profile HTTP/1.1\r\n"));
                in_flight.push(stream);
            }
            reply(
                in_flight.pop().unwrap(),
                "429 Too Many Requests",
                "Retry-After: 20\r\n",
                "",
            );
            wait_finished.recv_timeout(Duration::from_secs(4)).unwrap();
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == ErrorKind::WouldBlock)
            );
        });

        let accounts = fixture_accounts(&origin);
        let prepared = prepare(&accounts, "TargetName", 1).unwrap();
        let result = preflight(prepared, "TargetName").await;
        finished.send(()).unwrap();
        server.join().unwrap();
        assert!(result.is_err_and(|error| error.is::<ReadRateLimit>()));
    }

    #[tokio::test]
    async fn reconciliation_keeps_confirmed_renames_and_stops_queued_reads_on_429() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (finished, wait_finished) = mpsc::channel();
        let server = thread::spawn(move || {
            reply_to_preflight(&listener);
            for _ in IDS {
                let (stream, request, _) = receive(&listener);
                assert!(request.starts_with("PUT /minecraft/profile/name/TargetName HTTP/1.1\r\n"));
                reply(stream, "500 Internal Server Error", "", "");
            }

            let mut in_flight = Vec::new();
            for _ in 0..READ_CONCURRENCY {
                let (stream, request, index) = receive(&listener);
                assert!(request.starts_with("GET /minecraft/profile HTTP/1.1\r\n"));
                in_flight.push((stream, index));
            }
            let (stream, confirmed) = in_flight.pop().unwrap();
            let body = format!(r#"{{"id":"{}","name":"TargetName"}}"#, IDS[confirmed]);
            reply(stream, "200 OK", "", &body);
            let (pending, request, _) = receive(&listener);
            assert!(request.starts_with("GET /minecraft/profile HTTP/1.1\r\n"));
            let (limited, _) = in_flight.pop().unwrap();
            reply(limited, "429 Too Many Requests", "", "");
            wait_finished.recv_timeout(Duration::from_secs(4)).unwrap();
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == ErrorKind::WouldBlock)
            );
            drop(pending);
            confirmed
        });

        let accounts = fixture_accounts(&origin);
        let prepared = prepare(&accounts, "TargetName", 1).unwrap();
        let (mut ready, _) = preflight(prepared, "TargetName").await.unwrap();
        let batch = dispatch(&mut ready, "TargetName").await;
        finished.send(()).unwrap();
        let confirmed = server.join().unwrap();
        assert_eq!(batch.outcome, ClaimOutcome::Claimed);
        for (index, detail) in batch.details.iter().enumerate() {
            if index == confirmed {
                assert_eq!(detail.outcome, ClaimOutcome::Claimed);
            } else {
                assert!(matches!(detail.outcome, ClaimOutcome::Uncertain(_)));
            }
        }
    }

    fn reply_to_preflight(listener: &TcpListener) {
        for _ in IDS {
            let (stream, request, account) = receive(listener);
            assert!(request.starts_with("GET /minecraft/profile HTTP/1.1\r\n"));
            let body = format!(r#"{{"id":"{}","name":"{}"}}"#, IDS[account], NAMES[account]);
            reply(stream, "200 OK", "", &body);
        }
        for _ in IDS {
            let (stream, request, _) = receive(listener);
            assert!(request.starts_with("GET /minecraft/profile/namechange HTTP/1.1\r\n"));
            reply(stream, "200 OK", "", r#"{"nameChangeAllowed":true}"#);
        }
    }

    fn fixture_accounts(origin: &str) -> [LiveAccount; IDS.len()] {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .retry(reqwest::retry::never())
            .build()
            .unwrap();
        std::array::from_fn(|index| LiveAccount {
            id: IDS[index],
            minecraft: Minecraft::with_test_origin(
                client.clone(),
                TOKENS[index].to_owned(),
                origin.to_owned(),
            )
            .unwrap(),
        })
    }

    fn receive(listener: &TcpListener) -> (TcpStream, String, usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for a fixture request"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("could not accept fixture connection: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut request = String::new();
        loop {
            let mut line = String::new();
            let bytes = reader.read_line(&mut line).unwrap();
            if bytes == 0 || line == "\r\n" {
                break;
            }
            request.push_str(&line);
        }
        let headers = request.to_ascii_lowercase();
        let account = TOKENS
            .iter()
            .position(|token| headers.contains(&format!("\r\nauthorization: bearer {token}\r\n")))
            .expect("fixture request authenticated with a known test token");
        (stream, request, account)
    }

    fn reply(mut stream: TcpStream, status: &str, headers: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }
}
