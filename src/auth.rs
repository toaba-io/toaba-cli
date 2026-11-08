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
