//! `toaba assets tag`: a bucket's tags, the names game clients download releases by.

use anyhow::Context as _;
use console::style;
use serde_json::json;

use super::release::{find_release, release_label};
use crate::output::{Output, ago, print_json, table};
use crate::prompt::YesArg;
use crate::target::{self, AppArg, Bucket, BucketRef};
use crate::{api, progress, theme};

/// The tag every bucket has, which follows its newest release by itself.
pub(super) const LATEST: &str = "latest";

/// A tag to set, as a command's argument: an id people choose, and not `latest`.
pub(super) fn tag_name(text: &str) -> Result<String, String> {
    if text == LATEST {
        Err("latest follows new releases by itself and can't be set".to_string())
    } else if target::is_slug(text) {
        Ok(text.to_string())
    } else {
        Err("a tag is lowercase letters, digits and hyphens, starting with a letter".to_string())
    }
}

#[derive(clap::Args)]
pub struct ListArgs {
    /// The bucket, as env/bucket
    bucket: BucketRef,
    #[command(flatten)]
    app: AppArg,
}

#[derive(clap::Args)]
pub struct SetArgs {
    /// The tag, such as live
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
}

pub(super) async fn list(args: ListArgs, output: Output) -> anyhow::Result<()> {
    let bucket = args.bucket;
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(args.app).await?;
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
                ago(&tag.update_time),
            ]
        })
        .collect();
    table(["TAG", "RELEASE", "BEFORE", "MOVED"], &rows);
    Ok(())
}

pub(super) async fn set(args: SetArgs, output: Output) -> anyhow::Result<()> {
    let SetArgs {
        tag,
        bucket,
        release,
        app: arg,
        yes,
    } = args;
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let (bucket, current, to) = progress::during("Reading the bucket…", async {
        let bucket = target.bucket(&bucket).await?;
        let to = find_release(&bucket, &release).await?;
        let current = bucket.tags().await?.into_iter().find(|t| t.id == tag);
        Ok((bucket, current, to))
    })
    .await?;

    let bold = |text: &str| style(text.to_string()).bold().for_stderr();
    let to_label = release_label(&to);
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

/// Points each tag at the release just cut, creating the tag or moving it.
pub(super) async fn point_tags(
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
