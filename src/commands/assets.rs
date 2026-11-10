//! `toaba assets`: an app's files, in the buckets of its environments, as Unity's `ugs ccd`. The CLI
//! covers what each build does; setting environments and buckets up stays in the dashboard.

use std::path::PathBuf;

use anyhow::bail;
use clap::Subcommand;
use console::style;
use indicatif::{HumanBytes, ProgressBar};
use serde_json::json;

use crate::output::{Output, print_json, width};
use crate::target::{self, AppArg, BucketRef};
use crate::{api, progress, prompt, sync, theme};

#[derive(Subcommand)]
pub enum Command {
    /// The app's buckets of files
    Bucket {
        #[command(subcommand)]
        command: BucketCommand,
    },
    /// Make a bucket's files match a local folder
    ///
    /// Uploads the files that are new or changed and removes the ones no longer in
    /// the folder. Players aren't affected until a release is tagged.
    Sync {
        /// The local folder
        dir: PathBuf,
        /// The bucket, as env/bucket
        bucket: BucketRef,
        #[command(flatten)]
        app: AppArg,
        /// Show what would change, and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Leave out local files matching this pattern, such as '*.meta'; repeatable
        #[arg(long, value_name = "GLOB")]
        exclude: Vec<String>,
        /// Go ahead without asking; needed where there's nobody to ask, as in CI
        #[arg(short, long)]
        yes: bool,
        /// How many files to upload at once
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=32))]
        concurrency: u8,
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
        Command::Sync {
            dir,
            bucket,
            app,
            dry_run,
            exclude,
            yes,
            concurrency,
        } => {
            let options = SyncOptions {
                dry_run,
                exclude,
                yes,
                concurrency: concurrency.into(),
            };
            sync(dir, bucket, app, options, output).await
        }
    }
}

struct SyncOptions {
    dry_run: bool,
    exclude: Vec<String>,
    yes: bool,
    concurrency: usize,
}

async fn sync(
    dir: PathBuf,
    bucket: BucketRef,
    arg: AppArg,
    options: SyncOptions,
    output: Output,
) -> anyhow::Result<()> {
    // Asked for up front, before any work, where there'd be nobody to confirm later.
    if !options.dry_run && !options.yes && !prompt::interactive() {
        bail!("sync changes the bucket: pass --yes to go ahead, or --dry-run to only look");
    }
    let files = sync::scan(&dir, &options.exclude)?;
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;

    let line = progress::start("Reading the bucket…");
    let remote = async {
        bucket.check(&target).await?;
        target
            .client
            .entries(&target.app_id, &bucket.environment_id, &bucket.bucket_id)
            .await
    }
    .await
    .inspect_err(|_| progress::failed(&line, "Reading the bucket"))?;
    line.finish_and_clear();

    let total: u64 = files.iter().map(|(_, _, size)| size).sum();
    let bar = ProgressBar::new(total).with_style(progress::bytes("Hashing"));
    let local = tokio::task::spawn_blocking({
        let bar = bar.clone();
        move || sync::hash(files, &bar)
    })
    .await??;
    bar.finish_and_clear();

    let plan = sync::plan(&local, &remote);
    let applies = !options.dry_run && !plan.is_empty();
    // With -o json the plan is still shown to the person asked to confirm, on stderr.
    if output == Output::Text {
        print!("{}", plan_text(&plan, true));
    } else if applies && !options.yes {
        eprint!("{}", plan_text(&plan, false));
    }

    if applies {
        prompt::confirm_or_yes(&format!("Sync {bucket} like this?"), options.yes)?;
        let bar = ProgressBar::new(plan.upload_bytes()).with_style(progress::bytes("Uploading"));
        let path = api::bucket_path(&target.app_id, &bucket.environment_id, &bucket.bucket_id);
        sync::apply(
            &target.client,
            &path,
            &local,
            &remote,
            &plan,
            options.concurrency,
            &bar,
        )
        .await
        .inspect_err(|_| bar.abandon())?;
        bar.finish_and_clear();
        eprintln!(
            "{} Synced {} in {}",
            theme::accent().apply_to("✓"),
            style(&bucket).bold().for_stderr(),
            target.app_name
        );
    }
    if output == Output::Json {
        print_json(&json!({
            "bucket": bucket.to_string(),
            "applied": applies,
            "new": plan.new,
            "changed": plan.changed,
            "removed": plan.removed,
            "unchanged_count": plan.unchanged_count,
            "upload_bytes": plan.upload_bytes(),
        }))?;
    }
    Ok(())
}

/// The plan's changes, a line each, then its summary; coloured for stdout or for stderr.
fn plan_text(plan: &sync::Plan, stdout: bool) -> String {
    let mark = |style: console::Style, mark: &'static str| {
        let style = if stdout {
            style.for_stdout()
        } else {
            style.for_stderr()
        };
        style.apply_to(mark).to_string()
    };
    let dim = |text: &str| {
        let style = console::Style::new().dim();
        let style = if stdout {
            style.for_stdout()
        } else {
            style.for_stderr()
        };
        style.apply_to(text).to_string()
    };
    let groups = [
        (mark(theme::accent(), "+"), &plan.new, "new"),
        (mark(theme::accent(), "~"), &plan.changed, "changed"),
        (mark(theme::failure(), "-"), &plan.removed, "to remove"),
    ];
    let all = || groups.iter().flat_map(|(_, changes, _)| changes.iter());
    let path_width = width("", all().map(|change| &change.path));
    let sizes: Vec<String> = all()
        .map(|change| HumanBytes(change.size_bytes).to_string())
        .collect();
    let size_width = width("", &sizes);

    let mut text = String::new();
    for (mark, changes, what) in &groups {
        for change in changes.iter() {
            text.push_str(&format!(
                "  {mark} {:<path_width$}  {:>size_width$}  {}\n",
                change.path,
                HumanBytes(change.size_bytes).to_string(),
                dim(what)
            ));
        }
    }
    if plan.is_empty() {
        text.push_str(&format!(
            "Nothing to do: all {} files match\n",
            plan.unchanged_count
        ));
    } else {
        text.push_str(&format!(
            "{} new, {} changed, {} to remove, {} unchanged · {} to upload\n",
            plan.new.len(),
            plan.changed.len(),
            plan.removed.len(),
            plan.unchanged_count,
            HumanBytes(plan.upload_bytes())
        ));
    }
    text
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
