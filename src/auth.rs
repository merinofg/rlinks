//! Stateless sessions (signed cookie holding the user id) and CSRF protection.
//! No DB lookup per request: D1 round-trips are expensive, so pages load the user row
//! inside their own batch when they need it.

use axum::extract::{FromRequestParts, Request};
use axum::http::{HeaderMap, Method, StatusCode, header, request::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, Key, SameSite, SignedCookieJar};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::AppState;
use crate::db::Db;
use crate::domain::{tokens, users};

const SESSION: &str = "session";

/// The logged-in user's id. The signed session cookie carries `<id>:<session nonce>`; the nonce
/// must still be the user's current one (it changes on password change/reset and deactivation,
/// and a reused id gets a new one), checked through a short-lived cache to spare D1 calls.
/// Rejects with a redirect to the login page.
pub struct CurrentUser(pub i64);

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let jar = SignedCookieJar::<Key>::from_request_parts(parts, state).await.unwrap();
        let Some((id, nonce)) = jar.get(SESSION).and_then(|c| {
            let (id, nonce) = c.value().split_once(':')?;
            Some((id.parse::<i64>().ok()?, nonce.to_string()))
        }) else {
            return Err(login_redirect(&parts.headers));
        };
        let current = match state
            .sessions
            .get_or_load(id, users::session_nonce(&state.db, id))
            .await
        {
            Ok(current) => current,
            Err(e) => {
                tracing::error!("session lookup: {e:#}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
        };
        match current {
            Some(current) if !nonce.is_empty() && current == nonce => Ok(CurrentUser(id)),
            _ => Err(login_redirect(&parts.headers)),
        }
    }
}

fn login_redirect(headers: &HeaderMap) -> Response {
    if headers.contains_key("hx-request") {
        // htmx would swap the login page into a fragment; ask it for a full navigation instead.
        return (StatusCode::OK, [("hx-redirect", "/login")]).into_response();
    }
    Redirect::to("/login").into_response()
}

pub fn login(jar: SignedCookieJar, session: &users::Session) -> SignedCookieJar {
    let cookie = Cookie::build((SESSION, format!("{}:{}", session.id, session.nonce)))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(30))
        .build();
    jar.add(cookie)
}

/// Non-secret display preferences (theme, custom CSS hash) read by a script in base.html, so
/// pages don't need a profile read to apply them. Set at login and when settings are saved.
pub fn prefs_cookie(p: &crate::domain::profile::Profile) -> Cookie<'static> {
    Cookie::build(("prefs", p.prefs_cookie()))
        .path("/")
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(365))
        .build()
}

pub fn logout(jar: SignedCookieJar) -> SignedCookieJar {
    jar.remove(Cookie::build(SESSION).path("/"))
}

/// Reject cross-site state-changing requests. Combined with SameSite=Lax cookies this covers
/// CSRF without tokens.
///
/// Only REST API calls carrying a `Token`/`Bearer` credential are exempt (browser extensions
/// send `Origin: chrome-extension://...`): `ApiUser` then authenticates by that header alone,
/// never by cookie, so there is nothing to forge. The exemption is deliberately narrow: browsers
/// attach some Authorization headers on their own (e.g. Basic auth from a reverse proxy), so
/// "has an Authorization header" must never bypass the check for cookie-authenticated routes.
pub async fn csrf_guard(req: Request, next: Next) -> Response {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if !safe && !is_api_token_request(&req) && is_cross_site(req.headers()) {
        return (StatusCode::FORBIDDEN, "cross-site request rejected").into_response();
    }
    next.run(req).await
}

fn is_api_token_request(req: &Request) -> bool {
    let scheme = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_whitespace().next())
        .map(str::to_ascii_lowercase);
    req.uri().path().starts_with("/api/") && matches!(scheme.as_deref(), Some("token" | "bearer"))
}

fn is_cross_site(h: &HeaderMap) -> bool {
    if h.get("sec-fetch-site").is_some_and(|v| v == "cross-site") {
        return true;
    }
    let (Some(origin), Some(host)) = (h.get(header::ORIGIN), h.get(header::HOST)) else {
        return false;
    };
    let origin = origin.to_str().unwrap_or_default();
    let origin_host = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    origin_host != host.to_str().unwrap_or_default()
}

/// REST API caller: `Authorization: Token <key>` (or `Bearer <key>`, like linkding), or the
/// web session cookie when no Authorization header is sent.
pub struct ApiUser(pub i64);

impl FromRequestParts<AppState> for ApiUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let Some(header) = parts.headers.get(header::AUTHORIZATION) else {
            return CurrentUser::from_request_parts(parts, state)
                .await
                .map(|u| ApiUser(u.0))
                .map_err(|_| unauthorized("Authentication credentials were not provided."));
        };
        let mut words = header.to_str().unwrap_or_default().split_whitespace();
        let key = match (
            words.next().map(str::to_ascii_lowercase).as_deref(),
            words.next(),
            words.next(),
        ) {
            (Some("token" | "bearer"), Some(key), None) => key,
            _ => return Err(unauthorized("Invalid token header.")),
        };
        match state.tokens.resolve(&state.db, key).await {
            Ok(Some(user)) => Ok(ApiUser(user)),
            Ok(None) => Err(unauthorized("Invalid token.")),
            Err(e) => {
                tracing::error!("token lookup: {e:#}");
                Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
            }
        }
    }
}

fn unauthorized(detail: &str) -> Response {
    let body = axum::Json(serde_json::json!({ "detail": detail }));
    (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Token")], body).into_response()
}

/// Small per-instance cache with a time-to-live, sparing D1 round-trips on every request for
/// API token and session checks. Misses are cached too, so bad credentials can't burn the D1
/// rate limit.
// ponytail: per-instance; a revoked token/session keeps working on *other* instances for up to
// CACHE_TTL (this instance clears immediately). Use a shared store if running many instances.
pub struct TtlCache<K, V>(Mutex<Entries<K, V>>);

struct Entries<K, V> {
    map: HashMap<K, (V, Instant)>,
    /// Bumped by `clear`; a lookup that started before a clear must not cache its (possibly
    /// stale) result afterwards.
    generation: u64,
}

const CACHE_TTL: Duration = Duration::from_secs(60);

impl<K: std::hash::Hash + Eq, V: Clone> Default for TtlCache<K, V> {
    fn default() -> Self {
        TtlCache(Mutex::new(Entries {
            map: HashMap::new(),
            generation: 0,
        }))
    }
}

impl<K: std::hash::Hash + Eq, V: Clone> TtlCache<K, V> {
    pub fn get(&self, key: &K) -> Option<V> {
        let cache = self.0.lock().unwrap();
        cache
            .map
            .get(key)
            .filter(|(_, at)| at.elapsed() < CACHE_TTL)
            .map(|(v, _)| v.clone())
    }

    /// Cached value, or `load` it and cache the result unless the cache was cleared meanwhile
    /// (e.g. a password reset racing with this lookup: the loaded value may already be stale).
    pub async fn get_or_load<F>(&self, key: K, load: F) -> anyhow::Result<V>
    where
        F: std::future::Future<Output = anyhow::Result<V>>,
    {
        if let Some(v) = self.get(&key) {
            return Ok(v);
        }
        let generation = self.0.lock().unwrap().generation;
        let value = load.await?;
        let mut cache = self.0.lock().unwrap();
        if cache.generation == generation {
            if cache.map.len() > 10_000 {
                cache.map.clear(); // bound memory under a flood of random credentials
            }
            cache.map.insert(key, (value.clone(), Instant::now()));
        }
        Ok(value)
    }

    /// Call after revoking tokens or ending sessions.
    pub fn clear(&self) {
        let mut cache = self.0.lock().unwrap();
        cache.map.clear();
        cache.generation += 1;
    }
}

/// API token hash → user id.
pub type TokenCache = TtlCache<String, Option<i64>>;
/// User id → current session nonce (`None`: deleted or deactivated).
pub type SessionCache = TtlCache<i64, Option<String>>;

impl TokenCache {
    pub async fn resolve(&self, db: &Db, key: &str) -> anyhow::Result<Option<i64>> {
        let hash = tokens::hash(key);
        self.get_or_load(hash.clone(), tokens::resolve(db, &hash)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cache_skips_results_of_lookups_that_raced_a_clear() {
        let cache: TtlCache<i64, &str> = TtlCache::default();
        assert_eq!(cache.get_or_load(1, async { Ok("old") }).await.unwrap(), "old");
        assert_eq!(cache.get(&1), Some("old"), "cached");
        // A lookup reads the old value; the cache is cleared (password reset) before it finishes.
        let racing = cache.get_or_load(2, async {
            cache.clear();
            Ok("stale")
        });
        assert_eq!(racing.await.unwrap(), "stale", "the caller still gets what it read");
        assert_eq!(cache.get(&2), None, "but it isn't cached past the clear");
        assert_eq!(cache.get(&1), None, "clear empties the cache");
    }

    #[test]
    fn cross_site_detection() {
        let h = |pairs: &[(&'static str, &'static str)]| {
            let mut m = HeaderMap::new();
            for (k, v) in pairs {
                m.insert(*k, v.parse().unwrap());
            }
            m
        };
        assert!(!is_cross_site(&h(&[("host", "a.com"), ("origin", "https://a.com")])));
        assert!(is_cross_site(&h(&[("host", "a.com"), ("origin", "https://evil.com")])));
        assert!(is_cross_site(&h(&[("sec-fetch-site", "cross-site")])));
        assert!(!is_cross_site(&h(&[("host", "a.com")])), "no Origin: API client");
    }
}
