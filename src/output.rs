//! How commands print their results: text for people, or JSON for scripts and agents, as glab's
//! `--output json`. JSON goes to stdout alone; messages for people stay on stderr.

use std::sync::atomic::AtomicBool;

use serde::Serialize;

/// Set while a command has cleaning up to do on Ctrl+C, which it then handles itself. Otherwise
/// Ctrl+C stops the CLI at once.
pub static CLEANS_UP_ON_CTRL_C: AtomicBool = AtomicBool::new(false);

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

/// The widest of `header` and `values`, in terminal cells, to line up a column. An emoji or a CJK
/// character takes two cells, so widths are measured as displayed, not by counting characters.
pub fn width<S: AsRef<str>>(header: &str, values: impl IntoIterator<Item = S>) -> usize {
    values
        .into_iter()
        .map(|value| console::measure_text_width(value.as_ref()))
        .chain([console::measure_text_width(header)])
        .max()
        .unwrap_or(0)
}

/// `text` with each control character, such as an escape or a line break, replaced by \u{fffd}.
pub fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

/// `text` cut to `max` terminal cells, ending in … when it was longer.
pub fn short(text: &str, max: usize) -> String {
    console::truncate_str(text, max, "…").into_owned()
}

/// The first three of `items`, then how many more there are.
pub fn few(items: &[String], separator: &str) -> String {
    match items.len() {
        0..=3 => items.join(separator),
        n => format!("{}{separator}+{} more", items[..3].join(separator), n - 3),
    }
}

/// `text` with spaces after it up to `width` terminal cells.
pub fn pad(text: &str, width: usize) -> String {
    let missing = width.saturating_sub(console::measure_text_width(text));
    format!("{text}{}", " ".repeat(missing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_and_long_lists_are_cut_short() {
        assert_eq!(short("v1.4", 10), "v1.4");
        // A two-cell character that would straddle the limit is left out.
        let cut = short(&"夏".repeat(40), 10);
        assert!(cut.ends_with('…') && console::measure_text_width(&cut) <= 10);
        let tags: Vec<String> = (1..=5).map(|n| format!("t{n}")).collect();
        assert_eq!(few(&tags, ", "), "t1, t2, t3, +2 more");
        assert_eq!(few(&tags[..3], ", "), "t1, t2, t3");
    }

    #[test]
    fn columns_line_up_by_displayed_width() {
        let names = ["Beta", "夏イベント", "Summer 🎉"];
        let widest = width("NAME", names);
        assert_eq!(widest, 10);
        for name in names {
            assert_eq!(console::measure_text_width(&pad(name, widest)), 10);
        }
    }
}
