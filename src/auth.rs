//! The token a command calls the API with, found as glab's `token_source.go` finds it:
//! `TOABA_TOKEN` as it is, for CI, or a login's, renewed when under 5 minutes are left.

use anyhow::Context;
use chrono::{TimeDelta, Utc};

use crate::{api, config, oauth};

/// How early a token counts as expired, so a command doesn't outlive it.
const GRACE: TimeDelta = TimeDelta::minutes(5);

pub struct Credential {
    pub api_url: String,
    pub token: String,
    pub source: Source,
}

pub enum Source {
    Env,
    Login { org_id: String, org_name: String },
}

impl Credential {
    /// A client for the API with this token. A refusal says what to do about it, which depends on
    /// where the token came from.
    pub fn client(&self) -> anyhow::Result<api::Client> {
        let refused = match &self.source {
            Source::Env => "TOABA_TOKEN was refused: it's unknown, expired or revoked".to_string(),
            Source::Login { org_name, .. } => {
                format!("your login to {org_name} has ended, run `toaba login` again")
            }
        };
        api::Client::new(&self.api_url, &self.token, refused)
    }

    /// The id of the token's org: the login's, or, for `TOABA_TOKEN`, the API's answer.
    pub async fn org_id(&self, client: &api::Client) -> anyhow::Result<String> {
        match &self.source {
            Source::Login { org_id, .. } => Ok(org_id.clone()),
            Source::Env => Ok(client.current_token().await?.org.id),
        }
    }
}

/// `TOABA_TOKEN`, when it's set. CI uses it, and it wins over every login.
pub fn from_env() -> Option<Credential> {
    let token = std::env::var("TOABA_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())?;
    Some(Credential {
        api_url: api::url(),
        token,
        source: Source::Env,
    })
}

/// The token of the org's login.
pub async fn for_login(org_id: String, login: config::Login) -> anyhow::Result<Credential> {
    let token = login_token(&org_id, &login).await?;
    Ok(Credential {
        api_url: login.api_url,
        token,
        source: Source::Login {
            org_id,
            org_name: login.org_name,
        },
    })
}

/// The login's access token, renewed under the config lock when it's about to expire.
async fn login_token(org_id: &str, login: &config::Login) -> anyhow::Result<String> {
    let missing = || {
        format!(
            "the tokens of {} are missing, run `toaba login` again",
            login.org_name
        )
    };
    let tokens = config::tokens(org_id)?.with_context(missing)?;
    if fresh(&tokens) {
        return Ok(tokens.access_token);
    }

    let _lock = config::lock()?;
    // Another toaba command may have renewed it while this one waited for the lock.
    let tokens = config::tokens(org_id)?.with_context(missing)?;
    if fresh(&tokens) {
        return Ok(tokens.access_token);
    }

    let renewed = oauth::refresh(&login.api_url, &tokens.refresh_token, &login.org_name).await?;
    let access_token = renewed.access_token.clone();
    config::save_tokens(org_id, renewed)?;
    Ok(access_token)
}

fn fresh(tokens: &config::Tokens) -> bool {
    Utc::now() + GRACE < tokens.expire_time
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// Saves a login to `org_a` at `api_url`, with a token expiring `minutes` from now, in a config
    /// folder that lasts as long as the first value returned.
    fn saved_login(api_url: &str, minutes: i64) -> (config::TestDir, config::Login) {
        let dir = config::use_test_dir();
        let login = config::Login {
            api_url: api_url.to_string(),
            org_name: "Acme".into(),
            email: "dev@toaba.io".into(),
            apps: Default::default(),
        };
        let tokens = config::Tokens {
            access_token: "toaba_oat_old".into(),
            refresh_token: "toaba_ort_old".into(),
            expire_time: Utc::now() + TimeDelta::minutes(minutes),
        };
        config::save_login("org_a", login.clone(), tokens).unwrap();
        (dir, login)
    }

    #[tokio::test]
    async fn a_token_with_time_left_is_used_as_it_is() {
        // Nothing listens there: any request would fail the test.
        let (_dir, login) = saved_login("http://127.0.0.1:9", 60);
        let credential = for_login("org_a".into(), login).await.unwrap();
        assert_eq!(credential.token, "toaba_oat_old");
    }

    #[tokio::test]
    async fn a_token_about_to_expire_is_renewed_and_saved() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=toaba_ort_old"))
            .and(body_string_contains("client_id=toaba-cli"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "toaba_oat_new",
                "token_type": "Bearer",
                "expires_in": 7200,
                "refresh_token": "toaba_ort_new",
            })))
            .expect(1)
            .mount(&server)
            .await;
        // Under the 5 minutes of grace, so it counts as expired.
        let (_dir, login) = saved_login(&server.uri(), 2);

        let credential = for_login("org_a".into(), login.clone()).await.unwrap();
        assert_eq!(credential.token, "toaba_oat_new");
        let saved = config::tokens("org_a").unwrap().unwrap();
        assert_eq!(saved.refresh_token, "toaba_ort_new");
        assert!(saved.expire_time > Utc::now() + TimeDelta::minutes(100));

        // The renewed token is used from then on, with no second refresh.
        let again = for_login("org_a".into(), login).await.unwrap();
        assert_eq!(again.token, "toaba_oat_new");
    }

    #[tokio::test]
    async fn a_refresh_toaba_refuses_means_the_login_has_ended() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(json!({ "error": "invalid_grant" })),
            )
            .mount(&server)
            .await;
        let (_dir, login) = saved_login(&server.uri(), -5);

        let err = for_login("org_a".into(), login).await.err().unwrap();
        assert!(err.is::<api::Refused>());
        assert_eq!(
            err.to_string(),
            "your login to Acme has ended, run `toaba login` again"
        );
        // The old tokens stay, for `logout` to revoke.
        let saved = config::tokens("org_a").unwrap().unwrap();
        assert_eq!(saved.refresh_token, "toaba_ort_old");
    }

    #[tokio::test]
    async fn a_login_without_tokens_says_to_log_in_again() {
        let (_dir, login) = saved_login("http://127.0.0.1:9", 60);
        let err = for_login("org_missing".into(), login).await.err().unwrap();
        assert_eq!(
            err.to_string(),
            "the tokens of Acme are missing, run `toaba login` again"
        );
    }
}
