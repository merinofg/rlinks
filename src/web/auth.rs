use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::{CookieJar, SignedCookieJar};
use serde::Deserialize;

use super::{AppError, render};
use crate::domain::profile;
use crate::{AppState, auth, domain::users};

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    error: bool,
}

pub async fn login_page() -> Result<Html<String>, AppError> {
    render(&LoginPage { error: false })
}

#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
}

pub async fn login(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    cookies: CookieJar,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    match users::authenticate(&state.db, &form.username, &form.password).await? {
        Some(session) => {
            let cookies = match profile::get(&state.db, session.id).await? {
                Some(p) => cookies.add(auth::prefs_cookie(&p)),
                None => cookies,
            };
            Ok((auth::login(jar, &session), cookies, Redirect::to("/bookmarks")).into_response())
        }
        None => Ok(render(&LoginPage { error: true })?.into_response()),
    }
}

pub async fn logout(jar: SignedCookieJar) -> impl IntoResponse {
    (auth::logout(jar), Redirect::to("/login"))
}
