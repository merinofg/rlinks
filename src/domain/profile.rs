use anyhow::Result;
use serde::Deserialize;

use crate::db::{Db, Stmt, bool_from_int};
use crate::domain::bookmarks::checkbox;
use crate::domain::tokens;
use crate::stmt;

/// User profile settings (linkding `UserProfile`).
#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub username: String,
    #[serde(deserialize_with = "bool_from_int")]
    pub is_admin: bool,
    /// auto | light | dark
    pub theme: String,
    /// relative | absolute | hidden
    pub bookmark_date_display: String,
    pub bookmark_description_max_lines: i64,
    /// _blank | _self
    pub bookmark_link_target: String,
    /// enabled | disabled: show Wayback Machine links
    pub web_archive_integration: String,
    /// strict | lax
    pub tag_search: String,
    /// alphabetical | disabled
    pub tag_grouping: String,
    #[serde(deserialize_with = "bool_from_int")]
    pub display_url: bool,
    #[serde(deserialize_with = "bool_from_int")]
    pub permanent_notes: bool,
    pub custom_css: String,
    pub custom_css_hash: String,
    #[serde(deserialize_with = "bool_from_int")]
    pub enable_sharing: bool,
    #[serde(deserialize_with = "bool_from_int")]
    pub enable_public_sharing: bool,
    #[serde(deserialize_with = "bool_from_int")]
    pub default_mark_unread: bool,
    #[serde(deserialize_with = "bool_from_int")]
    pub default_mark_shared: bool,
    pub items_per_page: i64,
    #[serde(deserialize_with = "bool_from_int")]
    pub hide_bundles: bool,
    #[serde(deserialize_with = "bool_from_int")]
    pub enable_automatic_html_snapshots: bool,
    #[serde(deserialize_with = "bool_from_int")]
    pub enable_favicons: bool,
    #[serde(deserialize_with = "bool_from_int")]
    pub enable_preview_images: bool,
    pub auto_tagging_rules: String,
    /// JSON object (linkding's saved search defaults), passed through by the REST API.
    pub search_preferences: String,
}

impl Profile {
    /// Display settings for anonymous visitors (the column defaults).
    pub fn guest() -> Profile {
        Profile {
            username: String::new(),
            is_admin: false,
            theme: "auto".into(),
            bookmark_date_display: "relative".into(),
            bookmark_description_max_lines: 1,
            bookmark_link_target: "_blank".into(),
            web_archive_integration: "disabled".into(),
            tag_search: "strict".into(),
            tag_grouping: "alphabetical".into(),
            display_url: false,
            permanent_notes: false,
            custom_css: String::new(),
            custom_css_hash: String::new(),
            enable_sharing: false,
            enable_public_sharing: false,
            default_mark_unread: false,
            default_mark_shared: false,
            items_per_page: 30,
            hide_bundles: true,
            enable_automatic_html_snapshots: false,
            enable_favicons: false,
            enable_preview_images: false,
            auto_tagging_rules: String::new(),
            search_preferences: "{}".into(),
        }
    }

    pub fn lax(&self) -> bool {
        self.tag_search == "lax"
    }

    /// Value of the `prefs` cookie: `<theme>:<custom css hash>:<admin 0|1>`. Lets every page apply
    /// the theme and custom CSS, and show admin-only links, without reading the profile (see
    /// base.html). Purely cosmetic: the server checks admin rights on every admin request.
    pub fn prefs_cookie(&self) -> String {
        format!("{}:{}:{}", self.theme, self.custom_css_hash, self.is_admin as u8)
    }
}

/// Loads the profile of an *active* user; returns no row for deactivated or deleted accounts,
/// which handlers treat as logged out. Meant to ride along in a page's batch.
pub fn stmt(user_id: i64) -> Stmt {
    stmt!(
        "SELECT u.username, u.is_admin, p.* FROM users u JOIN user_profiles p ON p.user_id = u.id \
         WHERE u.id = ? AND u.is_active = 1",
        user_id
    )
}

pub async fn get(db: &Db, user_id: i64) -> Result<Option<Profile>> {
    Ok(db.query(stmt(user_id)).await?.pop())
}

/// The user's profile, or the guest defaults when the account is gone (e.g. deleted meanwhile).
pub async fn get_or_guest(db: &Db, user_id: i64) -> Result<Profile> {
    Ok(get(db, user_id).await?.unwrap_or_else(Profile::guest))
}

/// The General settings form (checkboxes are absent when unticked).
#[derive(Deserialize)]
pub struct Settings {
    pub theme: String,
    pub bookmark_date_display: String,
    pub bookmark_description_max_lines: i64,
    pub bookmark_link_target: String,
    pub tag_search: String,
    pub tag_grouping: String,
    pub items_per_page: i64,
    #[serde(default, deserialize_with = "checkbox")]
    pub display_url: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub permanent_notes: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub hide_bundles: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub web_archive_links: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub enable_favicons: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub enable_preview_images: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub enable_automatic_html_snapshots: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub enable_sharing: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub enable_public_sharing: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub default_mark_unread: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub default_mark_shared: bool,
    #[serde(default)]
    pub auto_tagging_rules: String,
    #[serde(default)]
    pub custom_css: String,
}

impl Settings {
    pub fn validate(&self) -> Result<(), &'static str> {
        let one_of = |v: &str, allowed: &[&str]| allowed.contains(&v);
        if !one_of(&self.theme, &["auto", "light", "dark"])
            || !one_of(&self.bookmark_date_display, &["relative", "absolute", "hidden"])
            || !one_of(&self.bookmark_link_target, &["_blank", "_self"])
            || !one_of(&self.tag_search, &["strict", "lax"])
            || !one_of(&self.tag_grouping, &["alphabetical", "disabled"])
        {
            return Err("One of the choices isn't valid.");
        }
        if !(10..=1000).contains(&self.items_per_page) {
            return Err("Bookmarks per page must be between 10 and 1000.");
        }
        if !(1..=10).contains(&self.bookmark_description_max_lines) {
            return Err("Description lines must be between 1 and 10.");
        }
        if self.custom_css.len() > 100_000 {
            return Err("Custom CSS is limited to 100 KB.");
        }
        Ok(())
    }
}

/// Saves the General settings form (validate it first).
pub fn save_stmt(user_id: i64, f: &Settings) -> Stmt {
    let css_hash = match f.custom_css.trim() {
        "" => String::new(),
        css => tokens::hash(css)[..16].to_string(),
    };
    stmt!(
        "UPDATE user_profiles SET theme = ?, bookmark_date_display = ?, bookmark_description_max_lines = ?, \
                 bookmark_link_target = ?, tag_search = ?, tag_grouping = ?, items_per_page = ?, display_url = ?, \
                 permanent_notes = ?, hide_bundles = ?, web_archive_integration = ?, enable_favicons = ?, \
                 enable_preview_images = ?, default_mark_unread = ?, default_mark_shared = ?, \
                 enable_sharing = ?, enable_public_sharing = ?, enable_automatic_html_snapshots = ?, \
                 auto_tagging_rules = ?, custom_css = ?, custom_css_hash = ? WHERE user_id = ?",
        f.theme,
        f.bookmark_date_display,
        f.bookmark_description_max_lines,
        f.bookmark_link_target,
        f.tag_search,
        f.tag_grouping,
        f.items_per_page,
        f.display_url,
        f.permanent_notes,
        f.hide_bundles,
        if f.web_archive_links { "enabled" } else { "disabled" },
        f.enable_favicons,
        f.enable_preview_images,
        f.default_mark_unread,
        f.default_mark_shared,
        f.enable_sharing,
        f.enable_sharing && f.enable_public_sharing,
        f.enable_automatic_html_snapshots,
        f.auto_tagging_rules,
        f.custom_css,
        css_hash,
        user_id
    )
}
