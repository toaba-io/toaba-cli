//! `toaba assets release`: a bucket's releases, and cutting one, which `sync --release` shares.

use anyhow::{Context as _, bail};
use console::style;
use indicatif::HumanBytes;
use serde_json::json;

use crate::output::{Output, ago, few, print_json, short, table};
use crate::prompt::{self, YesArg};
use crate::target::{self, AppArg, Bucket, BucketRef};
use crate::{api, progress, theme};

#[derive(clap::Args)]
pub struct ListArgs {
    /// The bucket, as env/bucket
    bucket: BucketRef,
    #[command(flatten)]
    app: AppArg,
    /// How many releases to show at most
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=1000))]
    limit: u16,
}

#[derive(clap::Args)]
pub struct CreateArgs {
    /// The bucket, as env/bucket
    bucket: BucketRef,
    /// A name for the release, such as v1.4
    name: String,
    #[command(flatten)]
    app: AppArg,
    /// Notes to keep with the release
    #[arg(long)]
    notes: Option<String>,
    #[command(flatten)]
    yes: YesArg,
}

/// A release of the bucket named as people name one: by its number, or by a tag pointing at it.
pub(super) async fn find_release(
    bucket: &Bucket<'_>,
    release: &str,
) -> anyhow::Result<api::Release> {
    let release_num = match release.trim_start_matches('#').parse::<u64>() {
        Ok(num) => num,
        Err(_) => bucket
            .tags()
            .await?
            .into_iter()
            .find(|tag| tag.id == release)
            .map(|tag| tag.release.release_num)
            .with_context(|| {
                format!("{release} is neither a release number nor a tag of {bucket}")
            })?,
    };
    bucket
        .release_by_num(release_num)
        .await?
        .with_context(|| format!("{bucket} has no release #{release_num}"))
}

/// `#9 v2.0`: a release as messages name it, its name cut short if it's long.
pub(super) fn release_label(release: &api::Release) -> String {
    format!("#{} {}", release.release_num, short(&release.name, 60))
}

pub(super) async fn list(args: ListArgs, output: Output) -> anyhow::Result<()> {
    let ListArgs {
        bucket,
        app: arg,
        limit,
    } = args;
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let (releases, more) = progress::during("Loading the releases…", async {
        target.bucket(&bucket).await?.releases(limit).await
    })
    .await?;

    if output == Output::Json {
        return print_json(&json!({ "releases": releases, "more": more }));
    }
    if releases.is_empty() {
        eprintln!("{bucket} has no releases yet");
        return Ok(());
    }
    let rows: Vec<[String; 6]> = releases
        .iter()
        .map(|release| {
            let tags: Vec<String> = release.tags.iter().map(|tag| tag.id.clone()).collect();
            let mut tags = few(&tags, " ");
            if let Some(from) = &release.promoted_from {
                let from = format!(
                    "(from {}/{} #{})",
                    from.environment_id, from.bucket_id, from.release_num
                );
                tags = format!("{tags} {from}").trim_start().to_string();
            }
            [
                format!("#{}", release.release_num),
                short(&release.name, 40),
                release.entry_count.to_string(),
                HumanBytes(release.size_bytes).to_string(),
                ago(&release.create_time),
                tags,
            ]
        })
        .collect();
    table(
        ["RELEASE", "NAME", "FILES", "SIZE", "CREATED", "TAGS"],
        &rows,
    );
    if more {
        eprintln!("Showing the newest {limit}, pass --limit for more");
    }
    Ok(())
}

pub(super) async fn create(args: CreateArgs, output: Output) -> anyhow::Result<()> {
    let CreateArgs {
        bucket,
        name,
        app: arg,
        notes,
        yes,
    } = args;
    yes.require("cutting a release moves the bucket's latest tag: pass --yes to go ahead")?;
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let bucket = progress::during("Reading the bucket…", async {
        let bucket = target.bucket(&bucket).await?;
        bucket.require_open("cutting a release")?;
        Ok(bucket)
    })
    .await?;

    let cut = CutRelease {
        name: &name,
        notes: notes.as_deref(),
        tags: &[],
        confirmed: yes.yes,
        shown: false,
    };
    let Some(release) = cut_release(&bucket, cut).await? else {
        bail!("{bucket} has nothing new to release: its files match the latest release");
    };
    if output == Output::Json {
        print_json(&release)?;
    }
    Ok(())
}

/// A release to cut, and how much to say and ask first.
pub(super) struct CutRelease<'a> {
    pub name: &'a str,
    pub notes: Option<&'a str>,
    /// Tags that will be pointed at it, to name in the question.
    pub tags: &'a [String],
    /// Already agreed to, so there's nothing to ask.
    pub confirmed: bool,
    /// Already shown as a sync's plan, so there's nothing to show again.
    pub shown: bool,
}

/// Cuts a release of the bucket's files as they are now, after showing what it holds and asking.
/// None when nothing differs from the latest release.
pub(super) async fn cut_release(
    bucket: &Bucket<'_>,
    cut: CutRelease<'_>,
) -> anyhow::Result<Option<api::Release>> {
    let unreleased = progress::during("Reading the bucket…", bucket.unreleased()).await?;
    if unreleased.is_empty() {
        return Ok(None);
    }

    if !cut.shown {
        let delta = HumanBytes(unreleased.size_delta.unsigned_abs());
        let sign = if unreleased.size_delta < 0 { "-" } else { "+" };
        eprintln!(
            "{} of {bucket}: {} added, {} changed, {} removed, {} unchanged ({sign}{delta})",
            style(cut.name).bold().for_stderr(),
            unreleased.added,
            unreleased.changed,
            unreleased.removed,
            unreleased.unchanged,
        );
        eprintln!(
            "{}",
            style("The latest tag moves to it, so game clients following latest get it")
                .dim()
                .for_stderr()
        );
    }
    let question = if cut.tags.is_empty() {
        "Cut this release?".to_string()
    } else {
        format!("Cut this release, and point {} at it?", cut.tags.join(", "))
    };
    prompt::confirm_or_yes(&question, cut.confirmed)?;

    let release = progress::during(
        "Cutting the release…",
        bucket.create_release(cut.name, cut.notes),
    )
    .await?;
    eprintln!(
        "{} Released {} as #{} of {bucket} {}",
        theme::accent().apply_to("✓"),
        style(short(&release.name, 60)).bold().for_stderr(),
        release.release_num,
        release_size(&release)
    );
    Ok(Some(release))
}

/// `· 4 files, 195 KiB`, dimmed, to end the line that announces a release.
pub(super) fn release_size(release: &api::Release) -> String {
    style(format!(
        "· {} files, {}",
        release.entry_count,
        HumanBytes(release.size_bytes)
    ))
    .dim()
    .for_stderr()
    .to_string()
}
