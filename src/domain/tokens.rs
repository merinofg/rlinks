//! REST API tokens. Only a SHA-256 hash is stored; the plaintext key is shown once on creation.

use anyhow::Result;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::db::{Db, now, values};
use crate::stmt;

#[derive(Debug, Clone, Deserialize)]
pub struct ApiToken {
    pub id: i64,
    pub name: String,
    pub created: String,
}

pub fn hash(key: &str) -> String {
    crate::hex(&Sha256::digest(key.as_bytes()))
}

/// 40 random hex chars, like linkding's keys.
fn random_key() -> String {
    crate::hex(&rand::random::<[u8; 20]>())
}

/// Creates a token and returns its plaintext key (40 hex chars, like linkding).
pub async fn create(db: &Db, user: i64, name: &str) -> Result<String> {
    let key = random_key();
    db.execute(stmt!(
        "INSERT INTO api_tokens (key, user_id, name, created) VALUES (?, ?, ?, ?)",
        hash(&key),
        user,
        name,
        now()
    ))
    .await?;
    Ok(key)
}

pub async fn list(db: &Db, user: i64) -> Result<Vec<ApiToken>> {
    db.query(stmt!(
        "SELECT id, name, created FROM api_tokens WHERE user_id = ? ORDER BY id",
        user
    ))
    .await
}

pub async fn delete(db: &Db, user: i64, id: i64) -> Result<()> {
    db.execute(stmt!("DELETE FROM api_tokens WHERE id = ? AND user_id = ?", id, user))
        .await
}

/// The active user owning the token with this hash.
pub async fn resolve(db: &Db, key_hash: &str) -> Result<Option<i64>> {
    let rows = db
        .query(stmt!(
            "SELECT t.user_id FROM api_tokens t JOIN users u ON u.id = t.user_id \
             WHERE t.key = ? AND u.is_active = 1",
            key_hash
        ))
        .await?;
    Ok(values(rows, "user_id").pop())
}

/// The user's feed key, created on first use. Feed URLs embed it, so unlike API tokens it is
/// stored as is: it only grants read access to the user's bookmark feeds.
pub async fn feed_key(db: &Db, user: i64) -> Result<String> {
    let key = random_key();
    let mut res = db
        .batch(vec![
            stmt!(
                "INSERT INTO feed_tokens (key, user_id, created) VALUES (?, ?, ?) ON CONFLICT (user_id) DO NOTHING",
                key,
                user,
                now()
            ),
            stmt!("SELECT key FROM feed_tokens WHERE user_id = ?", user),
        ])
        .await?;
    let key = values(res.pop().unwrap_or_default(), "key").pop();
    key.ok_or_else(|| anyhow::anyhow!("feed token upsert returned no row"))
}

/// Invalidate the user's feed links; the next `feed_key` call issues a new key.
pub async fn reset_feed_key(db: &Db, user: i64) -> Result<()> {
    db.execute(stmt!("DELETE FROM feed_tokens WHERE user_id = ?", user))
        .await
}

/// The active user owning a feed key.
pub async fn feed_user(db: &Db, key: &str) -> Result<Option<i64>> {
    let rows = db
        .query(stmt!(
            "SELECT f.user_id FROM feed_tokens f JOIN users u ON u.id = f.user_id WHERE f.key = ? AND u.is_active = 1",
            key
        ))
        .await?;
    Ok(values(rows, "user_id").pop())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Row;
    use crate::domain::users;

    #[tokio::test]
    async fn create_resolve_delete() {
        let db = Db::sqlite(":memory:").unwrap();
        db.migrate().await.unwrap();
        users::create(&db, "u", "pw", false).await.unwrap();
        let key = create(&db, 1, "ext").await.unwrap();
        assert_eq!(key.len(), 40);
        let stored: Vec<Row> = db.query(stmt!("SELECT key FROM api_tokens")).await.unwrap();
        assert_ne!(stored[0]["key"], key.as_str(), "plaintext is never stored");
        assert_eq!(resolve(&db, &hash(&key)).await.unwrap(), Some(1));
        assert_eq!(resolve(&db, &hash("nope")).await.unwrap(), None);
        db.execute(stmt!("UPDATE users SET is_active = 0")).await.unwrap();
        assert_eq!(
            resolve(&db, &hash(&key)).await.unwrap(),
            None,
            "inactive users can't use tokens"
        );
        let id = list(&db, 1).await.unwrap()[0].id;
        delete(&db, 1, id).await.unwrap();
        assert!(list(&db, 1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn feed_keys_are_stable() {
        let db = Db::sqlite(":memory:").unwrap();
        db.migrate().await.unwrap();
        users::create(&db, "u", "pw", false).await.unwrap();
        let key = feed_key(&db, 1).await.unwrap();
        assert_eq!(feed_key(&db, 1).await.unwrap(), key);
        assert_eq!(feed_user(&db, &key).await.unwrap(), Some(1));
        assert_eq!(feed_user(&db, "nope").await.unwrap(), None);
        reset_feed_key(&db, 1).await.unwrap();
        assert_eq!(feed_user(&db, &key).await.unwrap(), None, "old links stop working");
        assert_ne!(feed_key(&db, 1).await.unwrap(), key);
    }
}
