//! `toaba assets sync`: uploads a local folder's new and changed files to a bucket, then optionally
//! cuts a release and points tags at it.

use std::path::PathBuf;

use anyhow::bail;
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

/// The options of a command that syncs a local folder into a bucket.
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
    /// Also remove the bucket's files that aren't in the folder, so the bucket matches it exactly
    #[arg(long)]
    pub delete: bool,
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
                delete,
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
    let plan = mirror::plan(&local, remote.iter().filter_map(api::Entry::file), delete);
    require_within_limits(&bucket.name, &bucket.info.limits, &local, &plan)?;
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

/// Fails when the bucket's limits refuse files the plan uploads, after listing them on stderr.
pub(super) fn require_within_limits(
    bucket: &BucketRef,
    limits: &api::Limits,
    local: &[mirror::LocalFile],
    plan: &mirror::Plan,
) -> anyhow::Result<()> {
    let refused = mirror::refused(local, plan, limits);
    if refused.is_empty() {
        return Ok(());
    }
    let too_big = refused
        .iter()
        .filter(|file| !limits.fits(file.size_bytes))
        .map(|file| (file.path.as_str(), HumanBytes(file.size_bytes).to_string()))
        .collect();
    let wrong_type: Vec<(&str, String)> = refused
        .iter()
        .filter(|file| !limits.allows(&file.content_type))
        .map(|file| (file.path.as_str(), file.content_type.clone()))
        .collect();
    let over = limits
        .max_file_size_bytes
        .map(|max| format!("over {}", HumanBytes(max)))
        .unwrap_or_default();
    let groups = [
        (too_big, over),
        (wrong_type, "not an allowed type".to_string()),
    ];
    eprint!("{}", refused_text(&groups));
    bail!("{bucket}'s limits refuse {} of the files", refused.len())
}

/// The refused files, a line each: for each reason, its files' paths with their size or type.
fn refused_text(groups: &[(Vec<(&str, String)>, String)]) -> String {
    let mark = theme::failure().apply_to("✗").to_string();
    let dim = |text: &str| style(text).dim().for_stderr().to_string();
    let listed = || {
        groups
            .iter()
            .flat_map(|(files, _)| files.iter().take(LISTED))
    };
    let path_width = width("", listed().map(|(path, _)| path));
    let detail_width = width("", listed().map(|(_, detail)| detail));
    groups
        .iter()
        .map(|(files, what)| {
            let rows: Vec<String> = files
                .iter()
                .take(LISTED)
                .map(|(path, detail)| {
                    format!(
                        "{}  {}",
                        pad(&plain(path), path_width),
                        pad(detail, detail_width)
                    )
                })
                .collect();
            listing(&mark, &rows, files.len(), what, &dim)
        })
        .collect()
}

/// A line for each row, ending in `what`, then how many of the `total` aren't among them.
fn listing(
    mark: &str,
    rows: &[String],
    total: usize,
    what: &str,
    dim: &dyn Fn(&str) -> String,
) -> String {
    let mut text = String::new();
    for row in rows {
        text.push_str(&format!("  {mark} {row}  {}\n", dim(what)));
    }
    if total > rows.len() {
        let more = format!("… and {} more {what}", total - rows.len());
        text.push_str(&format!("  {mark} {}\n", dim(&more)));
    }
    text
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
        "kept": plan.kept,
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
        let rows: Vec<String> = changes
            .iter()
            .take(LISTED)
            .map(|change| {
                format!(
                    "{}  {:>size_width$}",
                    pad(&plain(&change.path), path_width),
                    HumanBytes(change.size_bytes).to_string()
                )
            })
            .collect();
        text.push_str(&listing(mark, &rows, changes.len(), what, &dim));
    }
    // Removed and kept files are counted only when there are some. Kept files are never listed:
    // a bucket gathers old bundles over time.
    let tally = |n: usize, what: &str| match n {
        0 => String::new(),
        n => format!(", {n} {what}"),
    };
    let kept = tally(plan.kept.len(), "only in the bucket");
    if plan.is_empty() {
        text.push_str(&format!(
            "Nothing to do: all {} files match{kept}\n",
            plan.unchanged_count
        ));
    } else {
        let removed = tally(plan.removed.len(), "to remove");
        text.push_str(&format!(
            "{} new, {} changed{removed}, {} unchanged{kept} · {} to upload\n",
            plan.new.len(),
            plan.changed.len(),
            plan.unchanged_count,
            HumanBytes(plan.upload_bytes())
        ));
    }
    if !plan.kept.is_empty() {
        text.push_str(&format!(
            "{}\n",
            dim("Files only in the bucket stay; --delete removes them")
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
        assert!(text.contains("25 new, 0 changed, 3 to remove, 0 unchanged ·"));
        assert!(!text.contains("--delete"));
    }

    #[test]
    fn files_outside_the_bucket_s_limits_stop_the_sync_before_it_uploads() {
        let file = |path: &str, size_bytes: u64, content_type: &str| mirror::LocalFile {
            path: path.into(),
            source: path.into(),
            size_bytes,
            sha256: "aa".into(),
            content_type: content_type.into(),
        };
        let mut local = vec![
            file("hero.psd", 3 * 1024 * 1024, "image/vnd.adobe.photoshop"),
            file("logo.png", 1024, "image/png"),
            file("notes.txt", 10, "text/plain"),
        ];
        local.extend(
            (0..LISTED + 2).map(|n| file(&format!("z-{n:02}.png"), 2 * 1024 * 1024, "image/png")),
        );
        let plan = mirror::plan(&local, [], false);
        let limits = api::Limits {
            max_file_size_bytes: Some(1024 * 1024),
            allowed_content_types: Some(vec!["image/png".into(), "audio/*".into()]),
        };
        let bucket: BucketRef = "staging/android".parse().unwrap();

        let sizes: Vec<String> = (0..LISTED + 3).map(|n| format!("z-{n:02}.png")).collect();
        let groups = [
            (
                sizes
                    .iter()
                    .map(|path| (path.as_str(), "2.00 MiB".to_string()))
                    .collect(),
                "over 1.00 MiB".to_string(),
            ),
            (
                vec![("hero.psd", "image/vnd.adobe.photoshop".to_string())],
                "not an allowed type".to_string(),
            ),
        ];
        let text = refused_text(&groups);
        let text = console::strip_ansi_codes(&text);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), LISTED + 1 + 1);
        assert_eq!(
            lines[0],
            "  ✗ z-00.png  2.00 MiB                   over 1.00 MiB"
        );
        assert_eq!(lines[LISTED], "  ✗ … and 3 more over 1.00 MiB");
        assert_eq!(
            lines[LISTED + 1],
            "  ✗ hero.psd  image/vnd.adobe.photoshop  not an allowed type"
        );

        let err = require_within_limits(&bucket, &limits, &local, &plan).unwrap_err();
        assert_eq!(
            err.to_string(),
            "staging/android's limits refuse 24 of the files"
        );
        assert!(require_within_limits(&bucket, &api::Limits::default(), &local, &plan).is_ok());
        assert!(require_within_limits(&bucket, &limits, &local[1..2], &plan).is_ok());
    }

    #[test]
    fn kept_files_are_counted_with_a_hint_rather_than_listed() {
        let change = |n: usize| mirror::Change {
            path: format!("old-{n}.bundle"),
            size_bytes: 1,
        };
        let plan = mirror::Plan {
            new: vec![change(9)],
            kept: (0..2).map(change).collect(),
            unchanged_count: 4,
            ..Default::default()
        };
        let text = plan_text(&plan, true);
        assert!(!text.contains("old-0.bundle"));
        assert!(text.contains("1 new, 0 changed, 4 unchanged, 2 only in the bucket ·"));
        assert!(text.contains("--delete removes them"));

        let nothing = mirror::Plan {
            kept: plan.kept,
            unchanged_count: 4,
            ..Default::default()
        };
        assert!(
            plan_text(&nothing, true)
                .starts_with("Nothing to do: all 4 files match, 2 only in the bucket\n")
        );
    }
}
