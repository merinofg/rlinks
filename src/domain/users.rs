use anyhow::{Result, anyhow};
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use serde::Deserialize;
use std::sync::OnceLock;

use crate::db::{Db, bool_from_int, now, values};
use crate::stmt;

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    #[serde(deserialize_with = "bool_from_int")]
    pub is_admin: bool,
    #[serde(deserialize_with = "bool_from_int")]
    pub is_active: bool,
    pub date_joined: String,
}

const COLS: &str = "id, username, is_admin, is_active, date_joined";

/// Create a user and its profile in one batch. Returns false if the username is taken.
pub async fn create(db: &Db, username: &str, password: &str, is_admin: bool) -> Result<bool> {
    let hash = hash_password(password).await?;
    let res = db
        .batch(vec![
            stmt!(
                "INSERT INTO users (username, password_hash, is_admin, date_joined) VALUES (?, ?, ?, ?)
                 ON CONFLICT (username) DO NOTHING RETURNING id",
                username,
                hash,
                is_admin,
                now()
            ),
            stmt!(
                "INSERT INTO user_profiles (user_id) SELECT id FROM users WHERE username = ?
                 ON CONFLICT (user_id) DO NOTHING",
                username
            ),
        ])
        .await?;
    Ok(!res[0].is_empty())
}

pub async fn get(db: &Db, id: i64) -> Result<Option<User>> {
    let sql = format!("SELECT {COLS} FROM users WHERE id = ? AND is_active = 1");
    Ok(db.query(stmt!(sql, id)).await?.pop())
}

/// A logged-in user: the id plus the session nonce the session cookie must carry.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: i64,
    pub nonce: String,
}

/// The session for valid credentials of an active account.
pub async fn authenticate(db: &Db, username: &str, password: &str) -> Result<Option<Session>> {
    #[derive(Deserialize)]
    struct Account {
        id: i64,
        password_hash: String,
        session_nonce: String,
    }
    let account: Option<Account> = db
        .query(stmt!(
            "SELECT id, password_hash, session_nonce FROM users WHERE username = ? AND is_active = 1",
            username
        ))
        .await?
        .pop();
    // Without an account, verify against a dummy hash anyway so response time doesn't reveal
    // which usernames exist.
    let hash = account
        .as_ref()
        .map_or_else(|| dummy_hash().to_string(), |a| a.password_hash.clone());
    let ok = verify_password(password, hash).await?;
    Ok(account.filter(|_| ok).map(|a| Session {
        id: a.id,
        nonce: a.session_nonce,
    }))
}

/// Current session nonce of an active user (`None`: deleted or deactivated).
pub async fn session_nonce(db: &Db, id: i64) -> Result<Option<String>> {
    let rows = db
        .query(stmt!(
            "SELECT session_nonce FROM users WHERE id = ? AND is_active = 1",
            id
        ))
        .await?;
    Ok(values(rows, "session_nonce").pop())
}

pub async fn list(db: &Db) -> Result<Vec<User>> {
    db.query(stmt!(format!("SELECT {COLS} FROM users ORDER BY lower(username)")))
        .await
}

pub async fn is_admin(db: &Db, id: i64) -> Result<bool> {
    Ok(get(db, id).await?.is_some_and(|u| u.is_admin))
}

pub const MIN_PASSWORD_LEN: usize = 8;

pub fn validate_password(password: &str) -> Result<(), String> {
    match password.chars().count() {
        n if n < MIN_PASSWORD_LEN => Err(format!("Passwords need at least {MIN_PASSWORD_LEN} characters.")),
        n if n > 1024 => Err("That password is too long.".into()),
        _ => Ok(()),
    }
}

/// Set a new password and end all of the user's sessions. Returns the new session nonce.
pub async fn set_password(db: &Db, id: i64, password: &str) -> Result<Option<String>> {
    let hash = hash_password(password).await?;
    let rows = db
        .query(stmt!(
            "UPDATE users SET password_hash = ?, session_nonce = lower(hex(randomblob(16))) WHERE id = ? \
             RETURNING session_nonce",
            hash,
            id
        ))
        .await?;
    Ok(values(rows, "session_nonce").pop())
}

/// Change one's own password (ends other sessions). Returns the new session nonce, or `None`
/// when `current` is wrong.
pub async fn change_password(db: &Db, id: i64, current: &str, new: &str) -> Result<Option<String>> {
    let rows = db
        .query(stmt!("SELECT password_hash FROM users WHERE id = ?", id))
        .await?;
    let Some(hash) = values(rows, "password_hash").pop() else {
        return Ok(None);
    };
    if !verify_password(current, hash).await? {
        return Ok(None);
    }
    set_password(db, id, new).await
}

/// Deactivating also ends the user's sessions.
pub async fn set_flags(db: &Db, id: i64, is_active: Option<bool>, is_admin: Option<bool>) -> Result<()> {
    db.execute(stmt!(
        "UPDATE users SET is_active = coalesce(?, is_active), is_admin = coalesce(?, is_admin), \
         session_nonce = CASE WHEN ? = 0 THEN lower(hex(randomblob(16))) ELSE session_nonce END WHERE id = ?",
        is_active,
        is_admin,
        is_active,
        id
    ))
    .await
}

/// Deletes the user and, through cascades, all their bookmarks, tags, bundles and tokens.
pub async fn delete(db: &Db, id: i64) -> Result<()> {
    db.execute(stmt!("DELETE FROM users WHERE id = ?", id)).await
}

async fn hash_password(password: &str) -> Result<String> {
    let password = password.to_string();
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|e| anyhow!("hash password: {e}"))
    })
    .await?
}

async fn verify_password(password: &str, hash: String) -> Result<bool> {
    let password = password.to_string();
    tokio::task::spawn_blocking(move || {
        let parsed = PasswordHash::new(&hash).map_err(|e| anyhow!("stored hash: {e}"))?;
        Ok(Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
    })
    .await?
}

fn dummy_hash() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| Argon2::default().hash_password(b"dummy").unwrap().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Row;

    #[tokio::test]
    async fn create_and_authenticate() {
        let db = Db::sqlite(":memory:").unwrap();
        db.migrate().await.unwrap();
        assert!(create(&db, "alice", "pw", true).await.unwrap());
        assert!(
            !create(&db, "ALICE", "other", false).await.unwrap(),
            "usernames are case-insensitive"
        );

        let session = authenticate(&db, "alice", "pw").await.unwrap().expect("valid login");
        let id = session.id;
        assert_eq!(
            session_nonce(&db, id).await.unwrap().as_deref(),
            Some(session.nonce.as_str())
        );
        assert_eq!(session.nonce.len(), 32);
        assert!(authenticate(&db, "alice", "wrong").await.unwrap().is_none());
        assert!(authenticate(&db, "bob", "pw").await.unwrap().is_none());

        let user = get(&db, id).await.unwrap().unwrap();
        assert!(user.is_admin && user.username == "alice");
        let new_nonce = change_password(&db, id, "pw", "new-password")
            .await
            .unwrap()
            .expect("changed");
        assert_ne!(new_nonce, session.nonce, "password change ends other sessions");
        assert!(change_password(&db, id, "wrong", "x").await.unwrap().is_none());
        assert!(authenticate(&db, "alice", "new-password").await.unwrap().is_some());
        set_flags(&db, id, Some(false), None).await.unwrap();
        assert!(
            authenticate(&db, "alice", "new-password").await.unwrap().is_none(),
            "inactive users can't log in"
        );
        assert!(validate_password("short").is_err() && validate_password("long enough").is_ok());
        // A deleted user's id may be reused by SQLite; the new account gets a different nonce.
        create(&db, "zed", "pw-pw-pw-pw", false).await.unwrap();
        let zed = authenticate(&db, "zed", "pw-pw-pw-pw").await.unwrap().unwrap();
        delete(&db, zed.id).await.unwrap();
        create(&db, "newbie", "pw-pw-pw-pw", false).await.unwrap();
        let newbie = authenticate(&db, "newbie", "pw-pw-pw-pw").await.unwrap().unwrap();
        assert_eq!(newbie.id, zed.id, "SQLite reuses the highest id");
        assert_ne!(newbie.nonce, zed.nonce, "...but the old session can't match");
        let profiles: Vec<Row> = db.query(stmt!("SELECT user_id FROM user_profiles")).await.unwrap();
        assert_eq!(profiles.len(), 2, "alice and newbie; zed's profile went with zed");
    }
}
