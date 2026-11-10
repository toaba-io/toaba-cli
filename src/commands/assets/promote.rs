//! `toaba assets promote`: publishes a release of one bucket in another.

use console::style;

use super::release::{find_release, release_label, release_size};
use super::tag::{LATEST, tag_name};
use crate::output::{Output, print_json, short};
use crate::prompt::YesArg;
use crate::target::{self, AppArg, BucketRef};
use crate::{api, progress, theme};

#[derive(clap::Args)]
pub struct Args {
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
    /// A name for the new release, such as v1.4 [default: the promoted release's]
    #[arg(long)]
    name: Option<String>,
    /// Notes to keep with the new release [default: the promoted release's]
    #[arg(long)]
    notes: Option<String>,
    #[command(flatten)]
    yes: YesArg,
}

pub(super) async fn run(args: Args, output: Output) -> anyhow::Result<()> {
    let Args {
        source,
        release,
        target: to,
        app: arg,
        tag: tags,
        name,
        notes,
        yes,
    } = args;
    yes.require("promoting changes the target bucket and its latest tag: pass --yes to go ahead")?;
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(arg).await?;
    let (source, from, to, to_tags, diff) = progress::during("Reading the buckets…", async {
        let source = target.bucket(&source).await?;
        let from = find_release(&source, &release).await?;
        let to = target.bucket(&to).await?;
        let to_tags = to.tags().await?;
        // Against what the target's players have now: its latest release, if it has one.
        let diff = match to_tags.iter().find(|tag| tag.id == LATEST) {
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
    let from_label = release_label(&from);
    let same = diff.as_ref().is_some_and(api::DiffSummary::is_empty);
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
        "Promote {} from {source} into {}{}",
        bold(&from_label),
        bold(&to.to_string()),
        match &name {
            Some(name) => format!(" as {}", bold(&short(name, 60))),
            None => String::new(),
        }
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
        moves.push(api::PromotedTag {
            id: tag.clone(),
            expected_revision: current.map(|current| current.revision),
        });
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

    let promotion = api::Promotion {
        target_environment_id: &to.name.environment_id,
        target_bucket_id: &to.name.bucket_id,
        name: name.as_deref(),
        notes: notes.as_deref(),
        tags: moves,
    };
    let promoted = progress::during("Promoting…", source.promote(&from.id, &promotion)).await?;
    eprintln!(
        "{} Promoted {} to {to} as {} {}",
        theme::accent().apply_to("✓"),
        bold(&from_label),
        bold(&release_label(&promoted)),
        release_size(&promoted)
    );
    if output == Output::Json {
        print_json(&promoted)?;
    }
    Ok(())
}
