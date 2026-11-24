use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiJson, ApiResult, Paging, field_error, not_found, ok, origin, page};
use crate::AppState;
use crate::auth::ApiUser;
use crate::domain::bookmarks::{
    self as bm, Bookmark, BookmarkInput, ListQuery, Patch, Scope, Updated, parse_tag_string, validate_title,
    validate_url,
};
use crate::domain::bundles::Bundle;
use crate::domain::{auto_tagging, profile};

/// linkding's `BookmarkSerializer` output. `origin` makes file URLs absolute, as linkding does.
pub fn to_json(b: &Bookmark, origin: &str) -> Value {
    let file = |kind, file: &str| crate::web::file_url(kind, file).map(|path| format!("{origin}{path}"));
    json!({
        "id": b.id,
        "url": b.url,
        "title": b.title,
        "description": b.description,
        "notes": b.notes,
        "web_archive_snapshot_url": b.web_archive_url(),
        "favicon_url": file("favicons", &b.favicon_file),
        "preview_image_url": file("previews", &b.preview_image_file),
        "is_archived": b.is_archived,
        "unread": b.unread,
        "shared": b.shared,
        "tag_names": b.tag_names,
        "date_added": b.date_added,
        "date_modified": b.date_modified,
        "website_title": null,       // deprecated in linkding, always null
        "website_description": null, // deprecated in linkding, always null
    })
}

/// `?flag` or `?flag=...` present in the query string.
fn has_flag(query: Option<&str>, flag: &str) -> bool {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()).any(|(k, _)| k == flag)
}

fn tags_dedup(names: Vec<String>) -> Vec<String> {
    parse_tag_string(&names.join(" "), ' ')
}

/// API clients send tags as a list. linkding joins them with ',' and re-parses, so inner
/// whitespace becomes '-' rather than splitting a tag.
fn tags(names: &[String]) -> Vec<String> {
    parse_tag_string(&names.join(","), ',')
}

// ---------------------------------------------------------------------------------------------
// List / retrieve / check

pub async fn list(s: State<AppState>, u: ApiUser, q: Query<ListQuery>, h: HeaderMap, uri: Uri) -> ApiResult {
    list_scope(s, u, q, h, uri, Scope::Active).await
}

pub async fn list_archived(s: State<AppState>, u: ApiUser, q: Query<ListQuery>, h: HeaderMap, uri: Uri) -> ApiResult {
    list_scope(s, u, q, h, uri, Scope::Archived).await
}

async fn list_scope(
    State(s): State<AppState>,
    ApiUser(user): ApiUser,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
    uri: Uri,
    scope: Scope,
) -> ApiResult {
    let bundle = match q.bundle {
        Some(id) => match crate::domain::bundles::get(&s.db, user, id).await? {
            Some(b) => Some(b),
            None => return not_found(),
        },
        None => None,
    };
    list_page(&s, user, scope, &q, bundle.as_ref(), &headers, &uri).await
}

/// Other users' shared bookmarks. Open to anonymous callers (public shares only), like
/// linkding; a bad token is still rejected rather than treated as anonymous.
pub async fn list_shared(
    State(s): State<AppState>,
    caller: Result<ApiUser, axum::response::Response>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    let viewer = match caller {
        Ok(ApiUser(id)) => Some(id),
        Err(r) if headers.contains_key(axum::http::header::AUTHORIZATION) => return Ok(r),
        Err(_) => None,
    };
    let scope = Scope::Shared {
        public_only: viewer.is_none(),
    };
    list_page(&s, viewer.unwrap_or(0), scope, &q, None, &headers, &uri).await
}

/// One page of bookmarks as paginated JSON (`limit`/`offset` from the query string).
async fn list_page(
    s: &AppState,
    user: i64,
    scope: Scope,
    q: &ListQuery,
    bundle: Option<&Bundle>,
    headers: &HeaderMap,
    uri: &Uri,
) -> ApiResult {
    let paging = Paging {
        limit: q.limit,
        offset: q.offset,
    };
    let mut stmts = bm::list_stmts(
        user,
        scope,
        q,
        bundle,
        bm::Paging::LimitOffset(paging.limit(), paging.offset()),
    );
    stmts.truncate(2); // rows + count; the tag cloud isn't part of the API
    let res = bm::list_result(s.db.batch(stmts).await?)?;
    let o = origin(headers);
    let results = res.bookmarks.iter().map(|b| to_json(b, &o)).collect();
    ok(StatusCode::OK, page(headers, uri, &paging, res.total, results))
}

pub async fn retrieve(
    State(s): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> ApiResult {
    match bm::get(&s.db, user, id).await? {
        Some(b) => ok(StatusCode::OK, to_json(&b, &origin(&headers))),
        None => not_found(),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct CheckParams {
    url: String,
}

/// Is this URL bookmarked already? Plus scraped metadata and auto tags for the "add" form.
pub async fn check(
    State(s): State<AppState>,
    ApiUser(user): ApiUser,
    Query(p): Query<CheckParams>,
    headers: HeaderMap,
) -> ApiResult {
    let (bookmark, profile, meta) = tokio::join!(
        bm::find_by_url(&s.db, user, &p.url),
        profile::get_or_guest(&s.db, user),
        s.fetcher.metadata(&p.url)
    );
    let rules = profile?.auto_tagging_rules;
    ok(
        StatusCode::OK,
        json!({
            "bookmark": bookmark?.as_ref().map(|b| to_json(b, &origin(&headers))),
            "metadata": {
                "url": p.url,
                "title": meta.title,
                "description": meta.description,
                "preview_image": meta.preview_image,
            },
            "auto_tags": auto_tagging::get_tags(&rules, &p.url),
        }),
    )
}

// ---------------------------------------------------------------------------------------------
// Create / update

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct BookmarkBody {
    url: Option<String>,
    title: Option<String>,
    description: Option<String>,
    notes: Option<String>,
    is_archived: Option<bool>,
    unread: Option<bool>,
    shared: Option<bool>,
    tag_names: Option<Vec<String>>,
}

impl BookmarkBody {
    /// Validates the fields that are present.
    fn validate(&self) -> Result<(), bm::Invalid> {
        if let Some(url) = &self.url {
            validate_url(url)?;
        }
        if let Some(title) = &self.title {
            validate_title(title)?;
        }
        Ok(())
    }
}

/// Creates a bookmark; an already bookmarked URL silently updates the existing one (linkding).
/// An empty title/description is filled from the page unless `?disable_scraping` is given.
pub async fn create(
    State(s): State<AppState>,
    ApiUser(user): ApiUser,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    ApiJson(b): ApiJson<BookmarkBody>,
) -> ApiResult {
    let Some(url) = b.url.clone() else {
        return field_error("url", "This field is required.");
    };
    if let Err((field, msg)) = b.validate() {
        return field_error(field, msg);
    }
    let mut input = BookmarkInput {
        url,
        title: b.title.unwrap_or_default(),
        description: b.description.unwrap_or_default(),
        notes: b.notes.unwrap_or_default(),
        unread: b.unread.unwrap_or(false),
        shared: b.shared.unwrap_or(false),
        is_archived: b.is_archived.unwrap_or(false),
        tag_string: String::new(),
    };
    let scrape =
        !has_flag(query.as_deref(), "disable_scraping") && (input.title.is_empty() || input.description.is_empty());
    let profile = profile::get_or_guest(&s.db, user).await?;
    if scrape {
        let meta = s.fetcher.metadata(input.url.trim()).await;
        if input.title.is_empty() {
            input.title = meta.title.unwrap_or_default();
        }
        if input.description.is_empty() {
            input.description = meta.description.unwrap_or_default();
        }
    }
    let tags = bm::with_auto_tags(
        &tags(&b.tag_names.unwrap_or_default()),
        &profile.auto_tagging_rules,
        &input.url,
    );
    let saved = bm::create(&s.db, user, &input, &tags).await?;
    s.jobs.enqueue(saved.id);
    if profile.enable_automatic_html_snapshots && !has_flag(query.as_deref(), "disable_html_snapshot") {
        s.jobs.snapshot_bookmark(&s.db, user, saved.id).await?;
    }
    ok(StatusCode::CREATED, to_json(&saved, &origin(&headers)))
}

/// PUT requires `url`; otherwise PUT and PATCH both leave omitted fields unchanged (linkding).
pub async fn put(s: State<AppState>, u: ApiUser, id: Path<i64>, h: HeaderMap, b: ApiJson<BookmarkBody>) -> ApiResult {
    update(s, u, id, h, b, true).await
}

pub async fn patch(s: State<AppState>, u: ApiUser, id: Path<i64>, h: HeaderMap, b: ApiJson<BookmarkBody>) -> ApiResult {
    update(s, u, id, h, b, false).await
}

/// Auto-tagging rules apply on every update: merged into `tag_names` if given, otherwise added
/// to the bookmark's existing tags (linkding).
async fn update(
    State(s): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    ApiJson(b): ApiJson<BookmarkBody>,
    url_required: bool,
) -> ApiResult {
    if url_required && b.url.is_none() {
        return field_error("url", "This field is required.");
    }
    if let Err((field, msg)) = b.validate() {
        return field_error(field, msg);
    }
    let rules = profile::get_or_guest(&s.db, user).await?.auto_tagging_rules;
    // Auto tags need the URL; without one in the body they're applied by the next update with it.
    let auto = b
        .url
        .as_deref()
        .map(|u| auto_tagging::get_tags(&rules, u))
        .unwrap_or_default();
    let tags = b.tag_names.as_deref().map(|names| {
        let mut all = tags(names);
        all.extend(auto.iter().cloned());
        tags_dedup(all)
    });
    let patch = Patch {
        add_tags: if tags.is_none() { auto } else { vec![] },
        url: b.url,
        title: b.title,
        description: b.description,
        notes: b.notes,
        unread: b.unread,
        shared: b.shared,
        is_archived: b.is_archived,
        tags,
    };
    match bm::update(&s.db, user, id, &patch).await? {
        Updated::Ok(saved) => {
            s.jobs.enqueue(id);
            ok(StatusCode::OK, to_json(&saved, &origin(&headers)))
        }
        Updated::NotFound => not_found(),
        Updated::DuplicateUrl => field_error("url", "A bookmark with this URL already exists."),
    }
}

pub async fn archive(s: State<AppState>, u: ApiUser, id: Path<i64>) -> ApiResult {
    set_archived(s, u, id, true).await
}

pub async fn unarchive(s: State<AppState>, u: ApiUser, id: Path<i64>) -> ApiResult {
    set_archived(s, u, id, false).await
}

async fn set_archived(
    State(s): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
    archived: bool,
) -> ApiResult {
    let patch = Patch {
        is_archived: Some(archived),
        ..Default::default()
    };
    match bm::update(&s.db, user, id, &patch).await? {
        Updated::NotFound => not_found(),
        _ => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

pub async fn delete(State(s): State<AppState>, ApiUser(user): ApiUser, Path(id): Path<i64>) -> ApiResult {
    let files = crate::domain::assets::files_of(&s.db, user, &[id]).await?;
    if bm::delete(&s.db, user, id).await? {
        s.storage.remove_all(&files).await;
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        not_found()
    }
}
