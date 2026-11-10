//! The app a command acts on: `--app` or `TOABA_APP`, else the app set with `toaba app use`, else,
//! at a terminal, one picked from a list. An app belongs to one org for its whole life, so the app
//! leads to its org's login. Each login keeps the apps it reaches, so finding an app costs no API
//! call unless it's new or renamed since.

use std::collections::BTreeMap;

use anyhow::{Context as _, bail};

use crate::{api, auth, config, output, progress, prompt};

#[derive(clap::Args)]
pub struct AppArg {
    /// The app, by name or id [default: TOABA_APP, then the app in use]
    #[arg(long, env = "TOABA_APP")]
    app: Option<String>,
}

/// The app a command acts on, and how to reach it.
pub struct Target {
    pub client: api::Client,
    pub app_id: String,
    /// The app's name, or its id where the CLI doesn't know the name.
    pub app_name: String,
}

/// An app one of your logins reaches.
pub struct Reached {
    pub org_id: String,
    pub org_name: String,
    pub app_id: String,
    pub app_name: String,
}

/// A bucket the app is known to have: its name, what the API says about it, and its calls, which
/// it derefs to.
pub struct Bucket<'a> {
    pub name: BucketRef,
    pub info: api::Bucket,
    api: api::BucketApi<'a>,
}

impl<'a> std::ops::Deref for Bucket<'a> {
    type Target = api::BucketApi<'a>;

    fn deref(&self) -> &Self::Target {
        &self.api
    }
}

impl Bucket<'_> {
    /// Fails for a bucket that only takes promoted releases, which `doing` (uploading files, say)
    /// can't change, before any work is done on it.
    pub fn require_open(&self, doing: &str) -> anyhow::Result<()> {
        if self.info.write_access == "promotion_only" {
            bail!(
                "{self} only takes promoted releases, so {doing} there isn't possible: \
                 use `toaba assets promote`"
            );
        }
        Ok(())
    }
}

impl std::fmt::Display for Bucket<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.name.fmt(f)
    }
}

impl Target {
    /// The app's bucket. Fails when the app has none by that name, naming the ones it has.
    pub async fn bucket(&self, name: &BucketRef) -> anyhow::Result<Bucket<'_>> {
        if let Some(bucket) = self.find_bucket(name).await? {
            return Ok(bucket);
        }
        let known: Vec<String> = self
            .buckets()
            .await?
            .iter()
            .map(|(environment, bucket)| format!("{}/{}", environment.id, bucket.id))
            .collect();
        if known.is_empty() {
            bail!(
                "{} has no buckets yet, create one in the dashboard",
                self.app_name
            );
        }
        bail!(
            "{} has no bucket {name}, it has: {}",
            self.app_name,
            known.join(", ")
        )
    }

    /// The app's bucket, if it has one by that name, in an environment it may not have.
    pub async fn find_bucket(&self, name: &BucketRef) -> anyhow::Result<Option<Bucket<'_>>> {
        Ok(self
            .buckets_in(&name.environment_id)
            .await?
            .into_iter()
            .find(|bucket| bucket.id == name.bucket_id)
            .map(|info| self.known(name, info)))
    }

    /// The buckets of an environment, by name: none if the app has no such environment.
    pub async fn buckets_in(&self, environment_id: &str) -> anyhow::Result<Vec<api::Bucket>> {
        match self.client.buckets(&self.app_id, environment_id).await {
            Err(err) if api::answered(&err, "not_found") => Ok(Vec::new()),
            listed => listed,
        }
    }

    /// Creates the bucket, and its environment, called `environment_name`, if the app has none by
    /// that id. One made meanwhile, by another run of the same pipeline say, will do as well.
    pub async fn create_bucket(
        &self,
        name: &BucketRef,
        environment_name: &str,
    ) -> anyhow::Result<Bucket<'_>> {
        let (app, environment, bucket) = (&self.app_id, &name.environment_id, &name.bucket_id);
        let created = match self.client.create_bucket(app, environment, bucket).await {
            Err(err) if api::answered(&err, "not_found") => {
                match self
                    .client
                    .create_environment(app, environment, environment_name)
                    .await
                {
                    Err(err) if !api::answered(&err, "environment_exists") => return Err(err),
                    _ => self.client.create_bucket(app, environment, bucket).await,
                }
            }
            created => created,
        };
        match created {
            Ok(info) => Ok(self.known(name, info)),
            Err(err) if api::answered(&err, "bucket_exists") => self.bucket(name).await,
            Err(err) => Err(err),
        }
    }

    fn known(&self, name: &BucketRef, info: api::Bucket) -> Bucket<'_> {
        Bucket {
            api: self
                .client
                .bucket(&self.app_id, &name.environment_id, &name.bucket_id),
            name: name.clone(),
            info,
        }
    }

    /// Every bucket of the app, with its environment, by environment and then bucket.
    pub async fn buckets(&self) -> anyhow::Result<Vec<(api::Environment, api::Bucket)>> {
        let mut buckets = Vec::new();
        for environment in self.client.environments(&self.app_id).await? {
            for bucket in self.client.buckets(&self.app_id, &environment.id).await? {
                buckets.push((environment.clone(), bucket));
            }
        }
        Ok(buckets)
    }
}

pub async fn app(arg: AppArg) -> anyhow::Result<Target> {
    if let Some(credential) = auth::from_env() {
        return from_env(credential, arg.app).await;
    }
    let (org_id, app_id) = match arg.app.or(config::app()?) {
        Some(app) => find(&app).await?,
        None if prompt::interactive() => {
            let picked = pick(true).await?;
            if prompt::confirm(&format!("Use {} from now on?", picked.app_name), true)? {
                config::use_app(&picked.app_id)?;
            }
            (picked.org_id, picked.app_id)
        }
        None => bail!("no app given: pass --app, set TOABA_APP, or pick one with `toaba app use`"),
    };

    let login = config::logins()?
        .remove(&org_id)
        .context("the app's login went away, try again")?;
    let app_name = login
        .apps
        .get(&app_id)
        .cloned()
        .unwrap_or_else(|| app_id.clone());
    let credential = auth::for_login(org_id, login).await?;
    let client = credential.client()?;
    Ok(Target {
        client,
        app_id,
        app_name,
    })
}

/// The app among TOABA_TOKEN's, which are its org's alone. Nothing is remembered: CI says which
/// app each time.
async fn from_env(credential: auth::Credential, app: Option<String>) -> anyhow::Result<Target> {
    let client = credential.client()?;
    let (app_id, app_name) = match app {
        Some(app) if is_id(&app) => (app.clone(), app),
        Some(app) => {
            let apps = client.apps(&credential.org_id(&client).await?).await?;
            let found = apps
                .into_iter()
                .find(|known| known.name == app)
                .with_context(|| format!("TOABA_TOKEN reaches no app called {app}"))?;
            (found.id, found.name)
        }
        None if prompt::interactive() => {
            let mut apps = client.apps(&credential.org_id(&client).await?).await?;
            if apps.is_empty() {
                bail!("TOABA_TOKEN reaches no app yet");
            }
            let names: Vec<String> = apps.iter().map(|app| app.name.clone()).collect();
            let picked = apps.swap_remove(prompt::select("Which app?", &names, 0, true)?);
            (picked.id, picked.name)
        }
        None => bail!("no app given: pass --app or set TOABA_APP"),
    };
    Ok(Target {
        client,
        app_id,
        app_name,
    })
}

/// The org and id of `app`, an app id or name among the apps your logins reach. A miss reads them
/// all again once, for an app that's new or renamed since.
pub async fn find(app: &str) -> anyhow::Result<(String, String)> {
    if let Some(found) = lookup(&reached(&config::logins()?), app)? {
        return Ok(found);
    }
    refresh_all().await;
    lookup(&reached(&config::logins()?), app)?.with_context(|| {
        format!("none of your logins reaches an app called {app}, see `toaba app list`")
    })
}

/// The org and id of `app` among `apps`. A name is unique within an org but not across orgs, so a
/// name two orgs use fails, listing their ids. Case counts unless only one name matches without it.
fn lookup(apps: &[Reached], app: &str) -> anyhow::Result<Option<(String, String)>> {
    let mut matches: Vec<&Reached> = if is_id(app) {
        apps.iter().filter(|known| known.app_id == app).collect()
    } else {
        let exact: Vec<&Reached> = apps.iter().filter(|known| known.app_name == app).collect();
        if exact.is_empty() {
            let wanted = app.to_lowercase();
            apps.iter()
                .filter(|known| known.app_name.to_lowercase() == wanted)
                .collect()
        } else {
            exact
        }
    };
    match matches.len() {
        0 => Ok(None),
        1 => {
            let known = matches.remove(0);
            Ok(Some((known.org_id.clone(), known.app_id.clone())))
        }
        n => {
            let ids: Vec<String> = matches
                .iter()
                .map(|known| format!("{} in {}", known.app_id, known.org_name))
                .collect();
            bail!(
                "{n} of your apps are called {app}, pick one by its id: {}",
                ids.join(", ")
            )
        }
    }
}

/// Asks which app, among every app your logins reach, starting on the one in use. A picker is
/// where people look for an app they've just made, so the apps are read fresh. With `report`, the
/// pick stays on screen.
pub async fn pick(report: bool) -> anyhow::Result<Reached> {
    if config::logins()?.is_empty() {
        bail!("you're not logged in, run `toaba login`");
    }
    let line = progress::start("Loading your apps…");
    refresh_all().await;
    line.finish_and_clear();

    let mut apps = reached(&config::logins()?);
    if apps.is_empty() {
        bail!("none of your logins reaches an app yet, create one in the dashboard");
    }
    let width = output::width("", apps.iter().map(|app| &app.app_name));
    let labels: Vec<String> = apps
        .iter()
        .map(|app| format!("{}  {}", output::pad(&app.app_name, width), app.org_name))
        .collect();
    let in_use = config::app()?;
    let start = apps
        .iter()
        .position(|app| in_use.as_deref() == Some(app.app_id.as_str()))
        .unwrap_or(0);
    Ok(apps.swap_remove(prompt::select("Which app?", &labels, start, report)?))
}

/// Every app your logins reach, by org name and then app name.
pub fn reached(logins: &BTreeMap<String, config::Login>) -> Vec<Reached> {
    let mut apps: Vec<Reached> = logins
        .iter()
        .flat_map(|(org_id, login)| {
            login.apps.iter().map(move |(app_id, app_name)| Reached {
                org_id: org_id.clone(),
                org_name: login.org_name.clone(),
                app_id: app_id.clone(),
                app_name: app_name.clone(),
            })
        })
        .collect();
    apps.sort_by(|a, b| (&a.org_name, &a.app_name).cmp(&(&b.org_name, &b.app_name)));
    apps
}

/// Reads every login's apps again. A login that has ended can't say anything; the others still
/// can.
async fn refresh_all() {
    let Ok(logins) = config::logins() else {
        return;
    };
    for (org_id, login) in logins {
        let _ = refresh(org_id, login).await;
    }
}

/// Reads the apps the login reaches, and keeps them with it.
pub async fn refresh(org_id: String, login: config::Login) -> anyhow::Result<Vec<api::App>> {
    let credential = auth::for_login(org_id.clone(), login).await?;
    let apps = credential.client()?.apps(&org_id).await?;
    let names = apps.iter().map(|app| (app.id.clone(), app.name.clone()));
    config::set_apps(&org_id, names.collect())?;
    Ok(apps)
}

/// A bucket of an app's assets, written `env/bucket`, such as `staging/assets`. Both parts are ids:
/// lowercase letters, digits and single hyphens, starting with a letter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BucketRef {
    pub environment_id: String,
    pub bucket_id: String,
}

impl std::str::FromStr for BucketRef {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.split_once('/') {
            Some((environment, bucket)) if is_slug(environment) && is_slug(bucket) => Ok(Self {
                environment_id: environment.to_string(),
                bucket_id: bucket.to_string(),
            }),
            _ => Err("write the bucket as env/bucket, such as staging/assets".to_string()),
        }
    }
}

impl std::fmt::Display for BucketRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.environment_id, self.bucket_id)
    }
}

/// An id people choose: a lowercase letter, then lowercase letters and digits in parts joined by
/// single hyphens, 63 characters at most.
pub fn is_slug(text: &str) -> bool {
    text.len() <= 63
        && text.starts_with(|first: char| first.is_ascii_lowercase())
        && text.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

fn is_id(app: &str) -> bool {
    app.starts_with("app_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(org: &str, id: &str, name: &str) -> Reached {
        Reached {
            org_id: format!("org_{org}"),
            org_name: org.to_string(),
            app_id: id.to_string(),
            app_name: name.to_string(),
        }
    }

    #[test]
    fn a_bucket_is_written_env_slash_bucket() {
        let bucket: BucketRef = "staging/game-assets2".parse().unwrap();
        assert_eq!(bucket.environment_id, "staging");
        assert_eq!(bucket.bucket_id, "game-assets2");
        assert_eq!(bucket.to_string(), "staging/game-assets2");

        for bad in [
            "assets",
            "staging/",
            "/assets",
            "a/b/c",
            "Staging/assets",
            "staging/my--assets",
            "staging/-a",
            "1st/assets",
        ] {
            assert!(bad.parse::<BucketRef>().is_err(), "{bad}");
        }
    }

    #[test]
    fn an_app_is_found_by_id_or_by_a_name_only_one_app_has() {
        let apps = [
            app("a", "app_1", "Space Shooter"),
            app("a", "app_2", "Puzzle"),
            app("b", "app_3", "puzzle"),
            app("b", "app_4", "Tools"),
            app("c", "app_5", "Tools"),
        ];
        let found = |app: &str| lookup(&apps, app).unwrap().map(|(_, id)| id);

        assert_eq!(found("app_4").as_deref(), Some("app_4"));
        assert_eq!(found("space shooter").as_deref(), Some("app_1"));
        assert_eq!(found("puzzle").as_deref(), Some("app_3"));
        assert_eq!(found("Nope"), None);
        assert!(lookup(&apps, "PUZZLE").is_err());

        let shared = lookup(&apps, "Tools").unwrap_err().to_string();
        assert!(
            shared.contains("app_4 in b") && shared.contains("app_5 in c"),
            "{shared}"
        );
    }

    mod creating {
        use serde_json::json;
        use wiremock::matchers::{body_json, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use super::*;

        const ENVIRONMENTS: &str = "/storage/v1/apps/app_a/environments";
        const BUCKETS: &str = "/storage/v1/apps/app_a/environments/branches/buckets";

        async fn target(server: &MockServer) -> Target {
            Target {
                client: api::testing::client(server).await,
                app_id: "app_a".into(),
                app_name: "Space Shooter".into(),
            }
        }

        fn bucket() -> serde_json::Value {
            json!({ "id": "pr-1", "name": "pr-1", "write_access": "open", "revision": 1, "entry_count": 0, "release_count": 0,
                "unreleased_count": 0, "tags": [] })
        }

        fn refused(status: u16, code: &str) -> ResponseTemplate {
            ResponseTemplate::new(status).set_body_json(json!({ "error": code }))
        }

        #[tokio::test]
        async fn a_bucket_in_a_missing_environment_comes_with_the_environment() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(BUCKETS))
                .and(query_param("bucket_id", "pr-1"))
                .respond_with(refused(404, "not_found"))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(ENVIRONMENTS))
                .and(query_param("environment_id", "branches"))
                .and(body_json(json!({ "name": "Branches" })))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                    "data": { "id": "branches", "name": "Branches" }
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(BUCKETS))
                .and(body_json(json!({ "name": "pr-1" })))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "data": bucket() })))
                .expect(1)
                .mount(&server)
                .await;

            let target = target(&server).await;
            let name: BucketRef = "branches/pr-1".parse().unwrap();
            let created = target.create_bucket(&name, "Branches").await.unwrap();
            assert_eq!(created.info.id, "pr-1");
        }

        #[tokio::test]
        async fn a_bucket_another_run_just_created_will_do() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(BUCKETS))
                .respond_with(refused(409, "bucket_exists"))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(ENVIRONMENTS))
                .respond_with(refused(500, "internal_error"))
                .expect(0)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(BUCKETS))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({ "data": [bucket()] })),
                )
                .mount(&server)
                .await;

            let target = target(&server).await;
            let name: BucketRef = "branches/pr-1".parse().unwrap();
            let found = target.create_bucket(&name, "Branches").await.unwrap();
            assert_eq!(found.to_string(), "branches/pr-1");
        }

        #[tokio::test]
        async fn a_full_environment_is_a_failure() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(BUCKETS))
                .respond_with(refused(409, "too_many_buckets"))
                .mount(&server)
                .await;

            let target = target(&server).await;
            let name: BucketRef = "branches/pr-1".parse().unwrap();
            let Err(err) = target.create_bucket(&name, "Branches").await else {
                panic!("a full environment takes no bucket");
            };
            assert_eq!(
                err.to_string(),
                "the environment holds 1,000 buckets, the most it can"
            );
        }
    }
}
