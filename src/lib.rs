pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod domain;
pub mod jobs;
pub mod netscape;
pub mod scrape;
pub mod search;
pub mod snapshot;
pub mod storage;
pub mod web;

use axum::extract::FromRef;
use axum_extra::extract::cookie::Key;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<db::Db>,
    pub key: Key,
    pub tokens: Arc<auth::TokenCache>,
    pub sessions: Arc<auth::SessionCache>,
    pub storage: storage::Storage,
    pub fetcher: Arc<scrape::Fetcher>,
    pub jobs: jobs::Jobs,
}

/// Lowercase hex of `bytes` (token keys, hashes, random file names).
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl FromRef<AppState> for Key {
    fn from_ref(s: &AppState) -> Key {
        s.key.clone()
    }
}

impl AppState {
    /// `secret_key` must be at least 64 bytes (enforced by `Config::from_env`).
    pub fn new(ctx: jobs::Context, jobs: jobs::Jobs, secret_key: &str) -> AppState {
        AppState {
            db: ctx.db,
            key: Key::from(secret_key.as_bytes()),
            tokens: Arc::default(),
            sessions: Arc::default(),
            storage: ctx.storage,
            fetcher: ctx.fetcher,
            jobs,
        }
    }
}
