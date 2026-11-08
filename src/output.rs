//! How commands print their results: text for people, or JSON for scripts and agents, as glab's
//! `--output json`. JSON goes to stdout alone; messages for people stay on stderr.

use serde::Serialize;

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Output {
    Text,
    Json,
}

/// Prints `value` as JSON on stdout.
pub fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// `text` starting with a capital, for an error shown as a line of its own.
pub fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// An error the command has already shown: it exits with 1 and prints nothing more.
#[derive(Debug)]
pub struct Reported;

impl std::fmt::Display for Reported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("already reported")
    }
}

impl std::error::Error for Reported {}

/// The widest of `header` and `values`, in characters, to line up a column.
pub fn width<S: AsRef<str>>(header: &str, values: impl IntoIterator<Item = S>) -> usize {
    values
        .into_iter()
        .map(|value| value.as_ref().chars().count())
        .chain([header.chars().count()])
        .max()
        .unwrap_or(0)
}
