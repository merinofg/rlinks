use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use serde::Deserialize;
use serde_json::json;

use super::{ApiJson, ApiResult, Paging, field_error, not_found, ok, page};
use crate::AppState;
use crate::auth::ApiUser;
use crate::domain::tags;

pub async fn list(
    State(s): State<AppState>,
    ApiUser(user): ApiUser,
    Query(paging): Query<Paging>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    let (tags, total) = tags::list(&s.db, user, paging.limit(), paging.offset()).await?;
    let results = tags.iter().map(|t| json!(t)).collect();
    ok(StatusCode::OK, page(&headers, &uri, &paging, total, results))
}

pub async fn retrieve(State(s): State<AppState>, ApiUser(user): ApiUser, Path(id): Path<i64>) -> ApiResult {
    match tags::get(&s.db, user, id).await? {
        Some(t) => ok(StatusCode::OK, json!(t)),
        None => not_found(),
    }
}

#[derive(Deserialize)]
pub struct TagBody {
    name: Option<String>,
}

/// Returns the existing tag when one with that name (case-insensitive) exists, like linkding.
pub async fn create(State(s): State<AppState>, ApiUser(user): ApiUser, ApiJson(b): ApiJson<TagBody>) -> ApiResult {
    let name = match b.name.as_deref().map(str::trim) {
        None => return field_error("name", "This field is required."),
        Some("") => return field_error("name", "This field may not be blank."),
        Some(n) if n.chars().count() > tags::MAX_NAME_LEN => {
            let msg = format!("Ensure this field has no more than {} characters.", tags::MAX_NAME_LEN);
            return field_error("name", &msg);
        }
        Some(n) => n,
    };
    ok(
        StatusCode::CREATED,
        json!(tags::get_or_create(&s.db, user, name).await?),
    )
}
