//! REST API, wire-compatible with linkding's (`docs/api.md` upstream), so the official browser
//! extensions and third-party apps work unchanged: same paths (with trailing slashes),
//! `Authorization: Token <key>`, the same JSON fields, DRF-style errors and pagination.

mod assets;
mod bookmarks;
mod bundles;
mod tags;

use axum::extract::State;
use axum::extract::{FromRequest, Request};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::ApiUser;
use crate::domain::profile;
use crate::web::AppError;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/bookmarks/", get(bookmarks::list).post(bookmarks::create))
        .route("/api/bookmarks/archived/", get(bookmarks::list_archived))
        .route("/api/bookmarks/check/", get(bookmarks::check))
        .route("/api/bookmarks/shared/", get(bookmarks::list_shared))
        .route(
            "/api/bookmarks/singlefile/",
            post(assets::singlefile).layer(upload_limit()),
        )
        .route("/api/bookmarks/{id}/assets/", get(assets::list))
        .route(
            "/api/bookmarks/{id}/assets/upload/",
            post(assets::upload).layer(upload_limit()),
        )
        .route(
            "/api/bookmarks/{id}/assets/{asset}/",
            get(assets::retrieve).delete(assets::delete),
        )
        .route("/api/bookmarks/{id}/assets/{asset}/download/", get(assets::download))
        .route(
            "/api/bookmarks/{id}/",
            get(bookmarks::retrieve)
                .put(bookmarks::put)
                .patch(bookmarks::patch)
                .delete(bookmarks::delete),
        )
        .route("/api/bookmarks/{id}/archive/", post(bookmarks::archive))
        .route("/api/bookmarks/{id}/unarchive/", post(bookmarks::unarchive))
        .route("/api/tags/", get(tags::list).post(tags::create))
        .route("/api/tags/{id}/", get(tags::retrieve))
        .route("/api/bundles/", get(bundles::list).post(bundles::create))
        .route(
            "/api/bundles/{id}/",
            get(bundles::retrieve)
                .put(bundles::put)
                .patch(bundles::patch)
                .delete(bundles::delete),
        )
        .route("/api/user/profile/", get(user_profile))
}

fn upload_limit() -> axum::extract::DefaultBodyLimit {
    axum::extract::DefaultBodyLimit::max(crate::domain::assets::MAX_UPLOAD_BYTES + 64 * 1024)
}

// ---------------------------------------------------------------------------------------------
// Responses

type ApiResult = Result<Response, AppError>;

fn ok(status: StatusCode, body: Value) -> ApiResult {
    Ok((status, Json(body)).into_response())
}

fn not_found() -> ApiResult {
    ok(StatusCode::NOT_FOUND, json!({ "detail": "Not found." }))
}

/// DRF field error: `{"url": ["This field is required."]}`.
fn field_error(field: &str, msg: &str) -> ApiResult {
    ok(StatusCode::BAD_REQUEST, json!({ field: [msg] }))
}

/// A JSON request body. A malformed one is rejected the way DRF reports it:
/// 400 `{"detail": "..."}`.
pub struct ApiJson<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for ApiJson<T> {
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Response> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(t)) => Ok(ApiJson(t)),
            Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "detail": e.body_text() }))).into_response()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Pagination (DRF LimitOffsetPagination)

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Paging {
    limit: Option<i64>,
    offset: Option<i64>,
}

const DEFAULT_LIMIT: i64 = 100;

impl Paging {
    fn limit(&self) -> i64 {
        self.limit.filter(|l| *l > 0).unwrap_or(DEFAULT_LIMIT)
    }
    fn offset(&self) -> i64 {
        self.offset.unwrap_or(0).max(0)
    }
}

/// `{count, next, previous, results}` with absolute next/previous links built from the request.
fn page(headers: &HeaderMap, uri: &Uri, paging: &Paging, count: i64, results: Vec<Value>) -> Value {
    let (limit, offset) = (paging.limit(), paging.offset());
    let link = |offset: Option<i64>| {
        let mut qs = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
            if k != "limit" && k != "offset" {
                qs.append_pair(&k, &v);
            }
        }
        qs.append_pair("limit", &limit.to_string());
        if let Some(o) = offset {
            qs.append_pair("offset", &o.to_string());
        }
        format!("{}{}?{}", origin(headers), uri.path(), qs.finish())
    };
    let next = (offset + limit < count).then(|| link(Some(offset + limit)));
    let previous = (offset > 0).then(|| link((offset - limit > 0).then_some(offset - limit)));
    json!({ "count": count, "next": next, "previous": previous, "results": results })
}

/// A page of a list loaded in full.
// ponytail: only for short lists (a user's bundles, a bookmark's assets); page in SQL otherwise.
fn page_of<T: Serialize>(headers: &HeaderMap, uri: &Uri, paging: &Paging, all: &[T]) -> Value {
    let results = all
        .iter()
        .skip(paging.offset() as usize)
        .take(paging.limit() as usize)
        .map(|t| json!(t))
        .collect();
    page(headers, uri, paging, all.len() as i64, results)
}

/// `scheme://host` of this request, honouring a reverse proxy's X-Forwarded-Proto/Host.
pub fn origin(headers: &HeaderMap) -> String {
    let h = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let scheme = h("x-forwarded-proto").unwrap_or("http");
    let host = h("x-forwarded-host").or(h("host")).unwrap_or("localhost");
    format!("{scheme}://{host}")
}

// ---------------------------------------------------------------------------------------------
// User

async fn user_profile(State(s): State<AppState>, ApiUser(user): ApiUser) -> ApiResult {
    let Some(p) = profile::get(&s.db, user).await? else {
        return not_found();
    };
    let prefs: Value = serde_json::from_str(&p.search_preferences).unwrap_or(json!({}));
    ok(
        StatusCode::OK,
        json!({
            "theme": p.theme,
            "bookmark_date_display": p.bookmark_date_display,
            "bookmark_link_target": p.bookmark_link_target,
            "web_archive_integration": p.web_archive_integration,
            "tag_search": p.tag_search,
            "enable_sharing": p.enable_sharing,
            "enable_public_sharing": p.enable_public_sharing,
            "enable_favicons": p.enable_favicons,
            "display_url": p.display_url,
            "permanent_notes": p.permanent_notes,
            "search_preferences": prefs,
            "version": env!("CARGO_PKG_VERSION"),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_links() {
        let mut h = HeaderMap::new();
        h.insert("host", "ld.example".parse().unwrap());
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        let uri: Uri = "/api/bookmarks/?q=%23a&limit=10&offset=10".parse().unwrap();
        let p = Paging {
            limit: Some(10),
            offset: Some(10),
        };
        let v = page(&h, &uri, &p, 25, vec![]);
        assert_eq!(v["next"], "https://ld.example/api/bookmarks/?q=%23a&limit=10&offset=20");
        assert_eq!(v["previous"], "https://ld.example/api/bookmarks/?q=%23a&limit=10");
        let last = Paging {
            limit: Some(10),
            offset: Some(20),
        };
        assert_eq!(page(&h, &uri, &last, 25, vec![])["next"], Value::Null);
        assert_eq!(page(&h, &uri, &Paging::default(), 5, vec![])["previous"], Value::Null);
        assert_eq!(
            Paging {
                limit: Some(0),
                offset: Some(-3)
            }
            .limit(),
            DEFAULT_LIMIT
        );
    }
}
