//! The Toaba API, called with a token: its URL, the HTTP client every call shares, and `Client`,
//! which commands make their calls through. Answers come as `{"data": …}` and errors as
//! `{"error": "<code>"}`; `Client` unwraps the first and turns the second into `Error`, with one
//! wording per code, so every command reports problems the same way.

mod iam;
mod storage;

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use anyhow::Context as _;
use indicatif::HumanBytes;
use reqwest::header::{ACCEPT, RETRY_AFTER};
use reqwest::{Method, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use iam::*;
pub use storage::*;

/// The CLI's OAuth client id, which Toaba knows.
pub const CLIENT_ID: &str = "toaba-cli";

/// The one Toaba API that customers use.
pub const PRODUCTION_URL: &str = "https://api.toaba.io";

/// How many times a call waits when the API asks it to slow down, and the longest wait it accepts.
/// A longer one fails at once, so a command never sits for minutes without saying why.
const RATE_LIMIT_WAITS: u32 = 3;
const RATE_LIMIT_WAIT_MAX: u64 = 60;

/// How long a call may take in all, as glab's `auth status` allows.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The Toaba API. `TOABA_API_URL` points it elsewhere for development, such as a
/// local stack at `http://localhost:9080`.
pub fn url() -> String {
    let url = std::env::var("TOABA_API_URL").unwrap_or_else(|_| PRODUCTION_URL.into());
    url.trim_end_matches('/').to_string()
}

/// The HTTP client every call shares. It names the CLI and its version, doesn't follow redirects,
/// which would let a server send a request and its token elsewhere, and gives up after 10 seconds
/// without a connection and `TIMEOUT` in all, so a stuck API never leaves a command spinning.
pub fn http() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("toaba-cli/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(TIMEOUT)
        .build()?)
}

/// Calls the Toaba API with one token.
pub struct Client {
    http: reqwest::Client,
    api_url: String,
    token: String,
    /// What to say when the API refuses the token, which depends on where it came from.
    refused: String,
}

impl Client {
    pub fn new(api_url: &str, token: &str, refused: impl Into<String>) -> anyhow::Result<Self> {
        Ok(Self {
            http: http()?,
            api_url: api_url.to_string(),
            token: token.to_string(),
            refused: refused.into(),
        })
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        self.call(Method::GET, path, None::<&()>).await
    }

    pub async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> anyhow::Result<T> {
        self.call(Method::POST, path, Some(body)).await
    }

    pub async fn put<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> anyhow::Result<T> {
        self.call(Method::PUT, path, Some(body)).await
    }

    /// `POST path`, for an answer with nothing to read.
    pub async fn post_empty(&self, path: &str, body: &impl Serialize) -> anyhow::Result<()> {
        self.send(Method::POST, path, &[], Some(body)).await?;
        Ok(())
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> anyhow::Result<T> {
        let response = self.send(method, path, &[], body).await?;
        Ok(decode::<Data<T>>(response, path).await?.data)
    }

    /// `GET path` for an answer that isn't only `data`, such as a page with its next token.
    async fn get_whole<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<T> {
        let response = self.send(Method::GET, path, query, None::<&()>).await?;
        decode(response, path).await
    }

    /// `POST path` for something that must happen once, such as cutting a release. Sent again, up
    /// to three times, when no answer arrives: the idempotency key keeps that from doing it twice.
    async fn post_once<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> anyhow::Result<T> {
        let key = idempotency_key(path);
        let mut attempt = 1;
        loop {
            let sent = self
                .send_keyed(Method::POST, path, &[], Some(body), Some(&key))
                .await;
            match sent {
                Ok(response) => return Ok(decode::<Data<T>>(response, path).await?.data),
                // The API answered, with a refusal: sending again wouldn't change it.
                Err(err) if err.is::<Error>() || err.is::<Refused>() || attempt == 3 => {
                    return Err(err);
                }
                Err(_) => attempt += 1,
            }
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&impl Serialize>,
    ) -> anyhow::Result<Response> {
        self.send_keyed(method, path, query, body, None).await
    }

    /// `send`, with an `Idempotency-Key` when given: the API then answers a repeat of the request
    /// with its first answer, so it's safe to send again when no answer arrived.
    async fn send_keyed(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&impl Serialize>,
        idempotency_key: Option<&str>,
    ) -> anyhow::Result<Response> {
        let mut waits = 0;
        loop {
            let sent = self
                .send_once(method.clone(), path, query, body, idempotency_key)
                .await;
            // Rate limited: wait as long as the API says, a few times, rather than fail.
            let wait = sent
                .as_ref()
                .err()
                .and_then(|err| err.downcast_ref::<Error>())
                .and_then(Error::wait);
            match wait {
                Some(seconds) if waits < RATE_LIMIT_WAITS => {
                    waits += 1;
                    crate::progress::note(&format!(
                        "Toaba asked to slow down, continuing in {seconds} seconds…"
                    ));
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                }
                _ => return sent,
            }
        }
    }

    async fn send_once(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&impl Serialize>,
        idempotency_key: Option<&str>,
    ) -> anyhow::Result<Response> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.api_url))
            .query(query)
            .bearer_auth(&self.token)
            .header(ACCEPT, "application/json");
        if let Some(key) = idempotency_key {
            request = request.header("Idempotency-Key", key);
        }
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request.send().await.map_err(|err| {
            if err.is_timeout() {
                anyhow::anyhow!(
                    "the Toaba API at {} didn't answer within {} seconds",
                    self.api_url,
                    TIMEOUT.as_secs()
                )
            } else {
                anyhow::Error::new(err)
                    .context(format!("couldn't reach the Toaba API at {}", self.api_url))
            }
        })?;
        match response.status() {
            status if status.is_success() => Ok(response),
            StatusCode::UNAUTHORIZED => Err(Refused(self.refused.clone()).into()),
            _ => Err(Error::read(response).await.into()),
        }
    }
}

/// Reads an answer's JSON as `T`.
async fn decode<T: DeserializeOwned>(response: Response, path: &str) -> anyhow::Result<T> {
    response
        .json()
        .await
        .with_context(|| format!("the API's answer to {path} wasn't what the CLI expects"))
}

/// A key no other request has: the time, this process and the path, hashed.
fn idempotency_key(path: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    Sha256::digest(format!("{nanos}-{}-{path}", std::process::id()))
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Deserialize)]
struct Data<T> {
    data: T,
}

/// One page of a list, with the token that fetches the next.
#[derive(Deserialize)]
struct Page<T> {
    data: Vec<T>,
    next_page_token: Option<String>,
}

/// The API refused the token, or Toaba ended the login: the token is unknown, expired or revoked.
/// It carries what to tell the person, which depends on where the token came from.
#[derive(Debug)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// An error answer: its status, its code when it has one, the fields that failed validation, and,
/// for a 429, how many seconds to wait. Its wording is the CLI's for each code people meet; a
/// command that wants its own matches the code with `is`.
#[derive(Debug)]
pub struct Error {
    pub status: StatusCode,
    pub code: Option<String>,
    pub fields: BTreeMap<String, Vec<String>>,
    pub retry_after: Option<u64>,
    /// What else the answer carries, such as the byte counts of `storage_full`.
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
    #[serde(default)]
    errors: BTreeMap<String, Vec<String>>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

impl Error {
    async fn read(response: Response) -> Self {
        let status = response.status();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok());
        let (code, fields, extra) = match response.json::<ErrorBody>().await {
            Ok(body) => (Some(body.error), body.errors, body.extra),
            Err(_) => (None, BTreeMap::new(), BTreeMap::new()),
        };
        Self {
            status,
            code,
            fields,
            retry_after,
            extra,
        }
    }

    /// How many seconds to wait before trying again, when the API said so and it's short enough
    /// to sit through.
    fn wait(&self) -> Option<u64> {
        let seconds = self.retry_after?;
        (self.status == StatusCode::TOO_MANY_REQUESTS && seconds <= RATE_LIMIT_WAIT_MAX)
            .then_some(seconds.max(1))
    }

    pub fn is(&self, code: &str) -> bool {
        self.code.as_deref() == Some(code)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code.as_deref() {
            Some("forbidden") => f.write_str("your login isn't allowed to do that"),
            Some("not_found") => f.write_str("not found, or your login can't see it"),
            Some("org_restricted") => {
                f.write_str("the org has no active subscription, so it can't grow until it has")
            }
            Some("limits_unknown") => {
                f.write_str("the org's plan hasn't reached Toaba yet, try again in a minute")
            }
            Some("limit_reached") => f.write_str("the org has reached its plan's limit"),
            Some("name_taken") => f.write_str("that name is taken"),
            Some("promotion_only") => f.write_str(
                "the bucket only takes promoted releases, so its files can't be changed directly",
            ),
            Some("stale_tag") => f.write_str("the tag was moved meanwhile, run the command again"),
            Some("files_changed") => {
                f.write_str("the target bucket's files changed meanwhile, run the command again")
            }
            Some("stale_entries") => {
                f.write_str("the bucket's files changed meanwhile, run the command again")
            }
            Some("storage_unavailable") => {
                f.write_str("Toaba's file storage isn't answering, try again shortly")
            }
            Some("storage_full") => {
                let bytes = |key: &str| self.extra.get(key).and_then(|value| value.as_u64());
                match (
                    bytes("incoming_bytes"),
                    bytes("storage_max_bytes"),
                    bytes("stored_bytes"),
                    bytes("reserved_bytes"),
                ) {
                    (Some(incoming), Some(max), Some(stored), Some(reserved)) => write!(
                        f,
                        "the org's storage is full: this needs {}, and {} of {} is free",
                        HumanBytes(incoming),
                        HumanBytes(max.saturating_sub(stored + reserved)),
                        HumanBytes(max)
                    ),
                    _ => f.write_str("the org's storage is full"),
                }
            }
            Some("scheduled_for_deletion") => {
                f.write_str("it's scheduled for deletion, so it can't change until it's restored")
            }
            Some("rate_limited") => match self.retry_after {
                Some(seconds) => write!(f, "too many requests, try again in {seconds} seconds"),
                None => f.write_str("too many requests, try again shortly"),
            },
            Some("validation_failed") => {
                let fields: Vec<String> = self
                    .fields
                    .iter()
                    .map(|(field, messages)| format!("{field} {}", messages.join(", ")))
                    .collect();
                write!(f, "the API didn't accept it: {}", fields.join("; "))
            }
            Some(code) => write!(f, "the API answered {}: {code}", self.status),
            None => write!(f, "the API answered {}", self.status),
        }
    }
}

impl std::error::Error for Error {}

/// Reads a name someone chose, with any control characters replaced. Left in, a name holding
/// terminal escape codes or line breaks could redraw the screen or pass for the CLI's own output.
fn plain<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(crate::output::plain(&String::deserialize(deserializer)?))
}

fn plain_opt<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let text = Option::<String>::deserialize(deserializer)?;
    Ok(text.map(|text| crate::output::plain(&text)))
}

/// What the API tests share.
#[cfg(test)]
mod testing {
    use wiremock::MockServer;

    use super::Client;

    pub async fn client(server: &MockServer) -> Client {
        Client::new(&server.uri(), "toaba_pat_test", "the token was refused").unwrap()
    }

    pub fn message(err: anyhow::Error) -> String {
        format!("{err:#}")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{body_json, header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::testing::{client, message};
    use super::*;

    #[tokio::test]
    async fn a_call_sends_the_token_and_unwraps_the_data() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/iam/v1/apps/app_a"))
            .and(header("authorization", "Bearer toaba_pat_test"))
            .and(header("accept", "application/json"))
            .and(header_exists("user-agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "app_a", "name": "Space Shooter", "org_id": "org_a" }
            })))
            .mount(&server)
            .await;

        let app: App = client(&server)
            .await
            .get("/iam/v1/apps/app_a")
            .await
            .unwrap();
        assert_eq!(app.name, "Space Shooter");
        assert!(app.delete_time.is_none());
    }

    #[tokio::test]
    async fn a_post_sends_its_body_as_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/iam/v1/orgs/org_a/apps"))
            .and(body_json(json!({ "name": "Puzzle" })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": { "id": "app_b", "name": "Puzzle", "org_id": "org_a" }
            })))
            .mount(&server)
            .await;

        let app: App = client(&server)
            .await
            .post("/iam/v1/orgs/org_a/apps", &json!({ "name": "Puzzle" }))
            .await
            .unwrap();
        assert_eq!(app.id, "app_b");
    }

    #[tokio::test]
    async fn a_refused_token_says_what_to_do() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({ "error": "unauthenticated" })),
            )
            .mount(&server)
            .await;

        let err = client(&server).await.get::<App>("/x").await.unwrap_err();
        assert!(err.is::<Refused>());
        assert_eq!(message(err), "the token was refused");
    }

    #[tokio::test]
    async fn an_error_code_gets_the_cli_s_wording_and_stays_matchable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(409).set_body_json(json!({ "error": "name_taken" })),
            )
            .mount(&server)
            .await;

        let err = client(&server)
            .await
            .post::<App>("/x", &json!({}))
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<Error>().unwrap().is("name_taken"));
        assert_eq!(message(err), "that name is taken");
    }

    #[tokio::test]
    async fn validation_errors_name_their_fields() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(422).set_body_json(json!({
                "error": "validation_failed",
                "errors": { "name": ["can't be blank"] }
            })))
            .mount(&server)
            .await;

        let err = client(&server)
            .await
            .post::<App>("/x", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(
            message(err),
            "the API didn't accept it: name can't be blank"
        );
    }

    #[tokio::test]
    async fn a_long_rate_limit_fails_and_says_how_long_to_wait() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "300")
                    .set_body_json(json!({ "error": "rate_limited" })),
            )
            .mount(&server)
            .await;

        let err = client(&server).await.get::<App>("/x").await.unwrap_err();
        assert_eq!(message(err), "too many requests, try again in 300 seconds");
    }

    #[tokio::test]
    async fn a_short_rate_limit_is_waited_out() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "1")
                    .set_body_json(json!({ "error": "rate_limited" })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "app_a", "name": "Space Shooter", "org_id": "org_a" }
            })))
            .mount(&server)
            .await;

        let app: App = client(&server).await.get("/x").await.unwrap();
        assert_eq!(app.id, "app_a");
    }

    #[tokio::test]
    async fn an_answer_without_a_code_names_the_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"))
            .mount(&server)
            .await;

        let err = client(&server).await.get::<App>("/x").await.unwrap_err();
        assert_eq!(message(err), "the API answered 502 Bad Gateway");
    }
}
