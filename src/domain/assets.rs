//! Files attached to bookmarks: HTML snapshots and uploads (linkding `BookmarkAsset`).
//! Rows live in the database, contents in file storage under `assets/<random name>`.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::{Db, Row, bool_from_int, from_row, now, values};
use crate::stmt;

pub const SNAPSHOT: &str = "snapshot";
pub const UPLOAD: &str = "upload";
pub const PENDING: &str = "pending";
pub const COMPLETE: &str = "complete";
pub const FAILURE: &str = "failure";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub id: i64,
    /// Random id used in web URLs; the API uses `id` (linkding compatible).
    #[serde(skip_serializing)]
    pub uid: String,
    /// Bookmark id (named like linkding's API field).
    #[serde(rename = "bookmark", alias = "bookmark_id")]
    pub bookmark: i64,
    pub date_created: String,
    #[serde(skip_serializing)]
    pub file: String,
    pub file_size: Option<i64>,
    pub asset_type: String,
    pub content_type: String,
    pub display_name: String,
    pub status: String,
    #[serde(skip_serializing, deserialize_with = "bool_from_int")]
    pub gzip: bool,
}

impl Asset {
    /// File name for downloads: snapshots get `.html` like linkding.
    pub fn download_name(&self) -> String {
        match self.asset_type.as_str() {
            SNAPSHOT => format!("{}.html", self.display_name),
            _ => self.display_name.clone(),
        }
    }

    pub fn is_readable(&self) -> bool {
        self.status == COMPLETE && !self.file.is_empty()
    }
}

/// Assets of the owner's bookmark, newest first.
pub async fn list(db: &Db, owner: i64, bookmark: i64) -> Result<Vec<Asset>> {
    db.query(stmt!(
        "SELECT a.* FROM assets a JOIN bookmarks b ON b.id = a.bookmark_id \
             WHERE a.bookmark_id = ? AND b.owner_id = ? ORDER BY a.date_created DESC, a.id DESC",
        bookmark,
        owner
    ))
    .await
}

pub async fn get(db: &Db, owner: i64, id: i64) -> Result<Option<Asset>> {
    Ok(db
        .query(stmt!(
            "SELECT a.* FROM assets a JOIN bookmarks b ON b.id = a.bookmark_id WHERE a.id = ? AND b.owner_id = ?",
            id,
            owner
        ))
        .await?
        .pop())
}

/// The owner's asset by its random public id (web URLs).
pub async fn get_by_uid(db: &Db, owner: i64, uid: &str) -> Result<Option<Asset>> {
    Ok(db
        .query(stmt!(
            "SELECT a.* FROM assets a JOIN bookmarks b ON b.id = a.bookmark_id WHERE a.uid = ? AND b.owner_id = ?",
            uid,
            owner
        ))
        .await?
        .pop())
}

/// "HTML snapshot from 2024-05-01 12:30" for a stored timestamp.
fn snapshot_name(now: &str) -> String {
    format!(
        "HTML snapshot from {}",
        now.get(..16).unwrap_or_default().replace('T', " ")
    )
}

/// A pending snapshot for the owner's bookmark (the job fills it in). `None` if the bookmark
/// isn't the owner's.
pub async fn create_snapshot(db: &Db, owner: i64, bookmark: i64) -> Result<Option<i64>> {
    let now = now();
    let name = snapshot_name(&now);
    let rows: Vec<Row> = db
        .query(stmt!(
            "INSERT INTO assets (bookmark_id, date_created, asset_type, content_type, display_name, status, gzip) \
             SELECT id, ?, ?, 'text/html', ?, ?, 1 FROM bookmarks WHERE id = ? AND owner_id = ? RETURNING id",
            now,
            SNAPSHOT,
            name,
            PENDING,
            bookmark,
            owner
        ))
        .await?;
    Ok(values(rows, "id").pop())
}

/// Record an uploaded file (already in storage under `file`). `None` if the bookmark isn't the
/// owner's.
pub async fn create_upload(
    db: &Db,
    owner: i64,
    bookmark: i64,
    file: &str,
    size: usize,
    content_type: &str,
    display_name: &str,
) -> Result<Option<Asset>> {
    let rows: Vec<Row> = db
        .query(stmt!(
            "INSERT INTO assets (bookmark_id, date_created, file, file_size, asset_type, content_type, display_name, status, gzip) \
             SELECT id, ?, ?, ?, ?, ?, ?, ?, 0 FROM bookmarks WHERE id = ? AND owner_id = ? \
             RETURNING *",
            now(),
            file,
            size,
            UPLOAD,
            content_type,
            display_name,
            COMPLETE,
            bookmark,
            owner
        ))
        .await?;
    rows.into_iter().next().map(from_row).transpose()
}

/// A complete snapshot uploaded from elsewhere (the browser extension's SingleFile capture),
/// made the bookmark's latest. `None` if the bookmark isn't the owner's.
pub async fn add_snapshot(db: &Db, owner: i64, bookmark: i64, file: &str, size: usize) -> Result<Option<Asset>> {
    let now = now();
    let name = snapshot_name(&now);
    let mut res = db
        .batch(vec![
            stmt!(
                "INSERT INTO assets (bookmark_id, date_created, file, file_size, asset_type, content_type, display_name, status, gzip) \
                 SELECT id, ?, ?, ?, ?, 'text/html', ?, ?, 1 FROM bookmarks WHERE id = ? AND owner_id = ? \
                 RETURNING *",
                now,
                file,
                size,
                SNAPSHOT,
                name,
                COMPLETE,
                bookmark,
                owner
            ),
            stmt!(
                "UPDATE bookmarks SET latest_snapshot_id = (SELECT max(id) FROM assets WHERE bookmark_id = ? AND file = ?) \
                 WHERE id = ? AND owner_id = ?",
                bookmark,
                file,
                bookmark,
                owner
            ),
        ])
        .await?;
    res.truncate(1);
    res.pop().and_then(|mut r| r.pop()).map(from_row).transpose()
}

/// Snapshot job finished: store the file and make it the bookmark's latest snapshot.
pub async fn complete_snapshot(db: &Db, id: i64, file: &str, size: usize) -> Result<()> {
    db.batch(vec![
        stmt!(
            "UPDATE assets SET file = ?, file_size = ?, status = ? WHERE id = ?",
            file,
            size,
            COMPLETE,
            id
        ),
        stmt!(
            "UPDATE bookmarks SET latest_snapshot_id = ? WHERE id = (SELECT bookmark_id FROM assets WHERE id = ?)",
            id,
            id
        ),
    ])
    .await
    .map(drop)
}

pub async fn fail(db: &Db, id: i64) -> Result<()> {
    db.execute(stmt!("UPDATE assets SET status = ? WHERE id = ?", FAILURE, id))
        .await
}

/// Delete the owner's asset; returns it (so its file can be removed), or `None`.
pub async fn delete(db: &Db, owner: i64, id: i64) -> Result<Option<Asset>> {
    let rows: Vec<Row> = db
        .query(stmt!(
            "DELETE FROM assets WHERE id = ? AND bookmark_id IN (SELECT id FROM bookmarks WHERE owner_id = ?) \
             RETURNING *",
            id,
            owner
        ))
        .await?;
    rows.into_iter().next().map(from_row).transpose()
}

/// Storage keys of all files attached to these bookmarks of the owner (to remove them from
/// storage when the bookmarks are deleted; the rows go away by cascade).
pub async fn files_of(db: &Db, owner: i64, bookmarks: &[i64]) -> Result<Vec<String>> {
    let rows: Vec<Row> = db
        .query(stmt!(
            "SELECT a.file FROM assets a JOIN bookmarks b ON b.id = a.bookmark_id \
             WHERE b.owner_id = ? AND b.id IN (SELECT value FROM json_each(?)) AND a.file != ''",
            owner,
            json!(bookmarks).to_string()
        ))
        .await?;
    Ok(values(rows, "file"))
}

/// Snapshot jobs that didn't finish (e.g. the server restarted).
pub async fn pending_snapshots(db: &Db) -> Result<Vec<i64>> {
    let rows: Vec<Row> = db
        .query(stmt!(
            "SELECT id FROM assets WHERE asset_type = ? AND status = ?",
            SNAPSHOT,
            PENDING
        ))
        .await?;
    Ok(values(rows, "id"))
}

/// A fresh, unguessable storage key: `assets/<random hex><ext>`.
pub fn new_file_key(ext: &str) -> String {
    format!("assets/{}{ext}", crate::hex(&rand::random::<[u8; 16]>()))
}

/// Upload limit for attached files (uploads and browser-captured snapshots).
pub const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;

/// Upload validation: a display name (from the file name) and the size limit.
pub fn validate_upload(name: &str, size: usize) -> Result<()> {
    if name.trim().is_empty() {
        bail!("The file needs a name.");
    }
    if size == 0 {
        bail!("The file is empty.");
    }
    if size > MAX_UPLOAD_BYTES {
        bail!("The file is larger than {} MB.", MAX_UPLOAD_BYTES / (1024 * 1024));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::bookmarks::{self as bm, BookmarkInput};
    use crate::domain::users;

    #[tokio::test]
    async fn snapshot_lifecycle_uploads_and_ownership() {
        let db = Db::sqlite(":memory:").unwrap();
        db.migrate().await.unwrap();
        users::create(&db, "u", "pw", false).await.unwrap();
        users::create(&db, "other", "pw", false).await.unwrap();
        let b = bm::create(
            &db,
            1,
            &BookmarkInput {
                url: "https://a.com".into(),
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap()
        .id;

        assert!(
            create_snapshot(&db, 2, b).await.unwrap().is_none(),
            "other users can't add assets"
        );
        let snap = create_snapshot(&db, 1, b).await.unwrap().unwrap();
        assert_eq!(pending_snapshots(&db).await.unwrap(), [snap]);
        complete_snapshot(&db, snap, "assets/x.html.gz", 123).await.unwrap();
        assert!(pending_snapshots(&db).await.unwrap().is_empty());
        let a = get(&db, 1, snap).await.unwrap().unwrap();
        assert!(a.is_readable() && a.gzip && a.download_name().ends_with(".html"));
        assert_eq!(a.uid.len(), 32, "random public id");
        assert_eq!(get_by_uid(&db, 1, &a.uid).await.unwrap().unwrap().id, snap);
        assert!(get_by_uid(&db, 2, &a.uid).await.unwrap().is_none(), "owner only");
        let latest: Vec<Row> = db
            .query(stmt!("SELECT latest_snapshot_id FROM bookmarks"))
            .await
            .unwrap();
        assert_eq!(latest[0]["latest_snapshot_id"], snap);

        let up = create_upload(&db, 1, b, "assets/y.pdf", 10, "application/pdf", "paper.pdf")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((up.asset_type.as_str(), up.status.as_str()), (UPLOAD, COMPLETE));
        assert!(
            create_upload(&db, 2, b, "assets/z", 1, "x/y", "z")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(list(&db, 1, b).await.unwrap().len(), 2);
        assert!(list(&db, 2, b).await.unwrap().is_empty() && get(&db, 2, up.id).await.unwrap().is_none());

        let mut files = files_of(&db, 1, &[b]).await.unwrap();
        files.sort();
        assert_eq!(files, ["assets/x.html.gz", "assets/y.pdf"]);
        assert!(delete(&db, 2, up.id).await.unwrap().is_none());
        assert_eq!(delete(&db, 1, up.id).await.unwrap().unwrap().file, "assets/y.pdf");

        // deleting the latest snapshot clears the bookmark's pointer (FK ON DELETE SET NULL)
        delete(&db, 1, snap).await.unwrap();
        let latest: Vec<Row> = db
            .query(stmt!("SELECT latest_snapshot_id FROM bookmarks"))
            .await
            .unwrap();
        assert!(latest[0]["latest_snapshot_id"].is_null());
        assert!(validate_upload("a.pdf", 0).is_err() && validate_upload("a.pdf", MAX_UPLOAD_BYTES + 1).is_err());
        assert!(new_file_key(".pdf").starts_with("assets/") && new_file_key("") != new_file_key(""));
    }
}
