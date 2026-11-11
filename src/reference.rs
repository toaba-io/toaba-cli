//! The command reference on the docs site, `docs/commands.mdx`, rendered from the commands' own
//! help so the two can't drift; the docs copy it in with `pnpm cli:sync`. A test fails when the
//! committed file is out of date, and `UPDATE_REFERENCE=1 cargo test` rewrites it.

use clap::{Arg, Command, CommandFactory};

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/commands.mdx");

const HEADER: &str = "---
title: Command reference
description: Every toaba command, with its arguments and options.
---

{/* Generated from the CLI's own help by toaba-cli's tests. Don't edit it here. */}

Every command also shows this with `--help`, such as `toaba assets sync --help`.
";

/// The reference: the options every command takes, then each command, in the order of `--help`.
pub fn render() -> String {
    let mut cli = crate::Cli::command();
    cli.build();
    let mut text = String::from(HEADER);
    let global: Vec<&Arg> = cli
        .get_arguments()
        .filter(|arg| listed(arg) && arg.is_global_set())
        .collect();
    text.push_str("\n## Global options\n\nEvery command takes these.\n\n");
    text.push_str(&table("Option", &global, option_name));
    for command in visible(&cli) {
        section(command, &mut text);
    }
    text
}

/// A section for each command that runs something; a group of commands only leads to them.
fn section(command: &Command, text: &mut String) {
    if command.has_subcommands() {
        for sub in visible(command) {
            section(sub, text);
        }
        return;
    }
    let name = command.get_bin_name().unwrap_or(command.get_name());
    text.push_str(&format!("\n## {name}\n\n"));
    let about = command
        .get_about()
        .map(ToString::to_string)
        .unwrap_or_default();
    text.push_str(&format!("{}\n", escape(&sentence(&about))));
    // The long help starts with the short one, then says more.
    if let Some(long) = command.get_long_about().map(ToString::to_string) {
        let more = long.strip_prefix(&about).unwrap_or(&long).trim();
        for paragraph in more.split("\n\n").filter(|p| !p.is_empty()) {
            text.push_str(&format!("\n{}\n", escape(&flatten(paragraph))));
        }
    }
    let usage = command.clone().render_usage().to_string();
    let usage = usage.trim_start_matches("Usage: ");
    text.push_str(&format!("\n```text\n{usage}\n```\n"));
    let aliases: Vec<String> = command
        .get_visible_aliases()
        .map(|alias| format!("`{alias}`"))
        .collect();
    if !aliases.is_empty() {
        text.push_str(&format!("\nAliases: {}\n", aliases.join(", ")));
    }
    let (positional, options): (Vec<&Arg>, Vec<&Arg>) = command
        .get_arguments()
        .filter(|arg| listed(arg) && !arg.is_global_set())
        .partition(|arg| arg.is_positional());
    if !positional.is_empty() {
        text.push('\n');
        text.push_str(&table("Argument", &positional, |arg| {
            if arg.is_required_set() {
                format!("`<{}>`", value_name(arg))
            } else {
                format!("`[{}]`", value_name(arg))
            }
        }));
    }
    if !options.is_empty() {
        text.push('\n');
        text.push_str(&table("Option", &options, option_name));
    }
}

fn visible(command: &Command) -> impl Iterator<Item = &Command> {
    command
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
}

/// Whether the reference shows the argument: not hidden, and not `--help` or `--version`.
fn listed(arg: &Arg) -> bool {
    !arg.is_hide_set() && !matches!(arg.get_id().as_str(), "help" | "version")
}

fn table(heading: &str, args: &[&Arg], name: impl Fn(&Arg) -> String) -> String {
    let mut text = format!("| {heading} | Description |\n| --- | --- |\n");
    for arg in args {
        text.push_str(&format!("| {} | {} |\n", name(arg), description(arg)));
    }
    text
}

/// `-o`, `--output <OUTPUT>`: the flags, and the value they take.
fn option_name(arg: &Arg) -> String {
    let value = if arg.get_action().takes_values() {
        format!(" <{}>", value_name(arg))
    } else {
        String::new()
    };
    let mut names = Vec::new();
    if let Some(short) = arg.get_short() {
        names.push(format!("`-{short}`"));
    }
    if let Some(long) = arg.get_long() {
        names.push(format!("`--{long}{value}`"));
    }
    names.join(", ")
}

fn value_name(arg: &Arg) -> String {
    arg.get_value_names()
        .and_then(|names| names.first())
        .map(ToString::to_string)
        .unwrap_or_else(|| arg.get_id().as_str().to_uppercase())
}

/// The argument's help, then what it can be, defaults to and reads from the environment.
fn description(arg: &Arg) -> String {
    let help = arg
        .get_long_help()
        .or(arg.get_help())
        .map(ToString::to_string)
        .unwrap_or_default();
    let mut parts = vec![escape(&sentence(&flatten(&help)))];
    let possible: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| format!("`{}`", value.get_name()))
        .collect();
    if arg.get_action().takes_values() && !possible.is_empty() {
        parts.push(format!("One of {}.", possible.join(", ")));
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|value| format!("`{}`", value.to_string_lossy()))
        .collect();
    if arg.get_action().takes_values() && !defaults.is_empty() {
        parts.push(format!("Default: {}.", defaults.join(", ")));
    }
    if let Some(env) = arg.get_env() {
        parts.push(format!("Environment: `{}`.", env.to_string_lossy()));
    }
    parts.join(" ")
}

/// Text on one line, for a table cell or a paragraph.
fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Help as a sentence: help lines leave off the full stop, which the notes after them need.
fn sentence(text: &str) -> String {
    let text = text.trim();
    match text.chars().last() {
        Some('.' | '!' | '?' | ':') | None => text.to_string(),
        Some(_) => format!("{text}."),
    }
}

/// Text made safe for MDX outside code: `<` and braces would start JSX, `|` a table cell, and
/// `*` emphasis.
fn escape(text: &str) -> String {
    text.split('`')
        .enumerate()
        .map(|(index, part)| {
            if index % 2 == 1 {
                return part.to_string();
            }
            part.replace('\\', "\\\\")
                .replace('<', "\\<")
                .replace('{', "\\{")
                .replace('}', "\\}")
                .replace('|', "\\|")
                .replace('*', "\\*")
        })
        .collect::<Vec<_>>()
        .join("`")
}

#[test]
fn the_command_reference_is_current() {
    let rendered = render();
    if std::env::var_os("UPDATE_REFERENCE").is_some() {
        std::fs::create_dir_all(std::path::Path::new(PATH).parent().expect("in docs/"))
            .expect("docs/ can be created");
        std::fs::write(PATH, &rendered).expect("docs/commands.mdx can be written");
        return;
    }
    // A Windows checkout may turn line endings into CRLF.
    let committed = std::fs::read_to_string(PATH)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert!(
        committed == rendered,
        "docs/commands.mdx is out of date: run `UPDATE_REFERENCE=1 cargo test`"
    );
}

#[test]
fn prose_is_escaped_for_mdx_but_code_is_left_alone() {
    assert_eq!(
        escape("its bucket is branches/<branch> | `<DIR>` {x}"),
        "its bucket is branches/\\<branch> \\| `<DIR>` \\{x\\}"
    );
}
