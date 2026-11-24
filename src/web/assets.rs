//! Bookmark files: HTML snapshots and uploads. Only the bookmark's owner can see them.
//! Web URLs address files by a random id (`/assets/<uid>`), not the sequential row id.

use axum::extract::{Multipart, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};

use super::AppError;
use crate::domain::assets::{self, Asset};
use crate::{AppState, auth::CurrentUser};

fn details(bookmark: i64) -> Response {
    Redirect::to(&format!("/bookmarks/{bookmark}/details")).into_response()
}

pub async fn snapshot(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(bookmark): Path<i64>,
) -> Result<Response, AppError> {
    match s.jobs.snapshot_bookmark(&s.db, user, bookmark).await? {
        Some(_) => Ok(details(bookmark)),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

pub async fn upload(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(bookmark): Path<i64>,
    form: Multipart,
) -> Result<Response, AppError> {
    match store_upload(&s, user, bookmark, form).await? {
        Ok(_) => Ok(details(bookmark)),
        Err(status_and_msg) => Ok(status_and_msg.into_response()),
    }
}

/// Shared with the REST API: store the multipart `file` field and record it.
/// Returns the asset, or an error status with a message.
pub async fn store_upload(
    s: &AppState,
    user: i64,
    bookmark: i64,
    mut form: Multipart,
) -> Result<Result<Asset, (StatusCode, String)>, AppError> {
    while let Some(field) = form.next_field().await? {
        if field.name() != Some("file") {
            continue;
        }
        let name: String = field
            .file_name()
            .unwrap_or("upload")
            .chars()
            .filter(|c| !c.is_control())
            .take(255)
            .collect();
        let declared = field.content_type().unwrap_or_default().to_ascii_lowercase();
        let data = field.bytes().await?;
        if let Err(e) = assets::validate_upload(&name, data.len()) {
            return Ok(Err((StatusCode::BAD_REQUEST, e.to_string())));
        }
        let content_type = media_type(&declared, &data);
        let key = assets::new_file_key(&extension(&name));
        s.storage.put(&key, data.clone()).await?;
        return Ok(
            match assets::create_upload(&s.db, user, bookmark, &key, data.len(), &content_type, &name).await? {
                Some(asset) => Ok(asset),
                None => {
                    s.storage.remove_all(&[key]).await;
                    Err((StatusCode::NOT_FOUND, "Bookmark not found.".into()))
                }
            },
        );
    }
    Ok(Err((StatusCode::BAD_REQUEST, "No file provided.".into())))
}

/// The declared type, unless the bytes say PDF (the only type served unsandboxed, see `serve`).
fn media_type(declared: &str, data: &[u8]) -> String {
    if data.starts_with(b"%PDF-") {
        return "application/pdf".into();
    }
    let t = declared.split(';').next().unwrap_or_default().trim();
    match t {
        "application/pdf" | "" => "application/octet-stream".into(),
        t if t.chars().all(|c| c.is_ascii_alphanumeric() || "/+-.".contains(c)) => t.into(),
        _ => "application/octet-stream".into(),
    }
}

/// `.pdf` from `paper.pdf` (sanitized; empty when absent or odd).
fn extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()) => {
            format!(".{}", ext.to_ascii_lowercase())
        }
        _ => String::new(),
    }
}

pub async fn delete(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(uid): Path<String>,
) -> Result<Response, AppError> {
    let Some(asset) = assets::get_by_uid(&s.db, user, &uid).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    match assets::delete(&s.db, user, asset.id).await? {
        Some(asset) => {
            s.storage.remove_all(std::slice::from_ref(&asset.file)).await;
            Ok(details(asset.bookmark))
        }
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

pub async fn download(s: State<AppState>, u: CurrentUser, uid: Path<String>) -> Result<Response, AppError> {
    serve(s, u, uid, true).await
}

pub async fn read(s: State<AppState>, u: CurrentUser, uid: Path<String>) -> Result<Response, AppError> {
    serve(s, u, uid, false).await
}

async fn serve(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(uid): Path<String>,
    attachment: bool,
) -> Result<Response, AppError> {
    match assets::get_by_uid(&s.db, user, &uid).await? {
        Some(asset) => file_response(&s, &asset, attachment).await,
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

/// The asset's file with safe headers (404 while it isn't complete or the file is missing).
/// Shared with the REST API's download endpoint.
pub async fn file_response(s: &AppState, asset: &Asset, attachment: bool) -> Result<Response, AppError> {
    if !asset.is_readable() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let Some(data) = s.storage.get(&asset.file).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let body = if asset.gzip {
        crate::storage::gunzip(data).await?
    } else {
        data.to_vec()
    };
    Ok((asset_headers(asset, attachment), body).into_response())
}

/// Headers for serving an asset. Everything but PDFs is sandboxed: no scripts, no network,
/// an opaque origin — a snapshot or uploaded HTML file can't act on this site. (Chrome won't
/// render PDFs in a sandboxed document; PDFs are only served as such when the bytes are one.)
pub fn asset_headers(asset: &Asset, attachment: bool) -> [(header::HeaderName, String); 5] {
    let csp = if asset.content_type == "application/pdf" {
        "default-src 'none'; object-src 'self'".to_string()
    } else {
        "sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline' data:; font-src data:; media-src data:"
            .to_string()
    };
    let name = asset.download_name();
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let utf8: String = url::form_urlencoded::byte_serialize(name.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    let disposition = if attachment { "attachment" } else { "inline" };
    let content_type = if asset.content_type.starts_with("text/") {
        format!("{}; charset=utf-8", asset.content_type)
    } else {
        asset.content_type.clone()
    };
    [
        (header::CONTENT_TYPE, content_type),
        (
            header::CONTENT_DISPOSITION,
            format!("{disposition}; filename=\"{ascii}\"; filename*=UTF-8''{utf8}"),
        ),
        (header::CONTENT_SECURITY_POLICY, csp),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        (header::CACHE_CONTROL, "private, max-age=3600".into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_and_extensions() {
        assert_eq!(media_type("text/html", b"<html>"), "text/html");
        assert_eq!(
            media_type("application/pdf", b"<html>"),
            "application/octet-stream",
            "PDF only if it is one"
        );
        assert_eq!(media_type("text/plain", b"%PDF-1.7"), "application/pdf");
        assert_eq!(media_type("weird\"type", b"x"), "application/octet-stream");
        assert_eq!(extension("Paper.PDF"), ".pdf");
        assert_eq!(extension("noext"), "");
        assert_eq!(extension("x.<script>"), "");
    }
}
