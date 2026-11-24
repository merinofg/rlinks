//! HTML snapshots: the page as one self-contained HTML file, for reading later even if the site
//! changes or disappears.
//!
//! Every request (page, stylesheets, images, fonts) goes through the SSRF-guarded `Fetcher`, so a
//! page can't make the server fetch internal addresses for it. (linkding's SingleFile/Chromium
//! and the `monolith` crate both fetch sub-resources with their own unguarded clients.)
//!
//! Stylesheets are inlined, `url()`s in CSS and images become `data:` URIs, scripts and frames are
//! dropped: snapshots are served sandboxed without scripts anyway. Static pages come out well;
//! pages rendered by JavaScript are what a browser-side capture (SingleFile upload) is for.

use anyhow::{Result, bail};
use base64::Engine;
use futures::StreamExt;
use lol_html::html_content::ContentType;
use lol_html::{RewriteStrSettings, element, rewrite_str, text};
use scraper::{Html, Selector};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use url::Url;

use crate::scrape::Fetcher;

const MAX_PAGE: usize = 10 * 1024 * 1024;
const MAX_RESOURCE: usize = 5 * 1024 * 1024;
/// For everything a snapshot fetches besides the page: stylesheets, imports and resources share it.
const MAX_TOTAL: usize = 25 * 1024 * 1024;
const MAX_RESOURCES: usize = 200;
const PARALLEL: usize = 6;

/// Capture `page_url` as a single HTML document.
pub async fn capture(fetcher: &Fetcher, page_url: &str) -> Result<String> {
    let (body, media_type) = fetcher.fetch(page_url, MAX_PAGE).await?;
    if !media_type.is_empty() && !media_type.contains("html") {
        bail!("not an HTML page ({media_type})");
    }
    // ponytail: assumes UTF-8 (lossy), like metadata scraping; add charset detection if needed.
    let html = String::from_utf8_lossy(&body).into_owned();
    let page = Url::parse(page_url)?;
    let parsed = Parsed::new(&html, &page);
    // One byte budget for everything fetched besides the page.
    let budget = AtomicUsize::new(MAX_TOTAL);
    let text = |b: bytes::Bytes, _: String| String::from_utf8_lossy(&b).into_owned();

    // Stylesheets (and one level of @import) first: their url()s are resources too.
    let sheets = fetch_all(fetcher, parsed.stylesheets.clone(), &budget, text).await;
    let imports: Vec<Url> = sheets.iter().flat_map(|(u, css)| css_refs(css, u).imports).collect();
    let imported = fetch_all(fetcher, imports, &budget, text).await;

    let mut resources = parsed.images.clone();
    for (base, css) in sheets
        .iter()
        .chain(imported.iter())
        .map(|(u, c)| (u.clone(), c.as_str()))
        .chain(parsed.inline_css.iter().map(|c| (parsed.base.clone(), c.as_str())))
    {
        resources.extend(css_refs(css, &base).urls);
    }
    resources.sort();
    resources.dedup();
    resources.truncate(MAX_RESOURCES);
    let data = fetch_all(fetcher, resources, &budget, |b, t| data_uri(&t, &b)).await;

    // Rewrite CSS: url() → data URI, @import → data URI of the (rewritten) imported sheet.
    let imported_css: HashMap<Url, String> = imported
        .iter()
        .map(|(u, css)| (u.clone(), rewrite_css(css, u, &data, &HashMap::new())))
        .collect();
    let import_uris: HashMap<Url, String> = imported_css
        .iter()
        .map(|(u, css)| (u.clone(), data_uri("text/css", css.as_bytes())))
        .collect();
    let sheet_css: HashMap<Url, String> = sheets
        .iter()
        .map(|(u, css)| (u.clone(), rewrite_css(css, u, &data, &import_uris)))
        .collect();

    rewrite_html(&html, &parsed.base, &data, &sheet_css)
}

/// What the page references, resolved against its base URL.
struct Parsed {
    base: Url,
    stylesheets: Vec<Url>,
    images: Vec<Url>,
    inline_css: Vec<String>,
}

impl Parsed {
    fn new(html: &str, page: &Url) -> Parsed {
        let doc = Html::parse_document(html);
        let sel = |s: &str| Selector::parse(s).expect("valid selector");
        let base = doc
            .select(&sel("base[href]"))
            .next()
            .and_then(|b| page.join(b.value().attr("href")?).ok())
            .unwrap_or_else(|| page.clone());
        let resolve = |v: Option<&str>| v.and_then(|v| base.join(v.trim()).ok()).filter(is_fetchable);
        let stylesheets = doc
            .select(&sel("link[rel][href]"))
            .filter(|l| is_stylesheet(l.value().attr("rel").unwrap_or_default()))
            .filter_map(|l| resolve(l.value().attr("href")))
            .collect();
        let mut images: Vec<Url> = Vec::new();
        for (selector, attr) in [
            ("img[src]", "src"),
            ("input[type=image][src]", "src"),
            ("video[poster]", "poster"),
            ("link[rel~=icon][href]", "href"),
        ] {
            images.extend(doc.select(&sel(selector)).filter_map(|e| resolve(e.value().attr(attr))));
        }
        let mut inline_css: Vec<String> = doc.select(&sel("style")).map(|s| s.text().collect()).collect();
        inline_css.extend(
            doc.select(&sel("[style]"))
                .filter_map(|e| e.value().attr("style").map(String::from)),
        );
        Parsed {
            base,
            stylesheets,
            images,
            inline_css,
        }
    }
}

fn is_stylesheet(rel: &str) -> bool {
    rel.split_ascii_whitespace()
        .any(|r| r.eq_ignore_ascii_case("stylesheet"))
}

fn is_fetchable(u: &Url) -> bool {
    matches!(u.scheme(), "http" | "https")
}

/// Fetch URLs concurrently, drawing on the snapshot's shared byte budget; failures and
/// anything past the budget are skipped (the snapshot then lacks those resources).
async fn fetch_all<T>(
    fetcher: &Fetcher,
    urls: Vec<Url>,
    budget: &AtomicUsize,
    convert: impl Fn(bytes::Bytes, String) -> T,
) -> Vec<(Url, T)> {
    let mut out = Vec::new();
    let mut results = futures::stream::iter(urls)
        .map(|u| async move { (fetcher.fetch(u.as_str(), MAX_RESOURCE).await, u) })
        .buffer_unordered(PARALLEL);
    while let Some((res, url)) = results.next().await {
        match res {
            Ok((body, media_type)) => {
                let taken = budget.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(body.len())
                });
                match taken {
                    Ok(_) => out.push((url, convert(body, media_type))),
                    Err(_) => tracing::debug!("snapshot size budget exhausted, skipping {url}"),
                }
            }
            Err(e) => tracing::debug!("snapshot resource {url}: {e:#}"),
        }
    }
    out
}

fn data_uri(media_type: &str, body: &[u8]) -> String {
    let media_type = match media_type {
        t if !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || "/+-.".contains(c)) => t,
        _ => "application/octet-stream",
    };
    format!(
        "data:{media_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(body)
    )
}

// ---------------------------------------------------------------------------------------------
// CSS

/// A reference inside CSS: byte range of the URL text and the resolved URL.
struct CssRef {
    range: std::ops::Range<usize>,
    url: Url,
    import: bool,
}

#[derive(Default)]
struct Refs {
    urls: Vec<Url>,
    imports: Vec<Url>,
}

fn css_refs(css: &str, base: &Url) -> Refs {
    let mut refs = Refs::default();
    for r in scan_css(css, base) {
        if r.import {
            refs.imports.push(r.url)
        } else {
            refs.urls.push(r.url)
        }
    }
    refs
}

/// Finds `url(...)` and `@import "..."` references (comments aren't special-cased: a commented
/// out reference is merely fetched and inlined too).
fn scan_css(css: &str, base: &Url) -> Vec<CssRef> {
    // Work on bytes: `i` walks byte by byte and may sit inside a multi-byte character, so the
    // string is only sliced at positions next to ASCII delimiters.
    let bytes = css.as_bytes();
    let lower = css.to_ascii_lowercase().into_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &lower[i..];
        let (import, start) = if rest.starts_with(b"url(") {
            let before = lower[..i].trim_ascii_end();
            (before.ends_with(b"@import"), i + 4)
        } else if rest.starts_with(b"@import") {
            let mut j = i + 7;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                (true, j)
            } else {
                i += 7;
                continue;
            }
        } else {
            i += 1;
            continue;
        };
        // Value: quoted string or bare token up to ')'.
        let mut j = start;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let (value_start, value_end, next) = match bytes.get(j) {
            Some(&q @ (b'"' | b'\'')) => match css[j + 1..].find(q as char) {
                Some(len) => (j + 1, j + 1 + len, j + 2 + len),
                None => break,
            },
            _ => match css[j..].find(')') {
                Some(len) => (j, (j + len), j + len),
                None => break,
            },
        };
        let raw = css[value_start..value_end].trim();
        if !raw.starts_with("data:")
            && !raw.is_empty()
            && let Some(url) = base.join(raw).ok().filter(is_fetchable)
        {
            out.push(CssRef {
                range: value_start..value_end,
                url,
                import,
            });
        }
        i = next.max(i + 1);
    }
    out
}

/// Replace references with data URIs; unknown ones become absolute URLs.
fn rewrite_css(css: &str, base: &Url, data: &[(Url, String)], imports: &HashMap<Url, String>) -> String {
    let lookup: HashMap<&Url, &String> = data.iter().map(|(u, d)| (u, d)).collect();
    let mut out = String::with_capacity(css.len());
    let mut last = 0;
    for r in scan_css(css, base) {
        let replacement = if r.import {
            imports.get(&r.url)
        } else {
            lookup.get(&r.url).copied()
        };
        out.push_str(&css[last..r.range.start]);
        out.push_str(replacement.map_or(r.url.as_str(), |s| s.as_str()));
        last = r.range.end;
    }
    out.push_str(&css[last..]);
    out
}

// ---------------------------------------------------------------------------------------------
// HTML

fn rewrite_html(html: &str, base: &Url, data: &[(Url, String)], sheets: &HashMap<Url, String>) -> Result<String> {
    let lookup: HashMap<&Url, &String> = data.iter().map(|(u, d)| (u, d)).collect();
    // lol_html hands out raw attribute values (`a.php?x=1&amp;y=2`); decode them so they match
    // the URLs collected from the parsed document.
    let resolve = |v: Option<String>| v.and_then(|v| base.join(decode_entities(&v).trim()).ok());
    let inline = |v: Option<String>| resolve(v).map(|u| lookup.get(&u).map_or(u.to_string(), |d| d.to_string()));
    let style_text = std::cell::RefCell::new(String::new());
    let no_imports = HashMap::new();
    // Inlined CSS can't be allowed to close its <style> element.
    let css = |s: &str| rewrite_css(s, base, data, &no_imports).replace("</", "<\\/");

    let settings = RewriteStrSettings::new()
        // Nothing may run or load in a snapshot.
        .append_element_content_handler(element!(
            "script, iframe, frame, object, embed, base, meta[http-equiv]",
            |el| {
                el.remove();
                Ok(())
            }
        ))
        .append_element_content_handler(element!("link[rel][href]", |el| {
            let rel = el.get_attribute("rel").unwrap_or_default();
            if is_stylesheet(&rel) {
                match resolve(el.get_attribute("href")).and_then(|u| sheets.get(&u)) {
                    Some(sheet) => el.replace(
                        &format!("<style>{}</style>", sheet.replace("</", "<\\/")),
                        ContentType::Html,
                    ),
                    None => el.remove(),
                }
            } else if rel.to_ascii_lowercase().split_whitespace().any(|r| r == "icon") {
                if let Some(v) = inline(el.get_attribute("href")) {
                    el.set_attribute("href", &v)?;
                }
            } else {
                el.remove(); // preload, prefetch, manifest...
            }
            Ok(())
        }))
        .append_element_content_handler(element!("img, input[type=image]", |el| {
            if let Some(v) = inline(el.get_attribute("src")) {
                el.set_attribute("src", &v)?;
            }
            el.remove_attribute("srcset");
            el.remove_attribute("loading");
            Ok(())
        }))
        // srcset variants; the <img> fallback carries the inlined image.
        .append_element_content_handler(element!("picture source", |el| {
            el.remove();
            Ok(())
        }))
        .append_element_content_handler(element!("video[poster]", |el| {
            if let Some(v) = inline(el.get_attribute("poster")) {
                el.set_attribute("poster", &v)?;
            }
            Ok(())
        }))
        .append_element_content_handler(element!("[style]", |el| {
            if let Some(s) = el.get_attribute("style") {
                el.set_attribute("style", &rewrite_css(&decode_entities(&s), base, data, &no_imports))?;
            }
            Ok(())
        }))
        .append_element_content_handler(element!("a[href]", |el| {
            if let Some(u) = resolve(el.get_attribute("href")) {
                el.set_attribute("href", u.as_str())?;
            }
            Ok(())
        }))
        .append_element_content_handler(element!("head", |el| {
            el.prepend(r#"<meta charset="utf-8">"#, ContentType::Html);
            Ok(())
        }))
        .append_element_content_handler(text!("style", |t| {
            let mut buf = style_text.borrow_mut();
            buf.push_str(t.as_str());
            if t.last_in_text_node() {
                t.replace(&css(&buf), ContentType::Html);
                buf.clear();
            } else {
                t.remove();
            }
            Ok(())
        }));
    Ok(rewrite_str(html, settings)?)
}

/// Decode HTML character references in an attribute value: the named ones that occur in URLs
/// and CSS, plus numeric ones. Unknown references are kept as written.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let decoded = rest.find(';').filter(|&end| end <= 10).and_then(|end| {
            let name = &rest[1..end];
            let c = match name {
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "lt" => Some('<'),
                "gt" => Some('>'),
                _ => match name.strip_prefix('#') {
                    Some(n) => match n.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => n.parse().ok(),
                    }
                    .and_then(char::from_u32),
                    None => None,
                },
            }?;
            Some((c, end + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_references() {
        let base = Url::parse("https://ex.com/css/site.css").unwrap();
        let css = r#"@import "reset.css"; @import url('theme.css');
            body { background: url(bg.png) } .a{background:url( "/img/a.svg" )} .d{x:url(data:image/png;base64,AA)}"#;
        let refs = css_refs(css, &base);
        let s = |v: &[Url]| v.iter().map(|u| u.to_string()).collect::<Vec<_>>();
        assert_eq!(
            s(&refs.imports),
            ["https://ex.com/css/reset.css", "https://ex.com/css/theme.css"]
        );
        assert_eq!(s(&refs.urls), ["https://ex.com/css/bg.png", "https://ex.com/img/a.svg"]);

        let data = vec![(
            Url::parse("https://ex.com/css/bg.png").unwrap(),
            "data:image/png;base64,QQ==".to_string(),
        )];
        let out = rewrite_css(
            "body{background:url(bg.png)} .x{background:url('missing.png')}",
            &base,
            &data,
            &HashMap::new(),
        );
        assert_eq!(
            out,
            "body{background:url(data:image/png;base64,QQ==)} .x{background:url('https://ex.com/css/missing.png')}"
        );
    }

    #[test]
    fn entities_in_attributes() {
        assert_eq!(
            decode_entities("load.php?lang=en&amp;modules=a%7Cb&amp;only=styles"),
            "load.php?lang=en&modules=a%7Cb&only=styles"
        );
        assert_eq!(decode_entities("&#39;x&#x27;&quot;&lt;&gt;"), "'x'\"<>");
        assert_eq!(decode_entities("a & b &unknown; &"), "a & b &unknown; &");
        // the rewriter now inlines a stylesheet whose href contains &amp;
        let base = Url::parse("https://ex.com/").unwrap();
        let sheets = HashMap::from([(
            Url::parse("https://ex.com/load.php?a=1&b=2").unwrap(),
            "p{color:red}".to_string(),
        )]);
        let out = rewrite_html(
            r#"<link rel="stylesheet" href="/load.php?a=1&amp;b=2">"#,
            &base,
            &[],
            &sheets,
        )
        .unwrap();
        assert!(out.contains("<style>p{color:red}</style>"), "{out}");
    }

    #[test]
    fn css_with_non_ascii_text() {
        let base = Url::parse("https://ex.com/").unwrap();
        let css = r#".a::before{content:"→ é"} .b{background:url(ü.png)} .c{content:"日本"}"#;
        let refs = css_refs(css, &base);
        assert_eq!(refs.urls.len(), 1);
        assert_eq!(refs.urls[0].as_str(), "https://ex.com/%C3%BC.png");
        assert_eq!(rewrite_css(css, &base, &[], &HashMap::new()).matches("→ é").count(), 1);
    }

    #[test]
    fn html_is_made_self_contained_and_inert() {
        let base = Url::parse("https://ex.com/a/").unwrap();
        let data = vec![(
            Url::parse("https://ex.com/a/p.png").unwrap(),
            "data:image/png;base64,QQ==".to_string(),
        )];
        let sheets = HashMap::from([(
            Url::parse("https://ex.com/s.css").unwrap(),
            "h1{color:red}</style>".to_string(),
        )]);
        let html = r#"<html><head><link rel="stylesheet" href="/s.css"><link rel="preload" href="x.js">
            <script>alert(1)</script><meta http-equiv="refresh" content="0;url=https://evil"></head>
            <body><img src="p.png" srcset="p2.png 2x"><a href="../b">b</a><iframe src="https://x"></iframe>
            <div style="background:url(p.png)"></div><style>.x{background:url('p.png')}</style></body></html>"#;
        let out = rewrite_html(html, &base, &data, &sheets).unwrap();
        assert!(out.contains("<style>h1{color:red}<\\/style></style>"), "{out}");
        assert!(
            !out.contains("<script") && !out.contains("iframe") && !out.contains("refresh") && !out.contains("preload")
        );
        assert!(out.contains(r#"<img src="data:image/png;base64,QQ==">"#), "{out}");
        assert!(out.contains(r#"href="https://ex.com/b""#));
        assert!(out.contains("style=\"background:url(data:image/png;base64,QQ==)\""));
        assert!(out.contains(".x{background:url('data:image/png;base64,QQ==')}"));
        assert!(out.contains(r#"<head><meta charset="utf-8">"#));
    }
}
