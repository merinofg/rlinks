//! RSS feeds of a user's bookmarks, addressed by the user's feed key (linkding `feeds.py`).

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::AppError;
use crate::AppState;
use crate::domain::bookmarks::{self as bm, Bookmark, ListQuery, Paging, Scope};
use crate::domain::{bundles, tokens};

struct Item {
    title: String,
    link: String,
    description: String,
    pub_date: String,
    categories: Vec<String>,
}

#[derive(Template)]
#[template(path = "feed.xml")]
struct Feed {
    title: &'static str,
    link: String,
    items: Vec<Item>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct FeedQuery {
    q: String,
    /// Shared feeds: only this user's bookmarks.
    user: String,
    bundle: Option<i64>,
    limit: Option<i64>,
}

impl FeedQuery {
    /// The bookmark list a feed shows.
    fn list_query(self, unread_only: bool) -> ListQuery {
        ListQuery {
            q: self.q,
            user: self.user,
            unread: if unread_only { "yes".into() } else { String::new() },
            limit: self.limit,
            ..Default::default()
        }
    }
}

/// XML 1.0 can't carry most control characters; drop them (linkding's `sanitize`).
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
        .collect()
}

fn item(b: &Bookmark) -> Item {
    Item {
        title: sanitize(b.resolved_title()),
        link: b.url.clone(),
        description: sanitize(&b.description),
        pub_date: chrono::DateTime::parse_from_rfc3339(&b.date_added)
            .map(|t| t.to_rfc2822())
            .unwrap_or_default(),
        categories: b.tag_names.iter().map(|t| sanitize(t)).collect(),
    }
}

pub async fn feed(
    State(s): State<AppState>,
    Path((key, kind)): Path<(String, String)>,
    Query(fq): Query<FeedQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let (title, scope) = match kind.as_str() {
        "all" => ("All bookmarks", Scope::Active),
        "unread" => ("Unread bookmarks", Scope::Active),
        "shared" => ("Shared bookmarks", Scope::Shared { public_only: false }),
        _ => return Ok(StatusCode::NOT_FOUND.into_response()),
    };
    let Some(user) = tokens::feed_user(&s.db, &key).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let bundle = match fq.bundle {
        Some(id) if scope == Scope::Active => match bundles::get(&s.db, user, id).await? {
            Some(b) => Some(b),
            None => return Ok(StatusCode::NOT_FOUND.into_response()),
        },
        _ => None,
    };
    let q = fq.list_query(kind == "unread");
    render_feed(&s, &headers, title, user, scope, bundle.as_ref(), q).await
}

/// Public feed of bookmarks shared publicly; no key needed.
pub async fn public_shared(
    State(s): State<AppState>,
    Query(fq): Query<FeedQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let scope = Scope::Shared { public_only: true };
    render_feed(
        &s,
        &headers,
        "Public shared bookmarks",
        0,
        scope,
        None,
        fq.list_query(false),
    )
    .await
}

/// The newest `q.limit` (default 100) bookmarks matching `q`, as RSS.
async fn render_feed(
    s: &AppState,
    headers: &HeaderMap,
    title: &'static str,
    user: i64,
    scope: Scope,
    bundle: Option<&bundles::Bundle>,
    q: ListQuery,
) -> Result<Response, AppError> {
    let limit = q.limit.filter(|l| *l > 0).unwrap_or(100);
    let mut stmts = bm::list_stmts(user, scope, &q, bundle, Paging::LimitOffset(limit, 0));
    stmts.truncate(1);
    let bookmarks = bm::list_result(s.db.batch(stmts).await?)?.bookmarks;
    let feed = Feed {
        title,
        link: format!("{}/bookmarks", crate::api::origin(headers)),
        items: bookmarks.iter().map(item).collect(),
    };
    Ok((
        [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
        feed.render()?,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_control_characters() {
        assert_eq!(super::sanitize("a\u{0}b\u{1b}c\td\n"), "abc\td\n");
    }
}
