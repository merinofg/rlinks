//! Storage: Cloudflare D1 over its REST API in production, local SQLite for dev and tests.
//!
//! Both backends expose one operation, `batch`: a list of statements executed atomically in one
//! round-trip, each returning its rows as JSON objects (which serde maps onto structs).
//! D1's REST API has no interactive transactions and every call costs an Internet round-trip
//! plus a slot of the account's API rate limit, so callers build one batch per page/write.
//!
//! D1 REST assumptions, pending the milestone-0 spike against a real database:
//! - `POST .../d1/database/{id}/query` accepts `{"batch": [{"sql","params"}, ...]}`
//! - a batch runs atomically, statements in order, on one connection
//! - `result[i].results` holds statement i's rows

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::sync::Mutex;

pub type Row = Map<String, Value>;

#[derive(Debug, Clone)]
pub struct Stmt {
    pub sql: String,
    pub params: Vec<Value>,
}

/// `stmt!("SELECT * FROM t WHERE a = ? AND b = ?", a, b)`
#[macro_export]
macro_rules! stmt {
    ($sql:expr $(, $p:expr)* $(,)?) => {
        $crate::db::Stmt { sql: $sql.into(), params: vec![$(::serde_json::json!($p)),*] }
    };
}

pub enum Db {
    D1(D1),
    // ponytail: one global connection lock, run inline on the async thread. Fine for dev/tests;
    // move to a pool + spawn_blocking if SQLite is ever used as the production backend.
    Sqlite(Mutex<rusqlite::Connection>),
}

pub struct D1 {
    http: reqwest::Client,
    url: String,
    token: String,
}

const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../migrations/0001_init.sql"))];

impl Db {
    pub fn sqlite(path: &str) -> Result<Db> {
        if let Some(dir) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = rusqlite::Connection::open(path).with_context(|| format!("open {path}"))?;
        // D1 always enforces foreign keys; match it.
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
        Ok(Db::Sqlite(Mutex::new(conn)))
    }

    pub fn d1(account_id: &str, database_id: &str, token: &str) -> Db {
        Db::D1(D1 {
            http: reqwest::Client::new(),
            url: format!("https://api.cloudflare.com/client/v4/accounts/{account_id}/d1/database/{database_id}/query"),
            token: token.to_string(),
        })
    }

    /// True for D1: every call is a rate-limited Internet round-trip.
    pub fn is_remote(&self) -> bool {
        matches!(self, Db::D1(_))
    }

    /// Run all statements atomically in one round-trip; returns each statement's rows.
    pub async fn batch(&self, stmts: Vec<Stmt>) -> Result<Vec<Vec<Row>>> {
        if stmts.is_empty() {
            return Ok(vec![]);
        }
        tracing::debug!(n = stmts.len(), "db batch");
        match self {
            Db::D1(d1) => d1.batch(stmts).await,
            Db::Sqlite(conn) => sqlite_batch(&mut conn.lock().unwrap(), stmts),
        }
    }

    /// Single statement, rows deserialized into `T`.
    pub async fn query<T: DeserializeOwned>(&self, stmt: Stmt) -> Result<Vec<T>> {
        let rows = self.batch(vec![stmt]).await?.pop().unwrap_or_default();
        rows.into_iter().map(from_row).collect()
    }

    pub async fn execute(&self, stmt: Stmt) -> Result<()> {
        self.batch(vec![stmt]).await.map(drop)
    }

    /// Apply pending migrations. Each migration and its version row go in one atomic batch.
    pub async fn migrate(&self) -> Result<()> {
        self.execute(stmt!(
            "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY)"
        ))
        .await?;
        let applied: Vec<i64> = values(
            self.query(stmt!("SELECT version FROM schema_migrations")).await?,
            "version",
        );
        for &(version, sql) in MIGRATIONS {
            if applied.contains(&version) {
                continue;
            }
            let mut stmts: Vec<Stmt> = split_sql(sql).map(|s| stmt!(s)).collect();
            stmts.push(stmt!("INSERT INTO schema_migrations (version) VALUES (?)", version));
            self.batch(stmts)
                .await
                .with_context(|| format!("migration {version}"))?;
            tracing::info!(version, "applied migration");
        }
        Ok(())
    }
}

impl D1 {
    async fn batch(&self, stmts: Vec<Stmt>) -> Result<Vec<Vec<Row>>> {
        let n = stmts.len();
        let body = json!({
            "batch": stmts.into_iter().map(|s| json!({"sql": s.sql, "params": s.params})).collect::<Vec<_>>()
        });
        let resp: Value = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .context("D1 request")?
            .json()
            .await
            .context("D1 response body")?;
        if resp["success"] != Value::Bool(true) {
            bail!("D1 error: {}", resp["errors"]);
        }
        let results = resp["result"].as_array().context("D1 response without result")?;
        if results.len() != n {
            bail!("D1 returned {} results for {n} statements", results.len());
        }
        results
            .iter()
            .map(|r| {
                Ok(r["results"]
                    .as_array()
                    .map(|rows| rows.iter().filter_map(|row| row.as_object().cloned()).collect())
                    .unwrap_or_default())
            })
            .collect()
    }
}

fn sqlite_batch(conn: &mut rusqlite::Connection, stmts: Vec<Stmt>) -> Result<Vec<Vec<Row>>> {
    let tx = conn.transaction()?;
    let mut out = Vec::with_capacity(stmts.len());
    for s in &stmts {
        let mut prepared = tx.prepare(&s.sql).with_context(|| s.sql.clone())?;
        let names: Vec<String> = prepared.column_names().map(String::from).collect();
        let params: Vec<SqlValue> = s.params.iter().map(SqlValue).collect::<Vec<_>>();
        let mut rows = prepared.query(rusqlite::params_from_iter(params.iter()))?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let mut obj = Map::new();
            for (i, name) in names.iter().enumerate() {
                obj.insert(name.clone(), sqlite_to_json(row.get_ref(i)?));
            }
            result.push(obj);
        }
        out.push(result);
    }
    tx.commit()?;
    Ok(out)
}

struct SqlValue<'a>(&'a Value);

impl rusqlite::ToSql for SqlValue<'_> {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::{ToSqlOutput, Value as V};
        Ok(ToSqlOutput::Owned(match self.0 {
            Value::Null => V::Null,
            Value::Bool(b) => V::Integer(*b as i64),
            Value::Number(n) => n
                .as_i64()
                .map(V::Integer)
                .unwrap_or_else(|| V::Real(n.as_f64().unwrap_or(0.0))),
            Value::String(s) => V::Text(s.clone()),
            other => V::Text(other.to_string()),
        }))
    }
}

fn sqlite_to_json(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef as V;
    match v {
        V::Null => Value::Null,
        V::Integer(i) => i.into(),
        V::Real(f) => f.into(),
        V::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
        V::Blob(_) => Value::Null, // schema has no blobs; files live in object storage
    }
}

pub fn from_row<T: DeserializeOwned>(row: Row) -> Result<T> {
    Ok(serde_json::from_value(Value::Object(row))?)
}

/// Column `col` of every row, e.g. `values::<i64>(rows, "id")`. Rows where it is missing,
/// NULL or of another type are skipped; `.pop()` reads a single-row result.
pub fn values<T: DeserializeOwned>(rows: Vec<Row>, col: &str) -> Vec<T> {
    rows.into_iter()
        .filter_map(|mut r| serde_json::from_value(r.remove(col)?).ok())
        .collect()
}

/// Split a migration file into statements: drop `--` comment lines, then split on `;`.
fn split_sql(sql: &str) -> impl Iterator<Item = String> {
    let code: String = sql
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    code.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect::<Vec<_>>()
        .into_iter()
}

/// Serde helper: SQLite/D1 booleans arrive as 0/1.
pub fn bool_from_int<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    use serde::Deserialize;
    Ok(match Value::deserialize(d)? {
        Value::Bool(b) => b,
        Value::Number(n) => n.as_i64() != Some(0),
        _ => false,
    })
}

/// The one timestamp format stored in the DB (and returned by the API, like linkding):
/// `2020-09-26T09:46:23.006313Z`. Being fixed-width UTC, it sorts and compares as text.
pub fn format_time(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

pub fn now() -> String {
    format_time(chrono::Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrate_is_idempotent_and_batch_is_atomic() {
        let db = Db::sqlite(":memory:").unwrap();
        db.migrate().await.unwrap();
        db.migrate().await.unwrap();

        let ok = vec![
            stmt!(
                "INSERT INTO users (username, password_hash, date_joined) VALUES (?, 'x', ?)",
                "a",
                now()
            ),
            stmt!("SELECT username FROM users"),
        ];
        let res = db.batch(ok).await.unwrap();
        assert_eq!(res[1][0]["username"], "a");

        // Second insert violates UNIQUE(username): the whole batch must roll back.
        let bad = vec![
            stmt!(
                "INSERT INTO users (username, password_hash, date_joined) VALUES ('b', 'x', ?)",
                now()
            ),
            stmt!(
                "INSERT INTO users (username, password_hash, date_joined) VALUES ('A', 'x', ?)",
                now()
            ),
        ];
        assert!(db.batch(bad).await.is_err());
        let n: Vec<Row> = db.query(stmt!("SELECT count(*) AS n FROM users")).await.unwrap();
        assert_eq!(n[0]["n"], 1);
    }
}
