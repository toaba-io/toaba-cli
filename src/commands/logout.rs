//! `toaba logout`, as glab's `auth logout`, which only forgets the token. This one also ends the
//! login on Toaba, so a forgotten login doesn't stay active. If Toaba can't be reached, it still
//! forgets the login and warns, since getting the token off the machine matters most.

use anyhow::bail;
use console::style;
use serde_json::json;

use crate::output::{Output, print_json};
use crate::{api, config, oauth, progress, prompt, theme};

#[derive(clap::Args)]
pub struct Args {
    /// The org to log out of, by name or id; asked when you have several logins
    org: Option<String>,
}

pub async fn run(args: Args, output: Output) -> anyhow::Result<()> {
    let (org_id, login) = match args.org {
        Some(org) => config::find_login(&org)?,
        None => only_or_picked()?,
    };

    let line = progress::start("Logging out…");
    let revoked = match config::tokens(&org_id)? {
        Some(tokens) => oauth::revoke(&login.api_url, &tokens.refresh_token).await,
        None => Ok(()),
    };
    config::remove(&org_id).inspect_err(|_| progress::failed(&line, "Logging out"))?;
    progress::finish(
        &line,
        format!(
            "{} Logged out of {}",
            theme::accent().apply_to("✓"),
            style(&login.org_name).bold().for_stderr()
        ),
    );

    let warn = theme::warning().apply_to("!");
    if let Err(err) = &revoked {
        eprintln!("{warn} Couldn't end the login on Toaba ({err:#}), revoke it in the dashboard");
    }
    if std::env::var_os("TOABA_TOKEN").is_some() {
        eprintln!("{warn} TOABA_TOKEN is still set, so commands keep using it");
    }
    if output == Output::Json {
        print_json(&json!({
            "org": { "id": org_id, "name": login.org_name },
            "ended_on_toaba": revoked.is_ok(),
        }))?;
    }
    Ok(())
}

/// Your only login, or one picked from a list when you have several.
fn only_or_picked() -> anyhow::Result<(String, config::Login)> {
    let mut logins: Vec<(String, config::Login)> = config::logins()?.into_iter().collect();
    match logins.len() {
        0 => bail!("you're not logged in to any org"),
        1 => return Ok(logins.remove(0)),
        n if !prompt::interactive() => {
            bail!("you have {n} logins, name the org to log out of: `toaba logout <org>`")
        }
        _ => {}
    }
    logins.sort_by(|(_, a), (_, b)| a.org_name.cmp(&b.org_name));
    let labels: Vec<String> = logins
        .iter()
        .map(|(_, login)| {
            let label = format!("{}  {}", login.org_name, login.email);
            if login.api_url == api::PRODUCTION_URL {
                label
            } else {
                format!("{label}  {}", login.api_url)
            }
        })
        .collect();
    Ok(logins.swap_remove(prompt::select("Log out of", &labels, 0, false)?))
}
