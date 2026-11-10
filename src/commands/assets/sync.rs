//! `toaba assets sync`: makes a bucket's files match a local folder, then optionally cuts a release
//! and points tags at it.

use std::path::PathBuf;

use console::style;
use indicatif::HumanBytes;
use serde_json::json;

use super::release::{CutRelease, cut_release};
use super::tag::{point_tags, tag_name};
use crate::output::{Output, pad, plain, print_json, width};
use crate::prompt::YesArg;
use crate::target::{self, AppArg, Bucket, BucketRef};
use crate::{api, mirror, progress, theme};

#[derive(clap::Args)]
pub struct Args {
    /// The local folder
    dir: PathBuf,
    /// The bucket, as env/bucket
    bucket: BucketRef,
    #[command(flatten)]
    app: AppArg,
    #[command(flatten)]
    folder: FolderArgs,
    #[command(flatten)]
    yes: YesArg,
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

/// The options of a command that mirrors a local folder into a bucket.
#[derive(clap::Args)]
pub(super) struct FolderArgs {
    /// Show what would change, and change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Leave out local files matching this pattern, such as '*.meta'; repeatable
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,
    /// How many files to upload at once
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=32))]
    pub concurrency: u8,
}

pub(super) async fn run(args: Args, output: Output) -> anyhow::Result<()> {
    let Args {
        dir,
        bucket,
        app: arg,
        folder:
            FolderArgs {
                dry_run,
                exclude,
                concurrency,
            },
        yes,
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
        bucket.require_open("syncing files")?;
        let remote = bucket.entries().await?;
        Ok((bucket, remote))
    })
    .await?;

    let local = hash(files).await?;
    let plan = mirror::plan(&local, remote.iter().filter_map(api::Entry::file));
    let applies = !dry_run && !plan.is_empty();
    show_plan(&plan, output, applies && !yes.yes);

    if let (true, Output::Text, Some(name)) = (dry_run, output, &release_name) {
        // What the bucket holds unreleased isn't read in a dry run, so this can't say whether
        // there would be anything to release.
        match tag_names.as_slice() {
            [] => println!("Would then release {bucket} as {name}"),
            tags => println!(
                "Would then release {bucket} as {name}, and point {} at it",
                tags.join(", ")
            ),
        }
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
        upload(
            &bucket,
            &local,
            &remote,
            &plan,
            concurrency,
            &target.app_name,
        )
        .await?;
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
        print_json(&with_plan(
            &plan,
            json!({
                "bucket": bucket.to_string(),
                "applied": applies,
                "release": release,
                "tags": tags,
            }),
        ))?;
    }
    Ok(())
}

/// The scanned files, hashed, behind a bar of the bytes read.
pub(super) async fn hash(
    files: Vec<(String, PathBuf, u64)>,
) -> anyhow::Result<Vec<mirror::LocalFile>> {
    let total: u64 = files.iter().map(|(_, _, size)| size).sum();
    let bar = progress::bar(total, "Hashing");
    let local = tokio::task::spawn_blocking({
        let bar = bar.clone();
        move || mirror::hash(files, &bar)
    })
    .await??;
    bar.finish_and_clear();
    Ok(local)
}

/// Shows the plan: on stdout as the result, or with -o json on stderr, for the person `asking`.
pub(super) fn show_plan(plan: &mirror::Plan, output: Output, asking: bool) {
    if output == Output::Text {
        print!("{}", plan_text(plan, true));
    } else if asking {
        eprint!("{}", plan_text(plan, false));
    }
}

/// Makes the bucket match the plan behind a bar of the bytes sent, then says so.
pub(super) async fn upload(
    bucket: &Bucket<'_>,
    local: &[mirror::LocalFile],
    remote: &[api::Entry],
    plan: &mirror::Plan,
    concurrency: u8,
    app_name: &str,
) -> anyhow::Result<()> {
    let bar = progress::bar(plan.upload_bytes(), "Uploading");
    mirror::apply(bucket, local, remote, plan, concurrency.into(), &bar)
        .await
        .inspect_err(|_| bar.abandon())?;
    bar.finish_and_clear();
    eprintln!(
        "{} Synced {} in {app_name}",
        theme::accent().apply_to("✓"),
        style(&bucket.name).bold().for_stderr(),
    );
    Ok(())
}

/// A command's JSON result: `fields`, and the plan's lists and counts.
pub(super) fn with_plan(plan: &mirror::Plan, fields: serde_json::Value) -> serde_json::Value {
    let mut result = json!({
        "new": plan.new,
        "changed": plan.changed,
        "removed": plan.removed,
        "unchanged_count": plan.unchanged_count,
        "upload_bytes": plan.upload_bytes(),
    });
    if let (Some(result), serde_json::Value::Object(fields)) = (result.as_object_mut(), fields) {
        result.extend(fields);
    }
    result
}

/// How many changes of each kind a plan lists before it says how many more there are.
const LISTED: usize = 20;

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
    // A long plan shows the first of each kind: the summary has the counts, and JSON the rest.
    let all = || {
        groups
            .iter()
            .flat_map(|(_, changes, _)| changes.iter().take(LISTED))
    };
    let path_width = width("", all().map(|change| &change.path));
    let sizes: Vec<String> = all()
        .map(|change| HumanBytes(change.size_bytes).to_string())
        .collect();
    let size_width = width("", &sizes);

    let mut text = String::new();
    for (mark, changes, what) in &groups {
        for change in changes.iter().take(LISTED) {
            text.push_str(&format!(
                "  {mark} {}  {:>size_width$}  {}\n",
                pad(&plain(&change.path), path_width),
                HumanBytes(change.size_bytes).to_string(),
                dim(what)
            ));
        }
        if changes.len() > LISTED {
            let more = format!("… and {} more {what}", changes.len() - LISTED);
            text.push_str(&format!("  {mark} {}\n", dim(&more)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_plan_lists_the_first_of_each_kind_and_counts_the_rest() {
        let change = |n: usize| mirror::Change {
            path: format!("file-{n:03}.txt"),
            size_bytes: 1,
        };
        let plan = mirror::Plan {
            new: (0..LISTED + 5).map(change).collect(),
            removed: (0..3).map(change).collect(),
            ..Default::default()
        };
        let text = plan_text(&plan, true);

        assert_eq!(text.matches(" new\n").count(), LISTED + 1);
        assert!(text.contains("… and 5 more new"));
        assert!(!text.contains("file-020.txt"));
        assert_eq!(text.matches("to remove\n").count(), 3);
        assert!(text.contains("25 new, 0 changed, 3 to remove"));
    }
}
