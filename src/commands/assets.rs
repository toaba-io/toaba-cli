//! `toaba assets`: an app's files, in the buckets of its environments, as Unity's `ugs ccd`. The CLI
//! covers what each build does; setting environments and buckets up stays in the dashboard.

use clap::Subcommand;
use serde_json::json;

use crate::output::{Output, print_json, width};
use crate::progress;
use crate::target::{self, AppArg};

#[derive(Subcommand)]
pub enum Command {
    /// The app's buckets of files
    Bucket {
        #[command(subcommand)]
        command: BucketCommand,
    },
}

#[derive(Subcommand)]
pub enum BucketCommand {
    /// List the app's buckets, in every environment
    #[command(visible_alias = "ls")]
    List {
        #[command(flatten)]
        app: AppArg,
    },
}

pub async fn run(command: Command, output: Output) -> anyhow::Result<()> {
    match command {
        Command::Bucket {
            command: BucketCommand::List { app },
        } => list_buckets(app, output).await,
    }
}

async fn list_buckets(arg: AppArg, output: Output) -> anyhow::Result<()> {
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let line = progress::start("Loading the buckets…");
    let loaded = async {
        let mut buckets = Vec::new();
        for environment in target.client.environments(&target.app_id).await? {
            let found = target
                .client
                .buckets(&target.app_id, &environment.id)
                .await?;
            buckets.extend(
                found
                    .into_iter()
                    .map(|bucket| (environment.id.clone(), environment.name.clone(), bucket)),
            );
        }
        anyhow::Ok(buckets)
    }
    .await;
    let buckets = loaded.inspect_err(|_| progress::failed(&line, "Loading the buckets"))?;
    line.finish_and_clear();

    if output == Output::Json {
        let buckets: Vec<_> = buckets
            .iter()
            .map(|(environment_id, environment_name, bucket)| {
                json!({
                    "id": bucket.id,
                    "name": bucket.name,
                    "environment": { "id": environment_id, "name": environment_name },
                    "write_access": bucket.write_access,
                    "release_count": bucket.release_count,
                    "unreleased_count": bucket.unreleased_count,
                    "tags": bucket.tags.iter().map(|tag| json!({
                        "id": tag.id,
                        "release": { "id": tag.release.id, "release_num": tag.release.release_num },
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        return print_json(&json!({ "buckets": buckets }));
    }
    if buckets.is_empty() {
        eprintln!(
            "{} has no buckets yet, create one in the dashboard",
            target.app_name
        );
        return Ok(());
    }

    let rows: Vec<[String; 5]> = buckets
        .iter()
        .map(|(environment_id, _, bucket)| {
            let write = match bucket.write_access.as_str() {
                "promotion_only" => "promotion only".to_string(),
                other => other.to_string(),
            };
            let tags: Vec<String> = bucket
                .tags
                .iter()
                .map(|tag| format!("{} → #{}", tag.id, tag.release.release_num))
                .collect();
            [
                environment_id.clone(),
                bucket.id.clone(),
                write,
                bucket.unreleased_count.to_string(),
                tags.join(", "),
            ]
        })
        .collect();
    let headers = ["ENV", "BUCKET", "WRITE", "UNRELEASED", "TAGS"];
    let widths: Vec<usize> = (0..headers.len())
        .map(|column| width(headers[column], rows.iter().map(|row| &row[column])))
        .collect();
    let line = |cells: [&str; 5]| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        println!("{}", padded.join("  ").trim_end());
    };
    line(headers);
    for row in &rows {
        line([&row[0], &row[1], &row[2], &row[3], &row[4]]);
    }
    Ok(())
}
