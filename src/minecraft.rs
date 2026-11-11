use std::fmt::{self, Display};
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow, bail};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{Client, Method, Request, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, de::DeserializeOwned};
use uuid::Uuid;
use zeroize::Zeroizing;

const ORIGIN: &str = "https://api.minecraftservices.com";
const PROFILE: &str = "/minecraft/profile";
const ELIGIBILITY: &str = "/minecraft/profile/namechange";
const MAX_JSON_BYTES: usize = 256 * 1024;

pub struct Minecraft {
    client: Client,
    authorization: HeaderValue,
    origin: String,
}

pub struct PreparedClaim<'a> {
    name: &'a str,
    expected_profile_id: Uuid,
    request: Request,
}

#[derive(Debug, Deserialize)]
pub struct Profile {
    pub name: String,
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimOutcome {
    Claimed,
    Unavailable,
    RateLimited(Option<Duration>),
    Rejected(String),
    Uncertain(String),
}

#[derive(Clone, Copy, Debug)]
pub struct ReadRateLimit(Option<Duration>);

impl Display for ReadRateLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(delay) => write!(
                formatter,
                "Minecraft Services limited requests (HTTP 429). Retry-After is {} seconds",
                delay
                    .as_secs()
                    .saturating_add(u64::from(delay.subsec_nanos() != 0))
            ),
            None => formatter
                .write_str("Minecraft Services limited requests (HTTP 429). Try again later"),
        }
    }
}

impl std::error::Error for ReadRateLimit {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Eligibility {
    name_change_allowed: bool,
}

#[derive(Deserialize)]
struct Failure {
    details: Option<FailureDetails>,
}

#[derive(Deserialize)]
struct FailureDetails {
    status: Option<String>,
}

impl Minecraft {
    pub fn new(client: Client, token: String) -> Result<Self> {
        let token = Zeroizing::new(token);
        let token = bearer_value(&token);
        let unpadded = token.trim_end_matches('=');
        if unpadded.is_empty()
            || !unpadded.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
            })
        {
            bail!("Expected a Minecraft Services bearer token");
        }

        let header = Zeroizing::new(format!("Bearer {token}"));
        let mut authorization = HeaderValue::from_str(&header)
            .map_err(|_| anyhow!("Expected a Minecraft Services bearer token"))?;
        authorization.set_sensitive(true);

        Ok(Self {
            client,
            authorization,
            origin: ORIGIN.to_owned(),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_test_origin(client: Client, token: String, origin: String) -> Result<Self> {
        let mut minecraft = Self::new(client, token)?;
        minecraft.origin = origin;
        Ok(minecraft)
    }

    pub async fn profile(&self) -> Result<Profile> {
        let profile: Profile = self.get(PROFILE, "read the Java profile").await?;
        if !valid_profile(&profile) {
            bail!("Minecraft Services returned an invalid Java profile");
        }
        Ok(profile)
    }

    pub async fn eligible(&self) -> Result<bool> {
        let eligibility: Eligibility = self
            .get(ELIGIBILITY, "read name-change eligibility")
            .await?;
        Ok(eligibility.name_change_allowed)
    }

    pub fn prepare_claim<'a>(
        &self,
        name: &'a str,
        expected_profile_id: Uuid,
    ) -> Result<PreparedClaim<'a>> {
        if !valid_username(name) {
            bail!("Username must be 3–16 ASCII letters, digits, or underscores");
        }

        let path = format!("{PROFILE}/name/{name}");
        let request = self
            .request(Method::PUT, &path)
            .header(CONTENT_TYPE, "application/json")
            .build()
            .map_err(|_| anyhow!("Could not prepare the name-change request"))?;
        Ok(PreparedClaim {
            name,
            expected_profile_id,
            request,
        })
    }

    pub async fn claim(&self, prepared: PreparedClaim<'_>) -> ClaimOutcome {
        let PreparedClaim {
            name,
            expected_profile_id,
            request,
        } = prepared;
        let response = match self.client.execute(request).await {
            Ok(response) => response,
            Err(error) => {
                let message = if error.is_timeout() {
                    "The name-change request timed out"
                } else {
                    "Could not complete the name-change request"
                };
                return ClaimOutcome::Uncertain(message.into());
            }
        };

        let status = response.status();
        match status {
            StatusCode::OK => match read_json::<Profile>(response).await {
                Some(profile)
                    if profile.name.eq_ignore_ascii_case(name)
                        && Uuid::try_parse(&profile.id).ok() == Some(expected_profile_id) =>
                {
                    ClaimOutcome::Claimed
                }
                _ => ClaimOutcome::Uncertain(
                    "Minecraft Services returned HTTP 200 without a matching Java profile".into(),
                ),
            },
            StatusCode::TOO_MANY_REQUESTS => ClaimOutcome::RateLimited(retry_after(&response)),
            StatusCode::UNAUTHORIZED => ClaimOutcome::Rejected(
                "Minecraft Services rejected the bearer token (HTTP 401). Supply a fresh token".into(),
            ),
            StatusCode::NOT_FOUND => ClaimOutcome::Rejected(
                "Minecraft Services could not find an existing Java profile for this account (HTTP 404)"
                    .into(),
            ),
            StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN | StatusCode::CONFLICT => {
                let reason = read_json::<Failure>(response)
                    .await
                    .and_then(|failure| failure.details)
                    .and_then(|details| details.status);

                match reason.as_deref() {
                    Some("DUPLICATE") => ClaimOutcome::Unavailable,
                    Some("NOT_ALLOWED") => ClaimOutcome::Rejected(format!(
                        "Minecraft Services disallowed the name change (HTTP {})",
                        status.as_u16()
                    )),
                    _ => ClaimOutcome::Rejected(format!(
                        "Minecraft Services refused the name change (HTTP {})",
                        status.as_u16()
                    )),
                }
            }
            _ if status.is_client_error() && status != StatusCode::REQUEST_TIMEOUT => {
                ClaimOutcome::Rejected(format!(
                    "Minecraft Services refused the name change (HTTP {})",
                    status.as_u16()
                ))
            }
            _ => ClaimOutcome::Uncertain(format!(
                "Minecraft Services returned HTTP {} without confirming the name change",
                status.as_u16()
            )),
        }
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.origin))
            .header(AUTHORIZATION, self.authorization.clone())
            .header(ACCEPT, "application/json")
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, operation: &str) -> Result<T> {
        let response = self
            .request(Method::GET, path)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    anyhow!("Timed out trying to {operation}")
                } else {
                    anyhow!("Could not reach Minecraft Services to {operation}")
                }
            })?;

        match response.status() {
            StatusCode::OK => read_json(response).await.ok_or_else(|| {
                anyhow!(
                    "Minecraft Services returned an invalid response while trying to {operation}"
                )
            }),
            StatusCode::UNAUTHORIZED => {
                bail!(
                    "Minecraft Services rejected the bearer token (HTTP 401). Supply a fresh token"
                )
            }
            StatusCode::NOT_FOUND => {
                bail!(
                    "Minecraft Services could not find an existing Java profile for this account (HTTP 404)"
                )
            }
            StatusCode::TOO_MANY_REQUESTS => Err(ReadRateLimit(retry_after(&response)).into()),
            status => bail!(
                "Minecraft Services returned HTTP {} while trying to {operation}",
                status.as_u16()
            ),
        }
    }
}

pub(crate) fn bearer_value(value: &str) -> &str {
    let value = value.trim_start();
    match value.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("Bearer ") => value[7..].trim(),
        _ => value.trim_end(),
    }
}

pub fn valid_username(name: &str) -> bool {
    (3..=16).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_profile(profile: &Profile) -> bool {
    !profile.name.is_empty()
        && !profile.name.chars().any(char::is_control)
        && Uuid::try_parse(&profile.id).is_ok()
}

async fn read_json<T: DeserializeOwned>(mut response: Response) -> Option<T> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MAX_JSON_BYTES - body.len() {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}

fn retry_after(response: &Response) -> Option<Duration> {
    let value = response.headers().get(RETRY_AFTER)?.to_str().ok()?.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }

    let until = httpdate::parse_http_date(value).ok()?;
    Some(until.duration_since(SystemTime::now()).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

    use super::*;

    const TEST_TOKEN: &str = "test-token";
    const TEST_PROFILE_ID: Uuid = uuid::uuid!("853c80ef-3c37-49fd-aa49-938b674adae6");
    const PROFILE_JSON: &str = r#"{"id":"853c80ef3c3749fdaa49938b674adae6","name":"Test_Name"}"#;

    fn test_minecraft(origin: String) -> Minecraft {
        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap();
        Minecraft::with_test_origin(client, format!("Bearer {TEST_TOKEN}"), origin).unwrap()
    }

    fn read_request(reader: &mut impl BufRead) -> String {
        let mut request = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                return request;
            }
            request.push_str(&line);
        }
    }

    fn mock(status: &str, headers: &str, body: &str) -> (Minecraft, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
            body.len()
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let request = read_request(&mut BufReader::new(&mut stream));
            stream.write_all(response.as_bytes()).unwrap();
            request
        });
        (test_minecraft(origin), server)
    }

    async fn send_claim(minecraft: &Minecraft, name: &str) -> ClaimOutcome {
        minecraft
            .claim(minecraft.prepare_claim(name, TEST_PROFILE_ID).unwrap())
            .await
    }

    #[tokio::test]
    async fn reads_own_java_profile_and_rename_eligibility() {
        let (minecraft, server) = mock("200 OK", "", PROFILE_JSON);
        let profile = minecraft.profile().await.unwrap();
        assert_eq!(profile.name, "Test_Name");
        assert_eq!(profile.id, "853c80ef3c3749fdaa49938b674adae6");
        assert!(
            server
                .join()
                .unwrap()
                .starts_with("GET /minecraft/profile HTTP/1.1")
        );

        let (minecraft, server) = mock("200 OK", "", r#"{"nameChangeAllowed":false}"#);
        assert!(!minecraft.eligible().await.unwrap());
        assert!(
            server
                .join()
                .unwrap()
                .starts_with("GET /minecraft/profile/namechange HTTP/1.1")
        );
    }

    #[tokio::test]
    async fn sends_a_bodyless_authorized_put_and_confirms_the_target() {
        let (minecraft, server) = mock("200 OK", "", PROFILE_JSON);
        let prepared = minecraft
            .prepare_claim("Test_Name", TEST_PROFILE_ID)
            .unwrap();
        assert!(prepared.request.body().is_none());
        assert!(prepared.request.headers()[AUTHORIZATION].is_sensitive());
        assert_eq!(minecraft.claim(prepared).await, ClaimOutcome::Claimed);

        let request = server.join().unwrap();
        assert!(request.starts_with("PUT /minecraft/profile/name/Test_Name HTTP/1.1\r\n"));
        let request = request.to_ascii_lowercase();
        assert!(request.contains("\r\nauthorization: bearer test-token\r\n"));
        assert!(request.contains("\r\ncontent-type: application/json\r\n"));
        assert!(!request.contains("transfer-encoding:"));
        assert!(!request.contains("content-length:") || request.contains("content-length: 0\r\n"));
    }

    #[tokio::test]
    async fn preflight_and_prepared_claim_share_one_http_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let minecraft = test_minecraft(format!("http://{}", listener.local_addr().unwrap()));
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let original = r#"{"id":"853c80ef3c3749fdaa49938b674adae6","name":"Original"}"#;
            for (request, body) in [
                ("GET /minecraft/profile HTTP/1.1\r\n", original),
                (
                    "GET /minecraft/profile/namechange HTTP/1.1\r\n",
                    r#"{"nameChangeAllowed":true}"#,
                ),
                (
                    "PUT /minecraft/profile/name/Test_Name HTTP/1.1\r\n",
                    PROFILE_JSON,
                ),
            ] {
                assert!(read_request(&mut reader).starts_with(request));
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });

        let prepared = minecraft
            .prepare_claim("Test_Name", TEST_PROFILE_ID)
            .unwrap();
        assert_eq!(minecraft.profile().await.unwrap().name, "Original");
        assert!(minecraft.eligible().await.unwrap());
        assert_eq!(minecraft.claim(prepared).await, ClaimOutcome::Claimed);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn retries_only_when_the_response_identifies_an_unavailable_name() {
        let (minecraft, _) = mock("403 Forbidden", "", r#"{"details":{"status":"DUPLICATE"}}"#);
        assert_eq!(
            send_claim(&minecraft, "Test_Name").await,
            ClaimOutcome::Unavailable
        );

        let (minecraft, _) = mock(
            "403 Forbidden",
            "",
            r#"{"details":{"status":"NOT_ALLOWED"}}"#,
        );
        assert!(matches!(
            send_claim(&minecraft, "Test_Name").await,
            ClaimOutcome::Rejected(_)
        ));

        let (minecraft, _) = mock("403 Forbidden", "", "<html>Gateway block</html>");
        assert!(matches!(
            send_claim(&minecraft, "Test_Name").await,
            ClaimOutcome::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn interprets_rate_limits_and_keeps_ambiguous_mutations_separate() {
        let (minecraft, _) = mock("429 Too Many Requests", "Retry-After: 41\r\n", "");
        assert_eq!(
            send_claim(&minecraft, "Test_Name").await,
            ClaimOutcome::RateLimited(Some(Duration::from_secs(41)))
        );

        let (minecraft, _) = mock(
            "429 Too Many Requests",
            "Retry-After: Thu, 01 Jan 1970 00:00:00 GMT\r\n",
            "",
        );
        assert_eq!(
            send_claim(&minecraft, "Test_Name").await,
            ClaimOutcome::RateLimited(Some(Duration::ZERO))
        );

        let (minecraft, _) = mock("200 OK", "", PROFILE_JSON);
        assert!(matches!(
            send_claim(&minecraft, "Other_Name").await,
            ClaimOutcome::Uncertain(_)
        ));

        let (minecraft, _) = mock("204 No Content", "", "");
        assert!(matches!(
            send_claim(&minecraft, "Test_Name").await,
            ClaimOutcome::Uncertain(_)
        ));

        let (minecraft, _) = mock("503 Service Unavailable", "", "");
        assert!(matches!(
            send_claim(&minecraft, "Test_Name").await,
            ClaimOutcome::Uncertain(_)
        ));
    }

    #[tokio::test]
    async fn matching_name_without_the_authorized_uuid_is_uncertain() {
        for body in [
            r#"{"id":"363c80ef3c3749fdaa49938b674adae6","name":"Test_Name"}"#,
            r#"{"name":"Test_Name"}"#,
        ] {
            let (minecraft, server) = mock("200 OK", "", body);
            assert!(matches!(
                send_claim(&minecraft, "Test_Name").await,
                ClaimOutcome::Uncertain(_)
            ));
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn rejects_unsafe_input_and_redacts_server_errors() {
        let minecraft = Minecraft::new(Client::new(), TEST_TOKEN.to_owned()).unwrap();
        assert!(minecraft.authorization.is_sensitive());
        assert!(
            minecraft
                .prepare_claim("Bad/Name", TEST_PROFILE_ID)
                .is_err()
        );

        let (minecraft, _) = mock(
            "401 Unauthorized",
            "",
            r#"{"developerMessage":"test-token"}"#,
        );
        let error = minecraft.profile().await.unwrap_err().to_string();
        assert!(error.contains("HTTP 401"));
        assert!(!error.contains(TEST_TOKEN));
        assert!(Minecraft::new(Client::new(), "token\r\nNew-Header: value".into()).is_err());
    }
}
