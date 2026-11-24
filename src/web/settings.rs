use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use serde::Deserialize;

use super::{AppError, ago, render};
use crate::domain::profile::{self, Profile};
use crate::domain::tokens::{self, ApiToken};
use crate::{AppState, auth::CurrentUser};

#[derive(Template)]
#[template(path = "settings_general.html")]
struct GeneralPage {
    p: Profile,
    saved: bool,
    error: Option<String>,
}

pub async fn general(State(s): State<AppState>, CurrentUser(user): CurrentUser) -> Result<Response, AppError> {
    let Some(p) = profile::get(&s.db, user).await? else {
        return Ok(Redirect::to("/login").into_response());
    };
    Ok(render(&GeneralPage {
        p,
        saved: false,
        error: None,
    })?
    .into_response())
}

pub async fn save_general(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    cookies: CookieJar,
    Form(f): Form<profile::Settings>,
) -> Result<Response, AppError> {
    if let Err(e) = f.validate() {
        let Some(p) = profile::get(&s.db, user).await? else {
            return Ok(Redirect::to("/login").into_response());
        };
        return Ok(render(&GeneralPage {
            p,
            saved: false,
            error: Some(e.into()),
        })?
        .into_response());
    }
    let mut res =
        s.db.batch(vec![profile::save_stmt(user, &f), profile::stmt(user)])
            .await?;
    let Some(row) = res.pop().and_then(|mut r| r.pop()) else {
        return Ok(Redirect::to("/login").into_response());
    };
    if f.enable_favicons || f.enable_preview_images {
        s.jobs.enqueue_missing(&s.db, Some(user)).await?;
    }
    let p: Profile = crate::db::from_row(row)?;
    let cookies = cookies.add(crate::auth::prefs_cookie(&p));
    Ok((
        cookies,
        render(&GeneralPage {
            p,
            saved: true,
            error: None,
        })?,
    )
        .into_response())
}

/// The user's custom stylesheet; the URL carries its hash, so it can be cached for good.
pub async fn custom_css(State(s): State<AppState>, CurrentUser(user): CurrentUser) -> Result<Response, AppError> {
    let css = profile::get_or_guest(&s.db, user).await?.custom_css;
    let headers = [
        (axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8"),
        (
            axum::http::header::CACHE_CONTROL,
            "private, max-age=31536000, immutable",
        ),
        (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ];
    Ok((headers, css).into_response())
}

struct TokenRow {
    t: ApiToken,
    created: String,
}

#[derive(Template)]
#[template(path = "settings_integrations.html")]
struct IntegrationsPage {
    tokens: Vec<TokenRow>,
    /// Plaintext key of a token just created; shown once.
    new_key: Option<String>,
    origin: String,
    feed_key: String,
}

async fn page(s: &AppState, user: i64, headers: &HeaderMap, new_key: Option<String>) -> Result<Response, AppError> {
    let tokens = tokens::list(&s.db, user)
        .await?
        .into_iter()
        .map(|t| TokenRow {
            created: ago(&t.created),
            t,
        })
        .collect();
    let origin = crate::api::origin(headers);
    let feed_key = tokens::feed_key(&s.db, user).await?;
    Ok(render(&IntegrationsPage {
        tokens,
        new_key,
        origin,
        feed_key,
    })?
    .into_response())
}

pub async fn integrations(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    page(&s, user, &headers, None).await
}

#[derive(Deserialize)]
pub struct CreateToken {
    name: String,
}

pub async fn create_token(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
    Form(f): Form<CreateToken>,
) -> Result<Response, AppError> {
    let name = f.name.trim();
    let name = if name.is_empty() { "API token" } else { name };
    let name: String = name.chars().take(128).collect();
    let key = tokens::create(&s.db, user, &name).await?;
    page(&s, user, &headers, Some(key)).await
}

#[derive(Deserialize)]
pub struct DeleteToken {
    token_id: i64,
}

pub async fn reset_feed_key(State(s): State<AppState>, CurrentUser(user): CurrentUser) -> Result<Response, AppError> {
    tokens::reset_feed_key(&s.db, user).await?;
    Ok(Redirect::to("/settings/integrations").into_response())
}

pub async fn delete_token(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Form(f): Form<DeleteToken>,
) -> Result<Response, AppError> {
    tokens::delete(&s.db, user, f.token_id).await?;
    s.tokens.clear();
    Ok(Redirect::to("/settings/integrations").into_response())
}

// ---------------------------------------------------------------------------------------------
// Import & export

#[derive(Template)]
#[template(path = "settings_data.html")]
struct DataPage {
    notice: Option<String>,
    error: Option<String>,
}

pub async fn data(_: CurrentUser) -> Result<Response, AppError> {
    Ok(render(&DataPage {
        notice: None,
        error: None,
    })?
    .into_response())
}

/// Upload limit for bookmark files (large browser exports run to a few MB).
pub const MAX_IMPORT_BYTES: usize = 50 * 1024 * 1024;

pub async fn import(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    mut form: axum::extract::Multipart,
) -> Result<Response, AppError> {
    let (mut html, mut map_private) = (None, false);
    while let Some(field) = form.next_field().await? {
        match field.name() {
            Some("file") => html = Some(String::from_utf8_lossy(&field.bytes().await?).into_owned()),
            Some("map_private_flag") => map_private = true,
            _ => {}
        }
    }
    let page = match html.filter(|h| !h.trim().is_empty()) {
        None => DataPage {
            notice: None,
            error: Some("Choose a bookmarks file to import.".into()),
        },
        Some(html) => {
            let r = crate::netscape::import(&s.db, user, &html, map_private).await?;
            s.jobs.enqueue_missing(&s.db, Some(user)).await?;
            let skipped = match r.failed {
                0 => String::new(),
                n => format!(" {n} skipped (invalid or script links, or duplicates in the file)."),
            };
            DataPage {
                notice: Some(format!("Imported {} of {} bookmarks.{skipped}", r.success, r.total)),
                error: None,
            }
        }
    };
    Ok(render(&page)?.into_response())
}

pub async fn export(State(s): State<AppState>, CurrentUser(user): CurrentUser) -> Result<Response, AppError> {
    let html = crate::netscape::export(&crate::netscape::load_all(&s.db, user).await?);
    let headers = [
        (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
        (
            axum::http::header::CONTENT_DISPOSITION,
            "attachment; filename=\"bookmarks.html\"",
        ),
    ];
    Ok((headers, html).into_response())
}

// ---------------------------------------------------------------------------------------------
// Account

#[derive(Template)]
#[template(path = "settings_account.html")]
struct AccountPage {
    notice: Option<String>,
    error: Option<String>,
}

pub async fn account(_: CurrentUser) -> Result<Response, AppError> {
    Ok(render(&AccountPage {
        notice: None,
        error: None,
    })?
    .into_response())
}

#[derive(Deserialize)]
pub struct PasswordForm {
    current_password: String,
    new_password: String,
    confirm_password: String,
}

/// Changing the password logs out other sessions; this one gets a fresh cookie.
pub async fn change_password(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    jar: axum_extra::extract::SignedCookieJar,
    Form(f): Form<PasswordForm>,
) -> Result<Response, AppError> {
    use crate::domain::users;
    let error = if f.new_password != f.confirm_password {
        "The new passwords don't match.".to_string()
    } else if let Err(e) = users::validate_password(&f.new_password) {
        e
    } else {
        match users::change_password(&s.db, user, &f.current_password, &f.new_password).await? {
            None => "Your current password isn't right.".into(),
            Some(nonce) => {
                s.sessions.clear();
                let jar = crate::auth::login(jar, &users::Session { id: user, nonce });
                let notice = Some("Password changed. Other devices were logged out.".to_string());
                return Ok((jar, render(&AccountPage { notice, error: None })?).into_response());
            }
        }
    };
    Ok(render(&AccountPage {
        notice: None,
        error: Some(error),
    })?
    .into_response())
}
