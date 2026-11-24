//! Configuration from environment variables.
//!
//! Storage defaults to local disk under `LD_DATA_DIR` (default `data`):
//! - database: `<data>/linkding.db` (SQLite), or Cloudflare D1 when `D1_DATABASE_ID` is set
//! - files (favicons, previews, snapshots): `<data>/files/`, or Cloudflare R2 when `R2_BUCKET` is set
//!
//! Partially configured remote storage is an error rather than a silent fallback to local disk.

use anyhow::{Context, Result, bail};
use std::env;
use std::path::PathBuf;

use crate::db::Db;
use crate::storage::Storage;

pub struct Config {
    pub bind: String,
    pub secret_key: String,
    pub superuser: Option<(String, String)>,
    /// `LD_ALLOWED_INTERNAL_HOSTS`: hosts/IPs/CIDRs the scraper may fetch despite being
    /// non-public (`*` allows everything). Empty: only public addresses.
    pub allowed_internal_hosts: String,
    /// `LD_FAVICON_PROVIDER`, with `{url}` / `{domain}` placeholders.
    pub favicon_provider: String,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        let secret_key =
            env::var("LD_SECRET_KEY").context("LD_SECRET_KEY is required (e.g. `openssl rand -hex 32`)")?;
        if secret_key.len() < 64 {
            bail!("LD_SECRET_KEY must be at least 64 characters (e.g. `openssl rand -hex 32`)");
        }
        let superuser = match (var("LD_SUPERUSER_NAME"), var("LD_SUPERUSER_PASSWORD")) {
            (Some(name), Some(pass)) => Some((name, pass)),
            _ => None,
        };
        Ok(Config {
            bind: var("LD_BIND").unwrap_or_else(|| "0.0.0.0:9090".into()),
            secret_key,
            superuser,
            allowed_internal_hosts: var("LD_ALLOWED_INTERNAL_HOSTS").unwrap_or_default(),
            favicon_provider: var("LD_FAVICON_PROVIDER").unwrap_or_else(|| {
                "https://t1.gstatic.com/faviconV2?client=SOCIAL&type=FAVICON&fallback_opts=TYPE,SIZE,URL&url={url}&size=32"
                    .into()
            }),
        })
    }
}

/// Non-empty env var.
fn var(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn require(key: &str, because: &str) -> Result<String> {
    var(key).with_context(|| format!("{key} is required when {because} is set"))
}

pub fn data_dir() -> PathBuf {
    PathBuf::from(var("LD_DATA_DIR").unwrap_or_else(|| "data".into()))
}

/// Cloudflare D1 when `D1_DATABASE_ID` is set, otherwise SQLite in the data dir.
pub fn db_from_env() -> Result<Db> {
    if var("D1_DATABASE_ID").is_some() {
        let id = require("D1_DATABASE_ID", "D1_DATABASE_ID")?;
        tracing::info!("database: Cloudflare D1 {id}");
        return Ok(Db::d1(
            &require("CF_ACCOUNT_ID", "D1_DATABASE_ID")?,
            &id,
            &require("CF_API_TOKEN", "D1_DATABASE_ID")?,
        ));
    }
    let path = data_dir().join("linkding.db");
    tracing::info!("database: SQLite {}", path.display());
    Db::sqlite(path.to_str().context("data dir path is not UTF-8")?)
}

/// Cloudflare R2 when `R2_BUCKET` is set, otherwise the local `files` dir in the data dir.
pub fn storage_from_env() -> Result<Storage> {
    if let Some(bucket) = var("R2_BUCKET") {
        let endpoint = match var("R2_ENDPOINT") {
            Some(e) => e,
            None => format!(
                "https://{}.r2.cloudflarestorage.com",
                require("CF_ACCOUNT_ID", "R2_BUCKET (or set R2_ENDPOINT)")?
            ),
        };
        tracing::info!("files: Cloudflare R2 bucket {bucket}");
        return Storage::r2(
            &endpoint,
            &bucket,
            &require("R2_ACCESS_KEY_ID", "R2_BUCKET")?,
            &require("R2_SECRET_ACCESS_KEY", "R2_BUCKET")?,
        );
    }
    let dir = data_dir().join("files");
    tracing::info!("files: local {}", dir.display());
    Storage::local(&dir)
}
