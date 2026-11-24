//! Bookmark rules and SQL. Every operation is a list of statements for ONE `Db::batch` call,
//! so a page or an action costs a single D1 round-trip. Lists (ids, tag names) are bound as one
//! JSON parameter and expanded with `json_each(?)`, which keeps us under D1's 100-param limit.

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::bundles::Bundle;
use crate::db::{Db, Row, Stmt, bool_from_int, from_row, now, values};
use crate::search::{self, Sql};
use crate::stmt;

#[derive(Debug, Clone, Deserialize)]
pub struct Bookmark {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub description: String,
    pub notes: String,
    pub web_archive_snapshot_url: String,
    pub favicon_file: String,
    pub preview_image_file: String,
    #[serde(deserialize_with = "bool_from_int")]
    pub unread: bool,
    #[serde(deserialize_with = "bool_from_int")]
    pub is_archived: bool,
    #[serde(deserialize_with = "bool_from_int")]
    pub shared: bool,
    pub date_added: String,
    pub date_modified: String,
    #[serde(deserialize_with = "space_separated")]
    pub tag_names: Vec<String>,
    /// Username of the owner (shown on the shared bookmarks page).
    #[serde(default)]
    pub owner_name: String,
    /// The most recent complete HTML snapshot (asset id, and its random public id for URLs).
    #[serde(default)]
    pub latest_snapshot_id: Option<i64>,
    #[serde(default)]
    pub latest_snapshot_uid: Option<String>,
}

/// Tag names come back as one space-joined string (tag names never contain whitespace).
fn space_separated<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let s: Option<String> = Deserialize::deserialize(d)?;
    let mut v: Vec<String> = s
        .unwrap_or_default()
        .split(' ')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    v.sort_by_key(|t| t.to_lowercase());
    Ok(v)
}

impl Bookmark {
    pub fn resolved_title(&self) -> &str {
        if self.title.is_empty() { &self.url } else { &self.title }
    }

    /// Stored Wayback snapshot URL, or a Wayback link for when the bookmark was added (linkding's
    /// `generate_fallback_webarchive_url`; the archive shows the closest snapshot).
    pub fn web_archive_url(&self) -> Option<String> {
        if !self.web_archive_snapshot_url.is_empty() {
            return Some(self.web_archive_snapshot_url.clone());
        }
        let added = chrono::DateTime::parse_from_rfc3339(&self.date_added).ok()?;
        Some(format!(
            "https://web.archive.org/web/{}/{}",
            added.to_utc().format("%Y%m%d%H%M%S"),
            self.url
        ))
    }

    /// The URL for use in `href`. Script-capable schemes are neutralized even though
    /// `validate()` rejects them, in case such rows arrive another way (e.g. an old database).
    pub fn href(&self) -> &str {
        if is_safe_url(&self.url) { &self.url } else { "#" }
    }
}

/// Rejects schemes that execute in the page when clicked. Parsing follows the WHATWG URL spec
/// (like browsers do), so tricks such as `java\tscript:` or leading whitespace are caught.
pub fn is_safe_url(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(u) => !matches!(u.scheme(), "javascript" | "data" | "vbscript"),
        Err(_) => false,
    }
}

const SELECT: &str = "SELECT b.id, b.url, b.title, b.description, b.notes, b.web_archive_snapshot_url, b.latest_snapshot_id, \
    b.favicon_file, b.preview_image_file, b.unread, b.is_archived, b.shared, b.date_added, b.date_modified, \
    (SELECT group_concat(t.name, ' ') FROM bookmark_tags bt JOIN tags t ON t.id = bt.tag_id \
     WHERE bt.bookmark_id = b.id) AS tag_names, \
    (SELECT username FROM users WHERE id = b.owner_id) AS owner_name, \
    (SELECT uid FROM assets WHERE id = b.latest_snapshot_id) AS latest_snapshot_uid \
    FROM bookmarks b";

// ---------------------------------------------------------------------------------------------
// Listing / search

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Scope {
    #[default]
    Active,
    Archived,
    /// Bookmarks other users chose to share (owners with sharing enabled and active accounts),
    /// archived ones included like linkding. `public_only`: just owners allowing public sharing
    /// (anonymous viewers). `owner` is then the viewer, used only for their search preferences.
    Shared {
        public_only: bool,
    },
}

/// The list page's query string (`BookmarkSearch` in linkding).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ListQuery {
    pub q: String,
    pub sort: String,
    pub unread: String,
    pub shared: String,
    pub page: Option<i64>,
    /// ISO 8601; invalid values are ignored (like linkding).
    pub added_since: String,
    pub modified_since: String,
    /// REST API paging (the web UI pages with `page` and the profile's page size).
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Restrict to a bundle's filters (the caller loads the bundle).
    pub bundle: Option<i64>,
    /// Shared bookmarks: only this user's (username).
    pub user: String,
}

impl ListQuery {
    pub fn page(&self) -> i64 {
        self.page.unwrap_or(1).max(1)
    }
}

pub const SORTS: &[(&str, &str)] = &[
    ("added_desc", "Newest first"),
    ("added_asc", "Oldest first"),
    ("modified_desc", "Recently modified"),
    ("modified_asc", "Least recently modified"),
    ("title_asc", "Title A–Z"),
    ("title_desc", "Title Z–A"),
];

/// WHERE clause (over `bookmarks b`) shared by the list, count and tag-cloud statements.
fn filter(owner: i64, scope: Scope, q: &ListQuery, bundle: Option<&Bundle>, sql: &mut Sql) -> String {
    let owner_p = sql.bind(owner);
    let mut w = match scope {
        Scope::Active => format!("b.owner_id = {owner_p} AND b.is_archived = 0"),
        Scope::Archived => format!("b.owner_id = {owner_p} AND b.is_archived = 1"),
        Scope::Shared { public_only } => {
            let public = if public_only {
                " AND p.enable_public_sharing = 1"
            } else {
                ""
            };
            // `{owner_p} IS NOT NULL`: always true, but keeps ?1 (the viewer, otherwise only used
            // by the lax-search and page-size subqueries) referenced in every statement, since
            // SQLite and D1 reject bound parameters a statement doesn't use.
            let mut w = format!(
                "{owner_p} IS NOT NULL AND b.shared = 1 AND b.owner_id IN (SELECT p.user_id FROM user_profiles p \
                 JOIN users u ON u.id = p.user_id WHERE u.is_active = 1 AND p.enable_sharing = 1{public})"
            );
            if !q.user.is_empty() {
                w += &format!(
                    " AND b.owner_id = (SELECT id FROM users WHERE username = {})",
                    sql.bind(q.user.as_str())
                );
            }
            w
        }
    };
    let lax = format!("coalesce((SELECT tag_search = 'lax' FROM user_profiles WHERE user_id = {owner_p}), 0)");
    match search::parse(&q.q) {
        Ok(Some(expr)) => w += &format!(" AND {}", search::to_sql(&expr, sql, &lax)),
        Ok(None) => {}
        Err(_) => w += " AND 0", // invalid query → no results, like linkding
    }
    if let Some(bundle) = bundle {
        w += &format!(" AND ({})", super::bundles::filter_sql(bundle, sql, &lax));
    }
    for (since, col) in [(&q.added_since, "b.date_added"), (&q.modified_since, "b.date_modified")] {
        // Stored timestamps share one UTC format, so they compare correctly as strings.
        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(since) {
            w += &format!(" AND {col} > {}", sql.bind(crate::db::format_time(t.to_utc())));
        }
    }
    for (param, col) in [(&q.unread, "b.unread"), (&q.shared, "b.shared")] {
        match param.as_str() {
            "yes" => w += &format!(" AND {col} = 1"),
            "no" => w += &format!(" AND {col} = 0"),
            _ => {}
        }
    }
    w
}

fn order_by(sort: &str) -> &'static str {
    match sort {
        "added_asc" => "b.date_added ASC",
        "modified_desc" => "b.date_modified DESC",
        "modified_asc" => "b.date_modified ASC",
        "title_asc" => "lower(CASE WHEN b.title != '' THEN b.title ELSE b.url END) ASC",
        "title_desc" => "lower(CASE WHEN b.title != '' THEN b.title ELSE b.url END) DESC",
        _ => "b.date_added DESC",
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Paging {
    /// Web UI: 1-based page, page size from the user's profile (read inside SQL).
    Page(i64),
    /// REST API.
    LimitOffset(i64, i64),
}

/// Statements for one list page: [page rows, total count, tag cloud names].
/// Page size comes from the profile inside SQL, so no extra round-trip is needed to read it.
pub fn list_stmts(owner: i64, scope: Scope, q: &ListQuery, bundle: Option<&Bundle>, paging: Paging) -> Vec<Stmt> {
    let mut sql = Sql::default();
    let w = filter(owner, scope, q, bundle, &mut sql);
    let filter_params = sql.params.clone();
    let count = format!("SELECT count(*) AS n FROM bookmarks b WHERE {w}");
    let tags = format!(
        "SELECT DISTINCT t.name FROM tags t JOIN bookmark_tags bt ON bt.tag_id = t.id \
         JOIN bookmarks b ON b.id = bt.bookmark_id WHERE {w} ORDER BY lower(t.name)"
    );
    let limit = match paging {
        Paging::Page(page) => {
            // Viewer's page size; 30 (the default) for anonymous viewers of shared bookmarks.
            let per_page = "coalesce((SELECT items_per_page FROM user_profiles WHERE user_id = ?1), 30)";
            format!("{per_page} OFFSET {per_page} * {}", sql.bind(page.max(1) - 1))
        }
        Paging::LimitOffset(limit, offset) => format!("{} OFFSET {}", sql.bind(limit), sql.bind(offset)),
    };
    let list = format!(
        "{SELECT} WHERE {w} ORDER BY {}, b.id DESC LIMIT {limit}",
        order_by(&q.sort)
    );
    let mut stmts = vec![
        Stmt {
            sql: list,
            params: sql.params,
        },
        Stmt {
            sql: count,
            params: filter_params.clone(),
        },
        Stmt {
            sql: tags,
            params: filter_params,
        },
    ];
    if let Scope::Shared { .. } = scope {
        // Sharing users for the sidebar: same filters, minus the selected user.
        let mut usql = Sql::default();
        let w = filter(
            owner,
            scope,
            &ListQuery {
                user: String::new(),
                ..q.clone()
            },
            bundle,
            &mut usql,
        );
        stmts.push(Stmt {
            sql: format!(
                "SELECT DISTINCT u.username FROM bookmarks b JOIN users u ON u.id = b.owner_id WHERE {w} \
                 ORDER BY lower(u.username)"
            ),
            params: usql.params,
        });
    }
    stmts
}

pub struct ListResult {
    pub bookmarks: Vec<Bookmark>,
    pub total: i64,
    pub tags: Vec<String>,
    /// Shared scope only: users with matching shared bookmarks.
    pub users: Vec<String>,
}

/// Decode the result sets produced by `list_stmts`, in order: rows, count, and (optionally,
/// callers may drop them) the tag cloud and, for the shared scope, the sharing users.
pub fn list_result(rows: Vec<Vec<Row>>) -> Result<ListResult> {
    let mut sets = rows.into_iter();
    let list = sets.next().unwrap_or_default();
    let count = sets.next().unwrap_or_default();
    let tags = sets.next().unwrap_or_default();
    let users = sets.next().unwrap_or_default();
    Ok(ListResult {
        bookmarks: list.into_iter().map(from_row).collect::<Result<_>>()?,
        total: values(count, "n").pop().unwrap_or(0),
        tags: values(tags, "name"),
        users: values(users, "username"),
    })
}

pub async fn get(db: &Db, owner: i64, id: i64) -> Result<Option<Bookmark>> {
    let sql = format!("{SELECT} WHERE b.id = ? AND b.owner_id = ?");
    Ok(db.query(stmt!(sql, id, owner)).await?.pop())
}

// ---------------------------------------------------------------------------------------------
// Create / update

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BookmarkInput {
    pub url: String,
    pub tag_string: String,
    pub title: String,
    pub description: String,
    pub notes: String,
    #[serde(deserialize_with = "checkbox")]
    pub unread: bool,
    #[serde(deserialize_with = "checkbox")]
    pub shared: bool,
    #[serde(deserialize_with = "checkbox")]
    pub is_archived: bool,
}

/// HTML checkboxes send "on" (or nothing); JSON clients send booleans.
pub fn checkbox<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Bool(b) => b,
        Value::String(s) => matches!(s.as_str(), "on" | "true" | "1" | "yes"),
        _ => false,
    })
}

/// A validation failure: (field, message), so the API can report it per field.
pub type Invalid = (&'static str, &'static str);

pub fn validate_url(url: &str) -> Result<(), Invalid> {
    let url = url.trim();
    if url.is_empty() {
        return Err(("url", "This field may not be blank."));
    }
    if url.len() > 2048 || url::Url::parse(url).is_err() {
        return Err(("url", "Enter a valid URL."));
    }
    if !is_safe_url(url) {
        return Err(("url", "This URL scheme is not allowed."));
    }
    Ok(())
}

pub fn validate_title(title: &str) -> Result<(), Invalid> {
    if title.chars().count() > 512 {
        return Err(("title", "Ensure this field has no more than 512 characters."));
    }
    Ok(())
}

impl BookmarkInput {
    pub fn validate(&self) -> Result<(), Invalid> {
        validate_url(&self.url)?;
        validate_title(&self.title)
    }

    /// Form semantics: tags separated by whitespace.
    pub fn tags(&self) -> Vec<String> {
        parse_tag_string(&self.tag_string, ' ')
    }

    /// Edit form: every field is set.
    pub fn to_patch(&self) -> Patch {
        Patch {
            url: Some(self.url.clone()),
            title: Some(self.title.clone()),
            description: Some(self.description.clone()),
            notes: Some(self.notes.clone()),
            unread: Some(self.unread),
            shared: Some(self.shared),
            is_archived: None,
            tags: Some(self.tags()),
            add_tags: vec![],
        }
    }
}

/// Create a bookmark, or, when the owner already has one with the same normalized URL, update
/// that one instead (linkding semantics; `is_archived` only applies to new bookmarks).
/// One round-trip; returns the saved bookmark.
pub async fn create(db: &Db, owner: i64, input: &BookmarkInput, tags: &[String]) -> Result<Bookmark> {
    let url = input.url.trim();
    let norm = normalize_url(url);
    let now = now();
    let existing = "(SELECT id FROM bookmarks WHERE owner_id = ? AND url_normalized = ? ORDER BY id LIMIT 1)";
    let mut stmts = vec![
        Stmt {
            sql: format!(
                "UPDATE bookmarks SET title = ?, description = ?, notes = ?, unread = ?, shared = ?, \
                 date_modified = ? WHERE id = {existing}"
            ),
            params: vec![
                json!(input.title),
                json!(input.description),
                json!(input.notes),
                json!(input.unread),
                json!(input.shared),
                json!(now),
                json!(owner),
                json!(norm),
            ],
        },
        stmt!(
            "INSERT INTO bookmarks (url, url_normalized, title, description, notes, unread, shared, \
             is_archived, date_added, date_modified, owner_id) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? \
             WHERE NOT EXISTS (SELECT 1 FROM bookmarks WHERE owner_id = ? AND url_normalized = ?)",
            url,
            norm,
            input.title,
            input.description,
            input.notes,
            input.unread,
            input.shared,
            input.is_archived,
            now,
            now,
            owner,
            owner,
            norm
        ),
    ];
    let target = (existing, vec![json!(owner), json!(norm)]);
    stmts.extend(set_tags_stmts(owner, &target, tags, &now));
    stmts.push(select_target(&target));
    let row = db.batch(stmts).await?.pop().and_then(|mut rows| rows.pop());
    match row {
        Some(row) => from_row(row),
        None => bail!("bookmark insert returned no row"),
    }
}

/// Partial update: `None` leaves a field unchanged (REST PATCH/PUT semantics).
#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub url: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub notes: Option<String>,
    pub unread: Option<bool>,
    pub shared: Option<bool>,
    pub is_archived: Option<bool>,
    /// Replace all tags.
    pub tags: Option<Vec<String>>,
    /// Add these tags, keeping existing ones (auto-tagging when `tags` isn't given).
    pub add_tags: Vec<String>,
}

pub enum Updated {
    Ok(Box<Bookmark>),
    NotFound,
    /// Another bookmark of the owner already has this (normalized) URL.
    DuplicateUrl,
}

/// Apply `patch` to the owner's bookmark `id`. One round-trip.
pub async fn update(db: &Db, owner: i64, id: i64, patch: &Patch) -> Result<Updated> {
    let url = patch.url.as_deref().map(str::trim);
    let norm = url.map(normalize_url);
    let now = now();
    // The target is NULL when the bookmark isn't the owner's or the new URL would duplicate
    // another bookmark, which turns every following statement into a no-op.
    let target = (
        "(SELECT id FROM bookmarks WHERE id = ? AND owner_id = ? AND NOT EXISTS \
          (SELECT 1 FROM bookmarks WHERE owner_id = ? AND url_normalized = ? AND id != ?))",
        vec![json!(id), json!(owner), json!(owner), json!(norm), json!(id)],
    );
    let mut stmts = vec![Stmt {
        sql: format!(
            "UPDATE bookmarks SET url = coalesce(?, url), url_normalized = coalesce(?, url_normalized), \
             title = coalesce(?, title), description = coalesce(?, description), notes = coalesce(?, notes), \
             unread = coalesce(?, unread), shared = coalesce(?, shared), is_archived = coalesce(?, is_archived), \
             date_modified = ? WHERE id = {}",
            target.0
        ),
        params: [
            json!(url),
            json!(norm),
            json!(patch.title),
            json!(patch.description),
            json!(patch.notes),
            json!(patch.unread),
            json!(patch.shared),
            json!(patch.is_archived),
            json!(now),
        ]
        .into_iter()
        .chain(target.1.iter().cloned())
        .collect(),
    }];
    if let Some(tags) = &patch.tags {
        stmts.extend(set_tags_stmts(owner, &target, tags, &now));
    }
    if !patch.add_tags.is_empty() {
        stmts.extend(add_tags_stmts(owner, &target, &patch.add_tags, &now));
    }
    stmts.push(stmt!(
        "SELECT count(*) AS n FROM bookmarks WHERE id = ? AND owner_id = ?",
        id,
        owner
    ));
    stmts.push(select_target(&target));
    let mut res = db.batch(stmts).await?;
    let row = res.pop().and_then(|mut rows| rows.pop());
    let exists = values(res.pop().unwrap_or_default(), "n").pop() == Some(1);
    Ok(match (row, exists) {
        (Some(row), _) => Updated::Ok(Box::new(from_row(row)?)),
        (None, true) => Updated::DuplicateUrl,
        (None, false) => Updated::NotFound,
    })
}

/// Returns false when the bookmark doesn't exist or isn't the owner's.
pub async fn delete(db: &Db, owner: i64, id: i64) -> Result<bool> {
    let rows: Vec<Row> = db
        .query(stmt!(
            "DELETE FROM bookmarks WHERE id = ? AND owner_id = ? RETURNING id",
            id,
            owner
        ))
        .await?;
    Ok(!rows.is_empty())
}

/// The owner's bookmark for `url` (matched on the normalized URL), if any.
pub async fn find_by_url(db: &Db, owner: i64, url: &str) -> Result<Option<Bookmark>> {
    let sql = format!("{SELECT} WHERE b.owner_id = ? AND b.url_normalized = ? ORDER BY b.id LIMIT 1");
    Ok(db.query(stmt!(sql, owner, normalize_url(url))).await?.pop())
}

type Target<'a> = (&'a str, Vec<Value>);

fn select_target(target: &Target) -> Stmt {
    Stmt {
        sql: format!("{SELECT} WHERE b.id = {}", target.0),
        params: target.1.clone(),
    }
}

/// Replace the tags of the bookmark selected by `target` (a scalar subquery + its params),
/// creating missing tags.
fn set_tags_stmts(owner: i64, target: &Target, names: &[String], now: &str) -> Vec<Stmt> {
    let (t, tp) = target;
    let unlink_all = Stmt {
        sql: format!("DELETE FROM bookmark_tags WHERE bookmark_id = {t}"),
        params: tp.clone(),
    };
    [vec![unlink_all], add_tags_stmts(owner, target, names, now)].concat()
}

/// Link these tags (creating missing ones) to the bookmark selected by `target`, keeping its others.
fn add_tags_stmts(owner: i64, target: &Target, names: &[String], now: &str) -> Vec<Stmt> {
    let (t, tp) = target;
    vec![
        upsert_tags_stmt(owner, names, now),
        Stmt {
            sql: format!(
                "INSERT INTO bookmark_tags (bookmark_id, tag_id) SELECT x.id, tags.id FROM (SELECT {t} AS id) x \
                 JOIN tags ON tags.owner_id = ? AND tags.name IN (SELECT value FROM json_each(?)) \
                 WHERE x.id IS NOT NULL ON CONFLICT DO NOTHING"
            ),
            params: tp
                .iter()
                .cloned()
                .chain([json!(owner), json!(json!(names).to_string())])
                .collect(),
        },
    ]
}

/// Tags plus auto tags, deduplicated case-insensitively (existing spelling wins), sorted.
pub fn with_auto_tags(tags: &[String], rules: &str, url: &str) -> Vec<String> {
    let all: Vec<String> = tags
        .iter()
        .cloned()
        .chain(super::auto_tagging::get_tags(rules, url))
        .collect();
    parse_tag_string(&all.join(" "), ' ')
}

fn upsert_tags_stmt(owner: i64, names: &[String], now: &str) -> Stmt {
    stmt!(
        "INSERT INTO tags (name, date_added, owner_id) SELECT value, ?, ? FROM json_each(?) WHERE true \
         ON CONFLICT (owner_id, name) DO NOTHING",
        now,
        owner,
        json!(names).to_string()
    )
}

// ---------------------------------------------------------------------------------------------
// Row and bulk actions

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Archive,
    Unarchive,
    Delete,
    MarkRead,
    MarkUnread,
    Share,
    Unshare,
    Tag,
    Untag,
}

impl Action {
    /// Names as used by linkding's bulk-edit form.
    pub fn parse(s: &str) -> Option<Action> {
        Some(match s {
            "archive" | "bulk_archive" => Action::Archive,
            "unarchive" | "bulk_unarchive" => Action::Unarchive,
            "remove" | "delete" | "bulk_delete" => Action::Delete,
            "mark_as_read" | "bulk_read" => Action::MarkRead,
            "mark_as_unread" | "bulk_unread" => Action::MarkUnread,
            "share" | "bulk_share" => Action::Share,
            "unshare" | "bulk_unshare" => Action::Unshare,
            "bulk_tag" => Action::Tag,
            "bulk_untag" => Action::Untag,
            _ => return None,
        })
    }
}

/// Statements applying `action` to the owner's bookmarks among `ids`. Ids of other users'
/// bookmarks are silently ignored.
pub fn action_stmts(owner: i64, action: Action, ids: &[i64], tag_string: &str) -> Vec<Stmt> {
    let ids = json!(ids).to_string();
    let now = now();
    let owned = "SELECT id FROM bookmarks WHERE owner_id = ? AND id IN (SELECT value FROM json_each(?))";
    // `assign` is either empty (only bump date_modified) or ends with ", ".
    let set = |assign: &str| Stmt {
        sql: format!("UPDATE bookmarks SET {assign}date_modified = ? WHERE id IN ({owned})"),
        params: vec![json!(now), json!(owner), json!(ids)],
    };
    let names = parse_tag_string(tag_string, ' ');
    match action {
        Action::Archive => vec![set("is_archived = 1, ")],
        Action::Unarchive => vec![set("is_archived = 0, ")],
        Action::MarkRead => vec![set("unread = 0, ")],
        Action::MarkUnread => vec![set("unread = 1, ")],
        Action::Share => vec![set("shared = 1, ")],
        Action::Unshare => vec![set("shared = 0, ")],
        Action::Delete => vec![Stmt {
            sql: format!("DELETE FROM bookmarks WHERE id IN ({owned})"),
            params: vec![json!(owner), json!(ids)],
        }],
        Action::Tag | Action::Untag if names.is_empty() => vec![],
        Action::Tag => vec![
            upsert_tags_stmt(owner, &names, &now),
            Stmt {
                sql: format!(
                    "INSERT INTO bookmark_tags (bookmark_id, tag_id) SELECT b.id, t.id FROM ({owned}) b \
                     JOIN tags t ON t.owner_id = ? AND t.name IN (SELECT value FROM json_each(?)) WHERE true \
                     ON CONFLICT DO NOTHING"
                ),
                params: vec![json!(owner), json!(ids), json!(owner), json!(json!(names).to_string())],
            },
            set(""),
        ],
        Action::Untag => vec![
            Stmt {
                sql: format!(
                    "DELETE FROM bookmark_tags WHERE bookmark_id IN ({owned}) AND tag_id IN \
                     (SELECT id FROM tags WHERE owner_id = ? AND name IN (SELECT value FROM json_each(?)))"
                ),
                params: vec![json!(owner), json!(ids), json!(owner), json!(json!(names).to_string())],
            },
            set(""),
        ],
    }
}

// ---------------------------------------------------------------------------------------------
// Pure helpers (ports of linkding's utils)

/// linkding `parse_tag_string`: split, trim, inner whitespace → '-', dedupe case-insensitively,
/// sort case-insensitively.
pub fn parse_tag_string(s: &str, delimiter: char) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in s.split(delimiter).map(str::trim).filter(|n| !n.is_empty()) {
        let name = super::tags::clean_name(name);
        if !out.iter().any(|o| o.to_lowercase() == name.to_lowercase()) {
            out.push(name);
        }
    }
    out.sort_by_key(|t| t.to_lowercase());
    out
}

/// linkding `normalize_url`: lowercase scheme and host, strip trailing slashes from the path,
/// sort query parameters, drop an empty fragment. Used to detect duplicate bookmarks.
pub fn normalize_url(url: &str) -> String {
    let url = url.trim();
    let (rest, fragment) = url.split_once('#').unwrap_or((url, ""));
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (prefix, path) = match rest.split_once("://") {
        Some((scheme, after)) => {
            let (netloc, path) = after.find('/').map_or((after, ""), |i| after.split_at(i));
            let netloc = match netloc.rsplit_once('@') {
                Some((auth, host)) => format!("{auth}@{}", host.to_lowercase()),
                None => netloc.to_lowercase(),
            };
            (format!("{}://{netloc}", scheme.to_lowercase()), path)
        }
        None => (String::new(), rest),
    };
    let mut out = prefix + path.trim_end_matches('/');
    if !query.is_empty() {
        let mut pairs: Vec<(String, String)> = query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (unquote_plus(k), unquote_plus(v))
            })
            .collect();
        pairs.sort();
        let q: Vec<String> = pairs
            .iter()
            .map(|(k, v)| format!("{}={}", quote(k), quote(v)))
            .collect();
        out += &format!("?{}", q.join("&"));
    }
    if !fragment.is_empty() {
        out += &format!("#{fragment}");
    }
    out
}

fn unquote_plus(s: &str) -> String {
    let s = s.replace('+', " ");
    url::form_urlencoded::parse(format!("x={}", s.replace('&', "%26").replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// Python `urllib.parse.quote(s, safe='')`.
fn quote(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::users;

    // Cases from linkding's bookmarks/tests/test_utils.py
    #[test]
    fn normalize() {
        for (input, want) in [
            ("https://example.com/", "https://example.com"),
            ("https://example.com/path/", "https://example.com/path"),
            ("https://example.com?z=1&a=2", "https://example.com?a=2&z=1"),
            ("https://example.com?c=3&b=2&a=1", "https://example.com?a=1&b=2&c=3"),
            (
                "https://example.com?empty=&filled=value",
                "https://example.com?empty=&filled=value",
            ),
            ("https://EXAMPLE.com/Path/To/Page", "https://example.com/Path/To/Page"),
            ("HTTPS://EXAMPLE.COM/path", "https://example.com/path"),
            (
                "https://example.com/path%20with%20spaces",
                "https://example.com/path%20with%20spaces",
            ),
            (
                "https://example.com/path?q=hello%20world",
                "https://example.com/path?q=hello%20world",
            ),
            (
                "https://example.com?z=2&a=1#fragment",
                "https://example.com?a=1&z=2#fragment",
            ),
            (
                "https://example.com?z=1&a=2&z=3&b=4",
                "https://example.com?a=2&b=4&z=1&z=3",
            ),
            (
                "https://example.com?special=%21%40%23%24%25",
                "https://example.com?special=%21%40%23%24%25",
            ),
            ("FTP://example.com", "ftp://example.com"),
            ("https://EXAMPLE.COM:8080", "https://example.com:8080"),
            ("https://user:pass@EXAMPLE.COM", "https://user:pass@example.com"),
            ("https://example.com#", "https://example.com"),
            ("https://example.com#section", "https://example.com#section"),
            ("", ""),
            ("   ", ""),
        ] {
            assert_eq!(normalize_url(input), want, "{input}");
        }
    }

    #[test]
    fn dangerous_schemes_rejected() {
        for bad in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            " javascript:alert(1)",
            "java\tscript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
        ] {
            assert!(!is_safe_url(bad), "{bad:?}");
            assert!(input(bad, "").validate().is_err(), "{bad:?}");
        }
        for good in [
            "https://a.com",
            "ftp://a.com/f",
            "mailto:x@y.z",
            "obsidian://open?vault=x",
        ] {
            assert!(is_safe_url(good), "{good}");
            assert!(input(good, "").validate().is_ok(), "{good}");
        }
    }

    #[test]
    fn tag_strings() {
        assert_eq!(parse_tag_string("b a  B c", ' '), vec!["a", "b", "c"]);
        assert_eq!(parse_tag_string(" one , two words ,", ','), vec!["one", "two-words"]);
        assert!(parse_tag_string("", ' ').is_empty());
    }

    async fn setup() -> (Db, i64) {
        let db = Db::sqlite(":memory:").unwrap();
        db.migrate().await.unwrap();
        users::create(&db, "u", "pw", false).await.unwrap();
        (db, 1)
    }

    async fn add(db: &Db, owner: i64, input: &BookmarkInput) -> Result<i64> {
        Ok(create(db, owner, input, &input.tags()).await?.id)
    }

    fn input(url: &str, tags: &str) -> BookmarkInput {
        BookmarkInput {
            url: url.into(),
            tag_string: tags.into(),
            ..Default::default()
        }
    }

    async fn list(db: &Db, owner: i64, scope: Scope, q: &str) -> ListResult {
        let q = ListQuery {
            q: q.into(),
            ..Default::default()
        };
        list_result(
            db.batch(list_stmts(owner, scope, &q, None, Paging::Page(1)))
                .await
                .unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn create_merges_duplicates_and_sets_tags() {
        let (db, u) = setup().await;
        let a = add(&db, u, &input("https://Example.com/a/", "Rust web")).await.unwrap();
        let mut again = input("https://example.com/a", "rust db");
        again.title = "new title".into();
        let b = add(&db, u, &again).await.unwrap();
        assert_eq!(a, b, "same normalized URL updates the existing bookmark");

        let bm = get(&db, u, a).await.unwrap().unwrap();
        assert_eq!(bm.title, "new title");
        assert_eq!(
            bm.tag_names,
            vec!["db", "Rust"],
            "tags replaced; existing tag keeps its casing"
        );
        let tags: Vec<Row> = db.query(stmt!("SELECT name FROM tags ORDER BY name")).await.unwrap();
        assert_eq!(tags.len(), 3, "orphan tags are kept, like linkding");

        let c = add(&db, u, &input("https://example.com/b", "")).await.unwrap();
        assert_ne!(a, c);
    }

    #[tokio::test]
    async fn update_respects_owner_patch_and_duplicates() {
        let (db, u) = setup().await;
        users::create(&db, "other", "pw", false).await.unwrap();
        let id = add(&db, u, &input("https://a.com", "x")).await.unwrap();
        let other = add(&db, u, &input("https://other.com", "")).await.unwrap();

        let evil = input("https://evil.com", "pwned").to_patch();
        assert!(matches!(update(&db, 2, id, &evil).await.unwrap(), Updated::NotFound));
        assert!(get(&db, 2, id).await.unwrap().is_none());

        let full = input("https://b.com", "y z").to_patch();
        let Updated::Ok(bm) = update(&db, u, id, &full).await.unwrap() else {
            panic!()
        };
        assert_eq!(
            (bm.url.as_str(), bm.tag_names.clone()),
            ("https://b.com", vec!["y".into(), "z".into()])
        );

        // PATCH: only given fields change, tags untouched when absent
        let patch = Patch {
            title: Some("T".into()),
            unread: Some(true),
            ..Default::default()
        };
        let Updated::Ok(bm) = update(&db, u, id, &patch).await.unwrap() else {
            panic!()
        };
        assert_eq!(
            (bm.title.as_str(), bm.unread, bm.url.as_str()),
            ("T", true, "https://b.com")
        );
        assert_eq!(bm.tag_names, vec!["y", "z"]);

        // changing the URL to another bookmark's (normalized) URL is rejected, nothing changes
        let dup = Patch {
            url: Some("https://OTHER.com/".into()),
            tags: Some(vec!["q".into()]),
            ..Default::default()
        };
        assert!(matches!(update(&db, u, id, &dup).await.unwrap(), Updated::DuplicateUrl));
        let bm = get(&db, u, id).await.unwrap().unwrap();
        assert_eq!(
            (bm.url.as_str(), bm.tag_names.clone()),
            ("https://b.com", vec!["y".into(), "z".into()])
        );
        // add_tags keeps existing tags
        let add = Patch {
            add_tags: vec!["auto".into(), "Y".into()],
            ..Default::default()
        };
        let Updated::Ok(bm) = update(&db, u, id, &add).await.unwrap() else {
            panic!()
        };
        assert_eq!(bm.tag_names, vec!["auto", "y", "z"]);
        assert_eq!(
            with_auto_tags(&["Web".into()], "rust-lang.org rust web", "https://www.rust-lang.org"),
            ["rust", "Web"]
        );
        // ...but re-saving a bookmark with its own URL is fine
        let same = Patch {
            url: Some("https://b.com".into()),
            ..Default::default()
        };
        assert!(matches!(update(&db, u, id, &same).await.unwrap(), Updated::Ok(_)));

        assert!(!delete(&db, 2, other).await.unwrap());
        assert!(delete(&db, u, other).await.unwrap());
        assert!(!delete(&db, u, other).await.unwrap());
    }

    #[tokio::test]
    async fn shared_scope_respects_owner_settings() {
        let (db, u) = setup().await;
        users::create(&db, "bob", "pw", false).await.unwrap();
        users::create(&db, "carol", "pw", false).await.unwrap();
        let share = |owner: i64, url: &str, shared: bool| {
            let i = BookmarkInput {
                shared,
                ..input(url, "t")
            };
            let db = &db;
            async move { add(db, owner, &i).await.unwrap() }
        };
        share(u, "https://u-shared.com", true).await;
        share(u, "https://u-private.com", false).await;
        share(2, "https://bob-shared.com", true).await;
        share(3, "https://carol-shared.com", true).await;
        db.execute(stmt!(
            "UPDATE user_profiles SET enable_sharing = 1 WHERE user_id IN (1, 2)"
        ))
        .await
        .unwrap();
        db.execute(stmt!(
            "UPDATE user_profiles SET enable_public_sharing = 1 WHERE user_id = 2"
        ))
        .await
        .unwrap();
        let run = |public_only: bool, user: &str| {
            let q = ListQuery {
                user: user.into(),
                ..Default::default()
            };
            let db = &db;
            async move {
                let r = list_result(
                    db.batch(list_stmts(0, Scope::Shared { public_only }, &q, None, Paging::Page(1)))
                        .await
                        .unwrap(),
                )
                .unwrap();
                let mut urls: Vec<String> = r.bookmarks.iter().map(|b| b.url.clone()).collect();
                urls.sort();
                (urls, r.users)
            }
        };
        let (urls, users) = run(false, "").await;
        assert_eq!(
            urls,
            ["https://bob-shared.com", "https://u-shared.com"],
            "carol hasn't enabled sharing"
        );
        assert_eq!(users, ["bob", "u"]);
        assert_eq!(
            run(true, "").await.0,
            ["https://bob-shared.com"],
            "only public sharers for anonymous"
        );
        let (urls, users) = run(false, "u").await;
        assert_eq!(
            (urls, users.len()),
            (vec!["https://u-shared.com".to_string()], 2),
            "user filter keeps the user list"
        );
        db.execute(stmt!("UPDATE users SET is_active = 0 WHERE id = 2"))
            .await
            .unwrap();
        assert_eq!(
            run(false, "").await.0,
            ["https://u-shared.com"],
            "inactive users' shares disappear"
        );
    }

    #[tokio::test]
    async fn since_filters_and_limit_offset() {
        let (db, u) = setup().await;
        for i in 0..5 {
            add(&db, u, &input(&format!("https://x.com/{i}"), "")).await.unwrap();
        }
        db.execute(stmt!(
            "UPDATE bookmarks SET date_added = '2020-01-01T00:00:00.000000Z' WHERE id <= 2"
        ))
        .await
        .unwrap();
        let run = |q: ListQuery, paging| {
            let db = &db;
            async move { list_result(db.batch(list_stmts(u, Scope::Active, &q, None, paging)).await.unwrap()).unwrap() }
        };
        let since = ListQuery {
            added_since: "2021-06-01T00:00:00+02:00".into(),
            ..Default::default()
        };
        assert_eq!(run(since, Paging::Page(1)).await.total, 3);
        let bad = ListQuery {
            added_since: "yesterday".into(),
            ..Default::default()
        };
        assert_eq!(run(bad, Paging::Page(1)).await.total, 5, "invalid dates are ignored");
        let page = run(ListQuery::default(), Paging::LimitOffset(2, 1)).await;
        assert_eq!((page.bookmarks.len(), page.total), (2, 5));
    }

    #[tokio::test]
    async fn search_and_bulk_actions() {
        let (db, u) = setup().await;
        let rust = add(
            &db,
            u,
            &BookmarkInput {
                title: "Rust book".into(),
                ..input("https://rust.org", "lang")
            },
        )
        .await
        .unwrap();
        let py = add(&db, u, &input("https://python.org", "lang snake")).await.unwrap();
        let plain = add(
            &db,
            u,
            &BookmarkInput {
                unread: true,
                ..input("https://plain.org", "")
            },
        )
        .await
        .unwrap();

        let ids = |r: ListResult| r.bookmarks.iter().map(|b| b.id).collect::<Vec<_>>();
        assert_eq!(ids(list(&db, u, Scope::Active, "rust").await), vec![rust]);
        assert_eq!(ids(list(&db, u, Scope::Active, "#lang not #snake").await), vec![rust]);
        assert_eq!(ids(list(&db, u, Scope::Active, "!untagged").await), vec![plain]);
        assert_eq!(ids(list(&db, u, Scope::Active, "!unread").await), vec![plain]);
        assert_eq!(ids(list(&db, u, Scope::Active, "(broken").await), Vec::<i64>::new());
        let all = list(&db, u, Scope::Active, "").await;
        assert_eq!((all.total, all.tags.clone()), (3, vec!["lang".into(), "snake".into()]));

        // lax mode: bare word matches tag names too
        assert!(ids(list(&db, u, Scope::Active, "snake").await).is_empty());
        db.execute(stmt!("UPDATE user_profiles SET tag_search = 'lax'"))
            .await
            .unwrap();
        assert_eq!(ids(list(&db, u, Scope::Active, "snake").await), vec![py]);

        db.batch(action_stmts(u, Action::Archive, &[rust, py], ""))
            .await
            .unwrap();
        assert_eq!(ids(list(&db, u, Scope::Archived, "").await).len(), 2);
        db.batch(action_stmts(u, Action::Tag, &[rust, plain], "new Lang"))
            .await
            .unwrap();
        assert_eq!(
            get(&db, u, plain).await.unwrap().unwrap().tag_names,
            vec!["lang", "new"]
        );
        db.batch(action_stmts(u, Action::Untag, &[rust], "lang")).await.unwrap();
        assert_eq!(get(&db, u, rust).await.unwrap().unwrap().tag_names, vec!["new"]);
        db.batch(action_stmts(2, Action::Delete, &[rust], "")).await.unwrap();
        assert!(get(&db, u, rust).await.unwrap().is_some(), "other users can't delete");
        db.batch(action_stmts(u, Action::Delete, &[rust], "")).await.unwrap();
        assert!(get(&db, u, rust).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn pagination_uses_profile_page_size() {
        let (db, u) = setup().await;
        db.execute(stmt!("UPDATE user_profiles SET items_per_page = 10"))
            .await
            .unwrap();
        for i in 0..12 {
            add(&db, u, &input(&format!("https://x.com/{i}"), "")).await.unwrap();
        }
        let q = |page| ListQuery {
            page: Some(page),
            ..Default::default()
        };
        let p1 = list_result(
            db.batch(list_stmts(u, Scope::Active, &q(1), None, Paging::Page(1)))
                .await
                .unwrap(),
        )
        .unwrap();
        let p2 = list_result(
            db.batch(list_stmts(u, Scope::Active, &q(2), None, Paging::Page(2)))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!((p1.bookmarks.len(), p2.bookmarks.len(), p1.total), (10, 2, 12));
    }
}
