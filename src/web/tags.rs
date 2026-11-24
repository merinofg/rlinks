use askama::Template;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Form;
use serde::Deserialize;

use super::{AppError, render};
use crate::domain::bookmarks::parse_tag_string;
use crate::domain::tags::{self, Renamed, TagCount};
use crate::{AppState, auth::CurrentUser};

#[derive(Template)]
#[template(path = "tags.html")]
struct TagsPage {
    tags: Vec<TagCount>,
    q: String,
    notice: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct TagsQuery {
    q: String,
}

async fn page(
    s: &AppState,
    user: i64,
    q: String,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Response, AppError> {
    let tags = tags::list_with_counts(&s.db, user, q.trim()).await?;
    Ok(render(&TagsPage { tags, q, notice, error })?.into_response())
}

pub async fn index(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(q): Query<TagsQuery>,
) -> Result<Response, AppError> {
    page(&s, user, q.q, None, None).await
}

/// One form endpoint for the page's actions (rename / delete / merge), like the bookmark list.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct TagAction {
    rename: Option<i64>,
    name: String,
    delete: Option<i64>,
    merge_target: String,
    merge_sources: String,
}

pub async fn action(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Form(f): Form<TagAction>,
) -> Result<Response, AppError> {
    let (notice, error) = if let Some(id) = f.rename {
        let name = f.name.trim();
        if name.is_empty() || name.chars().count() > tags::MAX_NAME_LEN {
            (
                None,
                Some(format!("Tag names need 1 to {} characters.", tags::MAX_NAME_LEN)),
            )
        } else {
            match tags::rename(&s.db, user, id, name).await? {
                Renamed::Ok => (Some(format!("Renamed to “{name}”.")), None),
                Renamed::Duplicate => (
                    None,
                    Some(format!("A tag named “{name}” already exists. Merge them instead.")),
                ),
                Renamed::NotFound => (None, Some("That tag no longer exists.".into())),
            }
        }
    } else if let Some(id) = f.delete {
        tags::delete(&s.db, user, id).await?;
        (Some("Tag deleted and removed from its bookmarks.".into()), None)
    } else {
        let target = f.merge_target.trim();
        let sources = parse_tag_string(&f.merge_sources, ' ');
        if target.is_empty() || sources.is_empty() {
            (None, Some("Enter a target tag and the tags to merge into it.".into()))
        } else {
            match tags::merge(&s.db, user, target, &sources).await? {
                0 => (
                    None,
                    Some(format!(
                        "Nothing merged: check that “{target}” and the other tags exist."
                    )),
                ),
                n => (
                    Some(format!(
                        "Merged {n} tag{} into “{target}”.",
                        if n == 1 { "" } else { "s" }
                    )),
                    None,
                ),
            }
        }
    };
    page(&s, user, String::new(), notice, error).await
}
