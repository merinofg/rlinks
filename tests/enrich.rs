//! Scraping, favicons, preview images, auto-tagging and the SSRF guard, end to end against a
//! local fixture server (no Internet access needed).

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use rlinks::db::Db;
use rlinks::domain::{tokens, users};
use rlinks::jobs::{self, Context, Jobs};
use rlinks::scrape::Fetcher;
use rlinks::storage::Storage;
use rlinks::{AppState, stmt, web};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-fake-png-bytes";

/// Serves a page with metadata, its og:image and a fake favicon provider on 127.0.0.1.
async fn fixture() -> String {
    let page = r#"<html><head><title>Fixture Page</title>
        <meta name="description" content="A page for tests">
        <meta property="og:image" content="/img.png"></head><body>hi</body></html>"#;
    let app = Router::new()
        .route(
            "/page",
            get(move || async move { ([(header::CONTENT_TYPE, "text/html")], page) }),
        )
        .route(
            "/plain",
            get(|| async { ([(header::CONTENT_TYPE, "text/html")], "<title>No preview</title>") }),
        )
        .route(
            "/img.png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], PNG) }),
        )
        .route(
            "/favicon",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], PNG) }),
        )
        .route(
            "/article",
            get(|| async {
                let html = r#"<html><head><title>Article</title><link rel="stylesheet" href="/site.css">
                    <script>document.title = "pwned"</script></head>
                    <body><h1>Hello</h1><img src="/img.png"><img src="http://169.254.169.254/latest/meta-data">
                    <a href="/other">other</a></body></html>"#;
                ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html)
            }),
        )
        .route(
            "/site.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css")],
                    "body{background:url(img.png)} h1{color:#123456}",
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{}", addr.port())
}

struct Env {
    router: Router,
    ctx: Context,
    token: String,
    base: String,
}

async fn env(allowed_internal_hosts: &str) -> Env {
    let base = fixture().await;
    let db = Db::sqlite(":memory:").unwrap();
    db.migrate().await.unwrap();
    users::create(&db, "alice", "pw", false).await.unwrap();
    let token = tokens::create(&db, 1, "t").await.unwrap();
    let dir = std::env::temp_dir().join(format!("ld-enrich-{}", rand::random::<u64>()));
    let ctx = Context {
        db: Arc::new(db),
        storage: Storage::local(&dir).unwrap(),
        secret: KEY.into(),
        fetcher: Arc::new(Fetcher::new(allowed_internal_hosts, &format!("{base}/favicon?url={{url}}")).unwrap()),
    };
    let router = web::router(AppState::new(ctx.clone(), Jobs::disabled(), KEY));
    Env {
        router,
        ctx,
        token,
        base,
    }
}

impl Env {
    async fn api(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut r = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "ld.test")
            .header(header::AUTHORIZATION, format!("Token {}", self.token));
        let body = match body {
            Some(b) => {
                r = r.header(header::CONTENT_TYPE, "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = self.router.clone().oneshot(r.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn scrape_favicon_and_preview() {
    let env = env("127.0.0.1").await;
    let page = format!("{}/page", env.base);

    let (_, check) = env.api("GET", &format!("/api/bookmarks/check/?url={page}"), None).await;
    assert_eq!(check["metadata"]["title"], "Fixture Page");
    assert_eq!(check["metadata"]["description"], "A page for tests");
    assert_eq!(check["metadata"]["preview_image"], format!("{}/img.png", env.base));

    // API create without title/description scrapes them; with disable_scraping it doesn't
    let (s, b) = env.api("POST", "/api/bookmarks/", Some(json!({"url": page}))).await;
    assert_eq!(
        (s, b["title"].clone(), b["description"].clone()),
        (StatusCode::CREATED, json!("Fixture Page"), json!("A page for tests"))
    );
    let (_, plain) = env
        .api(
            "POST",
            "/api/bookmarks/?disable_scraping",
            Some(json!({"url": format!("{}/plain", env.base)})),
        )
        .await;
    assert_eq!(plain["title"], "");

    // background job: nothing while disabled in the profile
    let id = b["id"].as_i64().unwrap();
    jobs::load_images(&env.ctx, id).await.unwrap();
    assert_eq!(
        env.api("GET", &format!("/api/bookmarks/{id}/"), None).await.1["favicon_url"],
        Value::Null
    );

    env.ctx
        .db
        .execute(stmt!(
            "UPDATE user_profiles SET enable_favicons = 1, enable_preview_images = 1"
        ))
        .await
        .unwrap();
    jobs::load_images(&env.ctx, id).await.unwrap();
    jobs::load_images(&env.ctx, plain["id"].as_i64().unwrap())
        .await
        .unwrap();
    let (_, b) = env.api("GET", &format!("/api/bookmarks/{id}/"), None).await;
    let favicon = b["favicon_url"].as_str().unwrap().to_string();
    let name = favicon.strip_prefix("http://ld.test/files/favicons/").unwrap();
    assert_eq!(
        name,
        jobs::file_name(KEY, "favicon", "http://127.0.0.1"),
        "keyed name of the site (scheme + host, no port)"
    );
    assert!(
        !favicon.contains("127_0_0_1"),
        "file names don't reveal what is bookmarked"
    );
    let preview = b["preview_image_url"].as_str().unwrap().to_string();
    assert!(
        preview.starts_with("http://ld.test/files/previews/") && preview.ends_with(".png"),
        "{preview}"
    );
    let (_, plain) = env.api("GET", &format!("/api/bookmarks/{}/", plain["id"]), None).await;
    assert_eq!(plain["preview_image_url"], Value::Null, "page without og:image");

    // files are served with the sniffed type and a sandboxing CSP
    for path in [
        favicon.replace("http://ld.test", ""),
        preview.replace("http://ld.test", ""),
    ] {
        let res = env
            .router
            .clone()
            .oneshot(Request::get(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        assert_eq!(res.headers()[header::CONTENT_TYPE], "image/png");
        assert!(
            res.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("sandbox")
        );
        assert_eq!(to_bytes(res.into_body(), usize::MAX).await.unwrap(), PNG);
    }
    for bad in [
        "/files/previews/..%2Fsecret",
        "/files/assets/x",
        "/files/favicons/.hidden",
    ] {
        let res = env
            .router
            .clone()
            .oneshot(Request::get(bad).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{bad}");
    }

    // missing images are found again (e.g. after a restart)
    env.ctx
        .db
        .execute(stmt!("UPDATE bookmarks SET favicon_file = ''"))
        .await
        .unwrap();
    assert_eq!(Jobs::disabled().enqueue_missing(&env.ctx.db, None).await.unwrap(), 2);
    assert_eq!(
        Jobs::disabled().enqueue_missing(&env.ctx.db, Some(99)).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn ssrf_guard_blocks_internal_addresses_by_default() {
    let env = env("").await;
    let page = format!("{}/page", env.base);
    let (_, check) = env.api("GET", &format!("/api/bookmarks/check/?url={page}"), None).await;
    assert_eq!(check["metadata"]["title"], Value::Null, "127.0.0.1 must not be fetched");
    let (_, b) = env.api("POST", "/api/bookmarks/", Some(json!({"url": page}))).await;
    assert_eq!(b["title"], "", "nothing scraped from an internal address");
    let (_, b) = env
        .api("GET", "/api/bookmarks/check/?url=http%3A%2F%2Flocalhost%3A1%2F", None)
        .await;
    assert_eq!(b["metadata"]["title"], Value::Null);
}

#[tokio::test]
async fn auto_tagging_rules_apply_on_create_update_and_check() {
    let env = env("").await;
    env.ctx
        .db
        .execute(stmt!(
            "UPDATE user_profiles SET auto_tagging_rules = 'example.com example\ngithub.com/rust rust # lang'"
        ))
        .await
        .unwrap();
    let (_, check) = env
        .api(
            "GET",
            "/api/bookmarks/check/?url=https%3A%2F%2Fgithub.com%2Frust%2Frfcs",
            None,
        )
        .await;
    assert_eq!(check["auto_tags"], json!(["rust"]));

    let (_, b) = env
        .api(
            "POST",
            "/api/bookmarks/?disable_scraping",
            Some(json!({"url": "https://www.example.com/x", "tag_names": ["mine"]})),
        )
        .await;
    assert_eq!(b["tag_names"], json!(["example", "mine"]));
    let uri = format!("/api/bookmarks/{}/", b["id"]);
    // PUT with explicit tags: auto tags merged in
    let (_, b) = env
        .api(
            "PUT",
            &uri,
            Some(json!({"url": "https://github.com/rust/rfcs", "tag_names": ["x"]})),
        )
        .await;
    assert_eq!(b["tag_names"], json!(["rust", "x"]));
    // PATCH with a URL but no tags: auto tags added to the existing ones
    let (_, b) = env
        .api("PATCH", &uri, Some(json!({"url": "https://example.com/y"})))
        .await;
    assert_eq!(b["tag_names"], json!(["example", "rust", "x"]));
}

#[tokio::test]
async fn html_snapshot_is_self_contained_and_guarded() {
    let env = env("127.0.0.1").await;
    let (_, b) = env
        .api(
            "POST",
            "/api/bookmarks/?disable_scraping",
            Some(json!({"url": format!("{}/article", env.base)})),
        )
        .await;
    let id = b["id"].as_i64().unwrap();
    // automatic snapshots are on by default (like linkding): a pending asset was created
    let pending = rlinks::domain::assets::pending_snapshots(&env.ctx.db).await.unwrap();
    assert_eq!(pending.len(), 1);
    jobs::snapshot(&env.ctx, pending[0]).await.unwrap();

    let asset = rlinks::domain::assets::get(&env.ctx.db, 1, pending[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(asset.status, "complete");
    let stored = env.ctx.storage.get(&asset.file).await.unwrap().unwrap();
    let html = String::from_utf8(rlinks::storage::gunzip(stored).await.unwrap()).unwrap();
    assert!(
        html.contains("<h1>Hello</h1>") && html.contains("color:#123456"),
        "stylesheet inlined"
    );
    assert!(html.contains("url(data:image/png;base64,"), "CSS image inlined");
    assert!(html.contains(r#"<img src="data:image/png;base64,"#), "image inlined");
    assert!(!html.contains("<script") && !html.contains("pwned"), "scripts dropped");
    assert!(
        html.contains(r#"src="http://169.254.169.254/latest/meta-data""#),
        "internal address not fetched"
    );
    assert!(
        html.contains(&format!(r#"href="{}/other""#, env.base)),
        "links made absolute"
    );
    let (_, b) = env.api("GET", &format!("/api/bookmarks/{id}/"), None).await;
    assert_eq!(b["id"], id);

    // a page that can't be fetched marks the snapshot failed instead of retrying forever
    let (_, gone) = env
        .api(
            "POST",
            "/api/bookmarks/?disable_scraping",
            Some(json!({"url": "http://127.0.0.1:1/gone"})),
        )
        .await;
    let failed = rlinks::domain::assets::pending_snapshots(&env.ctx.db).await.unwrap();
    assert!(jobs::snapshot(&env.ctx, failed[0]).await.is_err());
    let list = rlinks::domain::assets::list(&env.ctx.db, 1, gone["id"].as_i64().unwrap())
        .await
        .unwrap();
    assert_eq!(list[0].status, "failure");
}
