//! Netscape bookmark files (what browsers export), in linkding's dialect: `TAGS` (comma
//! separated), `TOREAD`, `PRIVATE`, notes inside `[linkding-notes]…[/linkding-notes]` in the
//! description, archived bookmarks tagged `linkding:bookmarks.archived`.
//! Port of linkding's `services/exporter.py`, `parser.py` and `importer.py`.

use anyhow::Result;
use chrono::{DateTime, Utc};
use scraper::{ElementRef, Html, Selector};
use serde_json::json;
use std::collections::HashSet;

use crate::db::{Db, format_time, now};
use crate::domain::bookmarks::{self as bm, Bookmark, normalize_url, parse_tag_string, validate_url};
use crate::domain::tags;
use crate::stmt;

const ARCHIVED_TAG: &str = "linkding:bookmarks.archived";
const NOTES_START: &str = "[linkding-notes]";
const NOTES_END: &str = "[/linkding-notes]";

// ---------------------------------------------------------------------------------------------
// Export

pub fn export(bookmarks: &[Bookmark]) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let mut doc = vec![
        "<!DOCTYPE NETSCAPE-Bookmark-file-1>".to_string(),
        r#"<META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=UTF-8">"#.into(),
        "<TITLE>Bookmarks</TITLE>".into(),
        "<H1>Bookmarks</H1>".into(),
        "<DL><p>".into(),
    ];
    for b in bookmarks {
        let mut tags: Vec<String> = b.tag_names.iter().map(|t| esc(t)).collect();
        if b.is_archived {
            tags.push(ARCHIVED_TAG.into());
        }
        let epoch = |t: &str| DateTime::parse_from_rfc3339(t).map(|t| t.timestamp()).unwrap_or(0);
        doc.push(format!(
            r#"<DT><A HREF="{}" ADD_DATE="{}" LAST_MODIFIED="{}" PRIVATE="{}" TOREAD="{}" TAGS="{}">{}</A>"#,
            esc(&b.url),
            epoch(&b.date_added),
            epoch(&b.date_modified),
            if b.shared { 0 } else { 1 },
            if b.unread { 1 } else { 0 },
            tags.join(","),
            esc(b.resolved_title()),
        ));
        let mut desc = esc(&b.description);
        if !b.notes.is_empty() {
            desc += &format!("{NOTES_START}{}{NOTES_END}", esc(&b.notes));
        }
        if !desc.is_empty() {
            doc.push(format!("<DD>{desc}"));
        }
    }
    doc.push("</DL><p>".into());
    doc.join("\n")
}

/// Every bookmark of the owner (archived included), oldest first.
pub async fn load_all(db: &Db, owner: i64) -> Result<Vec<Bookmark>> {
    const PAGE: i64 = 1000;
    let q = bm::ListQuery {
        sort: "added_asc".into(),
        ..Default::default()
    };
    let mut all = Vec::new();
    for scope in [bm::Scope::Active, bm::Scope::Archived] {
        for offset in (0..).step_by(PAGE as usize) {
            let mut stmts = bm::list_stmts(owner, scope, &q, None, bm::Paging::LimitOffset(PAGE, offset));
            stmts.truncate(1);
            let page = bm::list_result(db.batch(stmts).await?)?.bookmarks;
            let done = (page.len() as i64) < PAGE;
            all.extend(page);
            if done {
                break;
            }
        }
    }
    Ok(all)
}

// ---------------------------------------------------------------------------------------------
// Parse

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Entry {
    pub url: String,
    pub title: String,
    pub description: String,
    pub notes: String,
    pub added: Option<DateTime<Utc>>,
    pub modified: Option<DateTime<Utc>>,
    pub tags: Vec<String>,
    pub unread: bool,
    pub private: bool,
    pub archived: bool,
}

/// Bookmarks of a Netscape file, folders flattened. Browsers write `<DT><A …>title</A>` with an
/// optional `<DD>description` right after.
pub fn parse(html: &str) -> Vec<Entry> {
    let doc = Html::parse_document(html);
    let links = Selector::parse("dt > a[href]").expect("valid selector");
    doc.select(&links)
        .map(|a| {
            let attr = |name: &str| a.value().attr(name).unwrap_or_default();
            let mut tags = parse_tag_string(attr("tags"), ',');
            let archived = tags.iter().any(|t| t == ARCHIVED_TAG);
            tags.retain(|t| t != ARCHIVED_TAG);
            let dd = a
                .parent()
                .and_then(ElementRef::wrap)
                .and_then(|dt| dt.next_siblings().filter_map(ElementRef::wrap).next())
                .filter(|e| e.value().name() == "dd")
                .map(|dd| dd.text().collect::<String>().trim().to_string())
                .unwrap_or_default();
            let (description, notes) = match dd.split_once(NOTES_START) {
                Some((d, rest)) => (
                    d.to_string(),
                    rest.split(NOTES_END).next().unwrap_or_default().to_string(),
                ),
                None => (dd, String::new()),
            };
            Entry {
                url: attr("href").trim().to_string(),
                title: a.text().collect::<String>().trim().to_string(),
                description,
                notes,
                added: timestamp(attr("add_date")),
                modified: timestamp(attr("last_modified")),
                tags,
                unread: attr("toread") == "1",
                private: attr("private") != "0",
                archived,
            }
        })
        .collect()
}

/// Seconds since the epoch; values too large for seconds are taken as ms, µs, then ns (linkding).
fn timestamp(value: &str) -> Option<DateTime<Utc>> {
    let mut n: i64 = value.trim().parse().ok()?;
    while n.abs() > 100_000_000_000 {
        n /= 1000;
    }
    DateTime::from_timestamp(n, 0)
}

// ---------------------------------------------------------------------------------------------
// Import

#[derive(Debug, Default, PartialEq)]
pub struct ImportResult {
    pub total: usize,
    pub success: usize,
    pub failed: usize,
}

const CHUNK: usize = 200;

/// Import a Netscape file: new URLs are created, already bookmarked ones (by normalized URL)
/// updated, tags created and assigned. `map_private_flag`: share bookmarks the file marks public.
pub async fn import(db: &Db, owner: i64, html: &str, map_private_flag: bool) -> Result<ImportResult> {
    let entries = parse(html);
    let mut result = ImportResult {
        total: entries.len(),
        ..Default::default()
    };
    let mut seen = HashSet::new();
    let now = now();
    let mut rows = Vec::new();
    for e in entries {
        let norm = normalize_url(&e.url);
        // Invalid or script URLs (bookmarklets) and duplicates within the file are skipped.
        if validate_url(&e.url).is_err() || !seen.insert(norm.clone()) {
            result.failed += 1;
            continue;
        }
        let added = e.added.map(format_time).unwrap_or_else(|| now.clone());
        let tags: Vec<String> = e
            .tags
            .into_iter()
            .filter(|t| t.chars().count() <= tags::MAX_NAME_LEN)
            .collect();
        rows.push(json!({
            "url": e.url, "norm": norm, "title": e.title, "description": e.description, "notes": e.notes,
            "modified": e.modified.map(format_time).unwrap_or_else(|| added.clone()), "added": added,
            "unread": e.unread, "shared": map_private_flag && !e.private, "archived": e.archived, "tags": tags,
        }));
        result.success += 1;
    }
    for chunk in rows.chunks(CHUNK) {
        db.batch(import_stmts(owner, &json!(chunk).to_string(), map_private_flag, &now))
            .await?;
    }
    Ok(result)
}

/// One atomic batch per chunk; `data` is a JSON array of the rows built in `import`.
fn import_stmts(owner: i64, data: &str, map_private_flag: bool, now: &str) -> Vec<crate::db::Stmt> {
    let f = |field: &str| format!("json_extract(j.value, '$.{field}')");
    vec![
        stmt!(
            "INSERT INTO tags (name, date_added, owner_id) \
             SELECT DISTINCT jt.value, ?, ? FROM json_each(?) j, json_each(json_extract(j.value, '$.tags')) jt WHERE true \
             ON CONFLICT (owner_id, name) DO NOTHING",
            now,
            owner,
            data
        ),
        // Existing bookmarks: take the file's values; empty texts don't erase ours (linkding).
        stmt!(
            format!(
                "UPDATE bookmarks SET url = {url}, url_normalized = {norm}, date_added = {added}, \
                 date_modified = {modified}, unread = {unread}, \
                 shared = CASE WHEN ? THEN {shared} ELSE shared END, \
                 title = CASE WHEN {title} != '' THEN {title} ELSE title END, \
                 description = CASE WHEN {description} != '' THEN {description} ELSE description END, \
                 notes = CASE WHEN {notes} != '' THEN {notes} ELSE notes END, \
                 is_archived = CASE WHEN {archived} THEN 1 ELSE is_archived END \
                 FROM json_each(?) j WHERE bookmarks.owner_id = ? AND bookmarks.url_normalized = {norm}",
                url = f("url"),
                norm = f("norm"),
                added = f("added"),
                modified = f("modified"),
                unread = f("unread"),
                shared = f("shared"),
                title = f("title"),
                description = f("description"),
                notes = f("notes"),
                archived = f("archived"),
            ),
            map_private_flag,
            data,
            owner
        ),
        stmt!(
            format!(
                "INSERT INTO bookmarks (url, url_normalized, title, description, notes, unread, shared, is_archived, \
                 date_added, date_modified, owner_id) \
                 SELECT {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, ? FROM json_each(?) j \
                 WHERE NOT EXISTS (SELECT 1 FROM bookmarks b WHERE b.owner_id = ? AND b.url_normalized = {})",
                f("url"),
                f("norm"),
                f("title"),
                f("description"),
                f("notes"),
                f("unread"),
                f("shared"),
                f("archived"),
                f("added"),
                f("modified"),
                f("norm"),
            ),
            owner,
            data,
            owner
        ),
        stmt!(
            format!(
                "INSERT INTO bookmark_tags (bookmark_id, tag_id) SELECT b.id, t.id FROM json_each(?) j \
                 JOIN bookmarks b ON b.owner_id = ? AND b.url_normalized = {} \
                 JOIN json_each(json_extract(j.value, '$.tags')) jt \
                 JOIN tags t ON t.owner_id = ? AND t.name = jt.value WHERE true ON CONFLICT DO NOTHING",
                f("norm")
            ),
            data,
            owner,
            owner
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::users;

    const FIREFOX: &str = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=UTF-8">
<TITLE>Bookmarks</TITLE>
<H1>Bookmarks Menu</H1>
<DL><p>
    <DT><H3 ADD_DATE="1600000000">Folder</H3>
    <DL><p>
        <DT><A HREF="https://rust-lang.org/" ADD_DATE="1600000000" LAST_MODIFIED="1600000100" TAGS="rust,lang">Rust</A>
        <DD>A language &amp; more
        <DT><A HREF="javascript:alert(1)">Bookmarklet</A>
    </DL><p>
    <DT><A HREF="https://htmx.org" ADD_DATE="1700000000000" TOREAD="1" PRIVATE="0" TAGS="web,linkding:bookmarks.archived">htmx</A>
    <DD>desc[linkding-notes]**notes** here[/linkding-notes]
    <DT><A HREF="https://rust-lang.org">Duplicate</A>
</DL><p>"#;

    #[test]
    fn parse_browser_and_linkding_files() {
        let e = parse(FIREFOX);
        assert_eq!(e.len(), 4);
        assert_eq!(
            (e[0].url.as_str(), e[0].title.as_str(), e[0].description.as_str()),
            ("https://rust-lang.org/", "Rust", "A language & more")
        );
        assert_eq!(e[0].tags, ["lang", "rust"]);
        assert_eq!(e[0].added.unwrap().timestamp(), 1_600_000_000);
        assert!(e[0].private && !e[0].unread && !e[0].archived);
        assert_eq!(
            (e[2].description.as_str(), e[2].notes.as_str()),
            ("desc", "**notes** here")
        );
        assert_eq!(e[2].added.unwrap().timestamp(), 1_700_000_000, "milliseconds");
        assert!(e[2].unread && !e[2].private && e[2].archived);
        assert_eq!(e[2].tags, ["web"]);
        assert_eq!(e[3].description, "", "no DD");
    }

    #[tokio::test]
    async fn import_then_export_round_trips() {
        let db = Db::sqlite(":memory:").unwrap();
        db.migrate().await.unwrap();
        users::create(&db, "u", "pw", false).await.unwrap();
        // an existing bookmark gets updated, not duplicated
        let existing = bm::BookmarkInput {
            url: "https://htmx.org/".into(),
            title: "old".into(),
            ..Default::default()
        };
        bm::create(&db, 1, &existing, &["mine".into()]).await.unwrap();

        let res = import(&db, 1, FIREFOX, true).await.unwrap();
        assert_eq!(
            res,
            ImportResult {
                total: 4,
                success: 2,
                failed: 2
            },
            "bookmarklet and duplicate fail"
        );

        let all = load_all(&db, 1).await.unwrap();
        assert_eq!(all.len(), 2);
        let htmx = all.iter().find(|b| b.url == "https://htmx.org").unwrap();
        assert_eq!((htmx.title.as_str(), htmx.notes.as_str()), ("htmx", "**notes** here"));
        assert!(
            htmx.unread && htmx.shared && htmx.is_archived,
            "flags imported, public mapped to shared"
        );
        assert_eq!(htmx.tag_names, ["mine", "web"], "existing tags kept, new added");
        assert!(htmx.date_added.starts_with("2023-11-14"));

        // round trip: export, wipe, import again
        let file = export(&all);
        assert!(file.contains(r#"TAGS="mine,web,linkding:bookmarks.archived""#) && file.contains("[linkding-notes]"));
        db.execute(stmt!("DELETE FROM bookmarks")).await.unwrap();
        let res = import(&db, 1, &file, true).await.unwrap();
        assert_eq!((res.success, res.failed), (2, 0));
        let again = load_all(&db, 1).await.unwrap();
        let strip = |v: &[Bookmark]| {
            let mut v: Vec<_> = v
                .iter()
                .map(|b| {
                    (
                        b.url.clone(),
                        b.title.clone(),
                        b.description.clone(),
                        b.notes.clone(),
                        b.tag_names.clone(),
                        b.unread,
                        b.shared,
                        b.is_archived,
                    )
                })
                .collect();
            v.sort();
            v
        };
        assert_eq!(strip(&again), strip(&all));
    }
}
