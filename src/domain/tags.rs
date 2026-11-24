use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::db::{Db, Row, from_row, now, values};
use crate::stmt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub date_added: String,
}

/// Longest tag name, in characters (linkding's limit).
pub const MAX_NAME_LEN: usize = 64;

/// Tag names can't contain whitespace: inner runs become '-' (`two words` → `two-words`).
pub fn clean_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join("-")
}

/// One page of the owner's tags and the total count.
pub async fn list(db: &Db, owner: i64, limit: i64, offset: i64) -> Result<(Vec<Tag>, i64)> {
    let mut res = db
        .batch(vec![
            stmt!(
                "SELECT id, name, date_added FROM tags WHERE owner_id = ? ORDER BY id LIMIT ? OFFSET ?",
                owner,
                limit,
                offset
            ),
            stmt!("SELECT count(*) AS n FROM tags WHERE owner_id = ?", owner),
        ])
        .await?;
    let total = values(res.pop().unwrap_or_default(), "n").pop().unwrap_or(0);
    let tags = res
        .pop()
        .unwrap_or_default()
        .into_iter()
        .map(from_row)
        .collect::<Result<_>>()?;
    Ok((tags, total))
}

pub async fn get(db: &Db, owner: i64, id: i64) -> Result<Option<Tag>> {
    Ok(db
        .query(stmt!(
            "SELECT id, name, date_added FROM tags WHERE id = ? AND owner_id = ?",
            id,
            owner
        ))
        .await?
        .pop())
}

/// Existing tag with this name (case-insensitive) or a new one. Inner whitespace becomes '-',
/// as everywhere else tags are parsed.
pub async fn get_or_create(db: &Db, owner: i64, name: &str) -> Result<Tag> {
    let name = clean_name(name);
    let mut res = db
        .batch(vec![
            stmt!(
                "INSERT INTO tags (name, date_added, owner_id) VALUES (?, ?, ?) ON CONFLICT (owner_id, name) DO NOTHING",
                name,
                now(),
                owner
            ),
            stmt!("SELECT id, name, date_added FROM tags WHERE owner_id = ? AND name = ?", owner, name),
        ])
        .await?;
    match res.pop().and_then(|mut rows: Vec<Row>| rows.pop()) {
        Some(row) => from_row(row),
        None => bail!("tag upsert returned no row"),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TagCount {
    pub id: i64,
    pub name: String,
    pub bookmarks: i64,
}

/// The owner's tags with bookmark counts, filtered by a name substring, sorted by name.
pub async fn list_with_counts(db: &Db, owner: i64, name_filter: &str) -> Result<Vec<TagCount>> {
    let pattern = format!(
        "%{}%",
        name_filter
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    db.query(stmt!(
        "SELECT t.id, t.name, count(bt.bookmark_id) AS bookmarks FROM tags t \
         LEFT JOIN bookmark_tags bt ON bt.tag_id = t.id \
         WHERE t.owner_id = ? AND t.name LIKE ? ESCAPE '\\' GROUP BY t.id ORDER BY lower(t.name)",
        owner,
        pattern
    ))
    .await
}

pub enum Renamed {
    Ok,
    NotFound,
    /// Another tag of the owner already has that name.
    Duplicate,
}

/// Rename a tag (inner whitespace becomes '-'). Changing only the case is allowed.
pub async fn rename(db: &Db, owner: i64, id: i64, name: &str) -> Result<Renamed> {
    let name = clean_name(name);
    let mut res = db
        .batch(vec![
            stmt!(
                "UPDATE tags SET name = ? WHERE id = ? AND owner_id = ? \
                 AND NOT EXISTS (SELECT 1 FROM tags WHERE owner_id = ? AND name = ? AND id != ?) RETURNING id",
                name,
                id,
                owner,
                owner,
                name,
                id
            ),
            stmt!(
                "SELECT count(*) AS n FROM tags WHERE id = ? AND owner_id = ?",
                id,
                owner
            ),
        ])
        .await?;
    let exists = values(res.pop().unwrap_or_default(), "n").pop() == Some(1);
    let updated = res.pop().is_some_and(|r| !r.is_empty());
    Ok(match (updated, exists) {
        (true, _) => Renamed::Ok,
        (false, true) => Renamed::Duplicate,
        (false, false) => Renamed::NotFound,
    })
}

/// Delete a tag; it is removed from all bookmarks.
pub async fn delete(db: &Db, owner: i64, id: i64) -> Result<()> {
    db.execute(stmt!("DELETE FROM tags WHERE id = ? AND owner_id = ?", id, owner))
        .await
}

/// Move all bookmarks of the `sources` tags to `target` and delete the sources (by name,
/// case-insensitive). Returns how many tags were merged; 0 when the target doesn't exist.
pub async fn merge(db: &Db, owner: i64, target: &str, sources: &[String]) -> Result<i64> {
    let sources = serde_json::json!(sources).to_string();
    let target_id = "(SELECT id FROM tags WHERE owner_id = ? AND name = ?)";
    let source_ids =
        "SELECT id FROM tags WHERE owner_id = ? AND name IN (SELECT value FROM json_each(?)) AND name != ?";
    let mut res = db
        .batch(vec![
            stmt!(
                format!("SELECT count(*) AS n FROM ({source_ids}) WHERE {target_id} IS NOT NULL"),
                owner,
                sources,
                target,
                owner,
                target
            ),
            stmt!(
                format!(
                    "INSERT INTO bookmark_tags (bookmark_id, tag_id) SELECT bt.bookmark_id, {target_id} FROM bookmark_tags bt \
                     WHERE bt.tag_id IN ({source_ids}) AND {target_id} IS NOT NULL ON CONFLICT DO NOTHING"
                ),
                owner,
                target,
                owner,
                sources,
                target,
                owner,
                target
            ),
            stmt!(
                format!("DELETE FROM tags WHERE id IN ({source_ids}) AND {target_id} IS NOT NULL"),
                owner,
                sources,
                target,
                owner,
                target
            ),
        ])
        .await?;
    Ok(values(res.swap_remove(0), "n").pop().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::users;

    #[tokio::test]
    async fn get_or_create_is_case_insensitive() {
        let db = Db::sqlite(":memory:").unwrap();
        db.migrate().await.unwrap();
        users::create(&db, "u", "pw", false).await.unwrap();
        let a = get_or_create(&db, 1, "Rust").await.unwrap();
        let b = get_or_create(&db, 1, "rust").await.unwrap();
        assert_eq!((a.id, b.name.as_str()), (b.id, "Rust"));
        assert_eq!(get_or_create(&db, 1, "two words").await.unwrap().name, "two-words");
        let (page, total) = list(&db, 1, 1, 1).await.unwrap();
        assert_eq!((page.len(), total, page[0].name.as_str()), (1, 2, "two-words"));
        assert!(get(&db, 2, a.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rename_delete_merge() {
        use crate::domain::bookmarks::{BookmarkInput, create, get as get_bm};
        let db = Db::sqlite(":memory:").unwrap();
        db.migrate().await.unwrap();
        users::create(&db, "u", "pw", false).await.unwrap();
        users::create(&db, "other", "pw", false).await.unwrap();
        let add = |url: &str, tags: &[&str]| {
            let input = BookmarkInput {
                url: url.into(),
                ..Default::default()
            };
            let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
            let db = &db;
            async move { create(db, 1, &input, &tags).await.unwrap().id }
        };
        let a = add("https://a.com", &["js", "javascript"]).await;
        let b = add("https://b.com", &["ecmascript"]).await;
        let other_tag = get_or_create(&db, 2, "js").await.unwrap();

        let counts = list_with_counts(&db, 1, "script").await.unwrap();
        assert_eq!(
            counts
                .iter()
                .map(|t| (t.name.as_str(), t.bookmarks))
                .collect::<Vec<_>>(),
            [("ecmascript", 1), ("javascript", 1)]
        );

        let js = list_with_counts(&db, 1, "js").await.unwrap()[0].id;
        assert!(matches!(
            rename(&db, 1, js, "JavaScript").await.unwrap(),
            Renamed::Duplicate
        ));
        assert!(
            matches!(rename(&db, 1, js, "JS").await.unwrap(), Renamed::Ok),
            "case-only rename"
        );
        assert!(
            matches!(rename(&db, 2, js, "x").await.unwrap(), Renamed::NotFound),
            "other users' tags"
        );

        let merged = merge(
            &db,
            1,
            "javascript",
            &["JS".into(), "ecmascript".into(), "javascript".into()],
        )
        .await
        .unwrap();
        assert_eq!(merged, 2);
        assert_eq!(get_bm(&db, 1, a).await.unwrap().unwrap().tag_names, vec!["javascript"]);
        assert_eq!(get_bm(&db, 1, b).await.unwrap().unwrap().tag_names, vec!["javascript"]);
        assert_eq!(
            merge(&db, 1, "missing", &["javascript".into()]).await.unwrap(),
            0,
            "unknown target: no-op"
        );
        assert_eq!(list_with_counts(&db, 1, "").await.unwrap().len(), 1);
        assert!(
            get(&db, 2, other_tag.id).await.unwrap().is_some(),
            "other users' tags untouched"
        );

        delete(&db, 1, list_with_counts(&db, 1, "").await.unwrap()[0].id)
            .await
            .unwrap();
        assert!(get_bm(&db, 1, a).await.unwrap().unwrap().tag_names.is_empty());
    }
}
