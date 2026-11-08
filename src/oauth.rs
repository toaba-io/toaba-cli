//! Toaba's OAuth endpoints for the one public client, `toaba-cli`, all through the `oauth2` crate:
//! the device login (RFC 8628), renewing a login's tokens (RFC 6749 §6), and revoking them
//! (RFC 7009). The crate revokes only over HTTPS, as RFC 7009 asks, so against a plain-HTTP
//! local stack a login can't be ended on Toaba; the CLI still forgets it.

use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use chrono::Utc;
use oauth2::basic::{BasicClient, BasicErrorResponseType, BasicTokenResponse};
use oauth2::{
    ClientId, DeviceAuthorizationUrl, DeviceCodeErrorResponseType, EndpointNotSet, EndpointSet,
    RefreshToken, RequestTokenError, RevocationUrl, StandardDeviceAuthorizationResponse,
    StandardRevocableToken, TokenResponse, TokenUrl,
};

use crate::{api, config};

/// The crate's client, with Toaba's device login, revocation and token endpoints set.
type Client = BasicClient<EndpointNotSet, EndpointSet, EndpointNotSet, EndpointSet, EndpointSet>;

fn client(api_url: &str) -> anyhow::Result<Client> {
    Ok(BasicClient::new(ClientId::new(api::CLIENT_ID.to_string()))
        .set_device_authorization_url(DeviceAuthorizationUrl::new(format!(
            "{api_url}/oauth/authorize_device"
        ))?)
        .set_revocation_url(RevocationUrl::new(format!("{api_url}/oauth/revoke"))?)
        .set_token_uri(TokenUrl::new(format!("{api_url}/oauth/token"))?))
}

/// A device login, waiting for the person to approve it.
pub struct DeviceLogin {
    api_url: String,
    details: StandardDeviceAuthorizationResponse,
}

impl DeviceLogin {
    /// The one-time code the person checks in the browser.
    pub fn code(&self) -> &str {
        self.details.user_code().secret()
    }

    /// Where to approve the login, with the code filled in when Toaba gives that URL.
    pub fn url(&self) -> &str {
        match self.details.verification_uri_complete() {
            Some(url) => url.secret(),
            None => self.details.verification_uri().as_str(),
        }
    }

    pub fn expires_in(&self) -> Duration {
        self.details.expires_in()
    }
}

pub async fn start_device_login(api_url: &str) -> anyhow::Result<DeviceLogin> {
    let details = client(api_url)?
        .exchange_device_code()
        .request_async(&api::http()?)
        .await
        .context("couldn't start the login")?;
    Ok(DeviceLogin {
        api_url: api_url.to_string(),
        details,
    })
}

/// Waits for the person to approve the login, polling as often as Toaba allows, and answers its
/// tokens.
pub async fn wait_for_approval(login: &DeviceLogin) -> anyhow::Result<config::Tokens> {
    let http = api::http()?;
    let result = client(&login.api_url)?
        .exchange_device_access_token(&login.details)
        .request_async(&http, tokio::time::sleep, None)
        .await;
    match result {
        Ok(token) => tokens(&token, None),
        Err(RequestTokenError::ServerResponse(response)) => Err(match response.error() {
            DeviceCodeErrorResponseType::AccessDenied => anyhow!("the login was denied"),
            DeviceCodeErrorResponseType::ExpiredToken => {
                anyhow!("the code expired before it was approved, run `toaba login` again")
            }
            _ => anyhow!("the login failed: {response}"),
        }),
        Err(err) => Err(anyhow::Error::new(err).context("the login failed")),
    }
}

/// New tokens for a login, from its refresh token. Each refresh replaces both; one Toaba no
/// longer accepts means the login has ended.
pub async fn refresh(
    api_url: &str,
    refresh_token: &str,
    org_name: &str,
) -> anyhow::Result<config::Tokens> {
    let result = client(api_url)?
        .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
        .request_async(&api::http()?)
        .await;
    match result {
        Ok(token) => tokens(&token, Some(refresh_token)),
        Err(RequestTokenError::ServerResponse(response))
            if *response.error() == BasicErrorResponseType::InvalidGrant =>
        {
            Err(api::Refused(format!(
                "your login to {org_name} has ended, run `toaba login` again"
            ))
            .into())
        }
        Err(err) => Err(anyhow::Error::new(err).context("couldn't renew your login")),
    }
}

/// Ends a login on Toaba by revoking its refresh token, which ends its access token too. Toaba
/// answers the same for a login that has already ended.
pub async fn revoke(api_url: &str, refresh_token: &str) -> anyhow::Result<()> {
    let token = StandardRevocableToken::RefreshToken(RefreshToken::new(refresh_token.to_string()));
    client(api_url)?
        .revoke_token(token)?
        .request_async(&api::http()?)
        .await?;
    Ok(())
}

/// The tokens of an answer, expiring as Toaba says. A refresh that sends no new refresh token
/// keeps the one it was given.
fn tokens(token: &BasicTokenResponse, given: Option<&str>) -> anyhow::Result<config::Tokens> {
    let refresh_token = match (token.refresh_token(), given) {
        (Some(new), _) => new.secret().clone(),
        (None, Some(given)) => given.to_string(),
        (None, None) => bail!("the login came without a refresh token"),
    };
    Ok(config::Tokens {
        access_token: token.access_token().secret().clone(),
        refresh_token,
        expire_time: Utc::now() + token.expires_in().unwrap_or_default(),
    })
}
