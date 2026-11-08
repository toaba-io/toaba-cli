//! Your logins, one per org, keyed by the org's id since org names aren't unique, and the app
//! commands use. `config.toml` holds them; `credentials.toml`, readable only by its owner, holds the
//! logins' tokens. Both live in `$XDG_CONFIG_HOME/toaba`, or `~/.config/toaba`, as glab's and gh's
//! do, and in `%APPDATA%\toaba` on Windows.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct Config {
    /// The id of the app commands use, set with `toaba app use`. Its org's login reaches it.
    #[serde(skip_serializing_if = "Option::is_none")]
    app: Option<String>,
    #[serde(default)]
    logins: BTreeMap<String, Login>,
}

/// A login to one org.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Login {
    pub api_url: String,
    pub org_name: String,
    pub email: String,
    /// The apps the login reaches, id to name, as of the last time they were read. An app never
    /// moves to another org, so an entry can be missing but never under the wrong login.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub apps: BTreeMap<String, String>,
}

/// A login's tokens, saved together so a refresh replaces them in one write.
#[derive(Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expire_time: DateTime<Utc>,
}

/// How long a command waits for another to release the config: longer than a refresh, which
/// holds it for one API call of up to 30 seconds.
const LOCK_WAIT: Duration = Duration::from_secs(40);

/// Saves a login to the org `org_id`, replacing an earlier one. With no app in use yet, an org with
/// a single app makes that the app in use, and its id is returned.
pub fn save_login(org_id: &str, login: Login, tokens: Tokens) -> anyhow::Result<Option<String>> {
    let _lock = lock()?;
    let path = config_path()?;
    let mut config: Config = read(&path)?;
    let mut chosen = None;
    if config.app.is_none() && login.apps.len() == 1 {
        chosen = login.apps.keys().next().cloned();
        config.app.clone_from(&chosen);
    }
    config.logins.insert(org_id.to_string(), login);

    // The tokens first: a crash between the writes leaves unused tokens, never a login without.
    save_tokens(org_id, tokens)?;
    write(&path, &toml::to_string(&config)?, false)?;
    Ok(chosen)
}

/// Your logins by org id.
pub fn logins() -> anyhow::Result<BTreeMap<String, Login>> {
    Ok(read::<Config>(&config_path()?)?.logins)
}

/// The id of the app commands use, if one is set.
pub fn app() -> anyhow::Result<Option<String>> {
    Ok(read::<Config>(&config_path()?)?.app)
}

/// Makes `app_id` the app commands use.
pub fn use_app(app_id: &str) -> anyhow::Result<()> {
    let _lock = lock()?;
    let path = config_path()?;
    let mut config: Config = read(&path)?;
    config.app = Some(app_id.to_string());
    write(&path, &toml::to_string(&config)?, false)
}

/// The login to `org`: an org id, or the name of exactly one login's org, ignoring case. Org names
/// aren't unique, so two matches fail with their ids.
pub fn find_login(org: &str) -> anyhow::Result<(String, Login)> {
    pick(logins()?, org)
}

fn pick(mut logins: BTreeMap<String, Login>, org: &str) -> anyhow::Result<(String, Login)> {
    if let Some(login) = logins.remove(org) {
        return Ok((org.to_string(), login));
    }
    let wanted = org.to_lowercase();
    let mut matches: Vec<(String, Login)> = logins
        .into_iter()
        .filter(|(_, login)| login.org_name.to_lowercase() == wanted)
        .collect();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => bail!("you have no login to an org named {org}, see `toaba status`"),
        n => {
            let ids: Vec<&str> = matches.iter().map(|(id, _)| id.as_str()).collect();
            bail!(
                "{n} of your logins are to orgs named {org}, pick one by its id: {}",
                ids.join(", ")
            )
        }
    }
}

/// Replaces the apps the org's login reaches.
pub fn set_apps(org_id: &str, apps: BTreeMap<String, String>) -> anyhow::Result<()> {
    let _lock = lock()?;
    let path = config_path()?;
    let mut config: Config = read(&path)?;
    if let Some(login) = config.logins.get_mut(org_id) {
        login.apps = apps;
        write(&path, &toml::to_string(&config)?, false)?;
    }
    Ok(())
}

/// Removes the org's login and its tokens, and the app in use when it's one of the org's.
pub fn remove(org_id: &str) -> anyhow::Result<()> {
    let _lock = lock()?;
    let path = config_path()?;
    let mut config: Config = read(&path)?;
    if let Some(login) = config.logins.remove(org_id)
        && config
            .app
            .as_ref()
            .is_some_and(|app| login.apps.contains_key(app))
    {
        config.app = None;
    }

    // The login first: a crash between the writes leaves unused tokens, never a login without.
    write(&path, &toml::to_string(&config)?, false)?;
    let path = credentials_path()?;
    let mut credentials: BTreeMap<String, Tokens> = read(&path)?;
    credentials.remove(org_id);
    write(&path, &toml::to_string(&credentials)?, true)
}

/// The org's tokens, or `None` when they're missing.
pub fn tokens(org_id: &str) -> anyhow::Result<Option<Tokens>> {
    let mut credentials: BTreeMap<String, Tokens> = read(&credentials_path()?)?;
    Ok(credentials.remove(org_id))
}

/// Replaces the org's tokens, as a refresh does. The caller holds the lock.
pub fn save_tokens(org_id: &str, tokens: Tokens) -> anyhow::Result<()> {
    let path = credentials_path()?;
    let mut credentials: BTreeMap<String, Tokens> = read(&path)?;
    credentials.insert(org_id.to_string(), tokens);
    write(&path, &toml::to_string(&credentials)?, true)
}

pub fn credentials_path() -> anyhow::Result<PathBuf> {
    Ok(dir()?.join("credentials.toml"))
}

fn config_path() -> anyhow::Result<PathBuf> {
    Ok(dir()?.join("config.toml"))
}

/// Locks the config and credentials against other toaba commands until the returned file is
/// dropped. It waits up to `LOCK_WAIT`, then fails rather than go ahead unlocked: each refresh
/// replaces both tokens, so two at once would end the login.
pub fn lock() -> anyhow::Result<fs::File> {
    let dir = dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("couldn't create {}", dir.display()))?;
    let path = dir.join("config.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("couldn't open {}", path.display()))?;

    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(fs::TryLockError::WouldBlock) => {
                bail!(
                    "another toaba command kept the config locked for {} seconds, try again",
                    LOCK_WAIT.as_secs()
                )
            }
            Err(fs::TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("couldn't lock {}", path.display()));
            }
        }
    }
}

fn dir() -> anyhow::Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None if cfg!(windows) => {
            PathBuf::from(std::env::var_os("APPDATA").context("APPDATA isn't set")?)
        }
        None => std::env::home_dir()
            .context("couldn't find the home folder")?
            .join(".config"),
    };
    Ok(base.join("toaba"))
}

/// The TOML file at `path`, or the default before it exists.
fn read<T: DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => {
            toml::from_str(&text).with_context(|| format!("couldn't read {}", path.display()))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(err) => Err(err).with_context(|| format!("couldn't read {}", path.display())),
    }
}

/// Writes `text` to `path` through a temporary file renamed over it, so a crash never leaves half
/// a file. A `private` file is readable only by its owner, on Unix.
fn write(path: &Path, text: &str, private: bool) -> anyhow::Result<()> {
    let tmp = path.with_extension("toml.tmp");
    // A temporary file left by a crash would keep its permissions.
    let _ = fs::remove_file(&tmp);

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;

    let mut file = options
        .open(&tmp)
        .with_context(|| format!("couldn't write {}", tmp.display()))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, path).with_context(|| format!("couldn't write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(org_name: &str) -> Login {
        Login {
            api_url: "https://api.toaba.io".into(),
            org_name: org_name.into(),
            email: "dev@toaba.io".into(),
            apps: BTreeMap::new(),
        }
    }

    #[test]
    fn an_org_is_picked_by_id_or_by_a_name_only_one_org_has() {
        let logins = || {
            BTreeMap::from([
                ("org_a".to_string(), login("Acme")),
                ("org_b".to_string(), login("Side Project")),
                ("org_c".to_string(), login("side project")),
            ])
        };

        assert_eq!(pick(logins(), "org_b").unwrap().0, "org_b");
        assert_eq!(pick(logins(), "acme").unwrap().0, "org_a");

        let shared = pick(logins(), "Side Project").unwrap_err().to_string();
        assert!(
            shared.contains("org_b") && shared.contains("org_c"),
            "{shared}"
        );
        assert!(pick(logins(), "Nope").is_err());
    }

    #[test]
    fn credentials_are_private_and_read_back() {
        let dir = std::env::temp_dir().join(format!("toaba-config-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.toml");

        let missing: BTreeMap<String, Tokens> = read(&path).unwrap();
        assert!(missing.is_empty());

        let tokens = Tokens {
            access_token: "toaba_oat_a".into(),
            refresh_token: "toaba_ort_a".into(),
            expire_time: Utc::now(),
        };
        let credentials = BTreeMap::from([("org_a".to_string(), tokens)]);
        write(&path, &toml::to_string(&credentials).unwrap(), true).unwrap();

        let read_back: BTreeMap<String, Tokens> = read(&path).unwrap();
        assert_eq!(read_back["org_a"].refresh_token, "toaba_ort_a");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
