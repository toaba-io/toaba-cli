//! An app's assets: the buckets of its environments, and through `BucketApi` a bucket's files,
//! uploads, releases and tags.

use std::collections::BTreeMap;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Client, Page};

impl Client {
    /// The environments of the app's assets, by name.
    pub async fn environments(&self, app_id: &str) -> anyhow::Result<Vec<Environment>> {
        self.get(&format!("/storage/v1/apps/{app_id}/environments"))
            .await
    }

    /// The environment's buckets, by name.
    pub async fn buckets(&self, app_id: &str, environment_id: &str) -> anyhow::Result<Vec<Bucket>> {
        self.get(&format!(
            "/storage/v1/apps/{app_id}/environments/{environment_id}/buckets"
        ))
        .await
    }

    /// A bucket of the app's assets, to call its files, releases and tags through.
    pub fn bucket(&self, app_id: &str, environment_id: &str, bucket_id: &str) -> BucketApi<'_> {
        BucketApi {
            client: self,
            path: format!(
                "/storage/v1/apps/{app_id}/environments/{environment_id}/buckets/{bucket_id}"
            ),
        }
    }
}

/// One bucket of an app's assets: its files, uploads, releases and tags.
pub struct BucketApi<'a> {
    client: &'a Client,
    path: String,
}

impl BucketApi<'_> {
    /// Every file in the bucket, by path, read a thousand at a time.
    pub async fn entries(&self) -> anyhow::Result<Vec<Entry>> {
        let path = format!("{}/entries", self.path);
        let mut entries = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut query = vec![("recursive", "true"), ("page_size", "1000")];
            if let Some(token) = &page_token {
                query.push(("page_token", token));
            }
            let page: Page<Entry> = self.client.get_whole(&path, &query).await?;
            entries.extend(page.data);
            match page.next_page_token {
                Some(token) => page_token = Some(token),
                None => return Ok(entries),
            }
        }
    }

    /// The newest releases, `limit` at most (up to 1,000), and whether there are more.
    pub async fn releases(&self, limit: u16) -> anyhow::Result<(Vec<Release>, bool)> {
        let page = self.release_page(limit, None).await?;
        Ok((page.data, page.next_page_token.is_some()))
    }

    /// The release with this number, which is how people name one, if the bucket has it.
    pub async fn release_by_num(&self, release_num: u64) -> anyhow::Result<Option<Release>> {
        let mut page_token = None;
        loop {
            let page = self.release_page(1000, page_token.as_deref()).await?;
            // Newest first: once past the number, it isn't there.
            let passed = page
                .data
                .last()
                .is_none_or(|last| last.release_num < release_num);
            if let Some(found) = page.data.into_iter().find(|r| r.release_num == release_num) {
                return Ok(Some(found));
            }
            match page.next_page_token {
                Some(token) if !passed => page_token = Some(token),
                _ => return Ok(None),
            }
        }
    }

    async fn release_page(
        &self,
        page_size: u16,
        page_token: Option<&str>,
    ) -> anyhow::Result<Page<Release>> {
        let page_size = page_size.to_string();
        let mut query = vec![("page_size", page_size.as_str())];
        if let Some(token) = page_token {
            query.push(("page_token", token));
        }
        self.client
            .get_whole(&format!("{}/releases", self.path), &query)
            .await
    }

    /// Cuts a release of the bucket's files as they are now; `latest` moves to it.
    pub async fn create_release(&self, name: &str, notes: Option<&str>) -> anyhow::Result<Release> {
        self.client
            .post_once(
                &format!("{}/releases", self.path),
                &serde_json::json!({ "name": name, "notes": notes }),
            )
            .await
    }

    /// How the bucket's files differ from its latest release: everything is added before the first.
    pub async fn unreleased(&self) -> anyhow::Result<DiffSummary> {
        self.diff(&format!("{}/compare", self.path), &[("page_size", "1")])
            .await
    }

    /// How release `release_id` of this bucket differs from release `base_id`, which may be in
    /// another bucket of the app.
    pub async fn compare_releases(
        &self,
        release_id: &str,
        base_id: &str,
    ) -> anyhow::Result<DiffSummary> {
        self.diff(
            &format!("{}/releases/{release_id}/compare", self.path),
            &[("base_release_id", base_id), ("page_size", "1")],
        )
        .await
    }

    async fn diff(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<DiffSummary> {
        let page: DiffPage = self.client.get_whole(path, query).await?;
        page.summary
            .with_context(|| format!("the API's answer to {path} had no summary"))
    }

    /// Publishes a release of this bucket in the target bucket: its files become the release's, a
    /// new release is cut there, and `latest` and the given tags move to it, all or nothing. Each
    /// tag comes with the revision it was read at, none for a tag to create.
    pub async fn promote(
        &self,
        release_id: &str,
        target_environment_id: &str,
        target_bucket_id: &str,
        tags: &[(String, Option<u64>)],
    ) -> anyhow::Result<Release> {
        let tags: Vec<_> = tags
            .iter()
            .map(|(id, revision)| serde_json::json!({ "id": id, "expected_revision": revision }))
            .collect();
        self.client
            .post_once(
                &format!("{}/releases/{release_id}/promote", self.path),
                &serde_json::json!({
                    "target_environment_id": target_environment_id,
                    "target_bucket_id": target_bucket_id,
                    "tags": tags,
                }),
            )
            .await
    }

    /// The bucket's tags, by name. `latest` is among them once there's a release.
    pub async fn tags(&self) -> anyhow::Result<Vec<Tag>> {
        self.client.get(&format!("{}/tags", self.path)).await
    }

    /// Points a tag at a release: creates it when `revision` is none, or moves it from the state
    /// read at `revision`, so a move made meanwhile isn't overwritten.
    pub async fn set_tag(
        &self,
        tag_id: &str,
        release_id: &str,
        revision: Option<u64>,
    ) -> anyhow::Result<Tag> {
        self.client
            .put(
                &format!("{}/tags/{tag_id}", self.path),
                &serde_json::json!({ "release_id": release_id, "expected_revision": revision }),
            )
            .await
    }

    /// Asks where to upload files. A file the bucket already has, with the same content and type,
    /// comes back with no upload. The others get a signed URL to PUT to, which must start within
    /// five minutes, and their bytes are reserved against the org's storage until completed or
    /// cancelled.
    pub async fn prepare_uploads(
        &self,
        items: &[PrepareItem<'_>],
    ) -> anyhow::Result<Vec<Prepared>> {
        self.client
            .post(
                &format!("{}/uploads/prepare", self.path),
                &serde_json::json!({ "items": items }),
            )
            .await
    }

    /// Records uploaded files as their paths' new content. Each answers its own status.
    pub async fn complete_uploads(&self, items: &[UploadToken]) -> anyhow::Result<Vec<Completed>> {
        self.client
            .post(
                &format!("{}/uploads/complete", self.path),
                &serde_json::json!({ "items": items }),
            )
            .await
    }

    /// Gives up prepared uploads that weren't completed, freeing the room they reserved.
    pub async fn cancel_uploads(&self, items: &[UploadToken]) -> anyhow::Result<()> {
        self.client
            .post_empty(
                &format!("{}/uploads/cancel", self.path),
                &serde_json::json!({ "items": items }),
            )
            .await
    }

    /// Removes files from the bucket, all or none: each must still be at the revision given.
    pub async fn delete_entries(&self, items: &[EntryRevision]) -> anyhow::Result<()> {
        self.client
            .post_empty(
                &format!("{}/entries/batch_delete", self.path),
                &serde_json::json!({ "items": items }),
            )
            .await
    }
}

/// An environment of an app's assets, such as `production`.
#[derive(Clone, Debug, Deserialize)]
pub struct Environment {
    pub id: String,
    #[serde(deserialize_with = "super::plain")]
    pub name: String,
}

/// A bucket of files in an environment.
#[derive(Debug, Deserialize)]
pub struct Bucket {
    pub id: String,
    #[serde(deserialize_with = "super::plain")]
    pub name: String,
    /// `open`, or `promotion_only` for a bucket whose files and releases arrive only by promotion.
    pub write_access: String,
    pub release_count: u64,
    /// How many paths differ from the release `latest` points at.
    pub unreleased_count: u64,
    /// Ordered by id. Game clients download a bucket's files by tag.
    pub tags: Vec<Tag>,
}

/// A name for one of a bucket's releases, such as `latest` or `stable`.
#[derive(Debug, Deserialize, Serialize)]
pub struct Tag {
    pub id: String,
    pub release: ReleaseRef,
    /// The release it pointed at before its last move, if it has moved.
    pub previous_release: Option<ReleaseRef>,
    /// Echoed when moving the tag, so a move made meanwhile isn't overwritten.
    pub revision: u64,
    /// When it was created or last moved.
    pub update_time: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ReleaseRef {
    pub id: String,
    /// Counts up from 1 in each bucket; people know a release by it.
    pub release_num: u64,
}

/// A file in a bucket.
#[derive(Debug, Deserialize)]
pub struct Entry {
    pub id: String,
    /// Its path in the bucket, with `/` between folders.
    #[serde(deserialize_with = "super::plain")]
    pub path: String,
    /// Echoed when changing or removing the file, so a change made meanwhile isn't overwritten.
    pub revision: u64,
    pub current_version: Option<Version>,
}

/// The content of a file.
#[derive(Debug, Deserialize)]
pub struct Version {
    pub sha256: String,
    pub size_bytes: u64,
    pub content_type: String,
}

/// A file to upload, as `prepare_uploads` takes it.
#[derive(Serialize)]
pub struct PrepareItem<'a> {
    pub path: &'a str,
    pub sha256: &'a str,
    pub size_bytes: u64,
    pub content_type: &'a str,
}

/// What to do with a file asked about in `prepare_uploads`.
#[derive(Debug, Deserialize)]
pub struct Prepared {
    pub path: String,
    /// The path's revision now, or none if it never had a file.
    pub revision: Option<u64>,
    /// None when the bucket has the file already, unchanged.
    pub upload: Option<Upload>,
}

/// Where and how to PUT a file.
#[derive(Debug, Deserialize)]
pub struct Upload {
    pub token: String,
    pub url: String,
    /// Sent with the PUT, unchanged.
    pub headers: BTreeMap<String, String>,
}

/// An upload to complete or cancel. To complete, `expected_revision` is the revision
/// `prepare_uploads` gave, so a change made meanwhile isn't overwritten.
#[derive(Serialize)]
pub struct UploadToken {
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<Option<u64>>,
}

/// How completing one upload went.
#[derive(Debug, Deserialize)]
pub struct Completed {
    pub path: String,
    /// `committed`, or why not: `conflict`, `pending`, `rejected` or `expired`.
    pub status: String,
}

/// A file to remove, at the revision last read.
#[derive(Serialize)]
pub struct EntryRevision {
    pub id: String,
    pub expected_revision: u64,
}

/// A release: the bucket's files as they were when it was cut. Releases don't change.
#[derive(Debug, Deserialize, Serialize)]
pub struct Release {
    pub id: String,
    /// Counts up from 1 in each bucket; people know a release by it.
    pub release_num: u64,
    #[serde(deserialize_with = "super::plain")]
    pub name: String,
    #[serde(deserialize_with = "super::plain_opt", default)]
    pub notes: Option<String>,
    pub entry_count: u64,
    pub size_bytes: u64,
    /// The release this one was promoted from, if it was.
    pub promoted_from: Option<ReleaseLink>,
    /// The tags pointing at it now.
    pub tags: Vec<Tag>,
    pub create_time: DateTime<Utc>,
}

/// A release in another bucket, at the other end of a promotion.
#[derive(Debug, Deserialize, Serialize)]
pub struct ReleaseLink {
    pub release_num: u64,
    pub environment_id: String,
    pub bucket_id: String,
}

/// How many of a bucket's files differ from its latest release.
#[derive(Debug, Deserialize, Serialize)]
pub struct DiffSummary {
    pub added: u64,
    pub changed: u64,
    pub removed: u64,
    pub unchanged: u64,
    /// The bytes the files take now, minus what the latest release's take.
    pub size_delta: i64,
}

impl DiffSummary {
    /// Whether nothing differs.
    pub fn is_empty(&self) -> bool {
        self.added + self.changed + self.removed == 0
    }
}

#[derive(Deserialize)]
struct DiffPage {
    summary: Option<DiffSummary>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{
        body_json, header_exists, method, path, query_param, query_param_is_missing,
    };
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::api::Error;
    use crate::api::testing::client;

    #[tokio::test]
    async fn entries_come_from_every_page_of_the_flat_listing() {
        let server = MockServer::start().await;
        let entries = "/storage/v1/apps/app_a/environments/staging/buckets/assets/entries";
        let entry = |id: &str, path: &str| {
            json!({ "id": id, "path": path, "revision": 1, "current_version": {
                "sha256": "00", "size_bytes": 1, "content_type": "text/plain"
            }})
        };
        Mock::given(method("GET"))
            .and(path(entries))
            .and(query_param("recursive", "true"))
            .and(query_param_is_missing("page_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [entry("1", "a.txt")], "folders": [], "next_page_token": "more",
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(entries))
            .and(query_param("recursive", "true"))
            .and(query_param("page_token", "more"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [entry("2", "ui/c.txt")], "folders": [], "next_page_token": null,
            })))
            .mount(&server)
            .await;

        let found = client(&server)
            .await
            .bucket("app_a", "staging", "assets")
            .entries()
            .await
            .unwrap();
        let paths: Vec<&str> = found.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "ui/c.txt"]);
    }

    #[tokio::test]
    async fn releases_are_the_newest_page_and_say_when_there_are_more() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/storage/v1/apps/app_a/environments/staging/buckets/assets/releases",
            ))
            .and(query_param("page_size", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{
                    "id": "r2", "release_num": 2, "name": "v2", "notes": null,
                    "entry_count": 3, "size_bytes": 10, "partial": false,
                    "promoted_from": { "id": "r9", "release_num": 9,
                        "environment_id": "staging", "bucket_id": "assets" },
                    "promoted_to": [], "create_time": "2026-11-10T10:00:00Z",
                    "tags": [{ "id": "latest", "release": { "id": "r2", "release_num": 2 },
                        "previous_release": null, "revision": 1,
                        "update_time": "2026-11-10T10:00:00Z" }],
                    "content_sha256": "00"
                }],
                "next_page_token": "more",
            })))
            .mount(&server)
            .await;

        let client = client(&server).await;
        let bucket = client.bucket("app_a", "staging", "assets");
        let (releases, more) = bucket.releases(2).await.unwrap();
        assert!(more);
        assert_eq!(releases[0].release_num, 2);
        assert_eq!(releases[0].tags[0].id, "latest");
        assert_eq!(releases[0].promoted_from.as_ref().unwrap().release_num, 9);
    }

    #[tokio::test]
    async fn a_release_is_cut_with_an_idempotency_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/storage/v1/apps/app_a/environments/staging/buckets/assets/releases",
            ))
            .and(header_exists("idempotency-key"))
            .and(body_json(json!({ "name": "v2", "notes": null })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "data": {
                "id": "r2", "release_num": 2, "name": "v2", "notes": null,
                "entry_count": 3, "size_bytes": 10, "promoted_from": null,
                "create_time": "2026-11-10T10:00:00Z", "tags": [],
            }})))
            .expect(1)
            .mount(&server)
            .await;

        let release = client(&server)
            .await
            .bucket("app_a", "staging", "assets")
            .create_release("v2", None)
            .await
            .unwrap();
        assert_eq!(release.release_num, 2);
    }

    #[tokio::test]
    async fn a_refused_release_isn_t_sent_again() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(409).set_body_json(json!({ "error": "promotion_only" })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let err = client(&server)
            .await
            .bucket("app_a", "production", "assets")
            .create_release("v2", None)
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<Error>().unwrap().is("promotion_only"));
    }

    #[tokio::test]
    async fn a_release_is_found_by_its_number_or_not_at_all() {
        let server = MockServer::start().await;
        let release = |num: u64| {
            json!({ "id": format!("r{num}"), "release_num": num, "name": "v", "notes": null,
                "entry_count": 0, "size_bytes": 0, "promoted_from": null,
                "create_time": "2026-11-10T10:00:00Z", "tags": [] })
        };
        let releases = "/storage/v1/apps/app_a/environments/staging/buckets/assets/releases";
        Mock::given(method("GET"))
            .and(path(releases))
            .and(query_param_is_missing("page_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [release(5), release(4)], "next_page_token": "more",
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(releases))
            .and(query_param("page_token", "more"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [release(3), release(1)], "next_page_token": null,
            })))
            .mount(&server)
            .await;

        let client = client(&server).await;
        let bucket = client.bucket("app_a", "staging", "assets");
        assert_eq!(bucket.release_by_num(3).await.unwrap().unwrap().id, "r3");
        assert!(bucket.release_by_num(2).await.unwrap().is_none());
        assert!(bucket.release_by_num(9).await.unwrap().is_none());
    }
}
