//! `toaba assets bucket`: the app's buckets.

use indicatif::HumanBytes;
use serde_json::json;

use crate::output::{Output, few, print_json, table};
use crate::progress;
use crate::target::{self, AppArg};

#[derive(clap::Args)]
pub struct ListArgs {
    #[command(flatten)]
    app: AppArg,
}

pub(super) async fn list(args: ListArgs, output: Output) -> anyhow::Result<()> {
    // Found before the spinner starts, since finding it may ask which app.
    let target = target::app(args.app).await?;
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
                    "max_file_size_bytes": bucket.limits.max_file_size_bytes,
                    "allowed_content_types": bucket.limits.allowed_content_types,
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
    let rows: Vec<[String; 7]> = buckets
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
                match bucket.limits.max_file_size_bytes {
                    Some(bytes) => HumanBytes(bytes).to_string(),
                    None => "none".to_string(),
                },
                match &bucket.limits.allowed_content_types {
                    Some(types) => few(types, ", "),
                    None => "any".to_string(),
                },
                bucket.unreleased_count.to_string(),
                few(&tags, ", "),
            ]
        })
        .collect();
    let headers = [
        "ENV",
        "BUCKET",
        "WRITE",
        "MAX FILE SIZE",
        "TYPES",
        "UNRELEASED",
        "TAGS",
    ];
    table(headers, &rows);
    Ok(())
}
