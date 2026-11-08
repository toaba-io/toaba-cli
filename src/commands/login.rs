//! `toaba login`, as glab's `auth login --device`: the OAuth device flow, then the new token's
//! owner, org and apps, saved as the org's login. In a terminal one spinner line follows the login from
//! start to result, with a bar that drains as the code expires; elsewhere, such as in CI, it prints
//! glab's plain lines.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use console::style;
use indicatif::{ProgressState, ProgressStyle};
use serde_json::json;

use crate::output::{Output, print_json};
use crate::{api, config, oauth, progress, theme};

/// How many cells the countdown bar takes when full.
const COUNTDOWN_WIDTH: usize = 24;

pub async fn run(output: Output) -> anyhow::Result<()> {
    // Nobody approves a login in CI, which would wait out the code's 15 minutes.
    if std::env::var_os("CI").is_some() {
        bail!(
            "`toaba login` waits for a person to approve it in the browser; \
             in CI, set TOABA_TOKEN to a service account's token instead"
        );
    }

    let api_url = api::url();
    let line = progress::start("Starting the login…");
    let device = oauth::start_device_login(&api_url)
        .await
        .inspect_err(|_| progress::failed(&line, "Starting the login"))?;
    if line.is_hidden() {
        eprintln!("First copy your one-time code: {}", device.code());
        eprintln!(
            "Then open this URL on any device to approve it: {}",
            device.url()
        );
        eprintln!("Waiting for approval…");
    } else {
        line.suspend(|| {
            eprintln!("\n{}", style("Log in to Toaba").bold().for_stderr());
            eprintln!(
                "Your one-time code  {}",
                theme::badge().apply_to(format!(" {} ", device.code()))
            );
            eprintln!("Approve it in the browser if it shows the same code:");
            eprintln!("{}\n", style(device.url()).underlined().for_stderr());
        });
        open_browser(device.url());
    }

    line.set_style(countdown(device.expires_in()));
    line.set_message("Waiting for approval");
    let tokens = oauth::wait_for_approval(&device)
        .await
        .inspect_err(|_| progress::failed(&line, "Waiting for approval"))?;

    line.set_style(progress::spinner("{spinner} {msg}"));
    line.set_message("Approved, reading your login…");
    let client = api::Client::new(
        &api_url,
        &tokens.access_token,
        "the API refused the new login",
    )?;
    let current = client
        .current_token()
        .await
        .inspect_err(|_| progress::failed(&line, "Reading your login"))
        .context("couldn't read the new login")?;
    let apps = client
        .apps(&current.org.id)
        .await
        .inspect_err(|_| progress::failed(&line, "Reading your login"))
        .context("couldn't read the new login's apps")?;

    line.set_message("Saving your login…");
    // A new login to the org ends the one it replaces, so old logins don't pile up on Toaba.
    let replaced = config::tokens(&current.org.id)
        .inspect_err(|_| progress::failed(&line, "Saving your login"))?;
    let only_app = match apps.as_slice() {
        [app] => Some((app.id.clone(), app.name.clone())),
        _ => None,
    };
    let login = config::Login {
        api_url: api_url.clone(),
        org_name: current.org.name.clone(),
        email: current.owner.label().to_string(),
        apps: apps.into_iter().map(|app| (app.id, app.name)).collect(),
    };
    let chosen = config::save_login(&current.org.id, login, tokens)
        .inspect_err(|_| progress::failed(&line, "Saving your login"))?;
    let ended = match replaced {
        Some(old) => oauth::revoke(&api_url, &old.refresh_token).await,
        None => Ok(()),
    };

    progress::finish(
        &line,
        format!(
            "{} Logged in to {} as {}",
            theme::accent().apply_to("✓"),
            style(&current.org.name).bold().for_stderr(),
            current.owner.label()
        ),
    );
    let chosen = chosen.and(only_app);
    if let Some((_, name)) = &chosen {
        eprintln!(
            "{} Using {}, the org's only app",
            theme::accent().apply_to("✓"),
            style(name).bold().for_stderr()
        );
    }
    if let Err(err) = ended {
        eprintln!(
            "{} Couldn't end your previous login to {} ({err:#}), revoke it in the dashboard",
            theme::warning().apply_to("!"),
            current.org.name
        );
    }
    if output == Output::Json {
        print_json(&json!({
            "org": { "id": current.org.id, "name": current.org.name },
            "user": current.owner.label(),
            "app": chosen.map(|(id, name)| json!({ "id": id, "name": name })),
        }))?;
    }
    Ok(())
}

/// Opens `url` in the browser, but not over SSH, where it would open on the wrong machine. The URL
/// is on screen either way.
fn open_browser(url: &str) {
    if std::env::var_os("SSH_CONNECTION").is_none() && std::env::var_os("SSH_TTY").is_none() {
        let _ = open::that_detached(url);
    }
}

/// The spinner, then a Tangerine bar that drains as the code expires, and the time left.
fn countdown(expires_in: Duration) -> ProgressStyle {
    let deadline = Instant::now() + expires_in;
    let total = expires_in.as_secs_f64().max(1.0);
    progress::spinner("{spinner} {msg}  {countdown}").with_key(
        "countdown",
        move |_: &ProgressState, out: &mut dyn std::fmt::Write| {
            let left = deadline.saturating_duration_since(Instant::now());
            let filled = (left.as_secs_f64() / total * COUNTDOWN_WIDTH as f64).ceil() as usize;
            let filled = filled.min(COUNTDOWN_WIDTH);
            let _ = write!(
                out,
                "{}{}  {}:{:02}",
                theme::accent().apply_to("━".repeat(filled)),
                theme::track().apply_to("─".repeat(COUNTDOWN_WIDTH - filled)),
                left.as_secs() / 60,
                left.as_secs() % 60
            );
        },
    )
}
