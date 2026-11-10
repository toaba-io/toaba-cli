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

#[cfg(test)]
thread_local! {
    /// Where a test keeps its config, so tests never touch the real one or each other's.
    static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// A test's own config folder, removed when the test ends.
#[cfg(test)]
pub struct TestDir(PathBuf);

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        TEST_DIR.with(|test_dir| *test_dir.borrow_mut() = None);
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Points this test's thread at a config folder of its own, which starts empty, for as long as the
/// returned value lives.
#[cfg(test)]
#[must_use]
pub fn use_test_dir() -> TestDir {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "toaba-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    TEST_DIR.with(|test_dir| *test_dir.borrow_mut() = Some(dir.clone()));
    TestDir(dir)
}

fn dir() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    if let Some(dir) = TEST_DIR.with(|test_dir| test_dir.borrow().clone()) {
        return Ok(dir);
    }
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

    fn login_with_apps(org_name: &str, apps: &[(&str, &str)]) -> Login {
        Login {
            apps: apps
                .iter()
                .map(|(id, name)| (id.to_string(), name.to_string()))
                .collect(),
            ..login(org_name)
        }
    }

    fn some_tokens(access_token: &str) -> Tokens {
        Tokens {
            access_token: access_token.into(),
            refresh_token: "toaba_ort_a".into(),
            expire_time: Utc::now(),
        }
    }

    #[test]
    fn the_first_login_to_an_org_with_one_app_puts_that_app_in_use() {
        let _dir = use_test_dir();
        assert!(logins().unwrap().is_empty());
        assert_eq!(app().unwrap(), None);

        let two_apps = login_with_apps("Studio", &[("app_1", "Game"), ("app_2", "Tools")]);
        assert_eq!(
            save_login("org_s", two_apps, some_tokens("s")).unwrap(),
            None
        );
        assert_eq!(app().unwrap(), None);

        let one_app = login_with_apps("Solo", &[("app_3", "Puzzle")]);
        let chosen = save_login("org_o", one_app, some_tokens("o")).unwrap();
        assert_eq!(chosen.as_deref(), Some("app_3"));
        assert_eq!(app().unwrap().as_deref(), Some("app_3"));

        // An app is already in use, so another single-app org doesn't take over.
        let another = login_with_apps("Other", &[("app_4", "Racer")]);
        assert_eq!(
            save_login("org_x", another, some_tokens("x")).unwrap(),
            None
        );
        assert_eq!(app().unwrap().as_deref(), Some("app_3"));
        assert_eq!(logins().unwrap().len(), 3);
    }

    #[test]
    fn logging_in_again_replaces_the_login_and_its_tokens() {
        let _dir = use_test_dir();
        save_login("org_a", login("Old name"), some_tokens("first")).unwrap();
        save_login("org_a", login("New name"), some_tokens("second")).unwrap();

        let logins = logins().unwrap();
        assert_eq!(logins.len(), 1);
        assert_eq!(logins["org_a"].org_name, "New name");
        assert_eq!(tokens("org_a").unwrap().unwrap().access_token, "second");
    }

    #[test]
    fn removing_a_login_drops_its_tokens_and_its_app_in_use_only() {
        let _dir = use_test_dir();
        let solo = login_with_apps("Solo", &[("app_3", "Puzzle")]);
        save_login("org_o", solo, some_tokens("o")).unwrap();
        save_login("org_s", login("Studio"), some_tokens("s")).unwrap();

        remove("org_s").unwrap();
        assert!(tokens("org_s").unwrap().is_none());
        assert_eq!(app().unwrap().as_deref(), Some("app_3"));

        remove("org_o").unwrap();
        assert!(logins().unwrap().is_empty());
        assert!(tokens("org_o").unwrap().is_none());
        assert_eq!(app().unwrap(), None);
    }

    #[test]
    fn a_login_s_apps_are_replaced_and_the_app_in_use_can_change() {
        let _dir = use_test_dir();
        save_login(
            "org_a",
            login_with_apps("Acme", &[("app_1", "Game")]),
            some_tokens("a"),
        )
        .unwrap();

        set_apps(
            "org_a",
            BTreeMap::from([("app_2".to_string(), "Sequel".to_string())]),
        )
        .unwrap();
        let apps = &logins().unwrap()["org_a"].apps;
        assert_eq!(apps.len(), 1);
        assert_eq!(apps["app_2"], "Sequel");

        use_app("app_2").unwrap();
        assert_eq!(app().unwrap().as_deref(), Some("app_2"));
        assert_eq!(find_login("acme").unwrap().0, "org_a");
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
