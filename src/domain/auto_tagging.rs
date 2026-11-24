//! Auto-tagging rules (port of linkding's `services/auto_tagging.py`). One rule per line:
//! `<url pattern> <tag> [<tag>...]`, `#` starts a comment. A pattern matches when the URL's host
//! ends with the pattern's host (so subdomains match), and the URL's path, query parameters and
//! fragment start with / contain the pattern's. Everything is case-insensitive.

use std::collections::{BTreeSet, HashMap};
use url::Url;

pub fn get_tags(rules: &str, url: &str) -> Vec<String> {
    let mut tags = BTreeSet::new();
    let Some(url) = Url::parse(&url.to_lowercase()).ok().filter(|u| u.host_str().is_some()) else {
        return vec![];
    };
    for line in rules.to_lowercase().lines() {
        let line = strip_comment(line.trim());
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let pattern = parts[0].trim_start_matches("https://").trim_start_matches("http://");
        let Ok(pattern) = Url::parse(&format!("http://{pattern}")) else {
            continue;
        };
        if matches(&pattern, &url) {
            tags.extend(parts[1..].iter().map(|t| t.to_string()));
        }
    }
    tags.into_iter().collect()
}

/// Drops `# comment` (a `#` at line start, or preceded by whitespace; `/#frag` is no comment).
fn strip_comment(line: &str) -> &str {
    if line.starts_with('#') {
        return "";
    }
    let bytes = line.as_bytes();
    match (1..bytes.len()).find(|&i| bytes[i] == b'#' && bytes[i - 1].is_ascii_whitespace()) {
        Some(i) => line[..i].trim_end(),
        None => line,
    }
}

fn matches(pattern: &Url, url: &Url) -> bool {
    // Hosts are IDNA (punycode) encoded by the URL parser, so Unicode and punycode rules agree.
    let (Some(expected), Some(actual)) = (pattern.host_str(), url.host_str()) else {
        return false;
    };
    if !actual.ends_with(expected) {
        return false;
    }
    if pattern.path() != "/" && !url.path().starts_with(pattern.path()) {
        return false;
    }
    if pattern.query().is_some_and(|q| !q.is_empty()) {
        let mut actual: HashMap<String, Vec<String>> = HashMap::new();
        for (k, v) in url.query_pairs() {
            actual.entry(k.into_owned()).or_default().push(v.into_owned());
        }
        let ok = pattern.query_pairs().all(|(k, v)| {
            actual
                .get(k.as_ref())
                .is_some_and(|values| v.is_empty() || values.iter().any(|a| *a == v))
        });
        if !ok {
            return false;
        }
    }
    match pattern.fragment() {
        Some(f) if !f.is_empty() => url.fragment().unwrap_or_default().starts_with(f),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::get_tags;

    fn tags(rules: &str, url: &str) -> Vec<String> {
        get_tags(rules, url)
    }

    // Ported from linkding's bookmarks/tests/test_auto_tagging.py
    #[test]
    fn by_domain() {
        let rules = "example.com example\ntest.com test";
        assert_eq!(tags(rules, "https://example.com/"), ["example"]);
        assert!(tags(rules, "https://").is_empty());
        assert!(tags(rules, "example.com").is_empty());
        assert_eq!(tags(rules, "https://example.com:8080/"), ["example"]);
        assert_eq!(tags("EXAMPLE.com example", "https://example.com/"), ["example"]);
        assert_eq!(
            tags("example.com one two three", "https://example.com/"),
            ["one", "three", "two"]
        );
        assert!(tags("example.com example", "https://test.com/example.com").is_empty());
        assert!(tags("example.com example", "https://example.com.bad-website.com/").is_empty());
        assert!(tags("example.com", "https://example.com/").is_empty());
        let subdomains = "example.com example\ntest.example.com test\nsome.example.com some";
        assert_eq!(tags(subdomains, "https://test.example.com/"), ["example", "test"]);
        assert_eq!(
            tags(
                "https://example.com/ https\nhttp://example.com/ http",
                "http://example.com/"
            ),
            ["http", "https"]
        );
    }

    #[test]
    fn idn_domains() {
        assert_eq!(
            tags("रजिस्ट्री.भारत tag1", "https://www.xn--81bg3cc2b2bk5hb.xn--h2brj9c/"),
            ["tag1"]
        );
        assert_eq!(
            tags("xn--81bg3cc2b2bk5hb.xn--h2brj9c tag1", "https://www.रजिस्ट्री.भारत/"),
            ["tag1"]
        );
    }

    #[test]
    fn by_path() {
        let rules = "example.com/one one\nexample.com/two two\ntest.com test";
        assert_eq!(tags(rules, "https://example.com/one/"), ["one"]);
        assert_eq!(tags(rules, "https://example.com/one/two"), ["one"]);
        assert_eq!(tags("example.com/One one", "https://example.com/one/"), ["one"]);
    }

    #[test]
    fn by_query_string() {
        let rules = "
            example.com/page?a=b tag1     # true, matches a=b
            example.com/page?a=c&c=d tag2 # true, matches both a=c and c=d
            example.com/page?c=d&l=p tag3 # false, l=p doesn't exists
            example.com/page?a=bb tag4    # false bb != b
            example.com/page?a=b&a=c tag5 # true, matches both a=b and a=c
            example.com/page?a=B tag6     # true, matches a=b because case insensitive
            example.com/page?A=b tag7     # true, matches a=b because case insensitive
        ";
        assert_eq!(
            tags(rules, "https://example.com/page/some?z=x&a=b&v=b&c=d&o=p&a=c"),
            ["tag1", "tag2", "tag5", "tag6", "tag7"]
        );
        assert_eq!(
            tags(
                "example.com/page?a= tag1\nexample.com/page?b= tag2",
                "https://example.com/page/some?a=value"
            ),
            ["tag1"]
        );
        assert_eq!(
            tags(
                "example.com/page?a=йцу tag1\nexample.com/page?a=%D0%B9%D1%86%D1%83 tag2",
                "https://example.com/page?a=%D0%B9%D1%86%D1%83"
            ),
            ["tag1", "tag2"]
        );
    }

    #[test]
    fn by_fragment() {
        let rules = "example.com/#/section/1 section1\nexample.com/#/section/2 section2";
        assert_eq!(tags(rules, "https://example.com/#/section/1"), ["section1"]);
        assert_eq!(
            tags("example.com/#/section section", "https://example.com/#/section/1"),
            ["section"]
        );
        assert_eq!(
            tags("example.com/#SECTION section", "https://example.com/#section"),
            ["section"]
        );
        assert_eq!(
            tags(
                "example.com/#section1 section1 #This is a comment",
                "https://example.com/#section1"
            ),
            ["section1"]
        );
    }
}
