//! The Toaba API, called with a token: its URL, the HTTP client every call shares, and `Client`,
//! which commands make their calls through. Answers come as `{"data": …}` and errors as
//! `{"error": "<code>"}`; `Client` unwraps the first and turns the second into `Error`, with one
//! wording per code, so every command reports problems the same way.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use reqwest::header::{ACCEPT, RETRY_AFTER};
use reqwest::{Method, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The CLI's OAuth client id, which Toaba knows.
pub const CLIENT_ID: &str = "toaba-cli";

/// The one Toaba API that customers use.
pub const PRODUCTION_URL: &str = "https://api.toaba.io";

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

    #[allow(dead_code, reason = "the foundation for the commands to come")]
    pub async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> anyhow::Result<T> {
        self.call(Method::POST, path, Some(body)).await
    }

    #[allow(dead_code, reason = "the foundation for the commands to come")]
    pub async fn put<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> anyhow::Result<T> {
        self.call(Method::PUT, path, Some(body)).await
    }

    #[allow(dead_code, reason = "the foundation for the commands to come")]
    pub async fn patch<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> anyhow::Result<T> {
        self.call(Method::PATCH, path, Some(body)).await
    }

    #[allow(dead_code, reason = "the foundation for the commands to come")]
    /// `DELETE path`. Its answer, such as a 202 or a 204, has nothing to read.
    pub async fn delete(&self, path: &str) -> anyhow::Result<()> {
        self.send(Method::DELETE, path, None::<&()>).await?;
        Ok(())
    }

    /// The token in use.
    pub async fn current_token(&self) -> anyhow::Result<CurrentToken> {
        self.get("/iam/v1/access_tokens/current").await
    }

    /// The org's apps that the token reaches, by name.
    pub async fn apps(&self, org_id: &str) -> anyhow::Result<Vec<App>> {
        self.get(&format!("/iam/v1/orgs/{org_id}/apps")).await
    }

    /// The environments of the app's assets, by name.
    pub async fn environments(&self, app_id: &str) -> anyhow::Result<Vec<Environment>> {
        self.get(&format!("/storage/v1/apps/{app_id}/environments"))
            .await
    }

    /// The environment's buckets, by name.
    pub async fn buckets(&self, app_id: &str, environment_id: &str) -> anyhow::Result<Vec<Bucket>> {
        self.get(&format!(
            "/storage/v1/apps/{app_id}/environments/{environment_id}/buckets"
        ))
        .await
    }

    /// The app's publishable keys, oldest first.
    pub async fn publishable_keys(&self, app_id: &str) -> anyhow::Result<Vec<PublishableKey>> {
        self.get(&format!("/iam/v1/apps/{app_id}/publishable_keys"))
            .await
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> anyhow::Result<T> {
        let response = self.send(method, path, body).await?;
        let answer: Data<T> = response
            .json()
            .await
            .with_context(|| format!("the API's answer to {path} wasn't what the CLI expects"))?;
        Ok(answer.data)
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> anyhow::Result<Response> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.api_url))
            .bearer_auth(&self.token)
            .header(ACCEPT, "application/json");
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

#[derive(Deserialize)]
struct Data<T> {
    data: T,
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
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
    #[serde(default)]
    errors: BTreeMap<String, Vec<String>>,
}

impl Error {
    async fn read(response: Response) -> Self {
        let status = response.status();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok());
        let (code, fields) = match response.json::<ErrorBody>().await {
            Ok(body) => (Some(body.error), body.errors),
            Err(_) => (None, BTreeMap::new()),
        };
        Self {
            status,
            code,
            fields,
            retry_after,
        }
    }

    #[allow(dead_code, reason = "the foundation for the commands to come")]
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

/// The fields of an access token that the CLI shows.
#[derive(Deserialize)]
pub struct CurrentToken {
    pub owner: Owner,
    pub org: Org,
}

#[derive(Deserialize)]
pub struct Owner {
    pub name: Option<String>,
    /// A person's; service accounts have none.
    pub email: Option<String>,
}

impl Owner {
    /// A person's email, or a service account's name.
    pub fn label(&self) -> &str {
        self.email
            .as_deref()
            .or(self.name.as_deref())
            .unwrap_or("unknown")
    }
}

#[derive(Deserialize)]
pub struct Org {
    pub id: String,
    pub name: String,
}

/// The fields of an app that the CLI shows.
#[derive(Debug, Deserialize)]
pub struct App {
    pub id: String,
    pub name: String,
    pub org_id: String,
    /// When it was scheduled for deletion.
    pub delete_time: Option<DateTime<Utc>>,
    /// When an app scheduled for deletion goes for good.
    pub expire_time: Option<DateTime<Utc>>,
}

/// A publishable key: public, it ships inside game clients and identifies the app at the gateway.
/// The dashboard creates and revokes them; the CLI only reads them.
#[derive(Debug, Deserialize, Serialize)]
pub struct PublishableKey {
    pub id: String,
    pub key: String,
    pub create_time: DateTime<Utc>,
}

/// An environment of an app's assets, such as `production`.
#[derive(Debug, Deserialize)]
pub struct Environment {
    pub id: String,
    pub name: String,
}

/// A bucket of files in an environment.
#[derive(Debug, Deserialize)]
pub struct Bucket {
    pub id: String,
    pub name: String,
    /// `open`, or `promotion_only` for a bucket whose files and releases arrive only by promotion.
    pub write_access: String,
    pub release_count: u64,
    /// How many paths differ from the release `latest` points at.
    pub unreleased_count: u64,
    /// Ordered by id. Game clients download a bucket's files by tag.
    pub tags: Vec<Tag>,
}

/// A name for one of a bucket's releases, such as `latest` or `stable`.
#[derive(Debug, Deserialize)]
pub struct Tag {
    pub id: String,
    pub release: ReleaseRef,
}

#[derive(Debug, Deserialize)]
pub struct ReleaseRef {
    pub id: String,
    /// Counts up from 1 in each bucket; people know a release by it.
    pub release_num: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{body_json, header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn client(server: &MockServer) -> Client {
        Client::new(&server.uri(), "toaba_pat_test", "the token was refused").unwrap()
    }

    fn message(err: anyhow::Error) -> String {
        format!("{err:#}")
    }

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
    async fn a_delete_reads_nothing_from_its_answer() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        client(&server)
            .await
            .delete("/iam/v1/publishable_keys/pk_a")
            .await
            .unwrap();
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
    async fn a_rate_limit_says_how_long_to_wait() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "30")
                    .set_body_json(json!({ "error": "rate_limited" })),
            )
            .mount(&server)
            .await;

        let err = client(&server).await.get::<App>("/x").await.unwrap_err();
        assert_eq!(message(err), "too many requests, try again in 30 seconds");
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
