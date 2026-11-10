//! `toaba status`: what commands act on, and each of your logins with whether it still works, as
//! glab's `auth status` checks each host, printed on stdout like every command's results. With
//! TOABA_TOKEN, only that token. It exits with 1 when you aren't logged in or a login has ended;
//! with `-o json` it still prints the state then.

use std::collections::BTreeSet;

use anyhow::bail;
use console::style;
use serde_json::{Value, json};

use crate::output::{Output, Reported, pad, print_json, width};
use crate::{api, auth, config, progress, target, theme};

/// A login, or TOABA_TOKEN, and what the API says about its token.
struct Checked {
    api_url: String,
    /// The login's org, by id and name; none for TOABA_TOKEN.
    org: Option<(String, String)>,
    current: anyhow::Result<api::CurrentToken>,
}

pub async fn run(output: Output) -> anyhow::Result<()> {
    let from_env = auth::from_env().is_some();
    let line = progress::start("Checking your logins…");
    let checked = match check().await {
        Ok(checked) => checked,
        Err(err) => {
            progress::failed(&line, "Checking your logins");
            return Err(err);
        }
    };
    line.finish_and_clear();

    let in_use_id = if from_env { None } else { config::app()? };
    let in_use = match &in_use_id {
        Some(app_id) => target::reached(&config::logins()?)
            .into_iter()
            .find(|app| &app.app_id == app_id),
        None => None,
    };
    let failed = checked.iter().any(|checked| checked.current.is_err());

    if output == Output::Json {
        let logins: Vec<Value> = checked.iter().map(login_json).collect();
        print_json(&json!({
            "state": if failed { "error" } else { "ok" },
            "token_source": if from_env { "TOABA_TOKEN" } else { "logins" },
            "logins": logins,
            "app": in_use.as_ref().map(|app| json!({
                "id": app.app_id,
                "name": app.app_name,
                "org": { "id": app.org_id, "name": app.org_name },
            })),
        }))?;
    } else {
        let warn = theme::warning().for_stdout().apply_to("!");
        let bold = |text: &str| style(text.to_string()).bold();
        let dim = |text: &str| style(text.to_string()).dim();

        // What commands act on, first.
        match (&in_use, &in_use_id) {
            _ if from_env => println!("Using {}", bold("TOABA_TOKEN")),
            (Some(app), _) => println!("Using {} in {}", bold(&app.app_name), app.org_name),
            (None, Some(app_id)) => {
                println!("{warn} {app_id} is in use, but none of your logins reaches it")
            }
            (None, None) => println!(
                "{warn} No app in use {}",
                dim("· pick one with `toaba app use`")
            ),
        }

        println!("\n{}", bold(if from_env { "Token" } else { "Logins" }));
        let names: Vec<String> = checked.iter().map(org_name).collect();
        let name_width = width("", &names);
        for (checked, name) in checked.iter().zip(&names) {
            let name = pad(name, name_width);
            match &checked.current {
                Ok(current) => println!(
                    "  {} {}  {}",
                    theme::accent().for_stdout().apply_to("✓"),
                    bold(&name),
                    dim(current.owner.label())
                ),
                Err(err) => println!(
                    "  {} {}  {}",
                    theme::failure().for_stdout().apply_to("✗"),
                    bold(&name),
                    dim(&problem(err, from_env))
                ),
            }
        }

        // Only during development: customers always use the production API.
        let dev_apis: BTreeSet<&str> = checked
            .iter()
            .map(|checked| checked.api_url.as_str())
            .filter(|url| *url != api::PRODUCTION_URL)
            .collect();
        if !dev_apis.is_empty() {
            let apis: Vec<&str> = dev_apis.into_iter().collect();
            println!("\n{}  {}", bold("API"), dim(&apis.join(", ")));
        }
    }

    if failed {
        return Err(Reported.into());
    }
    Ok(())
}

/// TOABA_TOKEN alone when it's set, or else every login, each checked with the API.
async fn check() -> anyhow::Result<Vec<Checked>> {
    if let Some(credential) = auth::from_env() {
        let current = match credential.client() {
            Ok(client) => client.current_token().await,
            Err(err) => Err(err),
        };
        return Ok(vec![Checked {
            api_url: credential.api_url,
            org: None,
            current,
        }]);
    }

    let logins = config::logins()?;
    if logins.is_empty() {
        bail!("you're not logged in, run `toaba login` or set TOABA_TOKEN");
    }
    let mut checked = Vec::new();
    for (org_id, login) in logins {
        let api_url = login.api_url.clone();
        let org = (org_id.clone(), login.org_name.clone());
        let current = async {
            let credential = auth::for_login(org_id, login).await?;
            credential.client()?.current_token().await
        }
        .await;
        checked.push(Checked {
            api_url,
            org: Some(org),
            current,
        });
    }
    Ok(checked)
}

/// The org a row is about: the login's, or the one TOABA_TOKEN's answer names.
fn org_name(checked: &Checked) -> String {
    match (&checked.org, &checked.current) {
        (Some((_, name)), _) => name.clone(),
        (None, Ok(current)) => current.org.name.clone(),
        (None, Err(_)) => "TOABA_TOKEN".to_string(),
    }
}

/// What went wrong with a row's token, in a few words: its outermost message, without the
/// causes under it, which JSON keeps.
fn problem(err: &anyhow::Error, from_env: bool) -> String {
    match err.downcast_ref::<api::Refused>() {
        Some(_) if from_env => "refused: it's unknown, expired or revoked".to_string(),
        Some(_) => "ended, run `toaba login`".to_string(),
        None => err.to_string(),
    }
}

fn login_json(checked: &Checked) -> Value {
    match &checked.current {
        Ok(current) => json!({
            "state": "ok",
            "org": { "id": current.org.id, "name": current.org.name },
            "user": current.owner.label(),
            "api_url": checked.api_url,
        }),
        Err(err) => json!({
            "state": "error",
            "org": checked.org.as_ref().map(|(id, name)| json!({ "id": id, "name": name })),
            "api_url": checked.api_url,
            "error": format!("{err:#}"),
        }),
    }
}
