//! `toaba assets branch`: a bucket per branch or pull request, in the app's `branches` environment,
//! to try a build's content before it's merged, as GitLab's review apps or Supabase's preview
//! branches. A pipeline runs the same command on every push.

use std::path::PathBuf;

use anyhow::Context as _;
use console::style;
use serde_json::json;

use super::release::{CutRelease, cut_release, release_label, release_size};
use super::sync::{FolderArgs, hash, show_plan, upload, with_plan};
use super::tag::LATEST;
use crate::output::{Output, print_json};
use crate::prompt::YesArg;
use crate::target::{self, AppArg, Bucket, BucketRef, Target};
use crate::{api, mirror, progress, theme};

/// The environment branches' buckets are in, created with the first branch.
const ENVIRONMENT: &str = "branches";
const ENVIRONMENT_NAME: &str = "Branches";

/// A branch's name, as a command's argument: the bucket it names.
fn branch(text: &str) -> Result<BucketRef, String> {
    if target::is_slug(text) {
        Ok(BucketRef {
            environment_id: ENVIRONMENT.to_string(),
            bucket_id: text.to_string(),
        })
    } else {
        Err(
            "a branch is lowercase letters, digits and hyphens, starting with a letter, \
             such as pr-123"
                .to_string(),
        )
    }
}

#[derive(clap::Args)]
pub struct SyncArgs {
    /// The local folder
    dir: PathBuf,
    /// The branch, such as pr-123: its bucket is branches/<branch>
    #[arg(value_parser = branch)]
    branch: BucketRef,
    /// Start a new branch from this bucket's latest release, as env/bucket, so only the files that
    /// differ are uploaded
    #[arg(long, value_name = "BUCKET")]
    from: Option<BucketRef>,
    /// A name for the release, such as the commit [default: the branch]
    #[arg(long, value_name = "NAME")]
    release: Option<String>,
    #[command(flatten)]
    app: AppArg,
    #[command(flatten)]
    folder: FolderArgs,
    #[command(flatten)]
    yes: YesArg,
}

#[derive(clap::Args)]
pub struct CloseArgs {
    /// The branch, such as pr-123
    #[arg(value_parser = branch)]
    branch: BucketRef,
    #[command(flatten)]
    app: AppArg,
    #[command(flatten)]
    yes: YesArg,
}

/// The release a new branch starts from, and the files it holds.
struct Start<'a> {
    source: Bucket<'a>,
    release: api::Release,
    files: Vec<api::ReleaseFile>,
}

pub(super) async fn sync(args: SyncArgs, output: Output) -> anyhow::Result<()> {
    let SyncArgs {
        dir,
        branch: name,
        from,
        release,
        app: arg,
        folder:
            FolderArgs {
                dry_run,
                exclude,
                concurrency,
            },
        yes,
    } = args;
    let release_name = release.unwrap_or_else(|| name.bucket_id.clone());
    if !dry_run {
        yes.require(
            "branch sync changes the branch's bucket: pass --yes to go ahead, or --dry-run to only \
             look",
        )?;
    }
    let files = mirror::scan(&dir, &exclude)?;
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let (existing, start, remote) = progress::during("Reading the branch…", async {
        let existing = target.find_bucket(&name).await?;
        if let Some(bucket) = &existing {
            bucket.require_open("syncing files")?;
        }
        // A branch starts from --from until it has a release, so a first run that stopped
        // halfway starts over the same way.
        let fresh = existing
            .as_ref()
            .is_none_or(|bucket| bucket.info.release_count == 0);
        let start = match &from {
            Some(from) if fresh => Some(start_from(&target, from).await?),
            _ => None,
        };
        let remote = match (&existing, &start) {
            (Some(bucket), None) => bucket.entries().await?,
            _ => Vec::new(),
        };
        Ok((existing, start, remote))
    })
    .await?;

    let local = hash(files).await?;
    let mut plan = match &start {
        Some(start) => mirror::plan(&local, start.files.iter().map(api::ReleaseFile::file)),
        None => mirror::plan(&local, remote.iter().filter_map(api::Entry::file)),
    };
    let creates = existing.is_none();
    let applies = !dry_run && (creates || start.is_some() || !plan.is_empty());

    let bold = |text: &str| style(text.to_string()).bold().for_stderr();
    match (&start, creates) {
        (Some(start), _) => eprintln!(
            "{} starts from {} of {}",
            bold(&name.to_string()),
            release_label(&start.release),
            start.source
        ),
        (None, true) => eprintln!("{} is new: every file is uploaded", bold(&name.to_string())),
        (None, false) => {}
    }
    show_plan(&plan, output, applies && !yes.yes);
    if dry_run && output == Output::Text {
        println!("Would then release {name} as {release_name}");
    }

    let mut created = false;
    let mut release = None;
    if !dry_run {
        if applies {
            yes.confirm(&if creates {
                format!("Create {name}, sync it like this, and release it as {release_name}?")
            } else {
                format!("Sync {name} like this, and release it as {release_name}?")
            })?;
        }
        let bucket = match existing {
            Some(bucket) => bucket,
            None => {
                let bucket = progress::during(
                    "Creating the branch…",
                    target.create_bucket(&name, ENVIRONMENT_NAME),
                )
                .await?;
                created = true;
                eprintln!(
                    "{} Created {}",
                    theme::accent().apply_to("✓"),
                    bold(&name.to_string())
                );
                bucket
            }
        };
        // The promotion gives the bucket the starting release's files under new ids, so it's
        // read again, and planned again from what it now holds.
        let mut remote = remote;
        if let Some(start) = &start {
            let promoted;
            (promoted, remote) = progress::during("Starting the branch…", async {
                let promoted = start
                    .source
                    .promote(&start.release.id, ENVIRONMENT, &name.bucket_id, &[])
                    .await?;
                Ok((promoted, bucket.entries().await?))
            })
            .await?;
            eprintln!(
                "{} Started {name} from {} of {} {}",
                theme::accent().apply_to("✓"),
                bold(&release_label(&start.release)),
                start.source,
                release_size(&promoted)
            );
            plan = mirror::plan(&local, remote.iter().filter_map(api::Entry::file));
        }
        if !plan.is_empty() {
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
        let cut = CutRelease {
            name: &release_name,
            notes: None,
            tags: &[],
            confirmed: yes.yes || applies,
            shown: applies,
        };
        release = cut_release(&bucket, cut).await?;
        if release.is_none() {
            eprintln!("Nothing new to release: {name} matches its latest release");
        }
        eprintln!(
            "{}",
            style(format!(
                "Game builds pointed at {name} with the {LATEST} tag get it"
            ))
            .dim()
            .for_stderr()
        );
    }

    if output == Output::Json {
        let from = start.as_ref().map(|start| {
            json!({
                "bucket": start.source.to_string(),
                "release_num": start.release.release_num,
            })
        });
        print_json(&with_plan(
            &plan,
            json!({
                "bucket": name.to_string(),
                "created": created,
                "from": from,
                "applied": applies,
                "release": release,
            }),
        ))?;
    }
    Ok(())
}

pub(super) async fn close(args: CloseArgs, output: Output) -> anyhow::Result<()> {
    let CloseArgs {
        branch: name,
        app: arg,
        yes,
    } = args;
    yes.require("closing deletes the branch's bucket: pass --yes to go ahead")?;
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let bucket = progress::during("Reading the branch…", target.find_bucket(&name)).await?;

    let deleted = match bucket {
        None => {
            eprintln!("{name} is already gone");
            false
        }
        Some(bucket) => {
            eprintln!(
                "Delete {} for good: {} files, {} releases",
                style(&name).bold().for_stderr(),
                bucket.info.entry_count,
                bucket.info.release_count
            );
            eprintln!(
                "{}",
                style("Game builds pointed at it get nothing from then on")
                    .dim()
                    .for_stderr()
            );
            yes.confirm(&format!("Delete {name}?"))?;
            progress::during("Deleting the branch…", async {
                match bucket.delete(bucket.info.revision).await {
                    // Closed meanwhile, by another run say: gone all the same.
                    Err(err) if api::answered(&err, "not_found") => Ok(()),
                    deleted => deleted,
                }
            })
            .await?;
            eprintln!("{} Deleted {name}", theme::accent().apply_to("✓"));
            true
        }
    };
    if output == Output::Json {
        print_json(&json!({ "bucket": name.to_string(), "deleted": deleted }))?;
    }
    Ok(())
}

/// The latest release of `from`, which a new branch starts from, and its files.
async fn start_from<'a>(target: &'a Target, from: &BucketRef) -> anyhow::Result<Start<'a>> {
    let source = target.bucket(from).await?;
    let latest = source
        .info
        .tags
        .iter()
        .find(|tag| tag.id == LATEST)
        .with_context(|| format!("{from} has no release yet for the branch to start from"))?
        .release
        .release_num;
    let release = source
        .release_by_num(latest)
        .await?
        .with_context(|| format!("{from} has no release #{latest}"))?;
    let files = source.release_files(&release.id).await?;
    Ok(Start {
        source,
        release,
        files,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn a_branch_is_named_as_a_bucket_is() {
        assert_eq!(branch("pr-123").unwrap().to_string(), "branches/pr-123");
        for bad in ["feature/ui", "PR-1", "pr--1", "123", ""] {
            assert!(branch(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_new_branch_starts_from_the_latest_release_of_from() {
        let server = MockServer::start().await;
        let staging = "/storage/v1/apps/app_a/environments/staging/buckets";
        Mock::given(method("GET"))
            .and(path(staging))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [{
                "id": "assets", "name": "Assets", "write_access": "open", "revision": 1, "entry_count": 0, "release_count": 2,
                "unreleased_count": 0,
                "tags": [{ "id": "latest", "release": { "id": "r2", "release_num": 2 },
                    "previous_release": null, "revision": 2,
                    "update_time": "2026-11-10T10:00:00Z" }],
            }]})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{staging}/assets/releases")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "id": "r2", "release_num": 2, "name": "v1.4", "notes": null,
                    "entry_count": 1, "size_bytes": 3, "promoted_from": null, "tags": [],
                    "create_time": "2026-11-10T10:00:00Z" }],
                "next_page_token": null,
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{staging}/assets/releases/r2/entries")))
            .and(query_param("page_size", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "entry_id": "e1", "path": "ui/logo.png", "version": {
                    "sha256": "aa", "size_bytes": 3, "content_type": "image/png" } }],
                "next_page_token": null,
            })))
            .mount(&server)
            .await;

        let target = Target {
            client: api::testing::client(&server).await,
            app_id: "app_a".into(),
            app_name: "Space Shooter".into(),
        };
        let start = start_from(&target, &"staging/assets".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(release_label(&start.release), "#2 v1.4");
        let files: Vec<&str> = start.files.iter().map(|file| file.file().0).collect();
        assert_eq!(files, ["ui/logo.png"]);
    }

    #[tokio::test]
    async fn a_bucket_without_a_release_can_t_start_a_branch() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/storage/v1/apps/app_a/environments/staging/buckets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [{
                "id": "assets", "name": "Assets", "write_access": "open", "revision": 1, "entry_count": 0, "release_count": 0,
                "unreleased_count": 4, "tags": [],
            }]})))
            .mount(&server)
            .await;

        let target = Target {
            client: api::testing::client(&server).await,
            app_id: "app_a".into(),
            app_name: "Space Shooter".into(),
        };
        let Err(err) = start_from(&target, &"staging/assets".parse().unwrap()).await else {
            panic!("there's no release to start from");
        };
        assert_eq!(
            err.to_string(),
            "staging/assets has no release yet for the branch to start from"
        );
    }
}
