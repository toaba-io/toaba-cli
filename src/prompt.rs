//! Questions for a person at a terminal, drawn on stderr so stdout stays clean for JSON. They're
//! never asked without one: in CI or a script a question would wait until the job times out.

use std::io::IsTerminal;

use anyhow::bail;
use dialoguer::{Confirm, Select};

use crate::theme;

/// Whether a person can answer: stdin and stderr are terminals, and it isn't CI.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal()
        && console::Term::stderr().is_term()
        && std::env::var_os("CI").is_none()
}

/// Asks the person to pick one of `items`, starting on `default`. Esc or q cancels. With `report`,
/// the pick stays on screen afterwards, for commands that don't name it themselves.
pub fn select(
    prompt: &str,
    items: &[String],
    default: usize,
    report: bool,
) -> anyhow::Result<usize> {
    let picked = Select::with_theme(&theme::picker())
        .with_prompt(prompt)
        .items(items)
        .default(default)
        .report(report)
        .interact_on_opt(&console::Term::stderr())?;
    match picked {
        Some(index) => Ok(index),
        None => bail!("cancelled"),
    }
}

/// Asks a yes-or-no question, answered `default` on Enter. Esc or q counts as no.
pub fn confirm(question: &str, default: bool) -> anyhow::Result<bool> {
    let answer = Confirm::with_theme(&theme::picker())
        .with_prompt(question)
        .default(default)
        .report(false)
        .interact_on_opt(&console::Term::stderr())?;
    Ok(answer.unwrap_or(false))
}
