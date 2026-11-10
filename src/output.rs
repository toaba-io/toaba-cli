//! How commands print their results: text for people, or JSON for scripts and agents, as glab's
//! `--output json`. JSON goes to stdout alone; messages for people stay on stderr.

use std::sync::atomic::AtomicBool;

use chrono::{DateTime, Utc};
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

/// A day in the local time zone, such as `9 Nov 2026`.
pub fn date(time: &DateTime<Utc>) -> String {
    time.with_timezone(&chrono::Local)
        .format("%-d %b %Y")
        .to_string()
}

/// How long ago `time` was, such as `3 hours ago`, as glab's lists show times: it reads the same
/// in any time zone. JSON keeps the exact time.
pub fn ago(time: &DateTime<Utc>) -> String {
    ago_from(time, &Utc::now())
}

fn ago_from(time: &DateTime<Utc>, now: &DateTime<Utc>) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    let seconds = (*now - *time).num_seconds();
    let (count, unit) = if seconds < MINUTE {
        // A clock a little ahead of the server's makes a moment ago look like the future.
        return "just now".to_string();
    } else if seconds < HOUR {
        (seconds / MINUTE, "minute")
    } else if seconds < DAY {
        (seconds / HOUR, "hour")
    } else if seconds < MONTH {
        (seconds / DAY, "day")
    } else if seconds < YEAR {
        (seconds / MONTH, "month")
    } else {
        (seconds / YEAR, "year")
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural} ago")
}

/// The first three of `items`, then how many more there are.
pub fn few(items: &[String], separator: &str) -> String {
    match items.len() {
        0..=3 => items.join(separator),
        n => format!("{}{separator}+{} more", items[..3].join(separator), n - 3),
    }
}

/// Prints a table on stdout: the headers, then the rows, each column as wide as its widest cell.
pub fn table<const N: usize>(headers: [&str; N], rows: &[[String; N]]) {
    let widths: [usize; N] =
        std::array::from_fn(|column| width(headers[column], rows.iter().map(|row| &row[column])));
    let line = |cells: [&str; N]| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| pad(cell, *width))
            .collect();
        println!("{}", padded.join("  ").trim_end());
    };
    line(headers);
    for row in rows {
        line(std::array::from_fn(|column| row[column].as_str()));
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
    fn times_read_as_how_long_ago_in_the_largest_whole_unit() {
        let now: DateTime<Utc> = "2026-11-10T12:00:00Z".parse().unwrap();
        let ago = |time: &str| ago_from(&time.parse().unwrap(), &now);
        assert_eq!(ago("2026-11-10T12:00:05Z"), "just now");
        assert_eq!(ago("2026-11-10T11:59:30Z"), "just now");
        assert_eq!(ago("2026-11-10T11:59:00Z"), "1 minute ago");
        assert_eq!(ago("2026-11-10T09:30:00Z"), "2 hours ago");
        assert_eq!(ago("2026-11-09T11:00:00Z"), "1 day ago");
        assert_eq!(ago("2026-10-01T12:00:00Z"), "1 month ago");
        assert_eq!(ago("2024-11-01T12:00:00Z"), "2 years ago");
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
