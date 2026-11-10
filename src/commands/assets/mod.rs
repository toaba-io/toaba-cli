//! `toaba assets`: an app's files, in the buckets of its environments, as Unity's `ugs ccd`. The CLI
//! covers what each build does; setting environments and buckets up stays in the dashboard.

mod branch;
mod bucket;
mod promote;
mod release;
mod sync;
mod tag;

use clap::Subcommand;

use crate::output::Output;

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
    Promote(promote::Args),
    /// Make a bucket's files match a local folder
    ///
    /// Uploads the files that are new or changed and removes the ones no longer in
    /// the folder. Players aren't affected until a release is tagged.
    Sync(sync::Args),
    /// A bucket per branch or pull request, to try its content before it's merged
    Branch {
        #[command(subcommand)]
        command: BranchCommand,
    },
}

#[derive(Subcommand)]
pub enum BranchCommand {
    /// Make a branch's bucket match a local folder, and release it
    ///
    /// The bucket is branches/<branch>, created the first time with the environment.
    /// With --from, a new branch starts from that bucket's latest release, so only
    /// the files that differ are uploaded. Each sync cuts a release, which the
    /// bucket's latest tag follows. Run it again on every push.
    Sync(branch::SyncArgs),
}

#[derive(Subcommand)]
pub enum BucketCommand {
    /// List the app's buckets, in every environment
    #[command(visible_alias = "ls")]
    List(bucket::ListArgs),
}

#[derive(Subcommand)]
pub enum ReleaseCommand {
    /// List a bucket's releases, newest first
    #[command(visible_alias = "ls")]
    List(release::ListArgs),
    /// Cut a release of a bucket's files as they are now
    ///
    /// The bucket's `latest` tag moves to the new release, so game clients that
    /// follow `latest` get it. Other tags stay where they are.
    Create(release::CreateArgs),
}

#[derive(Subcommand)]
pub enum TagCommand {
    /// List a bucket's tags and the releases they point at
    #[command(visible_alias = "ls")]
    List(tag::ListArgs),
    /// Point a tag at a release, creating the tag or moving it
    ///
    /// Game clients that download by the tag get that release. Moving a tag back
    /// to an older release is how to roll back. `latest` can't be set: it follows
    /// new releases by itself.
    Set(tag::SetArgs),
}

pub async fn run(command: Command, output: Output) -> anyhow::Result<()> {
    match command {
        Command::Bucket {
            command: BucketCommand::List(args),
        } => bucket::list(args, output).await,
        Command::Release { command } => match command {
            ReleaseCommand::List(args) => release::list(args, output).await,
            ReleaseCommand::Create(args) => release::create(args, output).await,
        },
        Command::Tag { command } => match command {
            TagCommand::List(args) => tag::list(args, output).await,
            TagCommand::Set(args) => tag::set(args, output).await,
        },
        Command::Promote(args) => promote::run(args, output).await,
        Command::Sync(args) => sync::run(args, output).await,
        Command::Branch {
            command: BranchCommand::Sync(args),
        } => branch::sync(args, output).await,
    }
}
