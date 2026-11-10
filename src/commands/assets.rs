//! `toaba assets`: an app's files, in the buckets of its environments, as Unity's `ugs ccd`. The CLI
//! covers what each build does; setting environments and buckets up stays in the dashboard.

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use clap::Subcommand;
use console::style;
use indicatif::{HumanBytes, ProgressBar};
use serde_json::json;

use crate::output::{Output, few, pad, plain, print_json, short, table, width};
use crate::prompt::YesArg;
use crate::target::{self, AppArg, Bucket, BucketRef, tag_name};
use crate::{api, progress, prompt, sync, theme};

#[derive(Subcommand)]
pub enum Command {
    /// The app's buckets of files
    Bucket {
        #[command(subcommand)]
        command: BucketCommand,
    },
    /// A bucket's releases: its files as they were when each was cut
    Release {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
    /// A bucket's tags: the names game clients download releases by
    Tag {
        #[command(subcommand)]
        command: TagCommand,
    },
    /// Publish a release of one bucket in another, such as staging to production
    ///
    /// The target bucket's files become the release's, without copying any bytes;
    /// a new release is cut there and its `latest` tag moves to it. Unreleased
    /// changes in the target are discarded.
    Promote {
        /// The bucket the release is in, as env/bucket
        source: BucketRef,
        /// The release: its number, or a tag of the source bucket, such as latest
        release: String,
        /// The bucket to publish it in, as env/bucket
        target: BucketRef,
        #[command(flatten)]
        app: AppArg,
        /// Point this tag of the target bucket at the new release; repeatable
        #[arg(long, value_parser = tag_name)]
        tag: Vec<String>,
        #[command(flatten)]
        yes: YesArg,
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

#[derive(Subcommand)]
pub enum ReleaseCommand {
    /// List a bucket's releases, newest first
    #[command(visible_alias = "ls")]
    List {
        /// The bucket, as env/bucket
        bucket: BucketRef,
        #[command(flatten)]
        app: AppArg,
        /// How many releases to show at most
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=1000))]
        limit: u16,
    },
    /// Cut a release of a bucket's files as they are now
    ///
    /// The bucket's `latest` tag moves to the new release, so game clients that
    /// follow `latest` get it. Other tags stay where they are.
    Create {
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
    },
}

#[derive(Subcommand)]
pub enum TagCommand {
    /// List a bucket's tags and the releases they point at
    #[command(visible_alias = "ls")]
    List {
        /// The bucket, as env/bucket
        bucket: BucketRef,
        #[command(flatten)]
        app: AppArg,
    },
    /// Point a tag at a release, creating the tag or moving it
    ///
    /// Game clients that download by the tag get that release. Moving a tag back
    /// to an older release is how to roll back. `latest` can't be set: it follows
    /// new releases by itself.
    Set {
        /// The tag, such as stable
        #[arg(value_parser = tag_name)]
        tag: String,
        /// The bucket, as env/bucket
        bucket: BucketRef,
        /// The release: its number, or another tag to point at the same release, such as latest
        release: String,
        #[command(flatten)]
        app: AppArg,
        #[command(flatten)]
        yes: YesArg,
    },
}

pub async fn run(command: Command, output: Output) -> anyhow::Result<()> {
    match command {
        Command::Promote {
            source,
            release,
            target,
            app,
            tag,
            yes,
        } => promote(source, release, target, app, tag, yes, output).await,
        Command::Tag {
            command: TagCommand::List { bucket, app },
        } => list_tags(bucket, app, output).await,
        Command::Tag {
            command:
                TagCommand::Set {
                    tag,
                    bucket,
                    release,
                    app,
                    yes,
                },
        } => set_tag(tag, bucket, release, app, yes, output).await,
        Command::Release {
            command: ReleaseCommand::List { bucket, app, limit },
        } => list_releases(bucket, app, limit, output).await,
        Command::Release {
            command:
                ReleaseCommand::Create {
                    bucket,
                    name,
                    app,
                    notes,
                    yes,
                },
        } => create_release(bucket, name, app, notes, yes, output).await,
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
            release,
            notes,
            tag,
        } => {
            let options = SyncOptions {
                dry_run,
                exclude,
                yes,
                concurrency: concurrency.into(),
                release,
                notes,
                tags: tag,
            };
            sync(dir, bucket, app, options, output).await
        }
    }
}

/// `· 4 files, 195 KiB`, dimmed, to end the line that announces a release.
fn release_size(release: &api::Release) -> String {
    style(format!(
        "· {} files, {}",
        release.entry_count,
        HumanBytes(release.size_bytes)
    ))
    .dim()
    .for_stderr()
    .to_string()
}

struct SyncOptions {
    dry_run: bool,
    exclude: Vec<String>,
    yes: YesArg,
    concurrency: usize,
    release: Option<String>,
    notes: Option<String>,
    tags: Vec<String>,
}

async fn sync(
    dir: PathBuf,
    bucket: BucketRef,
    arg: AppArg,
    options: SyncOptions,
    output: Output,
) -> anyhow::Result<()> {
    if !options.dry_run {
        options.yes.require(
            "sync changes the bucket: pass --yes to go ahead, or --dry-run to only look",
        )?;
    }
    let files = sync::scan(&dir, &options.exclude)?;
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
        move || sync::hash(files, &bar)
    })
    .await??;
    bar.finish_and_clear();

    let plan = sync::plan(&local, &remote);
    let applies = !options.dry_run && !plan.is_empty();
    // With -o json the plan is still shown to the person asked to confirm, on stderr.
    if output == Output::Text {
        print!("{}", plan_text(&plan, true));
    } else if applies && !options.yes.yes {
        eprint!("{}", plan_text(&plan, false));
    }

    if applies {
        let question = match &options.release {
            Some(name) if options.tags.is_empty() => {
                format!("Sync {bucket} like this, and release it as {name}?")
            }
            Some(name) => format!(
                "Sync {bucket} like this, release it as {name}, and point {} at it?",
                options.tags.join(", ")
            ),
            None => format!("Sync {bucket} like this?"),
        };
        options.yes.confirm(&question)?;
        let bar = ProgressBar::new(plan.upload_bytes()).with_style(progress::bytes("Uploading"));
        sync::apply(&bucket, &local, &remote, &plan, options.concurrency, &bar)
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
    if let (Some(name), false) = (&options.release, options.dry_run) {
        let cut = CutRelease {
            name,
            notes: options.notes.as_deref(),
            tags: &options.tags,
            confirmed: options.yes.yes || applies,
            shown: applies,
        };
        release = cut_release(&bucket, cut).await?;
        match &release {
            Some(release) => tags = point_tags(&bucket, &options.tags, release).await?,
            None if options.tags.is_empty() => {
                eprintln!("Nothing new to release: {bucket} matches its latest release");
            }
            None => eprintln!(
                "Nothing new to release: {bucket} matches its latest release, so {} stays",
                options.tags.join(", ")
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
fn plan_text(plan: &sync::Plan, stdout: bool) -> String {
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

async fn create_release(
    bucket: BucketRef,
    name: String,
    arg: AppArg,
    notes: Option<String>,
    yes: YesArg,
    output: Output,
) -> anyhow::Result<()> {
    yes.require("cutting a release moves the bucket's latest tag: pass --yes to go ahead")?;
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let bucket = progress::during("Reading the bucket…", target.bucket(&bucket)).await?;

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

/// Points each tag at the release just cut, creating the tag or moving it.
async fn point_tags(
    bucket: &Bucket<'_>,
    tags: &[String],
    release: &api::Release,
) -> anyhow::Result<Vec<api::Tag>> {
    if tags.is_empty() {
        return Ok(Vec::new());
    }
    let set = progress::during("Setting the tags…", async {
        let current = bucket.tags().await?;
        let mut set = Vec::new();
        for tag in tags {
            let revision = current.iter().find(|c| &c.id == tag).map(|c| c.revision);
            let moved = bucket
                .set_tag(tag, &release.id, revision)
                .await
                .with_context(|| format!("couldn't point {tag} at #{}", release.release_num))?;
            set.push(moved);
        }
        Ok(set)
    })
    .await?;
    for tag in &set {
        eprintln!(
            "{} {} now points at #{} in {bucket}",
            theme::accent().apply_to("✓"),
            style(&tag.id).bold().for_stderr(),
            release.release_num
        );
    }
    Ok(set)
}

/// A release to cut, and how much to say and ask first.
struct CutRelease<'a> {
    name: &'a str,
    notes: Option<&'a str>,
    /// Tags that will be pointed at it, to name in the question.
    tags: &'a [String],
    /// Already agreed to, so there's nothing to ask.
    confirmed: bool,
    /// Already shown as a sync's plan, so there's nothing to show again.
    shown: bool,
}

/// Cuts a release of the bucket's files as they are now, after showing what it holds and asking.
/// None when nothing differs from the latest release.
async fn cut_release(
    bucket: &Bucket<'_>,
    cut: CutRelease<'_>,
) -> anyhow::Result<Option<api::Release>> {
    let unreleased = progress::during("Reading the bucket…", bucket.unreleased()).await?;
    if unreleased.added + unreleased.changed + unreleased.removed == 0 {
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

async fn promote(
    source: BucketRef,
    release: String,
    to: BucketRef,
    arg: AppArg,
    tags: Vec<String>,
    yes: YesArg,
    output: Output,
) -> anyhow::Result<()> {
    yes.require("promoting changes the target bucket and its latest tag: pass --yes to go ahead")?;
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let (source, from, to, to_tags, diff) = progress::during("Reading the buckets…", async {
        let source = target.bucket(&source).await?;
        let from = source.release(&release).await?;
        let to = target.bucket(&to).await?;
        let to_tags = to.tags().await?;
        // Against what the target's players have now: its latest release, if it has one.
        let diff = match to_tags.iter().find(|tag| tag.id == "latest") {
            Some(latest) => Some(
                source
                    .compare_releases(&from.id, &latest.release.id)
                    .await?,
            ),
            None => None,
        };
        Ok((source, from, to, to_tags, diff))
    })
    .await?;
    let unreleased = to.info.unreleased_count;

    let bold = |text: &str| style(text.to_string()).bold().for_stderr();
    let dim = |text: String| style(text).dim().for_stderr();
    let from_label = format!("#{} {}", from.release_num, short(&from.name, 60));
    let same = diff
        .as_ref()
        .is_some_and(|diff| diff.added + diff.changed + diff.removed == 0);
    if same && unreleased == 0 {
        eprintln!("{to} already has the files of {from_label}: nothing to promote");
        if !tags.is_empty() {
            eprintln!(
                "{}",
                dim(format!(
                    "To move a tag there, use `toaba assets tag set <tag> {to} latest`"
                ))
            );
        }
        return Ok(());
    }

    eprintln!(
        "Promote {} from {source} into {}",
        bold(&from_label),
        bold(&to.to_string())
    );
    match &diff {
        Some(diff) => eprintln!(
            "  Against what {to} has now: {} added, {} changed, {} removed, {} unchanged",
            diff.added, diff.changed, diff.removed, diff.unchanged
        ),
        None => eprintln!(
            "  {to} has no release yet: this is its first, with {} files",
            from.entry_count
        ),
    }
    if unreleased > 0 {
        eprintln!(
            "  {} {to} has {unreleased} unreleased changes, which are discarded",
            theme::warning().apply_to("!")
        );
    }
    let mut moves = Vec::new();
    for tag in &tags {
        let current = to_tags.iter().find(|current| &current.id == tag);
        match current {
            Some(current) => eprintln!(
                "  {tag} moves from #{} to the new release",
                current.release.release_num
            ),
            None => eprintln!("  {tag} is created, pointing at the new release"),
        }
        moves.push((tag.clone(), current.map(|current| current.revision)));
    }
    eprintln!(
        "{}",
        dim(format!(
            "Game clients of {to} following latest{} get it",
            if tags.is_empty() {
                String::new()
            } else {
                format!(" or {}", tags.join(", "))
            }
        ))
    );
    yes.confirm("Promote?")?;

    let promoted = progress::during(
        "Promoting…",
        source.promote(
            &from.id,
            &to.name.environment_id,
            &to.name.bucket_id,
            &moves,
        ),
    )
    .await?;
    eprintln!(
        "{} Promoted {} to {to} as #{} {}",
        theme::accent().apply_to("✓"),
        bold(&from_label),
        promoted.release_num,
        release_size(&promoted)
    );
    if output == Output::Json {
        print_json(&promoted)?;
    }
    Ok(())
}

async fn set_tag(
    tag: String,
    bucket: BucketRef,
    release: String,
    arg: AppArg,
    yes: YesArg,
    output: Output,
) -> anyhow::Result<()> {
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let (bucket, current, to) = progress::during("Reading the bucket…", async {
        let bucket = target.bucket(&bucket).await?;
        let to = bucket.release(&release).await?;
        let current = bucket.tags().await?.into_iter().find(|t| t.id == tag);
        Ok((bucket, current, to))
    })
    .await?;

    let bold = |text: &str| style(text.to_string()).bold().for_stderr();
    let to_label = format!("#{} {}", to.release_num, short(&to.name, 60));
    if let Some(current) = &current {
        if current.release.id == to.id {
            eprintln!("{} already points at {to_label} in {bucket}", bold(&tag));
            if output == Output::Json {
                print_json(current)?;
            }
            return Ok(());
        }
        eprintln!(
            "{} in {bucket}: #{} → {to_label}",
            bold(&tag),
            current.release.release_num
        );
        eprintln!(
            "{}",
            style(format!(
                "Game clients downloading by {tag} get release #{} from then on",
                to.release_num
            ))
            .dim()
            .for_stderr()
        );
        yes.confirm(&format!("Move {tag}?"))?;
    }

    let revision = current.as_ref().map(|current| current.revision);
    let set = progress::during("Setting the tag…", bucket.set_tag(&tag, &to.id, revision)).await?;
    eprintln!(
        "{} {} {} {to_label} in {bucket}",
        theme::accent().apply_to("✓"),
        bold(&tag),
        if current.is_some() {
            "now points at"
        } else {
            "created, pointing at"
        },
    );
    if output == Output::Json {
        print_json(&set)?;
    }
    Ok(())
}

async fn list_tags(bucket: BucketRef, arg: AppArg, output: Output) -> anyhow::Result<()> {
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let tags = progress::during("Loading the tags…", async {
        target.bucket(&bucket).await?.tags().await
    })
    .await?;

    if output == Output::Json {
        return print_json(&json!({ "tags": tags }));
    }
    if tags.is_empty() {
        eprintln!("{bucket} has no tags yet: latest appears with its first release");
        return Ok(());
    }
    let rows: Vec<[String; 4]> = tags
        .iter()
        .map(|tag| {
            [
                tag.id.clone(),
                format!("#{}", tag.release.release_num),
                tag.previous_release
                    .as_ref()
                    .map(|release| format!("#{}", release.release_num))
                    .unwrap_or_default(),
                tag.update_time.format("%-d %b %Y").to_string(),
            ]
        })
        .collect();
    table(["TAG", "RELEASE", "BEFORE", "MOVED"], &rows);
    Ok(())
}

async fn list_releases(
    bucket: BucketRef,
    arg: AppArg,
    limit: u16,
    output: Output,
) -> anyhow::Result<()> {
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
                release.create_time.format("%-d %b %Y").to_string(),
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

async fn list_buckets(arg: AppArg, output: Output) -> anyhow::Result<()> {
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let buckets = progress::during("Loading the buckets…", target.buckets()).await?;

    if output == Output::Json {
        let buckets: Vec<_> = buckets
            .iter()
            .map(|(environment, bucket)| {
                json!({
                    "id": bucket.id,
                    "name": bucket.name,
                    "environment": { "id": environment.id, "name": environment.name },
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
        .map(|(environment, bucket)| {
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
                environment.id.clone(),
                bucket.id.clone(),
                write,
                bucket.unreleased_count.to_string(),
                few(&tags, ", "),
            ]
        })
        .collect();
    table(["ENV", "BUCKET", "WRITE", "UNRELEASED", "TAGS"], &rows);
    Ok(())
}
