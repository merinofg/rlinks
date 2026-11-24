# rlinks

A fast, self-hosted bookmark manager: tag, search and archive your links, read later, and save
pages straight from your browser. rlinks is a Rust + htmx port of
[linkding](https://github.com/sissbruecker/linkding) and speaks its REST API, so linkding's
[browser extension](https://github.com/sissbruecker/linkding-extension) and third-party apps work
with it unchanged.

It ships as a single binary (templates, static files and migrations are compiled in). Data lives
on local disk, or in Cloudflare D1 (database) and R2 (files), so the app can run anywhere while
the data stays put.

## Features

- **Bookmarks**: tags, notes (Markdown), unread and archived states, bulk actions, and duplicate
  detection by normalized URL. Titles, descriptions, favicons and preview images are fetched
  automatically.
- **Search**: linkding's query language, e.g. `rust #book not #draft`, `"exact phrase"`,
  `!unread`, `!untagged`, with `and` / `or` / parentheses. It comes with a tag cloud and
  sorting, plus **bundles** (saved filters shown in the sidebar).
- **Auto-tagging** rules per URL pattern.
- **Files**: each bookmark can hold HTML snapshots and uploads (PDFs and the like).
  - A snapshot is a self-contained copy of the page, taken on demand or automatically for new
    bookmarks. The browser extension can also send a SingleFile capture.
  - Snapshots and uploads are served sandboxed (no scripts, no network).
- **Multi-user**: admins manage accounts. Users can share bookmarks with each other or
  publicly, and a public shared page can be the landing page.
- **Import / export** of browser bookmark files (Netscape HTML), **RSS feeds**, OpenSearch,
  installable as an app (PWA), and light / dark / custom CSS themes.

## Quick start

```sh
export LD_SECRET_KEY=$(openssl rand -hex 32)
export LD_SUPERUSER_NAME=admin LD_SUPERUSER_PASSWORD=change-me
cargo run --release -- serve                    # http://localhost:9090
```

With Docker Compose (`compose.yaml` lists every setting; the Cloudflare ones are commented out,
so by default everything is stored in the `rlinks-data` volume):

```sh
echo "LD_SECRET_KEY=$(openssl rand -hex 32)" >> .env
echo "LD_SUPERUSER_NAME=admin" >> .env; echo "LD_SUPERUSER_PASSWORD=change-me" >> .env
docker compose up -d                            # http://localhost:9090
```

Commands:

| Command | What it does |
|---|---|
| `rlinks serve` (default) | Runs migrations, then serves the app |
| `rlinks migrate` | Runs migrations only |
| `rlinks create-user <name> <password> [--admin]` | Adds a user |

Keep `LD_SECRET_KEY` the same between restarts: a new key logs everyone out and changes the
names of stored favicon and preview files. The simplest way is to keep it in a `.env` file (see
`.env.example`) and load it with `set -a; . ./.env; set +a`.

## Configuration

All settings are environment variables. `.env.example` lists them with comments.

| Variable | Default | Purpose |
|---|---|---|
| `LD_SECRET_KEY` | **required** | Key for signing session cookies and naming stored files (≥ 64 characters). |
| `LD_BIND` | `0.0.0.0:9090` | Listen address. |
| `LD_DATA_DIR` | `data` | Local storage: database in `linkding.db`, files in `files/`. |
| `LD_SUPERUSER_NAME`, `LD_SUPERUSER_PASSWORD` | – | Admin account created on startup if it doesn't exist. |
| `LD_ALLOWED_INTERNAL_HOSTS` | – (public addresses only) | Internal hosts the server may fetch from (comma-separated: hostnames, `.domain` for subdomains, IPs, CIDRs, or `*`). |
| `LD_FAVICON_PROVIDER` | Google's favicon service | Favicon URL template with `{url}` or `{domain}`. |
| `RUST_LOG` | `info` | Log filter (`tracing` env-filter syntax). |

The database and the files can each move to Cloudflare, independently of each other and of
where the app runs:

| Variables | Effect |
|---|---|
| `CF_ACCOUNT_ID`, `D1_DATABASE_ID`, `CF_API_TOKEN` | Database in Cloudflare D1 instead of SQLite. The token needs "D1 Edit"; create the database with `npx wrangler d1 create rlinks`. |
| `R2_BUCKET`, `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`, `R2_ENDPOINT` (optional) | Files in a Cloudflare R2 bucket (S3 API credentials). The endpoint defaults to `https://$CF_ACCOUNT_ID.r2.cloudflarestorage.com`. |

A backend that's only partly configured stops the server at startup instead of silently falling
back to local disk.

Per-user preferences (theme, page size, sharing, automatic snapshots, auto-tagging rules, …) are
set in the app under **Settings**. Admins manage users and the landing page under **Settings →
Users**.

## REST API and browser extension

The API matches linkding's: the same paths (with trailing slashes), `Authorization: Token <key>`,
the same JSON and `{count, next, previous, results}` pagination. To use it:

1. Create a token under **Settings → Integrations**.
2. Point the extension, or any client, at this server's URL.

```sh
curl -H "Authorization: Token <token>" "http://localhost:9090/api/bookmarks/?q=%23rust"
```

| Resource | Endpoints |
|---|---|
| Bookmarks | list, archived, shared, check, create, retrieve, update, delete, archive, unarchive |
| Assets | list, upload, download, delete, and the extension's `singlefile` upload |
| Tags | list, retrieve, create |
| Bundles | full CRUD |
| User | `/api/user/profile/` |

## Architecture

One crate, server-rendered HTML with [htmx](https://htmx.org) for partial updates, and no
JavaScript build step.

```
web/, api/   HTTP: parse input, render HTML (askama) or JSON. No SQL here.
   │
domain/      Rules and their SQL: bookmarks, tags, bundles, assets, users, profile, tokens…
   │
db.rs        Db::batch(statements) → rows, against D1 (REST) or SQLite
storage.rs   file storage via object_store: R2 or local directory
```

Other modules:

| Module | Purpose |
|---|---|
| `auth.rs` | Signed session cookies, a CSRF guard (Origin / Sec-Fetch-Site + SameSite), API tokens, and a small TTL cache for session and token checks |
| `search.rs` | Parser for the search language, compiled to a SQL `WHERE` clause |
| `jobs.rs` | In-process background worker for favicons, preview images and snapshots. The database itself is the queue: unfinished work is re-queued on startup |
| `scrape.rs` | HTTP fetching of user-supplied URLs behind an SSRF guard. DNS answers are checked in the resolver, so every redirect hop is covered too |
| `snapshot.rs` | Builds a self-contained HTML snapshot: stylesheets and images inlined, scripts removed |
| `netscape.rs` | Import and export of browser bookmark files |

Design rules that come from D1:

- **Few round-trips.** Every D1 query is an HTTPS call that counts against the account's rate
  limit.
  - A page render is one `Db::batch` call (profile, list, count and tag cloud together), and a
    write takes at most two.
  - Sessions are stateless cookies, so a normal request needs no extra lookup.
- **No interactive transactions.** A batch is atomic, so writes are written as single batches
  (`INSERT … SELECT`, `ON CONFLICT`, `RETURNING`).
- **A cap of 100 parameters per statement.** Lists (ids, tag names) are bound as one JSON
  parameter and expanded in SQL with `json_each(?)`.

Both database backends share the same SQL and the same `migrations/0001_init.sql`, and they
return rows as JSON objects that serde maps onto structs. SQLite also backs the tests.

## Development

```sh
cargo test                                      # unit + router integration tests (SQLite)
cargo clippy --all-targets -- -D warnings && cargo fmt --check
cargo test --test d1 -- --ignored --nocapture   # D1 REST check: needs CF_* env and a throwaway DB
```
