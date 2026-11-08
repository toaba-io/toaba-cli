mod api;
mod auth;
mod commands;
mod config;
mod oauth;
mod output;
mod progress;
mod prompt;
mod target;
mod theme;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "toaba", version, about = "The Toaba CLI")]
struct Cli {
    /// How to print results: text for people, json for scripts and agents
    #[arg(short, long, global = true, value_enum, default_value_t = output::Output::Text)]
    output: output::Output,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Log in to one of your orgs through the browser
    Login,
    /// Log out of an org, ending the login on Toaba
    Logout(commands::logout::Args),
    /// Show your logins, whether they still work, and the app commands use
    ///
    /// With TOABA_TOKEN set, shows that token alone. Exits with code 1 when
    /// you aren't logged in or a login has ended.
    #[command(visible_aliases = ["s", "st"])]
    Status,
    /// Your apps: list them, pick the one commands use, see their keys
    App {
        #[command(subcommand)]
        command: commands::app::Command,
    },
    /// Your app's files, in the buckets of its environments
    Assets {
        #[command(subcommand)]
        command: commands::assets::Command,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    // Ctrl+C stops at once, and gives back the cursor that a picker hides while it's open.
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = console::Term::stderr().show_cursor();
            std::process::exit(130);
        }
    });

    let cli = Cli::parse();
    let result = match cli.command {
        Command::Login => commands::login::run(cli.output).await,
        Command::Logout(args) => commands::logout::run(args, cli.output).await,
        Command::Status => commands::status::run(cli.output).await,
        Command::App { command } => commands::app::run(command, cli.output).await,
        Command::Assets { command } => commands::assets::run(command, cli.output).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // A command that has already shown what went wrong just exits. Otherwise people get
            // the error on stderr, and with -o json scripts get it on stdout too, as Supabase's
            // CLI does, so stdout always holds JSON.
            if !err.is::<output::Reported>() {
                eprintln!("Error: {err:?}");
                if cli.output == output::Output::Json {
                    let _ = output::print_json(&serde_json::json!({ "error": format!("{err:#}") }));
                }
            }
            ExitCode::FAILURE
        }
    }
}
