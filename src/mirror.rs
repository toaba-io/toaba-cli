//! Mirroring a folder into a bucket, for `toaba assets sync`: what it works out before changing anything (the files of a local folder,
//! and how they differ from a bucket's), then making the bucket match. A file has changed when its
//! SHA-256 or its content type has.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use futures_util::TryStreamExt;
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{HumanBytes, ProgressBar};
use reqwest::header::CONTENT_LENGTH;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::io::ReaderStream;
use walkdir::WalkDir;

use crate::api;
use crate::output::{CLEANS_UP_ON_CTRL_C, few};

/// A file of the local folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFile {
    /// Its path in the bucket: relative to the folder, with `/` between folders.
    pub path: String,
    pub source: PathBuf,
    pub size_bytes: u64,
    pub sha256: String,
    pub content_type: String,
}

/// A file the sync uploads or removes.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub size_bytes: u64,
}

/// How a folder differs from a bucket, each list by path.
#[derive(Debug, Default, Serialize)]
pub struct Plan {
    /// Files the bucket doesn't have.
    pub new: Vec<Change>,
    /// Files whose content or content type differs.
    pub changed: Vec<Change>,
    /// Files only the bucket has, which a sync removes.
    pub removed: Vec<Change>,
    pub unchanged_count: usize,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.new.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    pub fn upload_bytes(&self) -> u64 {
        self.new
            .iter()
            .chain(&self.changed)
            .map(|change| change.size_bytes)
            .sum()
    }
}

/// The files under `dir`, by path, leaving out those matching an `exclude` glob, which is matched
/// against the path relative to `dir`. Symlinks are followed.
pub fn scan(dir: &Path, exclude: &[String]) -> anyhow::Result<Vec<(String, PathBuf, u64)>> {
    if !dir.is_dir() {
        bail!("{} isn't a folder", dir.display());
    }
    let exclude = globs(exclude)?;
    let mut files = Vec::new();
    for entry in WalkDir::new(dir).follow_links(true) {
        let entry = entry.with_context(|| format!("couldn't read {}", dir.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(dir).expect("walked from dir");
        let parts: Option<Vec<&str>> = relative.iter().map(|part| part.to_str()).collect();
        let path = parts
            .with_context(|| format!("{} isn't valid UTF-8", entry.path().display()))?
            .join("/");
        if exclude.is_match(&path) {
            continue;
        }
        let size = entry
            .metadata()
            .with_context(|| format!("couldn't read {}", entry.path().display()))?
            .len();
        files.push((path, entry.into_path(), size));
    }
    files.sort();
    Ok(files)
}

fn globs(patterns: &[String]) -> anyhow::Result<GlobSet> {
    let mut set = GlobSetBuilder::new();
    for pattern in patterns {
        set.add(Glob::new(pattern).with_context(|| format!("{pattern} isn't a valid pattern"))?);
    }
    Ok(set.build()?)
}

/// Hashes the scanned files on every core, moving `progress` by the bytes read.
pub fn hash(
    files: Vec<(String, PathBuf, u64)>,
    progress: &ProgressBar,
) -> anyhow::Result<Vec<LocalFile>> {
    let next = AtomicUsize::new(0);
    let hashed: Mutex<Vec<(usize, anyhow::Result<String>)>> = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|scope| {
        for _ in 0..workers.min(files.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some((_, source, _)) = files.get(index) else {
                        break;
                    };
                    let result = sha256(source, progress);
                    hashed
                        .lock()
                        .expect("no worker panics")
                        .push((index, result));
                }
            });
        }
    });

    let mut hashes: BTreeMap<usize, String> = BTreeMap::new();
    for (index, result) in hashed.into_inner().expect("no worker panics") {
        hashes.insert(index, result?);
    }
    Ok(files
        .into_iter()
        .enumerate()
        .map(|(index, (path, source, size_bytes))| LocalFile {
            content_type: mime_guess::from_path(&source)
                .first_or_octet_stream()
                .to_string(),
            sha256: hashes.remove(&index).expect("every file was hashed"),
            path,
            source,
            size_bytes,
        })
        .collect())
}

fn sha256(path: &Path, progress: &ProgressBar) -> anyhow::Result<String> {
    let mut file = File::open(path).with_context(|| format!("couldn't read {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("couldn't read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        progress.inc(read as u64);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// How the local files differ from the bucket's.
pub fn plan(local: &[LocalFile], remote: &[api::Entry]) -> Plan {
    let mut remote: BTreeMap<&str, &api::Version> = remote
        .iter()
        .filter_map(|entry| Some((entry.path.as_str(), entry.current_version.as_ref()?)))
        .collect();
    let mut plan = Plan::default();
    for file in local {
        let change = || Change {
            path: file.path.clone(),
            size_bytes: file.size_bytes,
        };
        match remote.remove(file.path.as_str()) {
            None => plan.new.push(change()),
            Some(version)
                if version.sha256 == file.sha256 && version.content_type == file.content_type =>
            {
                plan.unchanged_count += 1;
            }
            Some(_) => plan.changed.push(change()),
        }
    }
    plan.removed = remote
        .into_iter()
        .map(|(path, version)| Change {
            path: path.to_string(),
            size_bytes: version.size_bytes,
        })
        .collect();
    plan
}

/// How many files one `prepare` asks about, and how many bytes at most: every PUT must start
/// within five minutes of it, so a batch has to stay small enough to get through in time.
const BATCH_FILES: usize = 200;
const BATCH_BYTES: u64 = 512 * 1024 * 1024;

/// An upload gives up when no bytes have moved for this long, however big the file.
const STALL: Duration = Duration::from_secs(60);

/// How many times a file's upload is tried.
const ATTEMPTS: u32 = 3;

/// Makes the bucket match the plan: uploads the new and changed files, `concurrency` at a time,
/// moving `bar` by the bytes sent, then removes the files gone from the folder. Uploads count only
/// once completed; on a failure or Ctrl+C the unfinished ones are cancelled, so they hold no room.
pub async fn apply(
    bucket: &api::BucketApi<'_>,
    local: &[LocalFile],
    remote: &[api::Entry],
    plan: &Plan,
    concurrency: usize,
    bar: &ProgressBar,
) -> anyhow::Result<()> {
    let wanted: BTreeSet<&str> = plan
        .new
        .iter()
        .chain(&plan.changed)
        .map(|change| change.path.as_str())
        .collect();
    let uploads: Vec<&LocalFile> = local
        .iter()
        .filter(|file| wanted.contains(file.path.as_str()))
        .collect();

    // The tokens of uploads prepared and not yet completed, to cancel if the sync stops.
    let unfinished: Mutex<Vec<String>> = Mutex::new(Vec::new());
    CLEANS_UP_ON_CTRL_C.store(true, Ordering::Relaxed);
    let outcome = tokio::select! {
        result = upload_all(bucket, &uploads, concurrency, bar, &unfinished) => Some(result),
        _ = tokio::signal::ctrl_c() => None,
    };
    CLEANS_UP_ON_CTRL_C.store(false, Ordering::Relaxed);

    if !matches!(outcome, Some(Ok(()))) {
        let tokens: Vec<api::UploadToken> =
            std::mem::take(&mut *unfinished.lock().expect("no panics"))
                .into_iter()
                .map(|token| api::UploadToken {
                    token,
                    expected_revision: None,
                })
                .collect();
        for chunk in tokens.chunks(1000) {
            // Best effort: uncancelled uploads expire by themselves.
            let _ = bucket.cancel_uploads(chunk).await;
        }
    }
    match outcome {
        Some(result) => result?,
        None => {
            bar.abandon();
            let _ = console::Term::stderr().show_cursor();
            eprintln!("Interrupted: the files already uploaded stay, the rest were cancelled");
            std::process::exit(130);
        }
    }

    let by_path: BTreeMap<&str, &api::Entry> = remote
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let removals: Vec<api::EntryRevision> = plan
        .removed
        .iter()
        .filter_map(|change| by_path.get(change.path.as_str()))
        .map(|entry| api::EntryRevision {
            id: entry.id.clone(),
            expected_revision: entry.revision,
        })
        .collect();
    for chunk in removals.chunks(1000) {
        bucket.delete_entries(chunk).await?;
    }
    Ok(())
}

async fn upload_all(
    bucket: &api::BucketApi<'_>,
    uploads: &[&LocalFile],
    concurrency: usize,
    bar: &ProgressBar,
    unfinished: &Mutex<Vec<String>>,
) -> anyhow::Result<()> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()?;
    for batch in batches(uploads) {
        let items: Vec<api::PrepareItem> = batch
            .iter()
            .map(|file| api::PrepareItem {
                path: &file.path,
                sha256: &file.sha256,
                size_bytes: file.size_bytes,
                content_type: &file.content_type,
            })
            .collect();
        let prepared = bucket.prepare_uploads(&items).await.map_err(|err| {
            let too_big = err
                .downcast_ref::<api::Error>()
                .and_then(|refused| too_big(&refused.fields, &batch));
            match too_big {
                Some(message) => anyhow::anyhow!(message),
                None => err,
            }
        })?;
        let by_path: BTreeMap<&str, &LocalFile> = batch
            .iter()
            .map(|file| (file.path.as_str(), *file))
            .collect();

        let mut to_complete = Vec::new();
        let mut puts = JoinSet::new();
        let slots = Arc::new(Semaphore::new(concurrency.max(1)));
        for prepared in prepared {
            let file = by_path.get(prepared.path.as_str()).with_context(|| {
                format!(
                    "the API answered about {}, which wasn't asked",
                    prepared.path
                )
            })?;
            let Some(upload) = prepared.upload else {
                // Someone uploaded the same content meanwhile.
                bar.inc(file.size_bytes);
                continue;
            };
            unfinished
                .lock()
                .expect("no panics")
                .push(upload.token.clone());
            to_complete.push(api::UploadToken {
                token: upload.token.clone(),
                expected_revision: Some(prepared.revision),
            });
            let (http, slots, bar) = (http.clone(), slots.clone(), bar.clone());
            let (source, path, size) = (file.source.clone(), file.path.clone(), file.size_bytes);
            puts.spawn(async move {
                let _slot = slots.acquire_owned().await.expect("never closed");
                put_with_retries(&http, &upload, &source, size, &bar)
                    .await
                    .with_context(|| format!("couldn't upload {path}"))
            });
        }
        while let Some(put) = puts.join_next().await {
            put.context("an upload stopped unexpectedly")??;
        }

        for chunk in to_complete.chunks(1000) {
            let failed: Vec<String> = bucket
                .complete_uploads(chunk)
                .await?
                .into_iter()
                .filter(|completed| completed.status != "committed")
                .map(|completed| match completed.status.as_str() {
                    "conflict" => format!("{} (changed in the bucket meanwhile)", completed.path),
                    status => format!("{} ({status})", completed.path),
                })
                .collect();
            if !failed.is_empty() {
                bail!(
                    "{} files weren't taken, run the sync again: {}",
                    failed.len(),
                    failed.join(", ")
                );
            }
            let done: BTreeSet<&str> = chunk.iter().map(|item| item.token.as_str()).collect();
            unfinished
                .lock()
                .expect("no panics")
                .retain(|token| !done.contains(token.as_str()));
        }
    }
    Ok(())
}

/// Says which files are over the org's file size limit, when that's why the API refused a batch.
/// It names them by their place in the batch, as `items.3.size_bytes`, with the limit in bytes.
fn too_big(fields: &BTreeMap<String, Vec<String>>, batch: &[&LocalFile]) -> Option<String> {
    let mut limit = None;
    let mut files = Vec::new();
    for (field, messages) in fields {
        let Some(index) = field
            .strip_prefix("items.")
            .and_then(|rest| rest.strip_suffix(".size_bytes"))
            .and_then(|index| index.parse::<usize>().ok())
        else {
            continue;
        };
        let file = batch.get(index)?;
        files.push(format!("{} ({})", file.path, HumanBytes(file.size_bytes)));
        limit = limit.or_else(|| {
            let digits: String = messages
                .iter()
                .flat_map(|message| message.chars())
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse::<u64>().ok()
        });
    }
    let limit = match limit {
        Some(bytes) => format!("the org's {} file size limit", HumanBytes(bytes)),
        None => "the org's file size limit".to_string(),
    };
    match files.len() {
        0 => None,
        1 => Some(format!("{} is over {limit}", files[0])),
        n => Some(format!("{n} files are over {limit}: {}", few(&files, ", "))),
    }
}

/// The uploads in batches of at most `BATCH_FILES` files and `BATCH_BYTES` bytes, a file bigger
/// than that alone in its batch.
fn batches<'a>(uploads: &[&'a LocalFile]) -> Vec<Vec<&'a LocalFile>> {
    let mut batches: Vec<Vec<&LocalFile>> = Vec::new();
    let mut bytes = 0;
    for file in uploads {
        let fits = batches.last().is_some_and(|batch| {
            batch.len() < BATCH_FILES && bytes + file.size_bytes <= BATCH_BYTES
        });
        if !fits {
            batches.push(Vec::new());
            bytes = 0;
        }
        batches.last_mut().expect("just pushed").push(file);
        bytes += file.size_bytes;
    }
    batches
}

async fn put_with_retries(
    http: &reqwest::Client,
    upload: &api::Upload,
    source: &Path,
    size: u64,
    bar: &ProgressBar,
) -> anyhow::Result<()> {
    let mut attempt = 1;
    loop {
        let sent = Arc::new(AtomicU64::new(0));
        match put(http, upload, source, size, bar, &sent, STALL).await {
            Ok(()) => return Ok(()),
            Err(err) if attempt == ATTEMPTS => return Err(err),
            Err(_) => {
                // The bar goes back by what this attempt had sent.
                bar.dec(sent.load(Ordering::Relaxed));
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// PUTs the file to its signed URL, streaming it from disk. A 412 means the object is already
/// there, from an earlier try, which completing then settles.
async fn put(
    http: &reqwest::Client,
    upload: &api::Upload,
    source: &Path,
    size: u64,
    bar: &ProgressBar,
    sent: &Arc<AtomicU64>,
    stall: Duration,
) -> anyhow::Result<()> {
    let file = tokio::fs::File::open(source)
        .await
        .with_context(|| format!("couldn't read {}", source.display()))?;
    let stream = ReaderStream::with_capacity(file, 256 * 1024).inspect_ok({
        let (bar, sent) = (bar.clone(), sent.clone());
        move |chunk| {
            bar.inc(chunk.len() as u64);
            sent.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
    });
    let mut request = http
        .put(&upload.url)
        .header(CONTENT_LENGTH, size)
        .body(reqwest::Body::wrap_stream(stream));
    for (name, value) in &upload.headers {
        request = request.header(name, value);
    }

    let response = request.send();
    tokio::pin!(response);
    let mut last = (sent.load(Ordering::Relaxed), Instant::now());
    let response = loop {
        tokio::select! {
            response = &mut response => break response.context("the storage couldn't be reached")?,
            _ = tokio::time::sleep(stall.min(Duration::from_secs(5))) => {
                let now = sent.load(Ordering::Relaxed);
                if now != last.0 {
                    last = (now, Instant::now());
                } else if last.1.elapsed() >= stall {
                    bail!("no bytes moved for {} seconds", stall.as_secs());
                }
            }
        }
    };
    let status = response.status();
    if status.is_success() || status == reqwest::StatusCode::PRECONDITION_FAILED {
        return Ok(());
    }
    bail!("the storage answered {status}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &str, sha256: &str, content_type: &str) -> LocalFile {
        LocalFile {
            path: path.into(),
            source: path.into(),
            size_bytes: 10,
            sha256: sha256.into(),
            content_type: content_type.into(),
        }
    }

    fn remote(path: &str, sha256: &str, content_type: &str) -> api::Entry {
        api::Entry {
            id: path.into(),
            path: path.into(),
            revision: 1,
            current_version: Some(api::Version {
                sha256: sha256.into(),
                size_bytes: 7,
                content_type: content_type.into(),
            }),
        }
    }

    #[test]
    fn a_plan_sorts_files_into_new_changed_removed_and_unchanged() {
        let plan = plan(
            &[
                local("new.txt", "aa", "text/plain"),
                local("same.txt", "bb", "text/plain"),
                local("edited.txt", "cc", "text/plain"),
                local("retyped.json", "dd", "application/json"),
            ],
            &[
                remote("same.txt", "bb", "text/plain"),
                remote("edited.txt", "00", "text/plain"),
                remote("retyped.json", "dd", "text/plain"),
                remote("gone.txt", "ee", "text/plain"),
            ],
        );

        let paths = |changes: &[Change]| -> Vec<String> {
            changes.iter().map(|change| change.path.clone()).collect()
        };
        assert_eq!(paths(&plan.new), ["new.txt"]);
        assert_eq!(paths(&plan.changed), ["edited.txt", "retyped.json"]);
        assert_eq!(paths(&plan.removed), ["gone.txt"]);
        assert_eq!(plan.removed[0].size_bytes, 7);
        assert_eq!(plan.unchanged_count, 1);
        assert_eq!(plan.upload_bytes(), 30);
    }

    #[test]
    fn a_refusal_for_size_names_the_files_and_the_limit() {
        let small = local("small.txt", "aa", "text/plain");
        let big = LocalFile {
            path: "big.bin".into(),
            size_bytes: 2 * 1024 * 1024 * 1024,
            ..small.clone()
        };
        let batch = [&small, &big];
        let refused = |field: &str| {
            BTreeMap::from([(
                field.to_string(),
                vec![
                    "must be at most 536870912 bytes, the organization's file size limit"
                        .to_string(),
                ],
            )])
        };

        assert_eq!(
            too_big(&refused("items.1.size_bytes"), &batch).unwrap(),
            "big.bin (2.00 GiB) is over the org's 512.00 MiB file size limit"
        );
        assert_eq!(too_big(&refused("items.1.path"), &batch), None);
        assert_eq!(too_big(&refused("items.7.size_bytes"), &batch), None);
    }

    #[tokio::test]
    async fn an_upload_gives_up_when_no_bytes_move() {
        // A server that takes the connection and never reads from it: the upload fills the
        // socket's buffers, then nothing moves.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/object", listener.local_addr().unwrap());
        let dir = std::env::temp_dir().join(format!("toaba-stall-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("big.bin");
        let size = 256 * 1024 * 1024;
        File::create(&source).unwrap().set_len(size).unwrap();

        let upload = api::Upload {
            token: "t".into(),
            url,
            headers: BTreeMap::new(),
        };
        let sent = Arc::new(AtomicU64::new(0));
        let started = Instant::now();
        let result = put(
            &reqwest::Client::new(),
            &upload,
            &source,
            size,
            &ProgressBar::hidden(),
            &sent,
            Duration::from_secs(1),
        )
        .await;

        assert_eq!(
            result.unwrap_err().to_string(),
            "no bytes moved for 1 seconds"
        );
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(sent.load(Ordering::Relaxed) < size);
        drop(listener);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn batches_stay_under_their_file_and_byte_limits() {
        let file = |size_bytes: u64| LocalFile {
            size_bytes,
            ..local("f", "aa", "text/plain")
        };
        let small: Vec<LocalFile> = (0..BATCH_FILES + 1).map(|_| file(1)).collect();
        let sizes = |files: &[LocalFile]| -> Vec<usize> {
            let refs: Vec<&LocalFile> = files.iter().collect();
            batches(&refs).iter().map(Vec::len).collect()
        };
        assert_eq!(sizes(&small), [BATCH_FILES, 1]);

        let big = [
            file(BATCH_BYTES + 1),
            file(BATCH_BYTES / 2),
            file(BATCH_BYTES / 2),
            file(1),
        ];
        assert_eq!(sizes(&big), [1, 2, 1]);
    }

    #[test]
    fn a_scan_finds_nested_files_and_leaves_out_excluded_ones() {
        let dir = std::env::temp_dir().join(format!("toaba-scan-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("ui/icons")).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        std::fs::write(dir.join("ui/icons/b.png"), "png").unwrap();
        std::fs::write(dir.join("ui/icons/b.png.meta"), "meta").unwrap();

        let files = scan(&dir, &["*.meta".to_string()]).unwrap();
        let paths: Vec<&str> = files.iter().map(|(path, _, _)| path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "ui/icons/b.png"]);

        let hashed = hash(files, &ProgressBar::hidden()).unwrap();
        assert_eq!(
            hashed[0].sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(hashed[0].content_type, "text/plain");
        assert_eq!(hashed[1].content_type, "image/png");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
