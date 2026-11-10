//! `toaba app`: the apps your logins reach, the one commands use, and their publishable keys, as
//! Supabase's `projects` works with projects. The app in use decides the org too, so there's
//! nothing else to switch. Apps and keys are created in the dashboard.

use anyhow::{Context as _, bail};
use clap::Subcommand;
use console::style;
use serde_json::json;

use crate::api::App;
use crate::output::{Output, ago, date, pad, print_json, sentence, table, width};
use crate::target::{self, AppArg};
use crate::{auth, config, progress, prompt, theme};

#[derive(Subcommand)]
pub enum Command {
    /// List the apps your logins reach; * marks the one in use
    #[command(visible_alias = "ls")]
    List,
    /// Make an app the one commands use
    Use {
        /// The app, by name or id; picked from a list when left out
        app: Option<String>,
    },
    /// List the app's publishable keys, which game clients ship with
    ///
    /// They're created and revoked in the dashboard. With --env, prints the app's
    /// id and its newest key as TOABA_APP_ID and TOABA_PUBLISHABLE_KEY, for a game
    /// build's environment.
    Keys {
        #[command(flatten)]
        app: AppArg,
        /// Print TOABA_APP_ID and TOABA_PUBLISHABLE_KEY lines instead of a table
        #[arg(long)]
        env: bool,
    },
}

pub async fn run(command: Command, output: Output) -> anyhow::Result<()> {
    match command {
        Command::List => list(output).await,
        Command::Use { app } => use_app(app, output).await,
        Command::Keys { app, env } => keys(app, env, output).await,
    }
}

async fn list(output: Output) -> anyhow::Result<()> {
    let (apps, ended) = progress::during("Loading your apps…", load()).await?;
    let in_use = match auth::from_env() {
        Some(_) => None,
        None => config::app()?,
    };
    let used = |app: &App| in_use.as_deref() == Some(app.id.as_str());

    if output == Output::Json {
        let apps: Vec<_> = apps
            .iter()
            .map(|(org_name, app)| {
                json!({
                    "id": app.id,
                    "name": app.name,
                    "org": { "id": app.org_id, "name": org_name },
                    "in_use": used(app),
                    "delete_time": app.delete_time,
                    "expire_time": app.expire_time,
                })
            })
            .collect();
        print_json(&json!({ "apps": apps }))?;
    } else if apps.is_empty() {
        eprintln!("No apps yet, create one in the dashboard");
    } else {
        let name_width = width("NAME", apps.iter().map(|(_, app)| &app.name));
        let org_width = width("ORG", apps.iter().map(|(org_name, _)| org_name));
        println!("  {:<name_width$}  {:<org_width$}  ID", "NAME", "ORG");
        for (org_name, app) in &apps {
            let name = pad(&app.name, name_width);
            let (marker, name) = if used(app) {
                ("*", style(name).bold().to_string())
            } else {
                (" ", name)
            };
            let note = deletion(app)
                .map(|note| format!("  {note}"))
                .unwrap_or_default();
            println!(
                "{marker} {name}  {}  {}{note}",
                pad(org_name, org_width),
                app.id
            );
        }
    }

    for err in ended {
        eprintln!("{} {}", theme::warning().apply_to("!"), sentence(&err));
    }
    Ok(())
}

/// Every app your logins reach, with its org's name, by org and then app; and the errors of
/// logins that couldn't say, such as ended ones. With TOABA_TOKEN, its org's apps alone.
async fn load() -> anyhow::Result<(Vec<(String, App)>, Vec<String>)> {
    if let Some(credential) = auth::from_env() {
        let client = credential.client()?;
        let current = client.current_token().await?;
        let apps = client.apps(&current.org.id).await?;
        let apps = apps.into_iter().map(|app| (current.org.name.clone(), app));
        return Ok((apps.collect(), Vec::new()));
    }

    let logins = config::logins()?;
    if logins.is_empty() {
        bail!("you're not logged in, run `toaba login` or set TOABA_TOKEN");
    }
    let mut apps = Vec::new();
    let mut ended = Vec::new();
    for (org_id, login) in logins {
        let org_name = login.org_name.clone();
        match target::refresh(org_id, login).await {
            Ok(found) => apps.extend(found.into_iter().map(|app| (org_name.clone(), app))),
            Err(err) => ended.push(format!("{err:#}")),
        }
    }
    apps.sort_by(|(a_org, a), (b_org, b)| (a_org, &a.name).cmp(&(b_org, &b.name)));
    Ok((apps, ended))
}

async fn use_app(app: Option<String>, output: Output) -> anyhow::Result<()> {
    if auth::from_env().is_some() {
        bail!("TOABA_TOKEN is set, which leaves your logins out: pass --app or set TOABA_APP");
    }
    let (org_id, app_id) = match app {
        Some(app) => target::find(&app).await?,
        None if prompt::interactive() => {
            let picked = target::pick(false).await?;
            (picked.org_id, picked.app_id)
        }
        None => bail!("name the app to use: `toaba app use <app>`"),
    };
    config::use_app(&app_id)?;

    let login = config::logins()?
        .remove(&org_id)
        .context("the app's login went away")?;
    let name = login
        .apps
        .get(&app_id)
        .cloned()
        .unwrap_or_else(|| app_id.clone());
    if output == Output::Json {
        return print_json(&json!({
            "id": app_id,
            "name": name,
            "org": { "id": org_id, "name": login.org_name },
        }));
    }
    eprintln!(
        "{} Using {} in {}",
        theme::accent().apply_to("✓"),
        style(name).bold().for_stderr(),
        login.org_name
    );
    Ok(())
}

async fn keys(arg: AppArg, env: bool, output: Output) -> anyhow::Result<()> {
    if env && output == Output::Json {
        bail!("--env and -o json are two different outputs, pick one");
    }
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let keys = progress::during(
        "Loading the keys…",
        target.client.publishable_keys(&target.app_id),
    )
    .await?;

    if env {
        // The newest, which is the one to ship while an older one is being retired.
        let newest = keys.last().with_context(|| {
            format!(
                "{} has no publishable key yet, create one in the dashboard",
                target.app_name
            )
        })?;
        println!("TOABA_APP_ID={}", target.app_id);
        println!("TOABA_PUBLISHABLE_KEY={}", newest.key);
        return Ok(());
    }
    if output == Output::Json {
        return print_json(&json!({ "publishable_keys": keys }));
    }
    if keys.is_empty() {
        eprintln!(
            "{} has no publishable keys yet, create one in the dashboard",
            target.app_name
        );
        return Ok(());
    }
    let rows: Vec<[String; 2]> = keys
        .iter()
        .map(|key| [key.key.clone(), ago(&key.create_time)])
        .collect();
    table(["KEY", "CREATED"], &rows);
    Ok(())
}

/// When an app scheduled for deletion goes for good.
fn deletion(app: &App) -> Option<String> {
    let expire_time = app.expire_time?;
    Some(format!(
        "scheduled for deletion, gone on {}",
        date(&expire_time)
    ))
}
