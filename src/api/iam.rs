//! Who and what a token reaches: the token itself, its org's apps, and an app's publishable keys.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::Client;

impl Client {
    /// The token in use.
    pub async fn current_token(&self) -> anyhow::Result<CurrentToken> {
        self.get("/iam/v1/access_tokens/current").await
    }

    /// The org's apps that the token reaches, by name.
    pub async fn apps(&self, org_id: &str) -> anyhow::Result<Vec<App>> {
        self.get(&format!("/iam/v1/orgs/{org_id}/apps")).await
    }

    /// The app's publishable keys, oldest first.
    pub async fn publishable_keys(&self, app_id: &str) -> anyhow::Result<Vec<PublishableKey>> {
        self.get(&format!("/iam/v1/apps/{app_id}/publishable_keys"))
            .await
    }
}

/// The fields of an access token that the CLI shows.
#[derive(Deserialize)]
pub struct CurrentToken {
    pub owner: Owner,
    pub org: Org,
}

#[derive(Deserialize)]
pub struct Owner {
    #[serde(deserialize_with = "super::plain_opt", default)]
    pub name: Option<String>,
    /// A person's; service accounts have none.
    #[serde(deserialize_with = "super::plain_opt", default)]
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
    #[serde(deserialize_with = "super::plain")]
    pub name: String,
}

/// The fields of an app that the CLI shows.
#[derive(Debug, Deserialize)]
pub struct App {
    pub id: String,
    #[serde(deserialize_with = "super::plain")]
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn names_lose_their_control_characters_as_they_are_read() {
        let org: Org = serde_json::from_value(json!({
            "id": "org_a", "name": "red \u{1b}[31mALERT\u{1b}[0m\nsecond line"
        }))
        .unwrap();
        assert_eq!(
            org.name,
            "red \u{fffd}[31mALERT\u{fffd}[0m\u{fffd}second line"
        );
    }
}
