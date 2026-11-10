//! `toaba assets sync`: makes a bucket's files match a local folder, then optionally cuts a release
//! and points tags at it.

use std::path::PathBuf;

use console::style;
use indicatif::{HumanBytes, ProgressBar};
use serde_json::json;

use super::release::{CutRelease, cut_release};
use super::tag::{point_tags, tag_name};
use crate::output::{Output, pad, plain, print_json, width};
use crate::prompt::YesArg;
use crate::target::{self, AppArg, BucketRef};
use crate::{mirror, progress, theme};

#[derive(clap::Args)]
pub struct Args {
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
    #[command(flatten)]
    yes: YesArg,
    /// How many files to upload at once
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=32))]
    concurrency: u8,
    /// After syncing, cut a release with this name, such as v1.4
    #[arg(long, value_name = "NAME")]
    release: Option<String>,
    /// Notes to keep with the release
    #[arg(long, requires = "release")]
    notes: Option<String>,
    /// Point this tag at the new release, creating or moving it; repeatable
    #[arg(long, requires = "release", value_parser = tag_name)]
    tag: Vec<String>,
}

pub(super) async fn run(args: Args, output: Output) -> anyhow::Result<()> {
    let Args {
        dir,
        bucket,
        app: arg,
        dry_run,
        exclude,
        yes,
        concurrency,
        release: release_name,
        notes,
        tag: tag_names,
    } = args;
    if !dry_run {
        yes.require("sync changes the bucket: pass --yes to go ahead, or --dry-run to only look")?;
    }
    let files = mirror::scan(&dir, &exclude)?;
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let (bucket, remote) = progress::during("Reading the bucket…", async {
        let bucket = target.bucket(&bucket).await?;
        let remote = bucket.entries().await?;
        Ok((bucket, remote))
    })
    .await?;

    let total: u64 = files.iter().map(|(_, _, size)| size).sum();
    let bar = ProgressBar::new(total).with_style(progress::bytes("Hashing"));
    let local = tokio::task::spawn_blocking({
        let bar = bar.clone();
        move || mirror::hash(files, &bar)
    })
    .await??;
    bar.finish_and_clear();

    let plan = mirror::plan(&local, &remote);
    let applies = !dry_run && !plan.is_empty();
    // With -o json the plan is still shown to the person asked to confirm, on stderr.
    if output == Output::Text {
        print!("{}", plan_text(&plan, true));
    } else if applies && !yes.yes {
        eprint!("{}", plan_text(&plan, false));
    }

    if applies {
        let question = match &release_name {
            Some(name) if tag_names.is_empty() => {
                format!("Sync {bucket} like this, and release it as {name}?")
            }
            Some(name) => format!(
                "Sync {bucket} like this, release it as {name}, and point {} at it?",
                tag_names.join(", ")
            ),
            None => format!("Sync {bucket} like this?"),
        };
        yes.confirm(&question)?;
        let bar = ProgressBar::new(plan.upload_bytes()).with_style(progress::bytes("Uploading"));
        mirror::apply(&bucket, &local, &remote, &plan, concurrency.into(), &bar)
            .await
            .inspect_err(|_| bar.abandon())?;
        bar.finish_and_clear();
        eprintln!(
            "{} Synced {} in {}",
            theme::accent().apply_to("✓"),
            style(&bucket.name).bold().for_stderr(),
            target.app_name
        );
    }

    // The release takes whatever the bucket holds unreleased, also from an earlier sync. A sync
    // that was just confirmed covers it; otherwise it asks for itself.
    let mut release = None;
    let mut tags = Vec::new();
    if let (Some(name), false) = (&release_name, dry_run) {
        let cut = CutRelease {
            name,
            notes: notes.as_deref(),
            tags: &tag_names,
            confirmed: yes.yes || applies,
            shown: applies,
        };
        release = cut_release(&bucket, cut).await?;
        match &release {
            Some(release) => tags = point_tags(&bucket, &tag_names, release).await?,
            None if tag_names.is_empty() => {
                eprintln!("Nothing new to release: {bucket} matches its latest release");
            }
            None => eprintln!(
                "Nothing new to release: {bucket} matches its latest release, so {} stays",
                tag_names.join(", ")
            ),
        }
    }
    if output == Output::Json {
        print_json(&json!({
            "bucket": bucket.to_string(),
            "applied": applies,
            "release": release,
            "tags": tags,
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
fn plan_text(plan: &mirror::Plan, stdout: bool) -> String {
    let paint = |style: console::Style, text: &str| {
        let style = if stdout {
            style.for_stdout()
        } else {
            style.for_stderr()
        };
        style.apply_to(text).to_string()
    };
    let dim = |text: &str| paint(console::Style::new().dim(), text);
    let groups = [
        (paint(theme::accent(), "+"), &plan.new, "new"),
        (paint(theme::accent(), "~"), &plan.changed, "changed"),
        (paint(theme::failure(), "-"), &plan.removed, "to remove"),
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
                "  {mark} {}  {:>size_width$}  {}\n",
                pad(&plain(&change.path), path_width),
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
