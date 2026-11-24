//! Site-wide settings (linkding `GlobalSettings`, a single row).

use anyhow::Result;

use crate::db::{Db, values};
use crate::stmt;

/// Where logged-out visitors to `/` go: `login` or `shared_bookmarks`.
pub async fn landing_page(db: &Db) -> Result<String> {
    let rows = db
        .query(stmt!("SELECT landing_page FROM global_settings WHERE id = 1"))
        .await?;
    Ok(values(rows, "landing_page").pop().unwrap_or_else(|| "login".into()))
}

pub async fn set_landing_page(db: &Db, page: &str) -> Result<()> {
    let page = if page == "shared_bookmarks" {
        "shared_bookmarks"
    } else {
        "login"
    };
    db.execute(stmt!("UPDATE global_settings SET landing_page = ? WHERE id = 1", page))
        .await
}
