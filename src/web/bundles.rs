use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::Form;
use serde::Deserialize;

use super::{AppError, render};
use crate::domain::bundles::{self, Bundle, BundleInput};
use crate::{AppState, auth::CurrentUser};

#[derive(Template)]
#[template(path = "bundles.html")]
struct BundlesPage {
    bundles: Vec<Bundle>,
}

pub async fn index(State(s): State<AppState>, CurrentUser(user): CurrentUser) -> Result<Response, AppError> {
    let bundles = bundles::list(&s.db, user).await?;
    Ok(render(&BundlesPage { bundles })?.into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct BundleAction {
    up: Option<i64>,
    down: Option<i64>,
    delete: Option<i64>,
}

pub async fn action(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Form(f): Form<BundleAction>,
) -> Result<Response, AppError> {
    match (f.up, f.down, f.delete) {
        (Some(id), _, _) => bundles::shift(&s.db, user, id, -1).await?,
        (_, Some(id), _) => bundles::shift(&s.db, user, id, 1).await?,
        (_, _, Some(id)) => {
            bundles::delete(&s.db, user, id).await?;
        }
        _ => {}
    }
    Ok(Redirect::to("/bundles").into_response())
}

#[derive(Template)]
#[template(path = "bundle_form.html")]
struct FormPage {
    heading: &'static str,
    action: String,
    b: Bundle,
    error: Option<String>,
}

fn form_page(heading: &'static str, action: String, b: Bundle, error: Option<String>) -> Result<Response, AppError> {
    Ok(render(&FormPage {
        heading,
        action,
        b,
        error,
    })?
    .into_response())
}

/// What the form shows after a failed submit: the submitted values.
fn echo(input: &BundleInput, id: i64) -> Bundle {
    let s = |v: &Option<String>| v.clone().unwrap_or_default();
    Bundle {
        id,
        name: s(&input.name),
        search: s(&input.search),
        any_tags: s(&input.any_tags),
        all_tags: s(&input.all_tags),
        excluded_tags: s(&input.excluded_tags),
        filter_unread: input.filter_unread.clone().unwrap_or_else(|| "off".into()),
        filter_shared: input.filter_shared.clone().unwrap_or_else(|| "off".into()),
        ..Default::default()
    }
}

pub async fn new_page(_: CurrentUser) -> Result<Response, AppError> {
    let b = Bundle {
        filter_unread: "off".into(),
        filter_shared: "off".into(),
        ..Default::default()
    };
    form_page("New bundle", "/bundles/new".into(), b, None)
}

pub async fn create(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Form(input): Form<BundleInput>,
) -> Result<Response, AppError> {
    let input = input.complete();
    if let Err((_, msg)) = input.validate() {
        return form_page("New bundle", "/bundles/new".into(), echo(&input, 0), Some(msg.into()));
    }
    let b = bundles::create(&s.db, user, &input).await?;
    Ok(Redirect::to(&format!("/bookmarks?bundle={}", b.id)).into_response())
}

pub async fn edit_page(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    match bundles::get(&s.db, user, id).await? {
        Some(b) => form_page("Edit bundle", format!("/bundles/{id}/edit"), b, None),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

pub async fn update(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Form(input): Form<BundleInput>,
) -> Result<Response, AppError> {
    if let Err((_, msg)) = input.validate() {
        return form_page(
            "Edit bundle",
            format!("/bundles/{id}/edit"),
            echo(&input, id),
            Some(msg.into()),
        );
    }
    match bundles::update(&s.db, user, id, &input).await? {
        Some(b) => Ok(Redirect::to(&format!("/bookmarks?bundle={}", b.id)).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}
