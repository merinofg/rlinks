-- Mirrors linkding's bookmarks/models.py. Booleans are INTEGER 0/1, timestamps ISO-8601 UTC TEXT.
-- Statements are split on ';' by the migration runner: no triggers or ';' inside literals.

CREATE TABLE users (
  id            INTEGER PRIMARY KEY,
  username      TEXT    NOT NULL UNIQUE COLLATE NOCASE,
  password_hash TEXT    NOT NULL,
  is_admin      INTEGER NOT NULL DEFAULT 0,
  is_active     INTEGER NOT NULL DEFAULT 1,
  date_joined   TEXT    NOT NULL,
  -- Carried in the signed session cookie and checked on each request. Rotated on password
  -- change/reset and deactivation, which ends existing sessions; random per user, so a deleted
  -- user's cookie never matches a new account that reuses the id.
  session_nonce TEXT    NOT NULL DEFAULT (lower(hex(randomblob(16))))
);

CREATE TABLE user_profiles (
  user_id                         INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  theme                           TEXT    NOT NULL DEFAULT 'auto',
  bookmark_date_display           TEXT    NOT NULL DEFAULT 'relative',
  bookmark_description_display    TEXT    NOT NULL DEFAULT 'inline',
  bookmark_description_max_lines  INTEGER NOT NULL DEFAULT 1,
  bookmark_link_target            TEXT    NOT NULL DEFAULT '_blank',
  web_archive_integration         TEXT    NOT NULL DEFAULT 'disabled',
  tag_search                      TEXT    NOT NULL DEFAULT 'strict',
  tag_grouping                    TEXT    NOT NULL DEFAULT 'alphabetical',
  enable_sharing                  INTEGER NOT NULL DEFAULT 0,
  enable_public_sharing           INTEGER NOT NULL DEFAULT 0,
  enable_favicons                 INTEGER NOT NULL DEFAULT 0,
  enable_preview_images           INTEGER NOT NULL DEFAULT 0,
  display_url                     INTEGER NOT NULL DEFAULT 0,
  display_view_bookmark_action    INTEGER NOT NULL DEFAULT 1,
  display_edit_bookmark_action    INTEGER NOT NULL DEFAULT 1,
  display_archive_bookmark_action INTEGER NOT NULL DEFAULT 1,
  display_remove_bookmark_action  INTEGER NOT NULL DEFAULT 1,
  permanent_notes                 INTEGER NOT NULL DEFAULT 0,
  custom_css                      TEXT    NOT NULL DEFAULT '',
  custom_css_hash                 TEXT    NOT NULL DEFAULT '',
  auto_tagging_rules              TEXT    NOT NULL DEFAULT '',
  search_preferences              TEXT    NOT NULL DEFAULT '{}',
  enable_automatic_html_snapshots INTEGER NOT NULL DEFAULT 1,
  default_mark_unread             INTEGER NOT NULL DEFAULT 0,
  default_mark_shared             INTEGER NOT NULL DEFAULT 0,
  items_per_page                  INTEGER NOT NULL DEFAULT 30 CHECK (items_per_page >= 10),
  collapse_side_panel             INTEGER NOT NULL DEFAULT 0,
  hide_bundles                    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE bookmarks (
  id                       INTEGER PRIMARY KEY,
  url                      TEXT    NOT NULL,
  url_normalized           TEXT    NOT NULL DEFAULT '',
  title                    TEXT    NOT NULL DEFAULT '',
  description              TEXT    NOT NULL DEFAULT '',
  notes                    TEXT    NOT NULL DEFAULT '',
  web_archive_snapshot_url TEXT    NOT NULL DEFAULT '',
  favicon_file             TEXT    NOT NULL DEFAULT '',
  preview_image_file       TEXT    NOT NULL DEFAULT '',
  unread                   INTEGER NOT NULL DEFAULT 0,
  is_archived              INTEGER NOT NULL DEFAULT 0,
  shared                   INTEGER NOT NULL DEFAULT 0,
  date_added               TEXT    NOT NULL,
  date_modified            TEXT    NOT NULL,
  date_accessed            TEXT,
  owner_id                 INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  latest_snapshot_id       INTEGER REFERENCES assets(id) ON DELETE SET NULL
);
CREATE INDEX bookmarks_owner_list ON bookmarks(owner_id, is_archived, date_added);
CREATE INDEX bookmarks_owner_url ON bookmarks(owner_id, url_normalized);
CREATE INDEX bookmarks_shared ON bookmarks(shared, date_added);

CREATE TABLE tags (
  id         INTEGER PRIMARY KEY,
  name       TEXT    NOT NULL COLLATE NOCASE,
  date_added TEXT    NOT NULL,
  owner_id   INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  UNIQUE (owner_id, name)
);

CREATE TABLE bookmark_tags (
  bookmark_id INTEGER NOT NULL REFERENCES bookmarks(id) ON DELETE CASCADE,
  tag_id      INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  PRIMARY KEY (bookmark_id, tag_id)
);
CREATE INDEX bookmark_tags_tag ON bookmark_tags(tag_id);

CREATE TABLE bundles (
  id            INTEGER PRIMARY KEY,
  name          TEXT    NOT NULL,
  search        TEXT    NOT NULL DEFAULT '',
  any_tags      TEXT    NOT NULL DEFAULT '',
  all_tags      TEXT    NOT NULL DEFAULT '',
  excluded_tags TEXT    NOT NULL DEFAULT '',
  filter_unread TEXT    NOT NULL DEFAULT 'off',
  filter_shared TEXT    NOT NULL DEFAULT 'off',
  "order"       INTEGER NOT NULL DEFAULT 0,
  date_created  TEXT    NOT NULL,
  date_modified TEXT    NOT NULL,
  owner_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE
);

CREATE TABLE assets (
  id           INTEGER PRIMARY KEY,
  -- Random public id for web URLs (/assets/<uid>/...), so URLs don't reveal how many files exist
  -- and can't be guessed. The REST API uses `id` (linkding compatible).
  uid          TEXT    NOT NULL UNIQUE DEFAULT (lower(hex(randomblob(16)))),
  bookmark_id  INTEGER NOT NULL REFERENCES bookmarks(id) ON DELETE CASCADE,
  date_created TEXT    NOT NULL,
  file         TEXT    NOT NULL DEFAULT '',
  file_size    INTEGER,
  asset_type   TEXT    NOT NULL,
  content_type TEXT    NOT NULL,
  display_name TEXT    NOT NULL DEFAULT '',
  status       TEXT    NOT NULL,
  gzip         INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX assets_bookmark ON assets(bookmark_id);
CREATE INDEX assets_status ON assets(status);

CREATE TABLE api_tokens (
  id      INTEGER PRIMARY KEY,
  key     TEXT    NOT NULL UNIQUE,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name    TEXT    NOT NULL,
  created TEXT    NOT NULL
);

CREATE TABLE feed_tokens (
  key     TEXT    PRIMARY KEY,
  user_id INTEGER NOT NULL UNIQUE REFERENCES users(id) ON DELETE CASCADE,
  created TEXT    NOT NULL
);

CREATE TABLE toasts (
  id           INTEGER PRIMARY KEY,
  key          TEXT    NOT NULL,
  message      TEXT    NOT NULL,
  acknowledged INTEGER NOT NULL DEFAULT 0,
  owner_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE
);

CREATE TABLE global_settings (
  id                    INTEGER PRIMARY KEY CHECK (id = 1),
  landing_page          TEXT    NOT NULL DEFAULT 'login',
  guest_profile_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
  enable_link_prefetch  INTEGER NOT NULL DEFAULT 0
);
INSERT INTO global_settings (id) VALUES (1);
