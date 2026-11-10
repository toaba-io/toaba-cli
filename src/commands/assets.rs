//! `toaba assets`: an app's files, in the buckets of its environments, as Unity's `ugs ccd`. The CLI
//! covers what each build does; setting environments and buckets up stays in the dashboard.

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use clap::Subcommand;
use console::style;
use indicatif::{HumanBytes, ProgressBar};
use serde_json::json;

use crate::output::{Output, pad, print_json, width};
use crate::target::{self, AppArg, BucketRef};
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
        /// After syncing, cut a release with this name, such as v1.4
        #[arg(long, value_name = "NAME")]
        release: Option<String>,
        /// Notes to keep with the release
        #[arg(long, requires = "release")]
        notes: Option<String>,
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
        /// Go ahead without asking; needed where there's nobody to ask, as in CI
        #[arg(short, long)]
        yes: bool,
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
        /// Go ahead without asking; needed where there's nobody to ask, as in CI
        #[arg(short, long)]
        yes: bool,
    },
}

/// A tag's name: lowercase letters, digits and single hyphens, starting with a letter.
fn tag_name(text: &str) -> Result<String, String> {
    if target::is_slug(text) {
        Ok(text.to_string())
    } else {
        Err("a tag is lowercase letters, digits and hyphens, starting with a letter".to_string())
    }
}

pub async fn run(command: Command, output: Output) -> anyhow::Result<()> {
    match command {
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
        } => {
            let options = SyncOptions {
                dry_run,
                exclude,
                yes,
                concurrency: concurrency.into(),
                release,
                notes,
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
    release: Option<String>,
    notes: Option<String>,
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
        let question = match &options.release {
            Some(name) => format!("Sync {bucket} like this, and release it as {name}?"),
            None => format!("Sync {bucket} like this?"),
        };
        prompt::confirm_or_yes(&question, options.yes)?;
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

    // The release takes whatever the bucket holds unreleased, also from an earlier sync. A sync
    // that was just confirmed covers it; otherwise it asks for itself.
    let mut release = None;
    if let (Some(name), false) = (&options.release, options.dry_run) {
        let confirmed = options.yes || applies;
        release = cut_release(
            &target,
            &bucket,
            name,
            options.notes.as_deref(),
            confirmed,
            applies,
        )
        .await?;
        if release.is_none() {
            eprintln!("Nothing new to release: {bucket} matches its latest release");
        }
    }
    if output == Output::Json {
        print_json(&json!({
            "bucket": bucket.to_string(),
            "applied": applies,
            "release": release,
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
                "  {mark} {}  {:>size_width$}  {}\n",
                pad(&change.path, path_width),
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
    yes: bool,
    output: Output,
) -> anyhow::Result<()> {
    if !yes && !prompt::interactive() {
        bail!("cutting a release moves the bucket's latest tag: pass --yes to go ahead");
    }
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let line = progress::start("Reading the bucket…");
    bucket
        .check(&target)
        .await
        .inspect_err(|_| progress::failed(&line, "Reading the bucket"))?;
    line.finish_and_clear();

    let Some(release) = cut_release(&target, &bucket, &name, notes.as_deref(), yes, false).await?
    else {
        bail!("{bucket} has nothing new to release: its files match the latest release");
    };
    if output == Output::Json {
        print_json(&release)?;
    }
    Ok(())
}

/// Cuts a release of the bucket's files as they are now, after showing what it holds and asking,
/// unless `confirmed`. After a sync that was just shown and agreed to, `shown`, it says neither
/// again. None when nothing differs from the latest release.
async fn cut_release(
    target: &target::Target,
    bucket: &BucketRef,
    name: &str,
    notes: Option<&str>,
    confirmed: bool,
    shown: bool,
) -> anyhow::Result<Option<api::Release>> {
    let path = api::bucket_path(&target.app_id, &bucket.environment_id, &bucket.bucket_id);
    let line = progress::start("Reading the bucket…");
    let unreleased = target
        .client
        .unreleased(&path)
        .await
        .inspect_err(|_| progress::failed(&line, "Reading the bucket"))?;
    line.finish_and_clear();
    if unreleased.added + unreleased.changed + unreleased.removed == 0 {
        return Ok(None);
    }

    if !shown {
        let delta = HumanBytes(unreleased.size_delta.unsigned_abs());
        let sign = if unreleased.size_delta < 0 { "-" } else { "+" };
        eprintln!(
            "{} of {bucket}: {} added, {} changed, {} removed, {} unchanged ({sign}{delta})",
            style(name).bold().for_stderr(),
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
    prompt::confirm_or_yes("Cut this release?", confirmed)?;

    let line = progress::start("Cutting the release…");
    let release = target
        .client
        .create_release(&path, name, notes)
        .await
        .inspect_err(|_| progress::failed(&line, "Cutting the release"))?;
    progress::finish(
        &line,
        format!(
            "{} Released {} as #{} of {bucket} {}",
            theme::accent().apply_to("✓"),
            style(&release.name).bold().for_stderr(),
            release.release_num,
            style(format!(
                "· {} files, {}",
                release.entry_count,
                HumanBytes(release.size_bytes)
            ))
            .dim()
            .for_stderr()
        ),
    );
    Ok(Some(release))
}

async fn set_tag(
    tag: String,
    bucket: BucketRef,
    release: String,
    arg: AppArg,
    yes: bool,
    output: Output,
) -> anyhow::Result<()> {
    if tag == "latest" {
        bail!("latest follows new releases by itself and can't be set");
    }
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let path = api::bucket_path(&target.app_id, &bucket.environment_id, &bucket.bucket_id);
    let line = progress::start("Reading the bucket…");
    let read = async {
        bucket.check(&target).await?;
        let tags = target.client.tags(&path).await?;
        let release_num = match release.trim_start_matches('#').parse::<u64>() {
            Ok(num) => num,
            Err(_) => tags
                .iter()
                .find(|other| other.id == release)
                .map(|other| other.release.release_num)
                .with_context(|| {
                    format!("{release} is neither a release number nor a tag of {bucket}")
                })?,
        };
        let to = target
            .client
            .release_by_num(&path, release_num)
            .await
            .map_err(|err| {
                // Only the not-found answer is reworded to name the bucket.
                if err.to_string().starts_with("the bucket has no release") {
                    anyhow::anyhow!("{bucket} has no release #{release_num}")
                } else {
                    err
                }
            })?;
        anyhow::Ok((tags.into_iter().find(|current| current.id == tag), to))
    }
    .await;
    let (current, to) = read.inspect_err(|_| progress::failed(&line, "Reading the bucket"))?;
    line.finish_and_clear();

    let bold = |text: &str| style(text.to_string()).bold().for_stderr();
    let to_label = format!("#{} {}", to.release_num, to.name);
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
        prompt::confirm_or_yes(&format!("Move {tag}?"), yes)?;
    }

    let line = progress::start("Setting the tag…");
    let set = target
        .client
        .set_tag(
            &path,
            &tag,
            &to.id,
            current.as_ref().map(|current| current.revision),
        )
        .await
        .inspect_err(|_| progress::failed(&line, "Setting the tag"))?;
    progress::finish(
        &line,
        format!(
            "{} {} {} {to_label} in {bucket}",
            theme::accent().apply_to("✓"),
            bold(&tag),
            if current.is_some() {
                "now points at"
            } else {
                "created, pointing at"
            },
        ),
    );
    if output == Output::Json {
        print_json(&set)?;
    }
    Ok(())
}

async fn list_tags(bucket: BucketRef, arg: AppArg, output: Output) -> anyhow::Result<()> {
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let line = progress::start("Loading the tags…");
    let loaded = async {
        bucket.check(&target).await?;
        let path = api::bucket_path(&target.app_id, &bucket.environment_id, &bucket.bucket_id);
        target.client.tags(&path).await
    }
    .await;
    let tags = loaded.inspect_err(|_| progress::failed(&line, "Loading the tags"))?;
    line.finish_and_clear();

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
    let headers = ["TAG", "RELEASE", "BEFORE", "MOVED"];
    let widths: Vec<usize> = (0..headers.len())
        .map(|column| width(headers[column], rows.iter().map(|row| &row[column])))
        .collect();
    let print = |cells: [&str; 4]| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| pad(cell, *width))
            .collect();
        println!("{}", padded.join("  ").trim_end());
    };
    print(headers);
    for row in &rows {
        print([&row[0], &row[1], &row[2], &row[3]]);
    }
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
    let line = progress::start("Loading the releases…");
    let loaded = async {
        bucket.check(&target).await?;
        let path = api::bucket_path(&target.app_id, &bucket.environment_id, &bucket.bucket_id);
        target.client.releases(&path, limit).await
    }
    .await;
    let (releases, more) =
        loaded.inspect_err(|_| progress::failed(&line, "Loading the releases"))?;
    line.finish_and_clear();

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
            let mut tags: Vec<String> = release.tags.iter().map(|tag| tag.id.clone()).collect();
            if let Some(from) = &release.promoted_from {
                tags.push(format!(
                    "(from {}/{} #{})",
                    from.environment_id, from.bucket_id, from.release_num
                ));
            }
            [
                format!("#{}", release.release_num),
                release.name.clone(),
                release.entry_count.to_string(),
                HumanBytes(release.size_bytes).to_string(),
                release.create_time.format("%-d %b %Y").to_string(),
                tags.join(" "),
            ]
        })
        .collect();
    let headers = ["RELEASE", "NAME", "FILES", "SIZE", "CREATED", "TAGS"];
    let widths: Vec<usize> = (0..headers.len())
        .map(|column| width(headers[column], rows.iter().map(|row| &row[column])))
        .collect();
    let print = |cells: [&str; 6]| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| pad(cell, *width))
            .collect();
        println!("{}", padded.join("  ").trim_end());
    };
    print(headers);
    for row in &rows {
        print([&row[0], &row[1], &row[2], &row[3], &row[4], &row[5]]);
    }
    if more {
        eprintln!("Showing the newest {limit}, pass --limit for more");
    }
    Ok(())
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
            .map(|(cell, width)| pad(cell, *width))
            .collect();
        println!("{}", padded.join("  ").trim_end());
    };
    line(headers);
    for row in &rows {
        line([&row[0], &row[1], &row[2], &row[3], &row[4]]);
    }
    Ok(())
}
