//! Bookmark pages. The list is one form: row buttons (`name="archive" value="<id>"`) and the
//! bulk bar post to `<list>/action?<current query>`. For htmx requests the write statements and
//! the re-render statements go out in the same batch: one D1 round-trip per action.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::{Form, SignedCookieJar};
use serde::Deserialize;

use super::{AppError, ago, file_url, is_htmx, markdown, render};
use crate::db::{Stmt, from_row};
use crate::domain::bookmarks::{self as bm, Action, Bookmark, BookmarkInput, ListQuery, Paging, Scope, Updated};
use crate::domain::bundles::{self, Bundle};
use crate::domain::profile::{self, Profile};
use crate::{AppState, auth, auth::CurrentUser, search};

// ---------------------------------------------------------------------------------------------
// List

struct Link {
    name: String,
    href: String,
}

struct TagGroup {
    letter: String,
    links: Vec<Link>,
}

struct BookmarkRow {
    b: Bookmark,
    /// Per the profile's date display: "3 days ago", "2024-05-01", or hidden.
    date: Option<String>,
    tags: Vec<Link>,
    favicon: Option<String>,
    preview: Option<String>,
    /// Rendered notes, when the profile shows notes in the list.
    notes_html: Option<String>,
    /// Wayback Machine link, when the profile enables it.
    archive_url: Option<String>,
}

#[derive(Template)]
#[template(path = "bookmarks.html")]
struct ListPage {
    heading: &'static str,
    archived: bool,
    base: &'static str,
    q: ListQuery,
    action_url: String,
    return_url: String,
    rows: Vec<BookmarkRow>,
    total: i64,
    page: i64,
    pages: i64,
    prev: Option<String>,
    next: Option<String>,
    selected_tags: Vec<Link>,
    cloud: Vec<TagGroup>,
    sorts: &'static [(&'static str, &'static str)],
    link_target: String,
    show_favicons: bool,
    show_previews: bool,
    display_url: bool,
    hide_bundles: bool,
    /// Description line clamp.
    desc_lines: i64,
    bundles: Vec<BundleLink>,
    /// Link that drops the selected bundle, when one is selected.
    clear_bundle: Option<String>,
    /// Read-only list of other users' shared bookmarks.
    shared_view: bool,
    /// No one logged in (public shared page).
    anonymous: bool,
    /// The viewer has sharing enabled: offer share/unshare.
    sharing: bool,
    /// Shared view: users with shared bookmarks (as filter links).
    users: Vec<BundleLink>,
    clear_user: Option<String>,
}

struct BundleLink {
    name: String,
    href: String,
    active: bool,
}

fn base(scope: Scope) -> &'static str {
    match scope {
        Scope::Active => "/bookmarks",
        Scope::Archived => "/bookmarks/archived",
        Scope::Shared { .. } => "/bookmarks/shared",
    }
}

/// `base?q=..&sort=..` with empty values dropped.
fn href(base: &str, q: &ListQuery, query: &str, page: i64) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    let page = page.to_string();
    for (k, v) in [
        ("q", query),
        ("sort", &q.sort),
        ("unread", &q.unread),
        ("shared", &q.shared),
    ] {
        if !v.is_empty() {
            s.append_pair(k, v);
        }
    }
    if let Some(b) = q.bundle {
        s.append_pair("bundle", &b.to_string());
    }
    if !q.user.is_empty() {
        s.append_pair("user", &q.user);
    }
    if page != "1" {
        s.append_pair("page", &page);
    }
    let qs = s.finish();
    if qs.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{qs}")
    }
}

/// Append `#tag` to the current query (tag-cloud / row tag click).
fn with_tag(q: &str, tag: &str) -> String {
    format!("{} #{tag}", q.trim()).trim().to_string()
}

/// Profile, sidebar bundles and list statements for one page (decoded by `render_list`).
fn page_stmts(user: i64, scope: Scope, q: &ListQuery, bundle: Option<&Bundle>) -> Vec<Stmt> {
    let mut stmts = vec![profile::stmt(user), bundles::list_stmt(user)];
    stmts.extend(bm::list_stmts(user, scope, q, bundle, Paging::Page(q.page())));
    stmts
}

/// The selected bundle, if any. Costs a round-trip, only when a bundle is selected: its filter
/// is compiled to SQL in Rust before the page batch can be built.
async fn selected_bundle(s: &AppState, user: i64, q: &ListQuery) -> Result<Option<Bundle>, AppError> {
    Ok(match q.bundle {
        Some(id) => bundles::get(&s.db, user, id).await?,
        None => None,
    })
}

fn render_list(
    scope: Scope,
    q: ListQuery,
    mut rows: Vec<Vec<crate::db::Row>>,
    jar: SignedCookieJar,
) -> Result<Response, AppError> {
    let list = rows.split_off(2);
    let sidebar_bundles: Vec<Bundle> = rows
        .pop()
        .unwrap_or_default()
        .into_iter()
        .map(from_row)
        .collect::<Result<_, _>>()?;
    let Some(profile) = rows.pop().and_then(|mut r| r.pop()) else {
        // Deactivated or deleted account still holding a valid cookie.
        return Ok((auth::logout(jar), Redirect::to("/login")).into_response());
    };
    let profile: Profile = from_row(profile)?;
    let page = list_page(scope, q, &profile, sidebar_bundles, bm::list_result(list)?, false);
    Ok(render(&page)?.into_response())
}

/// The list page view for any scope; `profile` is the viewer's (a guest profile when anonymous).
fn list_page(
    scope: Scope,
    q: ListQuery,
    profile: &Profile,
    sidebar_bundles: Vec<Bundle>,
    res: bm::ListResult,
    anonymous: bool,
) -> ListPage {
    let base = base(scope);
    let page = q.page();
    let pages = ((res.total + profile.items_per_page - 1) / profile.items_per_page).max(1);

    let selected = search::tag_names(&q.q, profile.lax());
    let is_selected = |t: &str| selected.iter().any(|s| s.eq_ignore_ascii_case(t));
    let mut cloud: Vec<TagGroup> = Vec::new();
    let grouped = profile.tag_grouping != "disabled";
    for t in res.tags.iter().filter(|t| !is_selected(t)) {
        let letter = match grouped {
            true => t.chars().next().unwrap_or('#').to_uppercase().to_string(),
            false => String::new(),
        };
        let link = Link {
            name: t.clone(),
            href: href(base, &q, &with_tag(&q.q, t), 1),
        };
        match cloud.last_mut() {
            Some(g) if g.letter == letter => g.links.push(link),
            _ => cloud.push(TagGroup {
                letter,
                links: vec![link],
            }),
        }
    }
    let selected_tags = res
        .tags
        .iter()
        .filter(|t| is_selected(t))
        .map(|t| Link {
            name: t.clone(),
            href: href(base, &q, &search::strip_tag(&q.q, t, profile.lax()), 1),
        })
        .collect();
    let rows = res
        .bookmarks
        .into_iter()
        .map(|b| BookmarkRow {
            date: match profile.bookmark_date_display.as_str() {
                "hidden" => None,
                "absolute" => Some(b.date_added.get(..10).unwrap_or_default().to_string()),
                _ => Some(ago(&b.date_added)),
            },
            notes_html: (profile.permanent_notes && !b.notes.is_empty()).then(|| markdown(&b.notes)),
            archive_url: (profile.web_archive_integration == "enabled")
                .then(|| b.web_archive_url())
                .flatten(),
            favicon: file_url("favicons", &b.favicon_file),
            preview: file_url("previews", &b.preview_image_file),
            tags: b
                .tag_names
                .iter()
                .map(|t| Link {
                    name: t.clone(),
                    href: href(base, &q, &with_tag(&q.q, t), 1),
                })
                .collect(),
            b,
        })
        .collect();
    let current = href(base, &q, &q.q, page);
    let shared_view = matches!(scope, Scope::Shared { .. });
    ListPage {
        heading: match scope {
            Scope::Active => "Bookmarks",
            Scope::Archived => "Archived bookmarks",
            Scope::Shared { .. } => "Shared bookmarks",
        },
        archived: scope == Scope::Archived,
        base,
        action_url: current.replacen(base, &format!("{base}/action"), 1),
        return_url: current,
        prev: (page > 1).then(|| href(base, &q, &q.q, page - 1)),
        next: (page < pages).then(|| href(base, &q, &q.q, page + 1)),
        page,
        pages,
        total: res.total,
        rows,
        selected_tags,
        cloud,
        sorts: bm::SORTS,
        link_target: profile.bookmark_link_target.clone(),
        bundles: sidebar_bundles
            .iter()
            .map(|b| BundleLink {
                name: b.name.clone(),
                href: format!("{base}?bundle={}", b.id),
                active: q.bundle == Some(b.id),
            })
            .collect(),
        clear_bundle: q.bundle.map(|_| {
            href(
                base,
                &ListQuery {
                    bundle: None,
                    ..q.clone()
                },
                &q.q,
                1,
            )
        }),
        users: res
            .users
            .iter()
            .map(|u| BundleLink {
                name: u.clone(),
                href: href(
                    base,
                    &ListQuery {
                        user: u.clone(),
                        ..q.clone()
                    },
                    &q.q,
                    1,
                ),
                active: q.user == *u,
            })
            .collect(),
        clear_user: (!q.user.is_empty()).then(|| {
            href(
                base,
                &ListQuery {
                    user: String::new(),
                    ..q.clone()
                },
                &q.q,
                1,
            )
        }),
        show_favicons: profile.enable_favicons,
        display_url: profile.display_url,
        hide_bundles: profile.hide_bundles || shared_view,
        desc_lines: profile.bookmark_description_max_lines,
        show_previews: profile.enable_preview_images,
        sharing: profile.enable_sharing,
        shared_view,
        anonymous,
        q,
    }
}

/// Other users' shared bookmarks. Logged-in viewers see everything shared; anonymous visitors
/// only bookmarks of users who allow public sharing.
pub async fn shared(
    State(s): State<AppState>,
    viewer: Result<CurrentUser, Response>,
    Query(q): Query<ListQuery>,
) -> Result<Response, AppError> {
    let viewer = viewer.ok().map(|CurrentUser(id)| id);
    let scope = Scope::Shared {
        public_only: viewer.is_none(),
    };
    let mut stmts: Vec<Stmt> = viewer.map(profile::stmt).into_iter().collect();
    stmts.extend(bm::list_stmts(
        viewer.unwrap_or(0),
        scope,
        &q,
        None,
        Paging::Page(q.page()),
    ));
    let mut rows = s.db.batch(stmts).await?;
    let profile = match viewer {
        Some(_) => rows.remove(0).pop().map(from_row::<Profile>).transpose()?,
        None => None,
    };
    // A stale session of a deactivated account browses like a guest (public bookmarks only).
    let (profile, anonymous) = match profile {
        Some(p) => (p, false),
        None if viewer.is_some() => {
            return Ok(Redirect::to("/login").into_response());
        }
        None => (Profile::guest(), true),
    };
    let page = list_page(scope, q, &profile, vec![], bm::list_result(rows)?, anonymous);
    Ok(render(&page)?.into_response())
}

pub async fn index(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(q): Query<ListQuery>,
    jar: SignedCookieJar,
) -> Result<Response, AppError> {
    list(s, user, Scope::Active, q, jar).await
}

pub async fn archived(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(q): Query<ListQuery>,
    jar: SignedCookieJar,
) -> Result<Response, AppError> {
    list(s, user, Scope::Archived, q, jar).await
}

async fn list(s: AppState, user: i64, scope: Scope, q: ListQuery, jar: SignedCookieJar) -> Result<Response, AppError> {
    let bundle = selected_bundle(&s, user, &q).await?;
    let rows = s.db.batch(page_stmts(user, scope, &q, bundle.as_ref())).await?;
    render_list(scope, q, rows, jar)
}

// ---------------------------------------------------------------------------------------------
// Row + bulk actions

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ActionForm {
    archive: Option<i64>,
    unarchive: Option<i64>,
    remove: Option<i64>,
    mark_as_read: Option<i64>,
    unshare: Option<i64>,
    bulk_execute: Option<String>,
    bulk_action: String,
    bulk_tag_string: String,
    bookmark_id: Vec<i64>,
}

impl ActionForm {
    fn action(&self) -> Option<(Action, Vec<i64>)> {
        let single = [
            (self.archive, Action::Archive),
            (self.unarchive, Action::Unarchive),
            (self.remove, Action::Delete),
            (self.mark_as_read, Action::MarkRead),
            (self.unshare, Action::Unshare),
        ];
        if let Some((id, action)) = single.iter().find_map(|(id, a)| id.map(|id| (id, *a))) {
            return Some((action, vec![id]));
        }
        self.bulk_execute.as_ref()?;
        Some((Action::parse(&self.bulk_action)?, self.bookmark_id.clone()))
    }
}

pub async fn action(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
    jar: SignedCookieJar,
    Form(form): Form<ActionForm>,
) -> Result<Response, AppError> {
    act(s, user, Scope::Active, q, headers, jar, form).await
}

pub async fn archived_action(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
    jar: SignedCookieJar,
    Form(form): Form<ActionForm>,
) -> Result<Response, AppError> {
    act(s, user, Scope::Archived, q, headers, jar, form).await
}

async fn act(
    s: AppState,
    user: i64,
    scope: Scope,
    q: ListQuery,
    headers: HeaderMap,
    jar: SignedCookieJar,
    form: ActionForm,
) -> Result<Response, AppError> {
    let action = form.action();
    // Deleting bookmarks: note their files first, remove them once the rows are gone.
    let files = match &action {
        Some((Action::Delete, ids)) => crate::domain::assets::files_of(&s.db, user, ids).await?,
        _ => vec![],
    };
    let writes = match action {
        Some((action, ids)) => bm::action_stmts(user, action, &ids, &form.bulk_tag_string),
        None => vec![],
    };
    if !is_htmx(&headers) {
        s.db.batch(writes).await?;
        s.storage.remove_all(&files).await;
        return Ok(Redirect::to(&href(base(scope), &q, &q.q, q.page())).into_response());
    }
    let bundle = selected_bundle(&s, user, &q).await?;
    let n = writes.len();
    let mut stmts = writes;
    stmts.extend(page_stmts(user, scope, &q, bundle.as_ref()));
    let rows = s.db.batch(stmts).await?.split_off(n);
    s.storage.remove_all(&files).await;
    render_list(scope, q, rows, jar)
}

// ---------------------------------------------------------------------------------------------
// New / edit

#[derive(Template)]
#[template(path = "bookmark_form.html")]
struct FormPage {
    heading: &'static str,
    /// The add form looks up the URL (already bookmarked? metadata, auto tags) as it's entered.
    is_new: bool,
    action: String,
    input: BookmarkInput,
    error: Option<String>,
    return_url: String,
    /// The user has sharing enabled: show the Share checkbox.
    sharing: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct NewQuery {
    url: String,
    title: String,
    description: String,
    notes: String,
    tags: String,
    return_url: String,
}

/// Only same-site relative paths, to avoid open redirects. Browsers strip tabs/newlines and
/// treat `\` like `/`, so `/\t/evil.com` or `/\evil.com` would become `//evil.com`.
fn safe_return(url: &str) -> String {
    let clean = !url.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\');
    if clean && url.starts_with('/') && !url.starts_with("//") {
        url.to_string()
    } else {
        "/bookmarks".to_string()
    }
}

fn with_return(path: &str, return_url: &str) -> String {
    let qs = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_url", return_url)
        .finish();
    format!("{path}?{qs}")
}

pub async fn new_page(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(q): Query<NewQuery>,
) -> Result<Response, AppError> {
    let p = profile::get_or_guest(&s.db, user).await?;
    let (unread, shared) = (p.default_mark_unread, p.default_mark_shared && p.enable_sharing);
    let return_url = safe_return(&q.return_url);
    let page = FormPage {
        heading: "New bookmark",
        is_new: true,
        action: with_return("/bookmarks/new", &return_url),
        input: BookmarkInput {
            url: q.url,
            title: q.title,
            description: q.description,
            notes: q.notes,
            tag_string: q.tags,
            unread,
            shared,
            ..Default::default()
        },
        error: None,
        return_url,
        sharing: p.enable_sharing,
    };
    Ok(render(&page)?.into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ReturnQuery {
    return_url: String,
}

pub async fn create(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(rq): Query<ReturnQuery>,
    Form(input): Form<BookmarkInput>,
) -> Result<Response, AppError> {
    let return_url = safe_return(&rq.return_url);
    let p = profile::get_or_guest(&s.db, user).await?;
    if let Err(e) = input.validate() {
        let action = with_return("/bookmarks/new", &return_url);
        let page = FormPage {
            heading: "New bookmark",
            is_new: true,
            action,
            input,
            error: Some(e.1.to_string()),
            return_url,
            sharing: p.enable_sharing,
        };
        return Ok(render(&page)?.into_response());
    }
    let tags = bm::with_auto_tags(&input.tags(), &p.auto_tagging_rules, &input.url);
    let saved = bm::create(&s.db, user, &input, &tags).await?;
    s.jobs.enqueue(saved.id);
    if p.enable_automatic_html_snapshots {
        s.jobs.snapshot_bookmark(&s.db, user, saved.id).await?;
    }
    Ok(Redirect::to(&return_url).into_response())
}

pub async fn edit_page(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Query(rq): Query<ReturnQuery>,
) -> Result<Response, AppError> {
    let (b, p) = tokio::try_join!(bm::get(&s.db, user, id), profile::get_or_guest(&s.db, user))?;
    let Some(b) = b else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let return_url = safe_return(&rq.return_url);
    let page = FormPage {
        sharing: p.enable_sharing,
        heading: "Edit bookmark",
        is_new: false,
        action: with_return(&format!("/bookmarks/{id}/edit"), &return_url),
        input: BookmarkInput {
            tag_string: b.tag_names.join(" "),
            url: b.url,
            title: b.title,
            description: b.description,
            notes: b.notes,
            unread: b.unread,
            shared: b.shared,
            is_archived: b.is_archived,
        },
        error: None,
        return_url,
    };
    Ok(render(&page)?.into_response())
}

pub async fn update(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
    Query(rq): Query<ReturnQuery>,
    Form(input): Form<BookmarkInput>,
) -> Result<Response, AppError> {
    let return_url = safe_return(&rq.return_url);
    let p = profile::get_or_guest(&s.db, user).await?;
    let mut patch = input.to_patch();
    patch.tags = Some(bm::with_auto_tags(&input.tags(), &p.auto_tagging_rules, &input.url));
    let error = match input.validate() {
        Err(e) => e.1,
        Ok(()) => match bm::update(&s.db, user, id, &patch).await? {
            Updated::Ok(_) => {
                s.jobs.enqueue(id);
                return Ok(Redirect::to(&return_url).into_response());
            }
            Updated::NotFound => return Ok(StatusCode::NOT_FOUND.into_response()),
            Updated::DuplicateUrl => "A bookmark with this URL already exists.",
        },
    };
    let page = FormPage {
        heading: "Edit bookmark",
        is_new: false,
        action: with_return(&format!("/bookmarks/{id}/edit"), &return_url),
        input,
        error: Some(error.to_string()),
        return_url,
        sharing: p.enable_sharing,
    };
    Ok(render(&page)?.into_response())
}

#[derive(Template)]
#[template(path = "url_check.html")]
struct UrlCheck {
    existing: Option<Bookmark>,
    title: String,
    description: String,
    auto_tags: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct CheckQuery {
    url: String,
}

/// Add form helper (htmx): is the URL bookmarked already, which auto tags apply, and the page's
/// title/description, which `app.js` copies into empty form fields.
pub async fn check(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(q): Query<CheckQuery>,
) -> Result<Response, AppError> {
    let url = q.url.trim();
    if bm::validate_url(url).is_err() {
        let empty = UrlCheck {
            existing: None,
            title: String::new(),
            description: String::new(),
            auto_tags: vec![],
        };
        return Ok(render(&empty)?.into_response());
    }
    let (existing, profile, meta) = tokio::join!(
        bm::find_by_url(&s.db, user, url),
        profile::get_or_guest(&s.db, user),
        s.fetcher.metadata(url)
    );
    let rules = profile?.auto_tagging_rules;
    let page = UrlCheck {
        existing: existing?,
        title: meta.title.unwrap_or_default(),
        description: meta.description.unwrap_or_default(),
        auto_tags: crate::domain::auto_tagging::get_tags(&rules, url),
    };
    Ok(render(&page)?.into_response())
}

// ---------------------------------------------------------------------------------------------
// Details (modal via htmx, standalone page without JS)

#[derive(Template)]
#[template(path = "details.html")]
struct DetailsPage {
    b: Bookmark,
    notes_html: String,
    added: String,
    modified: String,
    action_url: &'static str,
    files: Vec<FileRow>,
}

struct FileRow {
    a: crate::domain::assets::Asset,
    size: String,
    created: String,
}

pub async fn details(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let (b, files) = tokio::try_join!(bm::get(&s.db, user, id), crate::domain::assets::list(&s.db, user, id))?;
    let Some(b) = b else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let files = files
        .into_iter()
        .map(|a| FileRow {
            size: a.file_size.map(super::human_size).unwrap_or_default(),
            created: ago(&a.date_created),
            a,
        })
        .collect();
    let page = DetailsPage {
        files,
        notes_html: markdown(&b.notes),
        added: ago(&b.date_added),
        modified: ago(&b.date_modified),
        action_url: if b.is_archived {
            "/bookmarks/archived/action"
        } else {
            "/bookmarks/action"
        },
        b,
    };
    Ok(render(&page)?.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hrefs() {
        let q = ListQuery {
            sort: "title_asc".into(),
            ..Default::default()
        };
        assert_eq!(
            href("/bookmarks", &q, "#a b", 2),
            "/bookmarks?q=%23a+b&sort=title_asc&page=2"
        );
        assert_eq!(href("/bookmarks", &ListQuery::default(), "", 1), "/bookmarks");
        assert_eq!(with_tag(" rust ", "web"), "rust #web");
        assert_eq!(safe_return("//evil.com"), "/bookmarks");
        assert_eq!(safe_return("https://evil.com"), "/bookmarks");
        assert_eq!(safe_return("/bookmarks?q=x"), "/bookmarks?q=x");
        assert_eq!(safe_return("/\t/evil.com"), "/bookmarks");
        assert_eq!(safe_return("/\n/evil.com"), "/bookmarks");
        assert_eq!(safe_return("/\\evil.com"), "/bookmarks");
    }
}
