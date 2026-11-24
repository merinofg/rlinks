mod admin;
mod assets;
mod auth;
mod bookmarks;
mod bundles;
mod feeds;
mod settings;
mod tags;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::AppState;

use crate::auth::{CurrentUser, csrf_guard};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/static/{file}", get(static_file))
        .route("/manifest.json", get(manifest))
        .route("/opensearch.xml", get(opensearch))
        .route("/login", get(auth::login_page).post(auth::login))
        .route("/logout", post(auth::logout))
        .route("/bookmarks", get(bookmarks::index))
        .route("/bookmarks/action", post(bookmarks::action))
        .route("/bookmarks/archived", get(bookmarks::archived))
        .route("/bookmarks/shared", get(bookmarks::shared))
        .route("/bookmarks/archived/action", post(bookmarks::archived_action))
        .route("/bookmarks/new", get(bookmarks::new_page).post(bookmarks::create))
        .route(
            "/bookmarks/{id}/edit",
            get(bookmarks::edit_page).post(bookmarks::update),
        )
        .route("/bookmarks/{id}/details", get(bookmarks::details))
        .route("/bookmarks/{id}/assets/snapshot", post(assets::snapshot))
        .route(
            "/bookmarks/{id}/assets/upload",
            post(assets::upload).layer(axum::extract::DefaultBodyLimit::max(
                crate::domain::assets::MAX_UPLOAD_BYTES + 64 * 1024,
            )),
        )
        .route("/assets/{uid}", get(assets::download))
        .route("/assets/{uid}/read", get(assets::read))
        .route("/assets/{uid}/delete", post(assets::delete))
        .route("/bookmarks/new/check", get(bookmarks::check))
        .route("/files/{kind}/{name}", get(file))
        .route("/bundles", get(bundles::index))
        .route("/bundles/action", post(bundles::action))
        .route("/bundles/new", get(bundles::new_page).post(bundles::create))
        .route("/bundles/{id}/edit", get(bundles::edit_page).post(bundles::update))
        .route("/feeds/shared", get(feeds::public_shared))
        .route("/feeds/{key}/{kind}", get(feeds::feed))
        .route("/tags", get(tags::index))
        .route("/tags/action", post(tags::action))
        .route("/settings", get(|| async { Redirect::to("/settings/general") }))
        .route("/settings/general", get(settings::general).post(settings::save_general))
        .route("/settings/data", get(settings::data))
        .route(
            "/settings/account",
            get(settings::account).post(settings::change_password),
        )
        .route("/admin/users", get(admin::index))
        .route("/admin/users/action", post(admin::action))
        .route("/custom_css", get(settings::custom_css))
        .route(
            "/settings/import",
            post(settings::import).layer(axum::extract::DefaultBodyLimit::max(settings::MAX_IMPORT_BYTES)),
        )
        .route("/settings/export", get(settings::export))
        .route("/settings/integrations", get(settings::integrations))
        .route("/settings/integrations/create-api-token", post(settings::create_token))
        .route("/settings/integrations/delete-api-token", post(settings::delete_token))
        .route("/settings/integrations/reset-feed-key", post(settings::reset_feed_key))
        .merge(crate::api::router())
        .layer(middleware::from_fn(csrf_guard))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Logged in: your bookmarks. Otherwise the site's landing page (login or public shared bookmarks).
async fn root(State(s): State<AppState>, user: Result<CurrentUser, Response>) -> Result<Redirect, AppError> {
    if user.is_ok() {
        return Ok(Redirect::to("/bookmarks"));
    }
    Ok(match crate::domain::site::landing_page(&s.db).await?.as_str() {
        "shared_bookmarks" => Redirect::to("/bookmarks/shared"),
        _ => Redirect::to("/login"),
    })
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "status": "healthy" }))
}

/// Assets are compiled into the binary. Vendored files carry their version in the name and are
/// cached forever; our own files change between releases, so browsers revalidate them.
async fn static_file(Path(file): Path<String>) -> Response {
    let (body, mime): (&'static [u8], &str) = match file.as_str() {
        "htmx-2.0.4.min.js" => {
            let body = include_bytes!("../../static/htmx-2.0.4.min.js");
            let cache = "public, max-age=31536000, immutable";
            return (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, cache),
                ],
                &body[..],
            )
                .into_response();
        }
        "app.js" => (include_bytes!("../../static/app.js"), "text/javascript"),
        "app.css" => (include_bytes!("../../static/app.css"), "text/css"),
        "icon.svg" => (include_bytes!("../../static/icon.svg"), "image/svg+xml"),
        "icon-192.png" => (include_bytes!("../../static/icon-192.png"), "image/png"),
        "icon-512.png" => (include_bytes!("../../static/icon-512.png"), "image/png"),
        "icon-maskable-512.png" => (include_bytes!("../../static/icon-maskable-512.png"), "image/png"),
        "apple-touch-icon.png" => (include_bytes!("../../static/apple-touch-icon.png"), "image/png"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-cache")],
        body,
    )
        .into_response()
}

/// Web app manifest: installable as an app (PWA) on desktop and mobile.
async fn manifest() -> Response {
    let icon = |src: &str, sizes: &str, kind: &str, purpose: &str| serde_json::json!({ "src": src, "sizes": sizes, "type": kind, "purpose": purpose });
    let body = serde_json::json!({
        "name": "rlinks",
        "short_name": "rlinks",
        "description": "Self-hosted bookmark manager",
        "start_url": "/bookmarks",
        "scope": "/",
        "display": "standalone",
        "background_color": "#ffffff",
        "theme_color": "#5755d9",
        "icons": [
            icon("/static/icon.svg", "any", "image/svg+xml", "any"),
            icon("/static/icon-192.png", "192x192", "image/png", "any"),
            icon("/static/icon-512.png", "512x512", "image/png", "any"),
            icon("/static/icon-maskable-512.png", "512x512", "image/png", "maskable"),
        ],
    });
    ([(header::CONTENT_TYPE, "application/manifest+json")], body.to_string()).into_response()
}

#[derive(Template)]
#[template(path = "opensearch.xml")]
struct OpenSearch {
    /// From the request's Host header; the template escapes it.
    origin: String,
}

/// OpenSearch description: lets browsers add rlinks as a search engine for the address bar.
async fn opensearch(headers: HeaderMap) -> Result<Response, AppError> {
    let body = OpenSearch {
        origin: crate::api::origin(&headers),
    }
    .render()?;
    Ok(([(header::CONTENT_TYPE, "application/opensearchdescription+xml")], body).into_response())
}

/// Stored favicons and preview images. Public, like linkding's static files (names are derived
/// from site/URL hashes). Served with a sandboxing CSP so an SVG can't run script on our origin.
async fn file(State(s): State<AppState>, Path((kind, name)): Path<(String, String)>) -> Result<Response, AppError> {
    let valid_name = !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
    if !matches!(kind.as_str(), "favicons" | "previews") || !valid_name {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let Some(data) = s.storage.get(&format!("{kind}/{name}")).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let mime = crate::storage::sniff_image(&data);
    let headers = [
        (header::CONTENT_TYPE, mime),
        (header::CACHE_CONTROL, "public, max-age=86400"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'unsafe-inline'; sandbox",
        ),
    ];
    Ok((headers, data).into_response())
}

/// Path of a stored favicon/preview (`/files/<kind>/<file>`), if the bookmark has one.
pub fn file_url(kind: &str, file: &str) -> Option<String> {
    (!file.is_empty() && file != crate::jobs::NO_PREVIEW).then(|| format!("/files/{kind}/{file}"))
}

/// "1.2 MB" for a byte count.
pub fn human_size(bytes: i64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

pub use assets::{file_response, store_upload};

pub fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

/// Markdown (bookmark notes) to sanitized HTML.
pub fn markdown(md: &str) -> String {
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, pulldown_cmark::Parser::new(md));
    ammonia::clean(&html)
}

/// "3 days ago" for an RFC 3339 timestamp.
pub fn ago(iso: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return iso.to_string();
    };
    let secs = (chrono::Utc::now() - t.to_utc()).num_seconds().max(0);
    let (n, unit) = match secs {
        0..60 => return "just now".into(),
        60..3600 => (secs / 60, "minute"),
        3600..86400 => (secs / 3600, "hour"),
        86400..2_592_000 => (secs / 86400, "day"),
        2_592_000..31_536_000 => (secs / 2_592_000, "month"),
        _ => (secs / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

pub fn render(t: &impl Template) -> Result<Html<String>, AppError> {
    Ok(Html(t.render()?))
}

/// Any internal error becomes a logged 500 without leaking details to the client.
pub struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!("{:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_is_sanitized() {
        let html = markdown("**hi** <script>alert(1)</script> [x](javascript:alert(1))");
        assert!(html.contains("<strong>hi</strong>"));
        assert!(!html.contains("<script") && !html.contains("javascript:"));
    }

    #[test]
    fn relative_dates() {
        let at = |secs: i64| (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339();
        assert_eq!(ago(&at(5)), "just now");
        assert_eq!(ago(&at(60)), "1 minute ago");
        assert_eq!(ago(&at(3 * 86400)), "3 days ago");
        assert_eq!(ago("garbage"), "garbage");
    }
}
