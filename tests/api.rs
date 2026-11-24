//! linkding REST API compatibility (shapes and status codes from linkding's api docs/tests).

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rlinks::db::Db;
use rlinks::domain::{tokens, users};
use rlinks::{AppState, web};
use serde_json::{Value, json};
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

struct Api {
    router: Router,
    db: std::sync::Arc<Db>,
    token: String,
    other_token: String,
}

impl Api {
    async fn new() -> Api {
        let db = Db::sqlite(":memory:").unwrap();
        db.migrate().await.unwrap();
        users::create(&db, "alice", "secret", false).await.unwrap();
        users::create(&db, "bob", "secret", false).await.unwrap();
        let token = tokens::create(&db, 1, "test").await.unwrap();
        let other_token = tokens::create(&db, 2, "test").await.unwrap();
        let state = test_state(db);
        Api {
            db: state.db.clone(),
            router: web::router(state),
            token,
            other_token,
        }
    }

    async fn req(&self, method: &str, uri: &str, auth: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let mut r = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "ld.test");
        if let Some(a) = auth {
            r = r.header(header::AUTHORIZATION, a);
        }
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

    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let auth = format!("Token {}", self.token);
        self.req(method, uri, Some(&auth), body).await
    }
}

#[tokio::test]
async fn authentication() {
    let api = Api::new().await;
    let (s, b) = api.req("GET", "/api/bookmarks/", None, None).await;
    assert_eq!(
        (s, b["detail"].as_str()),
        (
            StatusCode::UNAUTHORIZED,
            Some("Authentication credentials were not provided.")
        )
    );
    let (s, b) = api.req("GET", "/api/bookmarks/", Some("Token nope"), None).await;
    assert_eq!(
        (s, b["detail"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("Invalid token."))
    );
    let (s, _) = api.req("GET", "/api/bookmarks/", Some("Basic abc"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let bearer = format!("Bearer {}", api.token);
    assert_eq!(
        api.req("GET", "/api/bookmarks/", Some(&bearer), None).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn create_and_serialize() {
    let api = Api::new().await;
    let (s, b) = api
        .call(
            "POST",
            "/api/bookmarks/",
            Some(json!({
                "url": "https://example.com", "title": "Example title", "description": "Example description",
                "notes": "Example notes", "unread": true, "tag_names": ["tag 1", "Tag2"]
            })),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let keys: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
    for k in [
        "id",
        "url",
        "title",
        "description",
        "notes",
        "web_archive_snapshot_url",
        "favicon_url",
        "preview_image_url",
        "is_archived",
        "unread",
        "shared",
        "tag_names",
        "date_added",
        "date_modified",
        "website_title",
        "website_description",
    ] {
        assert!(keys.contains(&k), "missing {k}");
    }
    assert_eq!(b["tag_names"], json!(["tag-1", "Tag2"]));
    assert_eq!(
        (b["unread"].clone(), b["is_archived"].clone()),
        (json!(true), json!(false))
    );
    let date = b["date_added"].as_str().unwrap();
    assert!(
        date.len() == 27 && date.ends_with('Z') && date.as_bytes()[19] == b'.',
        "linkding date format: {date}"
    );
    assert!(
        b["web_archive_snapshot_url"]
            .as_str()
            .unwrap()
            .starts_with("https://web.archive.org/web/20")
    );
    assert!(
        b["web_archive_snapshot_url"]
            .as_str()
            .unwrap()
            .ends_with("/https://example.com")
    );

    // same URL again → silently updates the existing bookmark
    let (s, again) = api
        .call(
            "POST",
            "/api/bookmarks/",
            Some(json!({"url": "https://example.com/", "title": "New"})),
        )
        .await;
    assert_eq!(
        (s, again["id"].clone(), again["title"].clone()),
        (StatusCode::CREATED, b["id"].clone(), json!("New"))
    );

    // is_archived on create
    let (_, a) = api
        .call(
            "POST",
            "/api/bookmarks/",
            Some(json!({"url": "https://archived.example", "is_archived": true})),
        )
        .await;
    assert_eq!(a["is_archived"], json!(true));
}

#[tokio::test]
async fn validation_errors() {
    let api = Api::new().await;
    let (s, b) = api
        .call("POST", "/api/bookmarks/", Some(json!({"title": "no url"})))
        .await;
    assert_eq!(
        (s, b),
        (StatusCode::BAD_REQUEST, json!({"url": ["This field is required."]}))
    );
    let (s, b) = api
        .call("POST", "/api/bookmarks/", Some(json!({"url": "javascript:alert(1)"})))
        .await;
    assert_eq!((s, b["url"].is_array()), (StatusCode::BAD_REQUEST, true));
    let (s, b) = api
        .call(
            "POST",
            "/api/bookmarks/",
            Some(json!({"url": "https://x.com", "title": "t".repeat(513)})),
        )
        .await;
    assert_eq!((s, b["title"].is_array()), (StatusCode::BAD_REQUEST, true));

    let auth = format!("Token {}", api.token);
    let res = api
        .router
        .clone()
        .oneshot(
            Request::post("/api/bookmarks/")
                .header(header::AUTHORIZATION, auth)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_search_and_pagination() {
    let api = Api::new().await;
    for i in 0..5 {
        let tags = if i % 2 == 0 { json!(["even"]) } else { json!([]) };
        api.call(
            "POST",
            "/api/bookmarks/",
            Some(json!({"url": format!("https://x.com/{i}"), "tag_names": tags})),
        )
        .await;
    }
    let (s, b) = api.call("GET", "/api/bookmarks/?limit=2&offset=2", None).await;
    assert_eq!(
        (s, b["count"].clone(), b["results"].as_array().unwrap().len()),
        (StatusCode::OK, json!(5), 2)
    );
    assert_eq!(b["next"], "http://ld.test/api/bookmarks/?limit=2&offset=4");
    assert_eq!(b["previous"], "http://ld.test/api/bookmarks/?limit=2");

    let (_, b) = api.call("GET", "/api/bookmarks/?q=%23even", None).await;
    assert_eq!(b["count"], 3);
    let (_, b) = api.call("GET", "/api/bookmarks/?q=not+%23even", None).await;
    assert_eq!(b["count"], 2);
    let (_, b) = api
        .call("GET", "/api/bookmarks/?added_since=2999-01-01T00:00:00Z", None)
        .await;
    assert_eq!(b["count"], 0);

    // default limit 100, newest first
    let (_, b) = api.call("GET", "/api/bookmarks/", None).await;
    assert_eq!(
        (b["next"].clone(), b["results"][0]["url"].clone()),
        (Value::Null, json!("https://x.com/4"))
    );
}

#[tokio::test]
async fn retrieve_update_archive_delete() {
    let api = Api::new().await;
    let (_, b) = api
        .call(
            "POST",
            "/api/bookmarks/",
            Some(json!({"url": "https://a.com", "tag_names": ["x"]})),
        )
        .await;
    let id = b["id"].as_i64().unwrap();
    api.call("POST", "/api/bookmarks/", Some(json!({"url": "https://b.com"})))
        .await;
    let uri = format!("/api/bookmarks/{id}/");

    assert_eq!(api.call("GET", &uri, None).await.0, StatusCode::OK);
    let bob = format!("Token {}", api.other_token);
    assert_eq!(
        api.req("GET", &uri, Some(&bob), None).await.0,
        StatusCode::NOT_FOUND,
        "other users' bookmarks are invisible"
    );
    assert_eq!(api.req("DELETE", &uri, Some(&bob), None).await.0, StatusCode::NOT_FOUND);

    // PATCH: partial, tags untouched
    let (s, b) = api.call("PATCH", &uri, Some(json!({"title": "Patched"}))).await;
    assert_eq!(
        (s, b["title"].clone(), b["tag_names"].clone()),
        (StatusCode::OK, json!("Patched"), json!(["x"]))
    );
    // PUT needs url; omitted fields stay
    assert_eq!(
        api.call("PUT", &uri, Some(json!({"title": "x"}))).await.0,
        StatusCode::BAD_REQUEST
    );
    let (s, b) = api
        .call("PUT", &uri, Some(json!({"url": "https://a2.com", "tag_names": ["y"]})))
        .await;
    assert_eq!(
        (s, b["title"].clone(), b["tag_names"].clone()),
        (StatusCode::OK, json!("Patched"), json!(["y"]))
    );
    // duplicate URL
    let (s, b) = api.call("PATCH", &uri, Some(json!({"url": "https://b.com"}))).await;
    assert_eq!(
        (s, b),
        (
            StatusCode::BAD_REQUEST,
            json!({"url": ["A bookmark with this URL already exists."]})
        )
    );

    assert_eq!(
        api.call("POST", &format!("{uri}archive/"), None).await.0,
        StatusCode::NO_CONTENT
    );
    let (_, archived) = api.call("GET", "/api/bookmarks/archived/", None).await;
    assert_eq!(archived["results"][0]["id"], id);
    assert_eq!(api.call("GET", "/api/bookmarks/", None).await.1["count"], 1);
    assert_eq!(
        api.call("POST", &format!("{uri}unarchive/"), None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        api.call("POST", "/api/bookmarks/999/archive/", None).await.0,
        StatusCode::NOT_FOUND
    );

    assert_eq!(api.call("DELETE", &uri, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.call("DELETE", &uri, None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn check_endpoint() {
    let api = Api::new().await;
    let (_, b) = api
        .call("GET", "/api/bookmarks/check/?url=https%3A%2F%2Fa.com", None)
        .await;
    assert_eq!(
        (b["bookmark"].clone(), b["auto_tags"].clone()),
        (Value::Null, json!([]))
    );
    assert_eq!(b["metadata"]["url"], "https://a.com");
    api.call("POST", "/api/bookmarks/", Some(json!({"url": "https://a.com/"})))
        .await;
    let (_, b) = api
        .call("GET", "/api/bookmarks/check/?url=https%3A%2F%2FA.com", None)
        .await;
    assert_eq!(b["bookmark"]["url"], "https://a.com/", "matched via normalized URL");
}

#[tokio::test]
async fn tags_and_profile() {
    let api = Api::new().await;
    let (s, t) = api.call("POST", "/api/tags/", Some(json!({"name": "Rust"}))).await;
    assert_eq!((s, t["name"].clone()), (StatusCode::CREATED, json!("Rust")));
    let (_, again) = api.call("POST", "/api/tags/", Some(json!({"name": "rust"}))).await;
    assert_eq!(again["id"], t["id"], "existing tag returned");
    assert_eq!(
        api.call("POST", "/api/tags/", Some(json!({}))).await.0,
        StatusCode::BAD_REQUEST
    );
    let (_, list) = api.call("GET", "/api/tags/", None).await;
    assert_eq!(
        (list["count"].clone(), list["results"][0]["name"].clone()),
        (json!(1), json!("Rust"))
    );
    let tag_uri = format!("/api/tags/{}/", t["id"]);
    assert_eq!(api.call("GET", &tag_uri, None).await.1["name"], "Rust");
    let bob = format!("Token {}", api.other_token);
    assert_eq!(
        api.req("GET", &tag_uri, Some(&bob), None).await.0,
        StatusCode::NOT_FOUND
    );

    let (s, p) = api.call("GET", "/api/user/profile/", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        (p["theme"].clone(), p["enable_sharing"].clone()),
        (json!("auto"), json!(false))
    );
    assert!(p["search_preferences"].is_object() && p["version"].is_string());
}

#[tokio::test]
async fn browser_extension_origin_is_allowed_with_token() {
    let api = Api::new().await;
    let req = Request::post("/api/bookmarks/")
        .header(header::HOST, "ld.test")
        .header(header::ORIGIN, "chrome-extension://abcdef")
        .header("sec-fetch-site", "cross-site")
        .header(header::AUTHORIZATION, format!("Token {}", api.token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"url": "https://ext.example"}"#))
        .unwrap();
    assert_eq!(
        api.router.clone().oneshot(req).await.unwrap().status(),
        StatusCode::CREATED
    );

    // without a token (cookie auth) the same cross-site request is refused
    let req = Request::post("/api/bookmarks/")
        .header(header::HOST, "ld.test")
        .header(header::ORIGIN, "https://evil.example")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"url": "https://ext.example"}"#))
        .unwrap();
    assert_eq!(
        api.router.clone().oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn bundles_api() {
    let api = Api::new().await;
    api.call(
        "POST",
        "/api/bookmarks/",
        Some(json!({"url": "https://r.com", "tag_names": ["rust"]})),
    )
    .await;
    api.call(
        "POST",
        "/api/bookmarks/",
        Some(json!({"url": "https://g.com", "tag_names": ["go"]})),
    )
    .await;

    assert_eq!(
        api.call("POST", "/api/bundles/", Some(json!({"search": "x"}))).await.0,
        StatusCode::BAD_REQUEST
    );
    let (s, b) = api
        .call(
            "POST",
            "/api/bundles/",
            Some(json!({"name": "Rust", "any_tags": "rust"})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    for k in [
        "id",
        "name",
        "search",
        "any_tags",
        "all_tags",
        "excluded_tags",
        "filter_unread",
        "filter_shared",
        "order",
        "date_created",
        "date_modified",
    ] {
        assert!(b.get(k).is_some(), "missing {k}");
    }
    let (_, second) = api.call("POST", "/api/bundles/", Some(json!({"name": "Second"}))).await;
    assert_eq!((b["order"].clone(), second["order"].clone()), (json!(0), json!(1)));

    let (_, list) = api
        .call("GET", &format!("/api/bookmarks/?bundle={}", b["id"]), None)
        .await;
    assert_eq!(
        (list["count"].clone(), list["results"][0]["url"].clone()),
        (json!(1), json!("https://r.com"))
    );
    assert_eq!(
        api.call("GET", "/api/bookmarks/?bundle=999", None).await.0,
        StatusCode::NOT_FOUND
    );

    let uri = format!("/api/bundles/{}/", b["id"]);
    let (_, p) = api.call("PATCH", &uri, Some(json!({"any_tags": "go"}))).await;
    assert_eq!((p["name"].clone(), p["any_tags"].clone()), (json!("Rust"), json!("go")));
    assert_eq!(
        api.call("PUT", &uri, Some(json!({"any_tags": "x"}))).await.0,
        StatusCode::BAD_REQUEST,
        "PUT needs name"
    );
    assert_eq!(
        api.call("PATCH", &uri, Some(json!({"filter_unread": "maybe"}))).await.0,
        StatusCode::BAD_REQUEST
    );
    let bob = format!("Token {}", api.other_token);
    assert_eq!(api.req("GET", &uri, Some(&bob), None).await.0, StatusCode::NOT_FOUND);
    let (_, all) = api.call("GET", "/api/bundles/?limit=1", None).await;
    assert_eq!(
        (all["count"].clone(), all["results"].as_array().unwrap().len()),
        (json!(2), 1)
    );
    assert_eq!(api.call("DELETE", &uri, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(
        api.call("GET", "/api/bundles/", None).await.1["results"][0]["order"],
        0,
        "renumbered"
    );
}

#[tokio::test]
async fn shared_bookmarks_api() {
    let api = Api::new().await;
    let alice = format!("Token {}", api.token);
    let bob = format!("Token {}", api.other_token);
    api.req(
        "POST",
        "/api/bookmarks/",
        Some(&alice),
        Some(json!({"url": "https://a.com", "shared": true})),
    )
    .await;
    api.req(
        "POST",
        "/api/bookmarks/",
        Some(&bob),
        Some(json!({"url": "https://b.com", "shared": true})),
    )
    .await;
    api.req(
        "POST",
        "/api/bookmarks/",
        Some(&bob),
        Some(json!({"url": "https://private.com"})),
    )
    .await;
    let urls = |v: &Value| {
        let mut u: Vec<String> = v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["url"].as_str().unwrap().into())
            .collect();
        u.sort();
        u
    };
    assert_eq!(
        api.req("GET", "/api/bookmarks/shared/", Some(&alice), None).await.1["count"],
        0,
        "sharing is off by default"
    );

    // alice shares publicly, bob only with other users
    let enable = "UPDATE user_profiles SET enable_sharing = 1, enable_public_sharing = (user_id = 1)";
    api.db.execute(rlinks::stmt!(enable)).await.unwrap();
    let (_, as_alice) = api.req("GET", "/api/bookmarks/shared/", Some(&alice), None).await;
    assert_eq!(urls(&as_alice), ["https://a.com", "https://b.com"]);
    let (s, anon) = api.req("GET", "/api/bookmarks/shared/", None, None).await;
    assert_eq!(
        (s, urls(&anon)),
        (StatusCode::OK, vec!["https://a.com".to_string()]),
        "anonymous: public only"
    );
    let (_, only_bob) = api
        .req("GET", "/api/bookmarks/shared/?user=bob", Some(&alice), None)
        .await;
    assert_eq!(urls(&only_bob), ["https://b.com"]);
    assert_eq!(
        api.req("GET", "/api/bookmarks/shared/", Some("Token nope"), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

impl Api {
    async fn multipart(
        &self,
        uri: &str,
        auth: &str,
        fields: &[(&str, Option<&str>, &[u8])],
    ) -> (StatusCode, Value, axum::http::HeaderMap) {
        let boundary = "XBOUNDARYX";
        let mut body = Vec::new();
        for (name, filename, data) in fields {
            let disposition = match filename {
                Some(f) => format!("form-data; name=\"{name}\"; filename=\"{f}\"\r\nContent-Type: text/html"),
                None => format!("form-data; name=\"{name}\""),
            };
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: {disposition}\r\n\r\n").as_bytes());
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let req = Request::post(uri)
            .header(header::HOST, "ld.test")
            .header(header::AUTHORIZATION, auth)
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        let res = self.router.clone().oneshot(req).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), headers)
    }
}

#[tokio::test]
async fn assets_api() {
    let api = Api::new().await;
    let alice = format!("Token {}", api.token);
    let bob = format!("Token {}", api.other_token);
    let (_, b) = api
        .call(
            "POST",
            "/api/bookmarks/?disable_html_snapshot",
            Some(json!({"url": "https://a.com"})),
        )
        .await;
    let id = b["id"].as_i64().unwrap();

    let (s, a, _) = api
        .multipart(
            &format!("/api/bookmarks/{id}/assets/upload/"),
            &alice,
            &[("file", Some("page.html"), b"<p>hi</p>")],
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{a}");
    let keys: Vec<&str> = a.as_object().unwrap().keys().map(String::as_str).collect();
    for k in [
        "id",
        "bookmark",
        "date_created",
        "file_size",
        "asset_type",
        "content_type",
        "display_name",
        "status",
    ] {
        assert!(keys.contains(&k), "missing {k}");
    }
    assert!(
        !keys.contains(&"file") && !keys.contains(&"gzip"),
        "storage details stay private"
    );
    assert_eq!(
        (
            a["bookmark"].clone(),
            a["asset_type"].clone(),
            a["display_name"].clone()
        ),
        (json!(id), json!("upload"), json!("page.html"))
    );
    let asset = a["id"].as_i64().unwrap();

    let (_, list) = api.call("GET", &format!("/api/bookmarks/{id}/assets/"), None).await;
    assert_eq!(
        (list["count"].clone(), list["results"][0]["id"].clone()),
        (json!(1), json!(asset))
    );
    assert_eq!(
        api.call("GET", &format!("/api/bookmarks/{id}/assets/{asset}/"), None)
            .await
            .1["status"],
        "complete"
    );

    let res = api
        .router
        .clone()
        .oneshot(
            Request::get(format!("/api/bookmarks/{id}/assets/{asset}/download/"))
                .header(header::AUTHORIZATION, &alice)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    assert_eq!(&to_bytes(res.into_body(), usize::MAX).await.unwrap()[..], b"<p>hi</p>");

    // ownership and path consistency
    assert_eq!(
        api.req("GET", &format!("/api/bookmarks/{id}/assets/"), Some(&bob), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.req("GET", &format!("/api/bookmarks/{id}/assets/{asset}/"), Some(&bob), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.multipart(
            &format!("/api/bookmarks/{id}/assets/upload/"),
            &bob,
            &[("file", Some("x.html"), b"x")]
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, other) = api
        .call(
            "POST",
            "/api/bookmarks/?disable_html_snapshot",
            Some(json!({"url": "https://b.com"})),
        )
        .await;
    assert_eq!(
        api.call("GET", &format!("/api/bookmarks/{}/assets/{asset}/", other["id"]), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let (s, e, _) = api
        .multipart(
            &format!("/api/bookmarks/{id}/assets/upload/"),
            &alice,
            &[("other", None, b"x")],
        )
        .await;
    assert_eq!(
        (s, e["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("No file provided."))
    );

    assert_eq!(
        api.call("DELETE", &format!("/api/bookmarks/{id}/assets/{asset}/"), None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        api.call("GET", &format!("/api/bookmarks/{id}/assets/"), None).await.1["count"],
        0
    );
}

#[tokio::test]
async fn singlefile_upload_from_the_browser_extension() {
    let api = Api::new().await;
    let alice = format!("Token {}", api.token);
    let page = b"<html><head><title>Captured</title></head><body>Logged-in content</body></html>";
    // unknown URL: the bookmark is created, the capture becomes its snapshot
    let (s, body, _) = api
        .multipart(
            "/api/bookmarks/singlefile/",
            &alice,
            &[
                ("url", None, b"https://app.example/inbox"),
                ("file", Some("x.html"), page),
            ],
        )
        .await;
    assert_eq!(
        (s, body["message"].as_str()),
        (StatusCode::CREATED, Some("Snapshot uploaded successfully."))
    );
    let (_, list) = api.call("GET", "/api/bookmarks/", None).await;
    assert_eq!(list["results"][0]["url"], "https://app.example/inbox");
    let id = list["results"][0]["id"].as_i64().unwrap();
    let (_, assets) = api.call("GET", &format!("/api/bookmarks/{id}/assets/"), None).await;
    assert_eq!(
        (
            assets["results"][0]["asset_type"].clone(),
            assets["results"][0]["status"].clone()
        ),
        (json!("snapshot"), json!("complete"))
    );
    let asset = assets["results"][0]["id"].as_i64().unwrap();
    let res = api
        .router
        .clone()
        .oneshot(
            Request::get(format!("/api/bookmarks/{id}/assets/{asset}/download/"))
                .header(header::AUTHORIZATION, &alice)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        &to_bytes(res.into_body(), usize::MAX).await.unwrap()[..],
        page,
        "stored gzipped, served as uploaded"
    );

    // known URL: attached to the existing bookmark
    api.multipart(
        "/api/bookmarks/singlefile/",
        &alice,
        &[
            ("url", None, b"https://app.example/inbox/"),
            ("file", Some("x.html"), page),
        ],
    )
    .await;
    assert_eq!(api.call("GET", "/api/bookmarks/", None).await.1["count"], 1);
    assert_eq!(
        api.call("GET", &format!("/api/bookmarks/{id}/assets/"), None).await.1["count"],
        2
    );

    let (s, e, _) = api
        .multipart("/api/bookmarks/singlefile/", &alice, &[("url", None, b"https://x.com")])
        .await;
    assert_eq!(
        (s, e["error"].as_str()),
        (
            StatusCode::BAD_REQUEST,
            Some("Both 'url' and 'file' parameters are required.")
        )
    );
    let (s, _, _) = api
        .multipart(
            "/api/bookmarks/singlefile/",
            &alice,
            &[("url", None, b"javascript:alert(1)"), ("file", Some("x"), b"x")],
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
