//! One spinner line that follows a command's steps and ends as its result, in the brand's colours.
//! indicatif hides it when stderr isn't a terminal.

use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

use crate::theme;

/// Starts the line, spinning beside `message`.
pub fn start(message: &'static str) -> ProgressBar {
    let line = ProgressBar::new_spinner()
        .with_style(spinner("{spinner} {msg}"))
        .with_message(message);
    line.enable_steady_tick(Duration::from_millis(100));
    line
}

/// Runs `work` behind the line. When it fails, the line ends with ✗ and the step, which is
/// `message` without its ellipsis; otherwise the line goes, leaving room for the result.
pub async fn during<T>(
    message: &'static str,
    work: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let line = start(message);
    let result = work.await;
    match &result {
        Ok(_) => line.finish_and_clear(),
        Err(_) => failed(&line, message.trim_end_matches('…')),
    }
    result
}

/// `template` with a Tangerine spinner.
pub fn spinner(template: &str) -> ProgressStyle {
    let ticks: Vec<String> = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "
        .chars()
        .map(|tick| theme::accent().apply_to(tick).to_string())
        .collect();
    let ticks: Vec<&str> = ticks.iter().map(String::as_str).collect();
    ProgressStyle::with_template(template)
        .expect("the template is valid")
        .tick_strings(&ticks)
}

/// Ends the line, leaving `message` in its place as an ordinary line of output.
pub fn finish(line: &ProgressBar, message: String) {
    line.finish_and_clear();
    eprintln!("{message}");
}

/// Ends the line with ✗ and the step that failed. The error follows on its own line.
pub fn failed(line: &ProgressBar, step: &str) {
    line.finish_and_clear();
    eprintln!("{} {step}", theme::failure().apply_to("✗"));
}

/// A Tangerine bar of bytes done, after `label`. 209 is Tangerine among the 256 colours.
pub fn bytes(label: &str) -> ProgressStyle {
    ProgressStyle::with_template(&format!(
        "{label}  {{bar:24.209/dim}}  {{bytes}} / {{total_bytes}}"
    ))
    .expect("the template is valid")
    .progress_chars("━━─")
}
