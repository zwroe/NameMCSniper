use std::fs::{File, OpenOptions, remove_file};
use std::io::{ErrorKind, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use futures_util::stream::{self, StreamExt};
use http::Uri;
use reqwest::{Proxy, StatusCode, Url};
use zeroize::Zeroizing;

const PROFILE: &str = "https://api.minecraftservices.com/minecraft/profile";
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_PROXIES: usize = 256;
const CONCURRENT_PROBES: usize = 8;

pub async fn check_file(path: &Path, output: Option<&Path>) -> Result<u8> {
    let contents = read_list(path)?;
    let entries = contents
        .trim_start_matches('\u{feff}')
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            Some((index + 1, line))
        })
        .collect::<Vec<_>>();

    ensure!(!entries.is_empty(), "The proxy list contains no entries.");
    ensure!(
        entries.len() <= MAX_PROXIES,
        "The proxy list may contain at most {MAX_PROXIES} entries."
    );

    println!("Testing {} proxy entries.", entries.len());
    let mut working = Vec::new();
    let mut reports = stream::iter(entries.iter().copied())
        .map(|(line, entry)| async move {
            let Some(url) = parse_proxy(entry) else {
                return (None, format!("Line {line}  invalid proxy address"));
            };
            let label = endpoint_label(&url);
            let started = Instant::now();
            let (healthy, status) = probe(url).await;
            let ms = started.elapsed().as_millis();
            (
                healthy.then_some(entry),
                format!("Line {line}  {label}  {status}  {ms} ms"),
            )
        })
        .buffered(CONCURRENT_PROBES);

    while let Some((entry, report)) = reports.next().await {
        println!("{report}");
        if let Some(entry) = entry {
            working.push(entry);
        }
    }

    let failed = entries.len() - working.len();
    println!("{} healthy, {failed} failed.", working.len());
    if let Some(output) = output {
        if working.is_empty() {
            println!("No healthy entries to export.");
        } else {
            export_working(output, &working)?;
            println!("Exported {} healthy proxy entries.", working.len());
        }
    }
    Ok(u8::from(failed != 0))
}

fn export_working(path: &Path, entries: &[&str]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(|error| {
        if error.kind() == ErrorKind::AlreadyExists {
            anyhow::anyhow!("The proxy output already exists. Choose a new file.")
        } else {
            anyhow::Error::new(error).context("Could not create the proxy output.")
        }
    })?;

    let written = entries
        .iter()
        .try_for_each(|entry| writeln!(file, "{entry}"))
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written {
        if let Err(cleanup_error) = remove_file(path)
            && cleanup_error.kind() != ErrorKind::NotFound
        {
            return Err(error)
                .context("Could not write the proxy output. A partial file may remain.");
        }
        return Err(error).context("Could not write the proxy output.");
    }
    Ok(())
}

fn read_list(path: &Path) -> Result<Zeroizing<String>> {
    let metadata = path.metadata().context("Could not open the proxy list.")?;
    ensure!(metadata.is_file(), "The proxy list must be a regular file.");
    ensure!(
        metadata.len() <= MAX_FILE_BYTES,
        "The proxy list may be at most 64 KiB."
    );

    let file = File::open(path).context("Could not open the proxy list.")?;
    let mut contents = Zeroizing::new(String::new());
    file.take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut contents)
        .context("Could not read the proxy list as UTF-8.")?;
    ensure!(
        contents.len() as u64 <= MAX_FILE_BYTES,
        "The proxy list may be at most 64 KiB."
    );
    Ok(contents)
}

fn parse_proxy(entry: &str) -> Option<Url> {
    if entry.chars().any(char::is_control) || entry.contains('\\') {
        return None;
    }

    let url = if let Some((scheme, address)) = entry
        .split_once("://")
        .filter(|(scheme, _)| !scheme.contains('@'))
    {
        if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
            || address.is_empty()
            || address.starts_with('/')
        {
            return None;
        }
        Url::parse(entry).ok()?
    } else {
        let (endpoint, credentials) = entry
            .split_once('@')
            .map_or((entry, None), |(endpoint, credentials)| {
                (endpoint, Some(credentials))
            });
        if endpoint.contains(['/', '?', '#']) {
            return None;
        }
        let (host, port) = endpoint.rsplit_once(':')?;
        if host.is_empty() || port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let mut url = Url::parse(&format!("http://{endpoint}")).ok()?;
        if let Some(credentials) = credentials {
            let (username, password) = credentials.split_once(':')?;
            if username.is_empty() {
                return None;
            }
            url.set_username(username).ok()?;
            url.set_password(Some(password)).ok()?;
        }
        url
    };

    if url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
        || url.port() == Some(0)
    {
        return None;
    }

    // The connector parses the URL as an HTTP URI and can ignore it if that parse fails.
    url.as_str().parse::<Uri>().ok()?;
    Some(url)
}

fn endpoint_label(url: &Url) -> String {
    format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().expect("parsed proxy has a host"),
        url.port_or_known_default()
            .expect("HTTP and HTTPS have default ports")
    )
}

async fn probe(url: Url) -> (bool, String) {
    let Ok(proxy) = Proxy::all(url) else {
        return (false, "invalid proxy configuration".into());
    };
    let Ok(client) = crate::http_client_builder().proxy(proxy).build() else {
        return (false, "could not initialize proxy client".into());
    };

    match client.get(PROFILE).send().await {
        Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
            (true, "healthy (HTTP 401)".into())
        }
        Ok(response) => (
            false,
            format!("unexpected HTTP {}", response.status().as_u16()),
        ),
        Err(error) if error.is_timeout() => (false, "timed out".into()),
        Err(error) if error.is_connect() => (false, "could not establish HTTPS tunnel".into()),
        Err(_) => (false, "request failed".into()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn parses_bare_and_authenticated_proxies_without_printing_credentials() {
        for (entry, label) in [
            ("proxy.example:80", "http://proxy.example:80"),
            ("[::1]:8080", "http://[::1]:8080"),
            (
                "proxy.example:8080@user:password",
                "http://proxy.example:8080",
            ),
            ("[::1]:8080@user:password", "http://[::1]:8080"),
            ("https://proxy.example", "https://proxy.example:443"),
            (
                "https://user:secret%40example@proxy.example:8443/",
                "https://proxy.example:8443",
            ),
        ] {
            assert_eq!(endpoint_label(&parse_proxy(entry).unwrap()), label);
        }
    }

    #[test]
    fn rejects_malformed_or_unsupported_entries() {
        for entry in [
            "proxy.example",
            "proxy.example:0",
            "proxy.example:65536",
            "proxy.example:8080@user",
            "proxy.example:8080@:password",
            "proxy.example:not-a-port@user:password",
            "https://",
            "https:///proxy.example",
            "http://private:secret@proxy.example:not-a-port",
            "http://proxy.example/path",
            "http://proxy.example?private=secret",
            "http://proxy.example#private",
            "http://proxy.\texample",
            "socks5://proxy.example:1080",
            "user:pass@proxy.example:8080",
        ] {
            assert!(parse_proxy(entry).is_none(), "accepted {entry}");
        }
    }

    #[test]
    fn rejects_a_url_the_connector_would_ignore() {
        let entry = r#"http://proxy"example:8080"#;
        assert!(Url::parse(entry).is_ok());
        assert!(parse_proxy(entry).is_none());
    }

    #[test]
    fn exports_original_entries_and_preserves_an_existing_file() {
        let path = std::env::temp_dir().join(format!(
            "namemcsniper-proxy-export-test-{}.txt",
            std::process::id()
        ));
        let entries = [
            "HTTPS://user:secret%40example@proxy.example:8443/",
            "[::1]:8080",
        ];
        export_working(&path, &entries).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        let collision = export_working(&path, &["replacement.example:80"]);
        let preserved = std::fs::read_to_string(&path).unwrap();
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            path.metadata().unwrap().permissions().mode() & 0o777
        };
        remove_file(&path).unwrap();

        assert_eq!(original, format!("{}\n{}\n", entries[0], entries[1]));
        assert!(collision.is_err());
        assert_eq!(preserved, original);
        #[cfg(unix)]
        assert_eq!(mode & 0o177, 0);
    }

    #[tokio::test]
    async fn probe_uses_connect_and_keeps_proxy_auth_separate_from_minecraft() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            socket
                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            request
        });

        let url = parse_proxy(&format!("{address}@user:p%40ss")).unwrap();
        let (healthy, result) = probe(url).await;
        assert!(!healthy);
        assert_eq!(result, "could not establish HTTPS tunnel");
        let request = server.join().unwrap();
        assert!(request.starts_with("CONNECT api.minecraftservices.com:443 HTTP/1.1\r\n"));
        let headers = request.to_ascii_lowercase();
        assert!(headers.contains("\r\nproxy-authorization: basic dxnlcjpwqhnz\r\n"));
        assert!(!headers.contains("\r\nauthorization:"));
    }
}
