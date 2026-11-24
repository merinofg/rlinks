use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use rlinks::{AppState, db::Db, domain::users, web};
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn test_state(db: Db) -> AppState {
    let dir = std::env::temp_dir().join(format!("ld-test-{}", rand::random::<u64>()));
    let ctx = rlinks::jobs::Context {
        db: std::sync::Arc::new(db),
        storage: rlinks::storage::Storage::local(&dir).unwrap(),
        secret: KEY.into(),
        fetcher: std::sync::Arc::new(rlinks::scrape::Fetcher::offline()),
    };
    AppState::new(ctx, rlinks::jobs::Jobs::disabled(), KEY)
}

struct Res {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Res {
    fn location(&self) -> &str {
        self.headers[header::LOCATION].to_str().unwrap()
    }
}

/// Router + a logged-in session cookie for user "alice" (a second user "bob" exists too).
struct App {
    router: Router,
    cookie: String,
}

impl App {
    async fn new() -> App {
        let db = Db::sqlite(":memory:").unwrap();
        db.migrate().await.unwrap();
        users::create(&db, "alice", "secret", true).await.unwrap();
        users::create(&db, "bob", "secret", false).await.unwrap();
        let mut app = App {
            router: web::router(test_state(db)),
            cookie: String::new(),
        };
        let res = app.post("/login", "username=alice&password=secret", false).await;
        assert_eq!(res.status, StatusCode::SEE_OTHER);
        app.cookie = res.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        app
    }

    async fn send(&self, req: Request<Body>) -> Res {
        let res = self.router.clone().oneshot(req).await.unwrap();
        let (parts, body) = res.into_parts();
        let body = String::from_utf8_lossy(&to_bytes(body, usize::MAX).await.unwrap()).into_owned();
        Res {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }

    async fn get(&self, uri: &str) -> Res {
        self.send(
            Request::get(uri)
                .header(header::COOKIE, &self.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn post(&self, uri: &str, form: &str, htmx: bool) -> Res {
        let mut req = Request::post(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::HOST, "localhost")
            .header(header::ORIGIN, "http://localhost")
            .header(header::COOKIE, &self.cookie);
        if htmx {
            req = req.header("hx-request", "true");
        }
        self.send(req.body(Body::from(form.to_string())).unwrap()).await
    }

    async fn add(&self, url: &str, tags: &str, title: &str) {
        let form = format!("url={}&tag_string={}&title={}", enc(url), enc(tags), enc(title));
        let res = self.post("/bookmarks/new", &form, false).await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    }
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[tokio::test]
async fn login_required_and_session() {
    let app = App::new().await;
    let anon = app.send(Request::get("/bookmarks").body(Body::empty()).unwrap()).await;
    assert_eq!((anon.status, anon.location()), (StatusCode::SEE_OTHER, "/login"));

    let bad = app.post("/login", "username=alice&password=wrong", false).await;
    assert!(
        bad.body.contains("That username and password don&#x27;t match")
            || bad.body.contains("That username and password don't match")
    );

    assert_eq!(app.get("/bookmarks").await.status, StatusCode::OK);
    let forged = format!("{}x", app.cookie);
    let req = Request::get("/bookmarks")
        .header(header::COOKIE, forged)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(req).await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn cross_site_post_is_rejected() {
    let app = App::new().await;
    let req = Request::post("/bookmarks/new")
        .header(header::HOST, "localhost")
        .header(header::ORIGIN, "https://evil.example")
        .header(header::COOKIE, &app.cookie)
        .body(Body::from("url=https://x.com"))
        .unwrap();
    assert_eq!(app.send(req).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn create_list_search_and_tag_cloud() {
    let app = App::new().await;
    app.add("https://www.rust-lang.org", "rust lang", "Rust").await;
    app.add("https://python.org", "python lang", "Python").await;

    let list = app.get("/bookmarks").await.body;
    assert!(list.contains("Rust") && list.contains("Python"));
    assert!(
        list.contains(r##"href="/bookmarks?q=%23rust">rust</a>"##),
        "tag cloud links add #tag to q"
    );

    let res = app.get("/bookmarks?q=%23rust").await.body;
    assert!(res.contains("Rust") && !res.contains(">Python<"));
    assert!(res.contains("Selected tags"));

    let empty = app.get("/bookmarks?q=%28broken").await.body;
    assert!(empty.contains("No bookmarks match your search."));

    let res = app.post("/bookmarks/new", "url=not-a-url", false).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("Enter a valid URL."));
}

#[tokio::test]
async fn htmx_row_actions_and_bulk() {
    let app = App::new().await;
    app.add("https://a.example", "", "Alpha").await;
    app.add("https://b.example", "", "Beta").await;

    // htmx: write + re-render in one response, no redirect
    let res = app.post("/bookmarks/action", "archive=1", true).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(!res.body.contains("Alpha") && res.body.contains("Beta"));
    assert!(app.get("/bookmarks/archived").await.body.contains("Alpha"));

    // no-JS: redirect back to the list, keeping the query
    let res = app
        .post("/bookmarks/archived/action?q=alpha", "unarchive=1", false)
        .await;
    assert_eq!(res.location(), "/bookmarks/archived?q=alpha");
    assert!(app.get("/bookmarks").await.body.contains("Alpha"));

    let res = app
        .post(
            "/bookmarks/action",
            "bulk_action=bulk_tag&bulk_tag_string=x+y&bookmark_id=1&bookmark_id=2&bulk_execute=1",
            true,
        )
        .await;
    assert!(res.body.matches("#x</a>").count() == 2, "{}", res.body);

    let res = app
        .post(
            "/bookmarks/action",
            "bulk_action=bulk_delete&bookmark_id=1&bookmark_id=2&bulk_execute=1",
            true,
        )
        .await;
    assert!(res.body.contains("No bookmarks yet."));
}

#[tokio::test]
async fn edit_details_and_isolation() {
    let app = App::new().await;
    app.add("https://a.example", "one", "Alpha").await;

    let edit = app.get("/bookmarks/1/edit").await;
    assert!(edit.body.contains(r#"value="https://a.example""#) && edit.body.contains(r#"value="one""#));

    let res = app
        .post(
            "/bookmarks/1/edit?return_url=%2Fbookmarks%3Fq%3Dx",
            "url=https://a.example&title=Renamed&notes=**bold**",
            false,
        )
        .await;
    assert_eq!(res.location(), "/bookmarks?q=x");
    let details = app.get("/bookmarks/1/details").await.body;
    assert!(details.contains("Renamed") && details.contains("<strong>bold</strong>"));

    // open-redirect protection
    let res = app
        .post(
            "/bookmarks/1/edit?return_url=https%3A%2F%2Fevil.com",
            "url=https://a.example",
            false,
        )
        .await;
    assert_eq!(res.location(), "/bookmarks");

    // bob can't see or edit alice's bookmark
    let mut bob = App {
        router: app.router.clone(),
        cookie: String::new(),
    };
    let login = bob.post("/login", "username=bob&password=secret", false).await;
    bob.cookie = login.headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert_eq!(bob.get("/bookmarks/1/details").await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        bob.post("/bookmarks/1/edit", "url=https://evil.com", false)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    bob.post("/bookmarks/action", "remove=1", false).await;
    assert_eq!(app.get("/bookmarks/1/details").await.status, StatusCode::OK);
}

#[tokio::test]
async fn health_and_static() {
    let app = App::new().await;
    let res = app.get("/health").await;
    assert!(res.status == StatusCode::OK && res.body.contains("healthy"));
    assert_eq!(
        app.get("/static/htmx-2.0.4.min.js").await.headers[header::CONTENT_TYPE],
        "text/javascript"
    );
    assert_eq!(app.get("/static/nope.js").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn edit_rejects_duplicate_url() {
    let app = App::new().await;
    app.add("https://a.example", "", "A").await;
    app.add("https://b.example", "", "B").await;
    let res = app
        .post("/bookmarks/1/edit", "url=https%3A%2F%2Fb.example%2F&title=A", false)
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("A bookmark with this URL already exists."));
}

#[tokio::test]
async fn api_tokens_page() {
    let app = App::new().await;
    assert!(app.get("/settings/integrations").await.body.contains("No tokens yet."));
    let res = app
        .post("/settings/integrations/create-api-token", "name=Firefox", false)
        .await;
    let key = res
        .body
        .split("value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    assert_eq!(key.len(), 40, "new key is shown once");

    let api = |key: String| {
        let router = app.router.clone();
        async move {
            let req = Request::get("/api/bookmarks/?limit=1")
                .header(header::AUTHORIZATION, format!("Token {key}"))
                .body(Body::empty())
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        }
    };
    assert_eq!(api(key.clone()).await, StatusCode::OK);

    let page = app.get("/settings/integrations").await.body;
    assert!(
        page.contains("Firefox") && !page.contains(&key),
        "key isn't shown again"
    );
    app.post("/settings/integrations/delete-api-token", "token_id=1", false)
        .await;
    assert_eq!(
        api(key).await,
        StatusCode::UNAUTHORIZED,
        "deleted token stops working immediately"
    );
}

#[tokio::test]
async fn authorization_header_does_not_bypass_csrf_on_web_routes() {
    // e.g. a reverse proxy with Basic auth: the browser adds this header to cross-site posts too
    let app = App::new().await;
    app.add("https://a.example", "", "A").await;
    for auth in ["Basic YWxpY2U6c2VjcmV0", "Token whatever"] {
        let req = Request::post("/bookmarks/action")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::HOST, "localhost")
            .header(header::ORIGIN, "https://evil.example")
            .header(header::AUTHORIZATION, auth)
            .header(header::COOKIE, &app.cookie)
            .body(Body::from("remove=1"))
            .unwrap();
        assert_eq!(app.send(req).await.status, StatusCode::FORBIDDEN, "{auth}");
    }
    assert_eq!(app.get("/bookmarks/1/details").await.status, StatusCode::OK);
}

#[tokio::test]
async fn manifest_and_icons() {
    let app = App::new().await;
    let res = app.get("/manifest.json").await;
    assert_eq!(res.headers[header::CONTENT_TYPE], "application/manifest+json");
    let manifest: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(manifest["name"], "rlinks");
    for icon in manifest["icons"].as_array().unwrap() {
        let src = icon["src"].as_str().unwrap();
        let res = app.get(src).await;
        assert_eq!(res.status, StatusCode::OK, "{src}");
        assert_eq!(
            res.headers[header::CONTENT_TYPE],
            icon["type"].as_str().unwrap(),
            "{src}"
        );
    }
    let page = app.get("/bookmarks").await.body;
    assert!(page.contains(r#"<link rel="manifest" href="/manifest.json">"#) && page.contains("apple-touch-icon"));
}

#[tokio::test]
async fn tags_page_rename_merge_delete() {
    let app = App::new().await;
    app.add("https://a.example", "js rust", "A").await;
    app.add("https://b.example", "javascript", "B").await;
    let page = app.get("/tags").await.body;
    assert!(page.contains("#javascript") && page.contains("#rust"));
    assert!(
        app.get("/tags?q=java").await.body.contains("#javascript")
            && !app.get("/tags?q=java").await.body.contains("#rust")
    );

    let res = app.post("/tags/action", "rename=1&name=rust", false).await;
    assert!(
        res.body.contains("already exists"),
        "rename onto an existing name is refused"
    );
    let res = app
        .post("/tags/action", "merge_sources=js&merge_target=javascript", false)
        .await;
    assert!(res.body.contains("Merged 1 tag into"), "{}", res.body);
    let list = app.get("/bookmarks?q=%23javascript").await.body;
    assert!(
        list.contains(">A<") || list.contains("A</a>"),
        "bookmark A moved to #javascript"
    );
    let res = app.post("/tags/action", "delete=2", false).await;
    assert!(res.body.contains("Tag deleted"));
    assert!(!app.get("/tags").await.body.contains("#rust"));
}

#[tokio::test]
async fn bundles_pages_and_sidebar() {
    let app = App::new().await;
    app.add("https://r.example", "rust", "Rusty").await;
    app.add("https://p.example", "python", "Snakey").await;
    assert!(app.get("/bookmarks").await.body.contains("Save searches as bundles."));

    let res = app.post("/bundles/new", "name=&search=", false).await;
    assert!(res.body.contains("may not be blank"));
    let res = app
        .post(
            "/bundles/new",
            "name=Rust+stuff&any_tags=rust&filter_unread=off&filter_shared=off",
            false,
        )
        .await;
    assert_eq!(res.location(), "/bookmarks?bundle=1");

    let list = app.get("/bookmarks?bundle=1").await.body;
    assert!(
        list.contains("Rusty") && !list.contains("Snakey"),
        "bundle filters the list"
    );
    assert!(list.contains(r#"aria-current="page">Rust stuff</a>"#) && list.contains("Show all bookmarks"));
    assert!(list.contains("bundle=1"), "links keep the selected bundle");

    let res = app
        .post(
            "/bundles/1/edit",
            "name=Python&any_tags=python&filter_unread=off&filter_shared=off",
            false,
        )
        .await;
    assert_eq!(res.location(), "/bookmarks?bundle=1");
    assert!(app.get("/bookmarks?bundle=1").await.body.contains("Snakey"));
    app.post("/bundles/action", "delete=1", false).await;
    assert!(app.get("/bundles").await.body.contains("No bundles yet."));
    assert!(
        app.get("/bookmarks?bundle=1").await.body.contains("Snakey"),
        "unknown bundle: unfiltered"
    );
}

#[tokio::test]
async fn import_and_export_files() {
    let app = App::new().await;
    let file = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1><DL><p>
        <DT><A HREF="https://a.example/" ADD_DATE="1600000000" TAGS="one,two">Alpha</A>
        <DD>first
        <DT><A HREF="javascript:void(0)">Bookmarklet</A>
        </DL><p>"#;
    let boundary = "XBOUNDARYX";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"b.html\"\r\nContent-Type: text/html\r\n\r\n{file}\r\n--{boundary}--\r\n"
    );
    let req = Request::post("/settings/import")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::HOST, "localhost")
        .header(header::ORIGIN, "http://localhost")
        .header(header::COOKIE, &app.cookie)
        .body(Body::from(body))
        .unwrap();
    let res = app.send(req).await;
    assert!(
        res.body.contains("Imported 1 of 2 bookmarks. 1 skipped"),
        "{}",
        res.body
    );
    let list = app.get("/bookmarks").await.body;
    assert!(list.contains("Alpha") && list.contains("#one"));

    let export = app.get("/settings/export").await;
    assert!(
        export.headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .contains("bookmarks.html")
    );
    assert!(export.body.contains(r#"HREF="https://a.example/""#) && export.body.contains("<DD>first"));
}

#[tokio::test]
async fn rss_feeds() {
    let app = App::new().await;
    app.add("https://a.example", "rust", "Rust & <friends>").await;
    app.add("https://b.example", "", "Plain").await;
    app.post(
        "/bookmarks/action",
        "bulk_action=bulk_unread&bookmark_id=2&bulk_execute=1",
        false,
    )
    .await;
    let page = app.get("/settings/integrations").await.body;
    let key = page
        .split("/feeds/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();

    let feed = app
        .send(
            Request::get(format!("/feeds/{key}/all"))
                .header(header::HOST, "ld.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(feed.headers[header::CONTENT_TYPE], "application/rss+xml; charset=utf-8");
    assert!(feed.body.starts_with("<?xml") && feed.body.matches("<item>").count() == 2);
    assert!(
        feed.body.contains("<title>Rust &#38; &#60;friends&#62;</title>"),
        "escaped: {}",
        feed.body
    );
    assert!(feed.body.contains("<category>rust</category>") && feed.body.contains("<pubDate>"));
    assert!(
        app.get(&format!("/feeds/{key}/all?q=%23rust"))
            .await
            .body
            .matches("<item>")
            .count()
            == 1
    );
    let unread = app.get(&format!("/feeds/{key}/unread")).await.body;
    assert!(
        unread.contains("Plain") && !unread.contains("<category>rust"),
        "read bookmarks aren't in the unread feed"
    );
    // no session needed, but the key must be right
    let anon = app
        .send(Request::get(format!("/feeds/{key}/all")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(anon.status, StatusCode::OK);
    assert_eq!(app.get("/feeds/wrong/all").await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        app.get(&format!("/feeds/{key}/other")).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn general_settings_theme_and_custom_css() {
    let app = App::new().await;
    let form = "theme=dark&bookmark_link_target=_self&items_per_page=20&tag_search=lax&bookmark_date_display=relative&bookmark_description_max_lines=1&tag_grouping=alphabetical&custom_css=body%7Bcolor%3Ared%7D";
    let res = app.post("/settings/general", form, false).await;
    assert!(res.body.contains("Settings saved."), "{}", res.body);
    let prefs = res.headers[header::SET_COOKIE].to_str().unwrap().to_string();
    assert!(
        prefs.starts_with("prefs=dark%3A") || prefs.starts_with("prefs=dark:"),
        "{prefs}"
    );
    assert!(app.get("/custom_css").await.body.contains("color:red"));
    let bad = app
        .post(
            "/settings/general",
            &form.replace("items_per_page=20", "items_per_page=5"),
            false,
        )
        .await;
    assert!(bad.body.contains("between 10 and 1000"));
}

#[tokio::test]
async fn display_settings_change_the_list() {
    let app = App::new().await;
    let res = app
        .post(
            "/bookmarks/new",
            "url=https%3A%2F%2Fa.example%2Fpath&title=A&tag_string=zeta+alpha&notes=**noted**",
            false,
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    app.post("/bundles/new", "name=Mine&any_tags=alpha", false).await;
    let before = app.get("/bookmarks").await.body;
    assert!(before.contains(">just now<") && !before.contains("<strong>noted</strong>") && !before.contains("Wayback"));
    assert!(before.contains("<b>A</b>") && before.contains("Mine"));

    let form = "theme=auto&bookmark_link_target=_blank&items_per_page=30&tag_search=strict&bookmark_date_display=absolute\
        &bookmark_description_max_lines=3&tag_grouping=disabled&display_url=on&permanent_notes=on&web_archive_links=on\
        &hide_bundles=on&default_mark_unread=on";
    assert!(
        app.post("/settings/general", form, false)
            .await
            .body
            .contains("Settings saved.")
    );
    let after = app.get("/bookmarks").await.body;
    let today = &rlinks::db::now()[..10];
    assert!(after.contains(&format!(">{today}<")), "absolute date");
    assert!(after.contains(r#"class="bm-url""#) && after.contains("<strong>noted</strong>"));
    assert!(after.contains("https://web.archive.org/web/"));
    assert!(
        !after.contains("<b>A</b>") && !after.contains(">Mine<"),
        "flat tag cloud, bundles hidden"
    );
    assert!(after.contains("--desc-lines: 3"));
    assert!(
        app.get("/bookmarks/new")
            .await
            .body
            .contains(r#"name="unread" checked"#),
        "default unread"
    );
}

#[tokio::test]
async fn opensearch_description() {
    let app = App::new().await;
    let req = Request::get("/opensearch.xml")
        .header(header::HOST, "ld.test")
        .body(Body::empty())
        .unwrap();
    let res = app.send(req).await;
    assert_eq!(
        res.headers[header::CONTENT_TYPE],
        "application/opensearchdescription+xml"
    );
    assert!(
        res.body
            .contains(r#"template="http://ld.test/bookmarks?client=opensearch&amp;q={searchTerms}""#),
        "{}",
        res.body
    );
    let evil = Request::get("/opensearch.xml")
        .header(header::HOST, "x\"><evil>")
        .body(Body::empty())
        .unwrap();
    let body = app.send(evil).await.body;
    assert!(!body.contains("<evil>"), "Host header is escaped");
    assert!(app.get("/bookmarks").await.body.contains(r#"rel="search""#));
    // the search URL works: extra client param is ignored
    assert_eq!(app.get("/bookmarks?client=opensearch&q=x").await.status, StatusCode::OK);
}

#[tokio::test]
async fn admin_manages_users() {
    let app = App::new().await; // alice is admin, bob is not
    let mut bob = App {
        router: app.router.clone(),
        cookie: String::new(),
    };
    let login = bob.post("/login", "username=bob&password=secret", false).await;
    bob.cookie = login.headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert_eq!(bob.get("/admin/users").await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        bob.post("/admin/users/action", "delete=1", false).await.status,
        StatusCode::FORBIDDEN
    );

    let page = app.get("/admin/users").await.body;
    assert!(page.contains("alice") && page.contains("(you)") && page.contains("bob"));
    let own = app.post("/admin/users/action", "deactivate=1", false).await.body;
    assert!(own.contains("deactivate, demote or delete your own account"), "{own}");
    assert!(
        app.post("/admin/users/action", "username=carol&password=short", false)
            .await
            .body
            .contains("at least 8")
    );
    assert!(
        app.post("/admin/users/action", "username=carol&password=long-enough", false)
            .await
            .body
            .contains("Created user")
    );
    assert!(
        app.post("/admin/users/action", "username=CAROL&password=long-enough", false)
            .await
            .body
            .contains("is taken")
    );

    // deactivating bob locks him out of pages and the API
    app.post("/admin/users/action", "deactivate=2", false).await;
    assert_eq!(bob.get("/bookmarks").await.location(), "/login");
    assert!(
        bob.post("/login", "username=bob&password=secret", false)
            .await
            .body
            .contains("match")
    );
    app.post("/admin/users/action", "activate=2", false).await;
    app.post(
        "/admin/users/action",
        "reset_password=2&new_password=brand-new-pw",
        false,
    )
    .await;
    assert_eq!(
        bob.post("/login", "username=bob&password=brand-new-pw", false)
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    app.post("/admin/users/action", "delete=2", false).await;
    assert!(!app.get("/admin/users").await.body.contains(">bob"));
}

#[tokio::test]
async fn change_own_password() {
    let app = App::new().await;
    let res = app
        .post(
            "/settings/account",
            "current_password=secret&new_password=new-secret&confirm_password=other",
            false,
        )
        .await;
    assert!(res.body.contains("don"));
    let res = app
        .post(
            "/settings/account",
            "current_password=wrong&new_password=new-secret&confirm_password=new-secret",
            false,
        )
        .await;
    assert!(res.body.contains("current password isn"));
    let res = app
        .post(
            "/settings/account",
            "current_password=secret&new_password=new-secret&confirm_password=new-secret",
            false,
        )
        .await;
    assert!(res.body.contains("Password changed."));
    assert_eq!(
        app.post("/login", "username=alice&password=new-secret", false)
            .await
            .status,
        StatusCode::SEE_OTHER
    );
}

async fn login_as(app: &App, user: &str, pass: &str) -> App {
    let mut other = App {
        router: app.router.clone(),
        cookie: String::new(),
    };
    let res = other
        .post("/login", &format!("username={user}&password={pass}"), false)
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "login {user}");
    other.cookie = res.headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    other
}

#[tokio::test]
async fn sessions_end_on_reset_password_change_and_delete() {
    let app = App::new().await; // alice (admin), bob
    let bob = login_as(&app, "bob", "secret").await;
    assert_eq!(bob.get("/bookmarks").await.status, StatusCode::OK);
    app.post(
        "/admin/users/action",
        "reset_password=2&new_password=reset-by-admin",
        false,
    )
    .await;
    assert_eq!(
        bob.get("/bookmarks").await.status,
        StatusCode::SEE_OTHER,
        "admin reset logs bob out"
    );

    // own password change: this session stays, the other one ends
    let laptop = login_as(&app, "alice", "secret").await;
    let res = app
        .post(
            "/settings/account",
            "current_password=secret&new_password=new-secret&confirm_password=new-secret",
            false,
        )
        .await;
    assert!(res.body.contains("Password changed."));
    let fresh = res.headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let phone = App {
        router: app.router.clone(),
        cookie: fresh,
    };
    assert_eq!(
        phone.get("/bookmarks").await.status,
        StatusCode::OK,
        "re-issued cookie works"
    );
    assert_eq!(
        laptop.get("/bookmarks").await.status,
        StatusCode::SEE_OTHER,
        "other sessions end"
    );

    // a deleted user's cookie must not work for a new account that reuses the id
    let admin = phone;
    admin
        .post("/admin/users/action", "username=zed&password=zed-password", false)
        .await;
    let zed = login_as(&admin, "zed", "zed-password").await;
    admin.post("/admin/users/action", "delete=3", false).await;
    admin
        .post("/admin/users/action", "username=newbie&password=newbie-password", false)
        .await;
    assert!(admin.get("/admin/users").await.body.contains("newbie"));
    assert_eq!(
        zed.get("/bookmarks").await.status,
        StatusCode::SEE_OTHER,
        "stale cookie rejected"
    );

    // cookies from before session nonces (just the id) are rejected
    let bare = App {
        router: app.router.clone(),
        cookie: "session=1".into(),
    };
    assert_eq!(bare.get("/bookmarks").await.status, StatusCode::SEE_OTHER);
}

const SHARING_FORM: &str = "theme=auto&bookmark_link_target=_blank&items_per_page=30&tag_search=strict\
    &bookmark_date_display=relative&bookmark_description_max_lines=1&tag_grouping=alphabetical";

#[tokio::test]
async fn sharing_between_users_and_publicly() {
    let app = App::new().await; // alice
    let bob = login_as(&app, "bob", "secret").await;
    assert!(
        !app.get("/bookmarks/new").await.body.contains(r#"name="shared""#),
        "no Share box while sharing is off"
    );
    app.post(
        "/settings/general",
        &format!("{SHARING_FORM}&enable_sharing=on&enable_public_sharing=on"),
        false,
    )
    .await;
    assert!(app.get("/bookmarks/new").await.body.contains(r#"name="shared""#));
    app.post(
        "/bookmarks/new",
        "url=https%3A%2F%2Fshared.example&title=Shared+one&tag_string=pub&shared=on",
        false,
    )
    .await;
    app.post(
        "/bookmarks/new",
        "url=https%3A%2F%2Fprivate.example&title=Private+one",
        false,
    )
    .await;
    app.post("/bookmarks/new", "url=https%3A%2F%2Fbulk.example&title=Bulk+one", false)
        .await;
    app.post(
        "/bookmarks/action",
        "bulk_action=bulk_share&bookmark_id=3&bulk_execute=1",
        false,
    )
    .await;

    let page = bob.get("/bookmarks/shared").await.body;
    assert!(page.contains("Shared one") && page.contains("Bulk one") && !page.contains("Private one"));
    assert!(
        page.contains(">alice</a>") && !page.contains(r#"name="bookmark_id""#),
        "read-only, shows the owner"
    );
    assert!(
        bob.get("/bookmarks/shared?user=bob")
            .await
            .body
            .contains("Nothing shared yet.")
    );

    let anon = app
        .send(Request::get("/bookmarks/shared").body(Body::empty()).unwrap())
        .await;
    assert_eq!(anon.status, StatusCode::OK);
    assert!(
        anon.body.contains("Shared one") && anon.body.contains("Log in"),
        "public sharing, logged-out nav"
    );
    let feed = app
        .send(Request::get("/feeds/shared").body(Body::empty()).unwrap())
        .await
        .body;
    assert!(feed.contains("Shared one") && !feed.contains("Private one"));

    // public sharing off: logged-in users still see it, anonymous visitors don't
    app.post("/settings/general", &format!("{SHARING_FORM}&enable_sharing=on"), false)
        .await;
    let anon = app
        .send(Request::get("/bookmarks/shared").body(Body::empty()).unwrap())
        .await;
    assert!(!anon.body.contains("Shared one"));
    assert!(bob.get("/bookmarks/shared").await.body.contains("Shared one"));
    let key = bob
        .get("/settings/integrations")
        .await
        .body
        .split("/feeds/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    assert!(
        bob.get(&format!("/feeds/{key}/shared"))
            .await
            .body
            .contains("Shared one")
    );

    // sharing off entirely: gone for everyone; the edit form keeps the flag in a hidden field
    app.post("/settings/general", SHARING_FORM, false).await;
    assert!(!bob.get("/bookmarks/shared").await.body.contains("Shared one"));
    assert!(
        app.get("/bookmarks/1/edit")
            .await
            .body
            .contains(r#"<input type="hidden" name="shared" value="on">"#)
    );
}

#[tokio::test]
async fn landing_page_for_logged_out_visitors() {
    let app = App::new().await;
    async fn anon(app: &App) -> Res {
        app.send(Request::get("/").body(Body::empty()).unwrap()).await
    }
    assert_eq!(anon(&app).await.location(), "/login");
    assert_eq!(app.get("/").await.location(), "/bookmarks");
    let bob = login_as(&app, "bob", "secret").await;
    assert_eq!(
        bob.post("/admin/users/action", "landing_page=shared_bookmarks", false)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert!(
        app.post("/admin/users/action", "landing_page=shared_bookmarks", false)
            .await
            .body
            .contains("Site settings saved.")
    );
    assert_eq!(anon(&app).await.location(), "/bookmarks/shared");
    assert_eq!(
        app.get("/").await.location(),
        "/bookmarks",
        "logged-in users still land on their bookmarks"
    );
}

fn multipart(name: &str, content_type: &str, data: &[u8]) -> (String, Vec<u8>) {
    let boundary = "XBOUNDARYX";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

impl App {
    async fn upload(&self, bookmark: i64, name: &str, content_type: &str, data: &[u8]) -> Res {
        let (ct, body) = multipart(name, content_type, data);
        let req = Request::post(format!("/bookmarks/{bookmark}/assets/upload"))
            .header(header::CONTENT_TYPE, ct)
            .header(header::HOST, "localhost")
            .header(header::ORIGIN, "http://localhost")
            .header(header::COOKIE, &self.cookie)
            .body(Body::from(body))
            .unwrap();
        self.send(req).await
    }
}

/// The random id of the file named `name`, from the bookmark's details page.
async fn asset_uid(app: &App, bookmark: i64, name: &str) -> String {
    let page = app.get(&format!("/bookmarks/{bookmark}/details")).await.body;
    let after = &page[page.find(name).unwrap_or_else(|| panic!("{name} not listed"))..];
    let start = after.find("/assets/").expect("asset link") + "/assets/".len();
    after[start..start + 32].to_string()
}

#[tokio::test]
async fn uploads_snapshots_and_serving() {
    let app = App::new().await;
    app.add("https://a.example", "", "A").await;
    let details = app.get("/bookmarks/1/details").await.body;
    assert!(
        details.contains("saving…"),
        "automatic snapshot pending (jobs don't run in tests)"
    );

    let res = app
        .upload(1, "notes.html", "text/html", b"<script>alert(1)</script><p>hi</p>")
        .await;
    assert_eq!(res.location(), "/bookmarks/1/details");
    let notes = asset_uid(&app, 1, "notes.html").await;
    assert!(
        notes.chars().all(|c| c.is_ascii_hexdigit()),
        "random id in URLs: {notes}"
    );
    assert_eq!(
        app.get("/assets/2/read").await.status,
        StatusCode::NOT_FOUND,
        "sequential ids don't resolve"
    );

    let read = app.get(&format!("/assets/{notes}/read")).await;
    assert_eq!(read.headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    let csp = read.headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(
        csp.starts_with("sandbox") && csp.contains("default-src 'none'"),
        "uploaded HTML can't run: {csp}"
    );
    assert!(read.body.contains("<p>hi</p>"));
    let dl = app.get(&format!("/assets/{notes}")).await;
    assert!(
        dl.headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment; filename=\"notes.html\"")
    );

    // only real PDFs are served as PDFs (and unsandboxed)
    app.upload(1, "fake.pdf", "application/pdf", b"<html>not a pdf</html>")
        .await;
    let fake = asset_uid(&app, 1, "fake.pdf").await;
    assert_eq!(
        app.get(&format!("/assets/{fake}/read")).await.headers[header::CONTENT_TYPE],
        "application/octet-stream"
    );
    app.upload(1, "real.pdf", "application/octet-stream", b"%PDF-1.7 ...")
        .await;
    let real = asset_uid(&app, 1, "real.pdf").await;
    let pdf = app.get(&format!("/assets/{real}/read")).await;
    assert_eq!(pdf.headers[header::CONTENT_TYPE], "application/pdf");
    assert!(
        !pdf.headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("sandbox")
    );

    assert_eq!(
        app.upload(1, "empty.txt", "text/plain", b"").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.post("/bookmarks/1/assets/snapshot", "", false).await.location(),
        "/bookmarks/1/details"
    );

    let bob = login_as(&app, "bob", "secret").await;
    assert_eq!(
        bob.get(&format!("/assets/{notes}/read")).await.status,
        StatusCode::NOT_FOUND,
        "owner only"
    );
    assert_eq!(
        bob.upload(1, "x.txt", "text/plain", b"x").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bob.post(&format!("/assets/{notes}/delete"), "", false).await.status,
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        app.post(&format!("/assets/{notes}/delete"), "", false).await.location(),
        "/bookmarks/1/details"
    );
    assert_eq!(
        app.get(&format!("/assets/{notes}/read")).await.status,
        StatusCode::NOT_FOUND
    );
}
