//! Bookmark files over the REST API (linkding's asset endpoints and `singlefile`).

use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use serde_json::json;

use super::{ApiResult, Paging, not_found, ok, page_of};
use crate::AppState;
use crate::auth::ApiUser;
use crate::domain::assets::{self, Asset};
use crate::domain::bookmarks::{self as bm, BookmarkInput, validate_url};
use crate::domain::{auto_tagging, profile};

/// The owner's asset, and only if it belongs to the bookmark in the path.
async fn asset(s: &AppState, user: i64, bookmark: i64, id: i64) -> anyhow::Result<Option<Asset>> {
    Ok(assets::get(&s.db, user, id).await?.filter(|a| a.bookmark == bookmark))
}

fn error(status: StatusCode, msg: &str) -> ApiResult {
    ok(status, json!({ "error": msg }))
}

pub async fn list(
    State(s): State<AppState>,
    ApiUser(user): ApiUser,
    Path(bookmark): Path<i64>,
    Query(paging): Query<Paging>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    if bm::get(&s.db, user, bookmark).await?.is_none() {
        return not_found();
    }
    let all = assets::list(&s.db, user, bookmark).await?;
    ok(StatusCode::OK, page_of(&headers, &uri, &paging, &all))
}

pub async fn retrieve(State(s): State<AppState>, ApiUser(user): ApiUser, Path((b, id)): Path<(i64, i64)>) -> ApiResult {
    match asset(&s, user, b, id).await? {
        Some(a) => ok(StatusCode::OK, json!(a)),
        None => not_found(),
    }
}

pub async fn download(State(s): State<AppState>, ApiUser(user): ApiUser, Path((b, id)): Path<(i64, i64)>) -> ApiResult {
    match asset(&s, user, b, id).await? {
        Some(a) => crate::web::file_response(&s, &a, true).await,
        None => not_found(),
    }
}

pub async fn upload(
    State(s): State<AppState>,
    ApiUser(user): ApiUser,
    Path(bookmark): Path<i64>,
    form: Multipart,
) -> ApiResult {
    match crate::web::store_upload(&s, user, bookmark, form).await? {
        Ok(asset) => ok(StatusCode::CREATED, json!(asset)),
        Err((status, msg)) => error(status, &msg),
    }
}

pub async fn delete(State(s): State<AppState>, ApiUser(user): ApiUser, Path((b, id)): Path<(i64, i64)>) -> ApiResult {
    if asset(&s, user, b, id).await?.is_none() {
        return not_found();
    }
    if let Some(a) = assets::delete(&s.db, user, id).await? {
        s.storage.remove_all(&[a.file]).await;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Snapshot captured in the browser (SingleFile, via the linkding browser extension): multipart
/// `url` + `file`. Creates the bookmark if needed (metadata scraped, no server-side snapshot).
pub async fn singlefile(State(s): State<AppState>, ApiUser(user): ApiUser, mut form: Multipart) -> ApiResult {
    let (mut url, mut html) = (None, None);
    while let Some(field) = form.next_field().await? {
        match field.name() {
            Some("url") => url = Some(field.text().await?.trim().to_string()),
            Some("file") => html = Some(field.bytes().await?),
            _ => {}
        }
    }
    let (Some(url), Some(html)) = (url.filter(|u| !u.is_empty()), html.filter(|h| !h.is_empty())) else {
        return error(
            StatusCode::BAD_REQUEST,
            "Both 'url' and 'file' parameters are required.",
        );
    };
    if let Err((_, msg)) = validate_url(&url) {
        return error(StatusCode::BAD_REQUEST, msg);
    }
    if html.len() > assets::MAX_UPLOAD_BYTES {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "The file is too large.");
    }
    let bookmark = match bm::find_by_url(&s.db, user, &url).await? {
        Some(b) => b.id,
        None => {
            let (p, meta) = tokio::join!(profile::get_or_guest(&s.db, user), s.fetcher.metadata(&url));
            let rules = p?.auto_tagging_rules;
            let input = BookmarkInput {
                url: url.clone(),
                title: meta.title.unwrap_or_default(),
                description: meta.description.unwrap_or_default(),
                ..Default::default()
            };
            let saved = bm::create(&s.db, user, &input, &auto_tagging::get_tags(&rules, &url)).await?;
            s.jobs.enqueue(saved.id);
            saved.id
        }
    };
    let gz = crate::storage::gzip(html.to_vec()).await?;
    let key = assets::new_file_key(".html.gz");
    let size = gz.len();
    s.storage.put(&key, gz.into()).await?;
    if assets::add_snapshot(&s.db, user, bookmark, &key, size).await?.is_none() {
        s.storage.remove_all(&[key]).await;
        return not_found();
    }
    ok(
        StatusCode::CREATED,
        json!({ "message": "Snapshot uploaded successfully." }),
    )
}
