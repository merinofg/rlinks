//! Background jobs: load a bookmark's favicon and preview image (when enabled in the owner's
//! profile) into file storage.
//!
//! There is no queue table: the database state is the queue. On startup, bookmarks still missing
//! a favicon/preview are re-enqueued; a preview that the page doesn't have is recorded as
//! `NO_PREVIEW` so it isn't retried forever. Each job costs two D1 calls (read + write); with D1
//! the worker paces itself to stay well inside the account's API rate limit.

use anyhow::Result;
use bytes::Bytes;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};

use crate::db::{Db, bool_from_int, values};
use crate::domain::assets;
use crate::scrape::{self, Fetcher};
use crate::stmt;
use crate::storage::Storage;

/// Stored in `preview_image_file` when the page has no usable preview image.
pub const NO_PREVIEW: &str = "-";
const FAVICON_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const CONCURRENCY: usize = 4;
/// Pause between job starts on D1: ≤ 2 calls/s, half the account's API budget.
const D1_PACE: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct Jobs(mpsc::UnboundedSender<Job>);

#[derive(Debug, Clone, Copy)]
enum Job {
    /// Favicon and preview image of a bookmark.
    Images(i64),
    /// Capture a pending HTML snapshot asset.
    Snapshot(i64),
}

#[derive(Clone)]
pub struct Context {
    pub db: Arc<Db>,
    pub storage: Storage,
    /// Keys the file names (`LD_SECRET_KEY`), see `file_name`.
    pub secret: Arc<str>,
    pub fetcher: Arc<Fetcher>,
}

impl Jobs {
    pub fn start(ctx: Context) -> Jobs {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(ctx, rx));
        Jobs(tx)
    }

    /// Discards jobs (tests, CLI commands).
    pub fn disabled() -> Jobs {
        Jobs(mpsc::unbounded_channel().0)
    }

    /// Load the bookmark's favicon and preview image (as its owner's profile allows).
    pub fn enqueue(&self, bookmark_id: i64) {
        let _ = self.0.send(Job::Images(bookmark_id)); // a closed queue only means jobs are disabled
    }

    /// Capture the pending snapshot asset `asset_id`.
    pub fn enqueue_snapshot(&self, asset_id: i64) {
        let _ = self.0.send(Job::Snapshot(asset_id));
    }

    /// Start an HTML snapshot of the owner's bookmark: a pending asset now, captured in the
    /// background. Returns the asset id (`None` if the bookmark isn't the owner's).
    pub async fn snapshot_bookmark(&self, db: &Db, owner: i64, bookmark: i64) -> Result<Option<i64>> {
        let asset = assets::create_snapshot(db, owner, bookmark).await?;
        if let Some(id) = asset {
            self.enqueue_snapshot(id);
        }
        Ok(asset)
    }

    /// Enqueue bookmarks (of one user, or everyone) whose enabled favicon/preview is missing.
    pub async fn enqueue_missing(&self, db: &Db, user: Option<i64>) -> Result<usize> {
        let rows = db
            .query(stmt!(
                "SELECT b.id FROM bookmarks b JOIN user_profiles p ON p.user_id = b.owner_id \
                 WHERE (? IS NULL OR b.owner_id = ?) AND ((p.enable_favicons = 1 AND b.favicon_file = '') \
                 OR (p.enable_preview_images = 1 AND b.preview_image_file = ''))",
                user,
                user
            ))
            .await?;
        let ids: Vec<i64> = values(rows, "id");
        for &id in &ids {
            self.enqueue(id);
        }
        Ok(ids.len())
    }
}

async fn run(ctx: Context, mut rx: mpsc::UnboundedReceiver<Job>) {
    let slots = Arc::new(Semaphore::new(CONCURRENCY));
    let remote = ctx.db.is_remote();
    while let Some(job) = rx.recv().await {
        let Ok(slot) = slots.clone().acquire_owned().await else {
            return;
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let result = match job {
                Job::Images(id) => load_images(&ctx, id).await,
                Job::Snapshot(id) => snapshot(&ctx, id).await,
            };
            if let Err(e) = result {
                tracing::warn!(?job, "background job failed: {e:#}");
            }
            drop(slot);
        });
        if remote {
            tokio::time::sleep(D1_PACE).await;
        }
    }
}

#[derive(Deserialize)]
struct Target {
    url: String,
    #[serde(deserialize_with = "bool_from_int")]
    enable_favicons: bool,
    #[serde(deserialize_with = "bool_from_int")]
    enable_preview_images: bool,
}

/// Load and store the favicon and preview image of one bookmark, as its owner's profile allows.
pub async fn load_images(ctx: &Context, id: i64) -> Result<()> {
    let Some(t): Option<Target> = ctx
        .db
        .query(stmt!(
            "SELECT b.url, p.enable_favicons, p.enable_preview_images FROM bookmarks b \
             JOIN user_profiles p ON p.user_id = b.owner_id WHERE b.id = ?",
            id
        ))
        .await?
        .pop()
    else {
        return Ok(()); // deleted meanwhile
    };
    let favicon = match t.enable_favicons {
        true => favicon(ctx, &t.url).await,
        false => None,
    };
    let preview = match t.enable_preview_images {
        true => Some(preview(ctx, &t.url).await),
        false => None,
    };
    if favicon.is_none() && preview.is_none() {
        return Ok(());
    }
    // `url = ?`: don't attach images of an old URL if the bookmark was edited meanwhile.
    ctx.db
        .execute(stmt!(
            "UPDATE bookmarks SET favicon_file = coalesce(?, favicon_file), \
             preview_image_file = coalesce(?, preview_image_file) WHERE id = ? AND url = ?",
            favicon,
            preview,
            id,
            t.url
        ))
        .await
}

/// Favicons are per site and shared between bookmarks; refreshed when older than a day.
async fn favicon(ctx: &Context, url: &str) -> Option<String> {
    let (site, _) = scrape::site_of(url)?;
    let name = file_name(&ctx.secret, "favicon", &site);
    let key = format!("favicons/{name}");
    let fresh = match ctx.storage.modified(&key).await {
        Ok(Some(at)) => (chrono::Utc::now() - at)
            .to_std()
            .is_ok_and(|age| age < FAVICON_MAX_AGE),
        _ => false,
    };
    if fresh {
        return Some(name);
    }
    match ctx.fetcher.favicon(url).await {
        Ok(data) => store(ctx, &key, data).await.then_some(name),
        Err(e) => {
            tracing::debug!("favicon for {url}: {e:#}");
            None
        }
    }
}

/// `previews/<hash of page url><ext>`, or `NO_PREVIEW`.
async fn preview(ctx: &Context, url: &str) -> String {
    let Some(image_url) = ctx.fetcher.metadata(url).await.preview_image else {
        return NO_PREVIEW.into();
    };
    match ctx.fetcher.preview_image(&image_url).await {
        Ok((data, ext)) => {
            let file = format!("{}{ext}", file_name(&ctx.secret, "preview", url));
            // ponytail: replaced/deleted bookmarks leave their old preview file behind; add a
            // sweep of unreferenced files if storage size ever matters.
            match store(ctx, &format!("previews/{file}"), data).await {
                true => file,
                false => NO_PREVIEW.into(),
            }
        }
        Err(e) => {
            tracing::debug!("preview image {image_url}: {e:#}");
            NO_PREVIEW.into()
        }
    }
}

/// Stored files are served publicly (API clients load `favicon_url` without credentials, shared
/// pages show them), so their names must not reveal what is bookmarked: a keyed hash (HMAC with
/// the server secret) is stable — favicons stay shared per site — but can't be computed, and so
/// can't be probed, from outside.
pub fn file_name(secret: &str, purpose: &str, input: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    mac.update(purpose.as_bytes());
    mac.update(b"\0");
    mac.update(input.as_bytes());
    crate::hex(&mac.finalize().into_bytes()[..16])
}

/// Longest a snapshot may take (the page and all its resources).
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(120);

/// Capture a pending snapshot: fetch and inline the page, store it gzipped, mark it complete
/// (or failed, so it isn't retried forever).
pub async fn snapshot(ctx: &Context, asset_id: i64) -> Result<()> {
    let rows = ctx
        .db
        .query(stmt!(
            "SELECT b.url FROM assets a JOIN bookmarks b ON b.id = a.bookmark_id WHERE a.id = ? AND a.status = ?",
            asset_id,
            assets::PENDING
        ))
        .await?;
    let Some(url) = values::<String>(rows, "url").pop() else {
        return Ok(()); // deleted, or already done
    };
    // Its own task, so that even a panic while processing some odd page marks the snapshot as
    // failed instead of leaving it pending forever.
    let fetcher = ctx.fetcher.clone();
    let page = url.clone();
    // The timeout runs inside the task: when it fires, the capture future is dropped, so no
    // fetching continues in the background after this job (and its slot) is done.
    let task =
        tokio::spawn(
            async move { tokio::time::timeout(SNAPSHOT_TIMEOUT, crate::snapshot::capture(&fetcher, &page)).await },
        );
    let outcome = match task.await {
        Ok(Ok(Ok(html))) => Ok(html),
        Ok(Ok(Err(e))) => Err(format!("snapshot of {url}: {e:#}")),
        Ok(Err(_)) => Err(format!("snapshot of {url} timed out")),
        Err(panic) => Err(format!("snapshot of {url} crashed: {panic}")),
    };
    let html = match outcome {
        Ok(html) => html,
        Err(reason) => {
            assets::fail(&ctx.db, asset_id).await?;
            anyhow::bail!(reason);
        }
    };
    let gz = crate::storage::gzip(html.into_bytes()).await?;
    let key = assets::new_file_key(".html.gz");
    let size = gz.len();
    ctx.storage.put(&key, gz.into()).await?;
    assets::complete_snapshot(&ctx.db, asset_id, &key, size).await
}

async fn store(ctx: &Context, key: &str, data: Bytes) -> bool {
    match ctx.storage.put(key, data).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("storing {key}: {e:#}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_names_are_keyed() {
        let a = super::file_name("k1", "favicon", "https://example.com");
        assert_eq!(a.len(), 32);
        assert_eq!(a, super::file_name("k1", "favicon", "https://example.com"), "stable");
        assert_ne!(
            a,
            super::file_name("k2", "favicon", "https://example.com"),
            "depends on the secret"
        );
        assert_ne!(
            a,
            super::file_name("k1", "preview", "https://example.com"),
            "separated by purpose"
        );
    }
}
