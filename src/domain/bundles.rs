//! Bundles: saved filters shown in the sidebar (linkding `BookmarkBundle`).

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::{Db, Row, Stmt, from_row, now};
use crate::domain::bookmarks::parse_tag_string;
use crate::search::{self, Sql};
use crate::stmt;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bundle {
    pub id: i64,
    pub name: String,
    pub search: String,
    pub any_tags: String,
    pub all_tags: String,
    pub excluded_tags: String,
    /// `off` | `yes` | `no`
    pub filter_unread: String,
    pub filter_shared: String,
    pub order: i64,
    pub date_created: String,
    pub date_modified: String,
}

const COLS: &str = r#"id, name, search, any_tags, all_tags, excluded_tags, filter_unread, filter_shared, "order", date_created, date_modified"#;

/// Fields set by users; `None` leaves a field unchanged on update.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BundleInput {
    pub name: Option<String>,
    pub search: Option<String>,
    pub any_tags: Option<String>,
    pub all_tags: Option<String>,
    pub excluded_tags: Option<String>,
    pub filter_unread: Option<String>,
    pub filter_shared: Option<String>,
    pub order: Option<i64>,
}

impl BundleInput {
    /// The web form sends every field, and empty ones arrive as `None`; make them explicit
    /// empty values so clearing a field in the form actually clears it.
    pub fn complete(mut self) -> Self {
        for f in [
            &mut self.name,
            &mut self.search,
            &mut self.any_tags,
            &mut self.all_tags,
            &mut self.excluded_tags,
        ] {
            f.get_or_insert_default();
        }
        self.filter_unread.get_or_insert_with(|| "off".into());
        self.filter_shared.get_or_insert_with(|| "off".into());
        self
    }

    pub fn validate(&self) -> Result<(), (&'static str, &'static str)> {
        if let Some(name) = &self.name {
            if name.trim().is_empty() {
                return Err(("name", "This field may not be blank."));
            }
            if name.chars().count() > 256 {
                return Err(("name", "Ensure this field has no more than 256 characters."));
            }
        }
        for (field, v) in [
            ("filter_unread", &self.filter_unread),
            ("filter_shared", &self.filter_shared),
        ] {
            if v.as_deref().is_some_and(|v| !matches!(v, "off" | "yes" | "no")) {
                return Err((field, "Must be one of: off, yes, no."));
            }
        }
        Ok(())
    }
}

pub async fn list(db: &Db, owner: i64) -> Result<Vec<Bundle>> {
    db.query(list_stmt(owner)).await
}

/// For page batches (sidebar).
pub fn list_stmt(owner: i64) -> Stmt {
    stmt!(
        format!(r#"SELECT {COLS} FROM bundles WHERE owner_id = ? ORDER BY "order", id"#),
        owner
    )
}

pub async fn get(db: &Db, owner: i64, id: i64) -> Result<Option<Bundle>> {
    Ok(db
        .query(stmt!(
            format!("SELECT {COLS} FROM bundles WHERE id = ? AND owner_id = ?"),
            id,
            owner
        ))
        .await?
        .pop())
}

/// New bundle; without an explicit order it goes last.
pub async fn create(db: &Db, owner: i64, b: &BundleInput) -> Result<Bundle> {
    let now = now();
    let rows: Vec<Row> = db
        .query(stmt!(
            format!(
                r#"INSERT INTO bundles (name, search, any_tags, all_tags, excluded_tags, filter_unread, filter_shared,
                   "order", date_created, date_modified, owner_id)
                   VALUES (?, ?, ?, ?, ?, ?, ?, coalesce(?, (SELECT max("order") + 1 FROM bundles WHERE owner_id = ?), 0), ?, ?, ?)
                   RETURNING {COLS}"#
            ),
            b.name.as_deref().unwrap_or_default().trim(),
            b.search.as_deref().unwrap_or_default(),
            b.any_tags.as_deref().unwrap_or_default(),
            b.all_tags.as_deref().unwrap_or_default(),
            b.excluded_tags.as_deref().unwrap_or_default(),
            b.filter_unread.as_deref().unwrap_or("off"),
            b.filter_shared.as_deref().unwrap_or("off"),
            b.order,
            owner,
            now,
            now,
            owner
        ))
        .await?;
    match rows.into_iter().next() {
        Some(row) => from_row(row),
        None => bail!("bundle insert returned no row"),
    }
}

/// Partial update; `None` when the bundle doesn't exist or isn't the owner's.
pub async fn update(db: &Db, owner: i64, id: i64, b: &BundleInput) -> Result<Option<Bundle>> {
    Ok(db
        .query(stmt!(
            format!(
                r#"UPDATE bundles SET name = coalesce(?, name), search = coalesce(?, search),
                   any_tags = coalesce(?, any_tags), all_tags = coalesce(?, all_tags),
                   excluded_tags = coalesce(?, excluded_tags), filter_unread = coalesce(?, filter_unread),
                   filter_shared = coalesce(?, filter_shared), "order" = coalesce(?, "order"), date_modified = ?
                   WHERE id = ? AND owner_id = ? RETURNING {COLS}"#
            ),
            b.name.as_deref().map(str::trim),
            b.search,
            b.any_tags,
            b.all_tags,
            b.excluded_tags,
            b.filter_unread,
            b.filter_shared,
            b.order,
            now(),
            id,
            owner
        ))
        .await?
        .pop())
}

/// Delete and renumber the remaining bundles 0..n. False when not found.
pub async fn delete(db: &Db, owner: i64, id: i64) -> Result<bool> {
    let mut res = db
        .batch(vec![
            stmt!(
                "DELETE FROM bundles WHERE id = ? AND owner_id = ? RETURNING id",
                id,
                owner
            ),
            renumber_stmt(owner),
        ])
        .await?;
    res.truncate(1);
    Ok(res.pop().is_some_and(|r| !r.is_empty()))
}

/// Move a bundle one position up (-1) or down (+1).
pub async fn shift(db: &Db, owner: i64, id: i64, delta: i64) -> Result<()> {
    let mut ids: Vec<i64> = list(db, owner).await?.into_iter().map(|b| b.id).collect();
    let Some(pos) = ids.iter().position(|&b| b == id) else {
        return Ok(());
    };
    let to = (pos as i64 + delta).clamp(0, ids.len() as i64 - 1) as usize;
    ids.swap(pos, to);
    db.execute(stmt!(
        r#"UPDATE bundles SET "order" = (SELECT key FROM json_each(?) WHERE value = bundles.id) WHERE owner_id = ?"#,
        json!(ids).to_string(),
        owner
    ))
    .await
}

fn renumber_stmt(owner: i64) -> Stmt {
    stmt!(
        r#"UPDATE bundles SET "order" = (SELECT count(*) FROM bundles o WHERE o.owner_id = bundles.owner_id
           AND (o."order" < bundles."order" OR (o."order" = bundles."order" AND o.id < bundles.id)))
           WHERE owner_id = ?"#,
        owner
    )
}

const TAG_IN: &str = "EXISTS (SELECT 1 FROM bookmark_tags bt JOIN tags t ON t.id = bt.tag_id \
                      WHERE bt.bookmark_id = b.id AND t.name IN (SELECT value FROM json_each(";

/// WHERE fragment (over `bookmarks b`) for a bundle's filters: search (full search language),
/// any / all / none of the tags, unread and shared state.
pub fn filter_sql(bundle: &Bundle, sql: &mut Sql, lax: &str) -> String {
    let mut w = Vec::new();
    match search::parse(&bundle.search) {
        Ok(Some(e)) => w.push(search::to_sql(&e, sql, lax)),
        Ok(None) => {}
        Err(_) => w.push("0".into()),
    }
    let any = parse_tag_string(&bundle.any_tags, ' ');
    if !any.is_empty() {
        w.push(format!("{TAG_IN}{})))", sql.bind(json!(any).to_string())));
    }
    for tag in parse_tag_string(&bundle.all_tags, ' ') {
        w.push(format!("{TAG_IN}{})))", sql.bind(json!([tag]).to_string())));
    }
    let excluded = parse_tag_string(&bundle.excluded_tags, ' ');
    if !excluded.is_empty() {
        w.push(format!("NOT {TAG_IN}{})))", sql.bind(json!(excluded).to_string())));
    }
    for (state, col) in [(&bundle.filter_unread, "b.unread"), (&bundle.filter_shared, "b.shared")] {
        match state.as_str() {
            "yes" => w.push(format!("{col} = 1")),
            "no" => w.push(format!("{col} = 0")),
            _ => {}
        }
    }
    if w.is_empty() { "1".into() } else { w.join(" AND ") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::bookmarks::{self as bm, BookmarkInput, ListQuery, Paging, Scope};
    use crate::domain::users;

    #[tokio::test]
    async fn crud_order_and_filter() {
        let db = Db::sqlite(":memory:").unwrap();
        db.migrate().await.unwrap();
        users::create(&db, "u", "pw", false).await.unwrap();
        let input = |name: &str| BundleInput {
            name: Some(name.into()),
            ..Default::default()
        };
        let a = create(&db, 1, &input("A")).await.unwrap();
        let b = create(&db, 1, &input("B")).await.unwrap();
        let c = create(&db, 1, &input("C")).await.unwrap();
        assert_eq!((a.order, b.order, c.order), (0, 1, 2));
        shift(&db, 1, c.id, -1).await.unwrap();
        let names = |v: Vec<Bundle>| v.into_iter().map(|b| b.name).collect::<Vec<_>>();
        assert_eq!(names(list(&db, 1).await.unwrap()), ["A", "C", "B"]);
        assert!(delete(&db, 1, a.id).await.unwrap());
        assert!(!delete(&db, 1, a.id).await.unwrap());
        assert_eq!(
            list(&db, 1).await.unwrap().iter().map(|b| b.order).collect::<Vec<_>>(),
            [0, 1],
            "renumbered"
        );
        assert!(
            update(&db, 2, b.id, &input("x")).await.unwrap().is_none(),
            "other users' bundles"
        );
        let patched = update(
            &db,
            1,
            b.id,
            &BundleInput {
                any_tags: Some("rust go".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!((patched.name.as_str(), patched.any_tags.as_str()), ("B", "rust go"));

        for (url, tags, unread) in [
            ("https://r.com", "rust web", false),
            ("https://g.com", "go", true),
            ("https://p.com", "python web", false),
        ] {
            let i = BookmarkInput {
                url: url.into(),
                unread,
                ..Default::default()
            };
            bm::create(&db, 1, &i, &bm::parse_tag_string(tags, ' ')).await.unwrap();
        }
        let run = |bundle: Bundle| {
            let db = &db;
            async move {
                let stmts = bm::list_stmts(1, Scope::Active, &ListQuery::default(), Some(&bundle), Paging::Page(1));
                let res = bm::list_result(db.batch(stmts).await.unwrap()).unwrap();
                let mut urls: Vec<String> = res.bookmarks.into_iter().map(|b| b.url).collect();
                urls.sort();
                urls
            }
        };
        let bundle = |f: fn(&mut Bundle)| {
            let mut b = Bundle {
                filter_unread: "off".into(),
                filter_shared: "off".into(),
                ..Default::default()
            };
            f(&mut b);
            b
        };
        assert_eq!(
            run(bundle(|b| b.any_tags = "rust go".into())).await,
            ["https://g.com", "https://r.com"]
        );
        assert_eq!(run(bundle(|b| b.all_tags = "web rust".into())).await, ["https://r.com"]);
        assert_eq!(run(bundle(|b| b.excluded_tags = "web".into())).await, ["https://g.com"]);
        assert_eq!(run(bundle(|b| b.filter_unread = "yes".into())).await, ["https://g.com"]);
        assert_eq!(
            run(bundle(|b| b.search = "#web not #python".into())).await,
            ["https://r.com"]
        );
    }
}
