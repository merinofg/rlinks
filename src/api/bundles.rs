use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use serde_json::json;

use super::{ApiJson, ApiResult, Paging, field_error, not_found, ok, page_of};
use crate::AppState;
use crate::auth::ApiUser;
use crate::domain::bundles::{self, BundleInput};

/// linkding paginates bundles like everything else (ordered by `order`).
pub async fn list(
    State(s): State<AppState>,
    ApiUser(user): ApiUser,
    Query(paging): Query<Paging>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult {
    let all = bundles::list(&s.db, user).await?;
    ok(StatusCode::OK, page_of(&headers, &uri, &paging, &all))
}

pub async fn retrieve(State(s): State<AppState>, ApiUser(user): ApiUser, Path(id): Path<i64>) -> ApiResult {
    match bundles::get(&s.db, user, id).await? {
        Some(b) => ok(StatusCode::OK, json!(b)),
        None => not_found(),
    }
}

pub async fn create(State(s): State<AppState>, ApiUser(user): ApiUser, ApiJson(b): ApiJson<BundleInput>) -> ApiResult {
    if b.name.is_none() {
        return field_error("name", "This field is required.");
    }
    if let Err((field, msg)) = b.validate() {
        return field_error(field, msg);
    }
    ok(StatusCode::CREATED, json!(bundles::create(&s.db, user, &b).await?))
}

/// PUT requires `name`; otherwise PUT and PATCH leave omitted fields unchanged.
pub async fn put(s: State<AppState>, u: ApiUser, id: Path<i64>, b: ApiJson<BundleInput>) -> ApiResult {
    update(s, u, id, b, true).await
}

pub async fn patch(s: State<AppState>, u: ApiUser, id: Path<i64>, b: ApiJson<BundleInput>) -> ApiResult {
    update(s, u, id, b, false).await
}

async fn update(
    State(s): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
    ApiJson(b): ApiJson<BundleInput>,
    name_required: bool,
) -> ApiResult {
    if name_required && b.name.is_none() {
        return field_error("name", "This field is required.");
    }
    if let Err((field, msg)) = b.validate() {
        return field_error(field, msg);
    }
    match bundles::update(&s.db, user, id, &b).await? {
        Some(saved) => ok(StatusCode::OK, json!(saved)),
        None => not_found(),
    }
}

pub async fn delete(State(s): State<AppState>, ApiUser(user): ApiUser, Path(id): Path<i64>) -> ApiResult {
    if bundles::delete(&s.db, user, id).await? {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        not_found()
    }
}
