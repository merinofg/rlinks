//! User management for admins (replaces linkding's Django admin for the common tasks).

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Form;
use serde::Deserialize;

use super::{AppError, ago, render};
use crate::domain::users::{self, User};
use crate::{AppState, auth::CurrentUser};

struct Row {
    u: User,
    joined: String,
    is_me: bool,
}

#[derive(Template)]
#[template(path = "admin_users.html")]
struct UsersPage {
    users: Vec<Row>,
    landing_page: String,
    notice: Option<String>,
    error: Option<String>,
}

async fn page(s: &AppState, me: i64, notice: Option<String>, error: Option<String>) -> Result<Response, AppError> {
    let users = users::list(&s.db)
        .await?
        .into_iter()
        .map(|u| Row {
            joined: ago(&u.date_joined),
            is_me: u.id == me,
            u,
        })
        .collect();
    let landing_page = crate::domain::site::landing_page(&s.db).await?;
    Ok(render(&UsersPage {
        users,
        landing_page,
        notice,
        error,
    })?
    .into_response())
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "Only admins can manage users.").into_response()
}

pub async fn index(State(s): State<AppState>, CurrentUser(me): CurrentUser) -> Result<Response, AppError> {
    if !users::is_admin(&s.db, me).await? {
        return Ok(forbidden());
    }
    page(&s, me, None, None).await
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct UserAction {
    // create
    username: Option<String>,
    password: Option<String>,
    is_admin: Option<String>,
    // per-user actions
    deactivate: Option<i64>,
    activate: Option<i64>,
    make_admin: Option<i64>,
    remove_admin: Option<i64>,
    delete: Option<i64>,
    reset_password: Option<i64>,
    new_password: Option<String>,
    // site settings
    landing_page: Option<String>,
}

pub async fn action(
    State(s): State<AppState>,
    CurrentUser(me): CurrentUser,
    Form(f): Form<UserAction>,
) -> Result<Response, AppError> {
    if !users::is_admin(&s.db, me).await? {
        return Ok(forbidden());
    }
    // Admins can't lock themselves out.
    let targets_me = [f.deactivate, f.remove_admin, f.delete].contains(&Some(me));
    let (notice, error): (Option<String>, Option<String>) = if targets_me {
        (
            None,
            Some("You can't deactivate, demote or delete your own account.".into()),
        )
    } else if let Some(page) = &f.landing_page {
        crate::domain::site::set_landing_page(&s.db, page).await?;
        (Some("Site settings saved.".into()), None)
    } else if let Some(id) = f.deactivate {
        users::set_flags(&s.db, id, Some(false), None).await?;
        // Their sessions and API tokens stop working now, not after the cache TTL.
        s.tokens.clear();
        s.sessions.clear();
        (
            Some("User deactivated. They can no longer log in or use the API.".into()),
            None,
        )
    } else if let Some(id) = f.activate {
        users::set_flags(&s.db, id, Some(true), None).await?;
        (Some("User activated.".into()), None)
    } else if let Some(id) = f.make_admin {
        users::set_flags(&s.db, id, None, Some(true)).await?;
        (Some("User is now an admin.".into()), None)
    } else if let Some(id) = f.remove_admin {
        users::set_flags(&s.db, id, None, Some(false)).await?;
        (Some("Admin rights removed.".into()), None)
    } else if let Some(id) = f.delete {
        users::delete(&s.db, id).await?;
        s.tokens.clear();
        s.sessions.clear();
        (Some("User and all their data deleted.".into()), None)
    } else if let Some(id) = f.reset_password {
        let password = f.new_password.unwrap_or_default();
        match users::validate_password(&password) {
            Err(e) => (None, Some(e)),
            Ok(()) => {
                users::set_password(&s.db, id, &password).await?;
                s.sessions.clear(); // ends the user's current sessions
                (Some("Password reset. The user was logged out everywhere.".into()), None)
            }
        }
    } else {
        let username = f.username.unwrap_or_default().trim().to_string();
        let password = f.password.unwrap_or_default();
        let valid_name = !username.is_empty()
            && username.len() <= 150
            && username.chars().all(|c| c.is_alphanumeric() || "@.+-_".contains(c));
        if !valid_name {
            (None, Some("Usernames use letters, digits and @ . + - _ only.".into()))
        } else if let Err(e) = users::validate_password(&password) {
            (None, Some(e))
        } else if users::create(&s.db, &username, &password, f.is_admin.is_some()).await? {
            (Some(format!("Created user “{username}”.")), None)
        } else {
            (None, Some(format!("The username “{username}” is taken.")))
        }
    };
    page(&s, me, notice, error).await
}
