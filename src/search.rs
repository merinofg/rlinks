//! linkding's search language (port of `bookmarks/services/search_query_parser.py` and the
//! AST→filter part of `bookmarks/queries.py`).
//!
//! `history rome` · `"history of rome"` · `#book` · `!unread` · `!untagged` ·
//! `#history and (#book or #article)` · `not #article`. Precedence: OR < AND (explicit or
//! implicit) < NOT (right-assoc) < primary. Operators are case-insensitive.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Term(String),
    Tag(String),
    Keyword(String),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Term(String),
    Tag(String),
    Keyword(String),
    And,
    Or,
    Not,
    LParen,
    RParen,
}

fn tokenize(query: &str) -> Vec<Tok> {
    let chars: Vec<char> = query.trim().chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    // Read until whitespace or one of `stop`.
    let read_word = |i: &mut usize, stop: &str| {
        let start = *i;
        while *i < chars.len() && !chars[*i].is_whitespace() && !stop.contains(chars[*i]) {
            *i += 1;
        }
        chars[start..*i].iter().collect::<String>()
    };
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' {
            out.push(Tok::LParen);
            i += 1;
        } else if c == ')' {
            out.push(Tok::RParen);
            i += 1;
        } else if c == '"' || c == '\'' {
            i += 1;
            let mut s = String::new();
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' {
                    i += 1;
                    if let Some(&e) = chars.get(i) {
                        s.push(match e {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            other => other, // covers \\ and the quote char
                        });
                        i += 1;
                    }
                } else {
                    s.push(chars[i]);
                    i += 1;
                }
            }
            i += 1; // closing quote; unclosed quotes run to the end
            out.push(Tok::Term(s));
        } else if c == '#' || c == '!' {
            i += 1;
            let w = read_word(&mut i, "()\"'");
            if !w.is_empty() {
                out.push(if c == '#' { Tok::Tag(w) } else { Tok::Keyword(w) });
            }
        } else {
            let w = read_word(&mut i, "()\"'#!");
            out.push(match w.to_lowercase().as_str() {
                "and" => Tok::And,
                "or" => Tok::Or,
                "not" => Tok::Not,
                _ => Tok::Term(w),
            });
        }
    }
    out
}

#[derive(Debug, PartialEq)]
pub struct ParseError(pub String);

/// `Ok(None)` for an empty query.
pub fn parse(query: &str) -> Result<Option<Expr>, ParseError> {
    let toks = tokenize(query);
    if toks.is_empty() {
        return Ok(None);
    }
    let mut p = Parser { toks, pos: 0 };
    let expr = p.or()?;
    match p.peek() {
        None => Ok(Some(expr)),
        Some(t) => Err(ParseError(format!("Unexpected token {t:?}"))),
    }
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn or(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.and()?;
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.not()?;
        loop {
            match self.peek() {
                Some(Tok::And) => self.pos += 1,
                Some(Tok::Term(_) | Tok::Tag(_) | Tok::Keyword(_) | Tok::LParen | Tok::Not) => {} // implicit AND
                _ => return Ok(left),
            }
            left = Expr::And(Box::new(left), Box::new(self.not()?));
        }
    }

    fn not(&mut self) -> Result<Expr, ParseError> {
        if self.peek() == Some(&Tok::Not) {
            self.pos += 1;
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        let tok = self.peek().cloned();
        self.pos += 1;
        match tok {
            Some(Tok::Term(s)) => Ok(Expr::Term(s)),
            Some(Tok::Tag(s)) => Ok(Expr::Tag(s)),
            Some(Tok::Keyword(s)) => Ok(Expr::Keyword(s)),
            Some(Tok::LParen) => {
                let e = self.or()?;
                match self.peek() {
                    Some(Tok::RParen) => {
                        self.pos += 1;
                        Ok(e)
                    }
                    t => Err(ParseError(format!("Expected RPAREN, got {t:?}"))),
                }
            }
            t => Err(ParseError(format!("Unexpected token {t:?}"))),
        }
    }
}

impl std::fmt::Display for Expr {
    /// Canonical query string (used to rebuild `q` after removing a tag from the tag cloud).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let grouped = |e: &Expr, wrap: bool| if wrap { format!("({e})") } else { e.to_string() };
        match self {
            Expr::Term(t) if t.contains([' ', '(', ')', '"', '\'']) => {
                write!(f, "\"{}\"", t.replace('\\', "\\\\").replace('"', "\\\""))
            }
            Expr::Term(t) => write!(f, "{t}"),
            Expr::Tag(t) => write!(f, "#{t}"),
            Expr::Keyword(k) => write!(f, "!{k}"),
            Expr::Not(e) => write!(f, "not {}", grouped(e, matches!(**e, Expr::And(..) | Expr::Or(..)))),
            Expr::And(l, r) => write!(
                f,
                "{} {}",
                grouped(l, matches!(**l, Expr::Or(..))),
                grouped(r, matches!(**r, Expr::Or(..)))
            ),
            Expr::Or(l, r) => write!(f, "{l} or {r}"),
        }
    }
}

/// Remove every reference to `tag` from the query (tag-cloud "unselect").
pub fn strip_tag(query: &str, tag: &str, lax: bool) -> String {
    fn strip(e: Expr, tag: &str, lax: bool) -> Option<Expr> {
        let eq = |s: &str| s.to_lowercase() == tag.to_lowercase();
        let both = |l: Box<Expr>, r: Box<Expr>, mk: fn(Box<Expr>, Box<Expr>) -> Expr| match (
            strip(*l, tag, lax),
            strip(*r, tag, lax),
        ) {
            (Some(l), Some(r)) => Some(mk(Box::new(l), Box::new(r))),
            (l, r) => l.or(r),
        };
        match e {
            Expr::Tag(t) if eq(&t) => None,
            Expr::Term(t) if lax && eq(&t) => None,
            Expr::Not(e) => strip(*e, tag, lax).map(|e| Expr::Not(Box::new(e))),
            Expr::And(l, r) => both(l, r, Expr::And),
            Expr::Or(l, r) => both(l, r, Expr::Or),
            e => Some(e),
        }
    }
    match parse(query) {
        Ok(Some(e)) => strip(e, tag, lax).map(|e| e.to_string()).unwrap_or_default(),
        Ok(None) => String::new(),
        Err(_) => query.to_string(),
    }
}

/// Tag names referenced by the query (the tag cloud's "selected tags").
pub fn tag_names(query: &str, lax: bool) -> Vec<String> {
    fn walk(e: &Expr, lax: bool, out: &mut Vec<String>) {
        match e {
            Expr::Tag(t) => out.push(t.clone()),
            Expr::Term(t) if lax => out.push(t.clone()),
            Expr::Not(e) => walk(e, lax, out),
            Expr::And(l, r) | Expr::Or(l, r) => {
                walk(l, lax, out);
                walk(r, lax, out);
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(Some(e)) = parse(query) {
        walk(&e, lax, &mut out);
    }
    out
}

/// Builds a SQL fragment with numbered parameters (`?1`, `?2`, ...), so a parameter
/// can be referenced several times without counting against D1's 100-param limit again.
#[derive(Default)]
pub struct Sql {
    pub params: Vec<Value>,
}

impl Sql {
    pub fn bind(&mut self, v: impl Into<Value>) -> String {
        self.params.push(v.into());
        format!("?{}", self.params.len())
    }
}

const TAG_EXISTS: &str = "EXISTS (SELECT 1 FROM bookmark_tags bt JOIN tags t ON t.id = bt.tag_id \
                          WHERE bt.bookmark_id = b.id AND t.name = ";

/// Compile a parsed query to a WHERE fragment over `bookmarks b`.
/// `lax` is a SQL boolean expression (lax tag mode also matches bare words against tags).
pub fn to_sql(e: &Expr, sql: &mut Sql, lax: &str) -> String {
    match e {
        Expr::Term(t) => {
            let p = sql.bind(format!("%{}%", escape_like(t)));
            let exact = sql.bind(t.as_str());
            format!(
                "(b.title LIKE {p} ESCAPE '\\' OR b.description LIKE {p} ESCAPE '\\' \
                 OR b.notes LIKE {p} ESCAPE '\\' OR b.url LIKE {p} ESCAPE '\\' \
                 OR ({lax} AND {TAG_EXISTS}{exact})))"
            )
        }
        Expr::Tag(t) => format!("{TAG_EXISTS}{})", sql.bind(t.as_str())),
        Expr::Keyword(k) => match k.to_lowercase().as_str() {
            "unread" => "b.unread = 1".into(),
            "untagged" => "NOT EXISTS (SELECT 1 FROM bookmark_tags bt WHERE bt.bookmark_id = b.id)".into(),
            _ => "1".into(), // unknown keywords match everything, like linkding
        },
        Expr::And(l, r) => format!("({} AND {})", to_sql(l, sql, lax), to_sql(r, sql, lax)),
        Expr::Or(l, r) => format!("({} OR {})", to_sql(l, sql, lax), to_sql(r, sql, lax)),
        Expr::Not(e) => format!("NOT {}", to_sql(e, sql, lax)),
    }
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::Expr::*;
    use super::*;

    fn t(s: &str) -> Expr {
        Term(s.into())
    }
    fn tag(s: &str) -> Expr {
        Tag(s.into())
    }
    fn kw(s: &str) -> Expr {
        Keyword(s.into())
    }
    fn and(l: Expr, r: Expr) -> Expr {
        And(Box::new(l), Box::new(r))
    }
    fn or(l: Expr, r: Expr) -> Expr {
        Or(Box::new(l), Box::new(r))
    }
    fn not(e: Expr) -> Expr {
        Not(Box::new(e))
    }
    fn p(q: &str) -> Expr {
        parse(q).unwrap().unwrap()
    }

    // Cases ported from linkding's bookmarks/tests/test_search_query_parser.py
    #[test]
    fn parser() {
        assert_eq!(parse("").unwrap(), None);
        assert_eq!(parse("   ").unwrap(), None);
        assert_eq!(p("programming"), t("programming"));
        assert_eq!(p("client-side"), t("client-side"));
        assert_eq!(p("android notarization"), and(t("android"), t("notarization")));
        assert_eq!(p("programming and books"), and(t("programming"), t("books")));
        assert_eq!(p("programming or books"), or(t("programming"), t("books")));
        assert_eq!(p("not programming"), not(t("programming")));
        assert_eq!(p("not not programming"), not(not(t("programming"))));
        assert_eq!(
            p("programming or books and streaming"),
            or(t("programming"), and(t("books"), t("streaming")))
        );
        assert_eq!(p("not programming and books"), and(not(t("programming")), t("books")));
        assert_eq!(
            p("programming and books and streaming"),
            and(and(t("programming"), t("books")), t("streaming"))
        );
        assert_eq!(p("((programming))"), t("programming"));
        assert_eq!(
            p("(programming or books) and streaming"),
            and(or(t("programming"), t("books")), t("streaming"))
        );
        assert_eq!(
            p("programming AND books OR streaming NOT videos"),
            or(and(t("programming"), t("books")), and(t("streaming"), not(t("videos"))))
        );
        assert_eq!(p("https://example.com/foo/bar"), t("https://example.com/foo/bar"));
        assert_eq!(
            p("test@example.com and file.txt"),
            and(t("test@example.com"), t("file.txt"))
        );
        assert_eq!(p("\"good and bad\""), t("good and bad"));
        assert_eq!(p("'hello world'"), t("hello world"));
        assert_eq!(p("'don\\'t worry'"), t("don't worry"));
        assert_eq!(p("\"say \\\"hello\\\"\""), t("say \"hello\""));
        assert_eq!(p("\"unclosed quote"), t("unclosed quote"));
        assert_eq!(
            p("programming and not \"bad practices\""),
            and(t("programming"), not(t("bad practices")))
        );
        assert_eq!(p("#python"), tag("python"));
        assert_eq!(p("#machine-learning"), tag("machine-learning"));
        assert_eq!(p("not #deprecated"), not(tag("deprecated")));
        assert_eq!(
            p("(#frontend or #backend) and javascript"),
            and(or(tag("frontend"), tag("backend")), t("javascript"))
        );
        assert_eq!(parse("#").unwrap(), None);
        assert_eq!(parse("# ").unwrap(), None);
        assert_eq!(p("python # and django"), and(t("python"), t("django")));
        assert_eq!(p("!unread"), kw("unread"));
        assert_eq!(p("not !unread"), not(kw("unread")));
        assert_eq!(
            p("!unread and #python and tutorial"),
            and(and(kw("unread"), tag("python")), t("tutorial"))
        );
        assert_eq!(p("'!unread and !untagged'"), t("!unread and !untagged"));
        assert_eq!(p("tutorial #python"), and(t("tutorial"), tag("python")));
    }

    #[test]
    fn parse_errors() {
        assert!(parse("(programming").is_err());
        assert!(parse("programming)").is_err());
        assert!(parse("and").is_err());
        assert!(parse("programming or").is_err());
        assert!(parse("not").is_err());
    }

    #[test]
    fn to_string_round_trips() {
        for q in [
            "programming",
            "#python and (#book or #article)",
            "not (a or b)",
            "\"hello world\" !unread",
            "a or b c",
        ] {
            let e = p(q);
            assert_eq!(p(&e.to_string()), e, "{q}");
        }
        assert_eq!(p("a and (b or c)").to_string(), "a (b or c)");
    }

    #[test]
    fn strip_and_extract_tags() {
        assert_eq!(strip_tag("#a #b", "A", false), "#b");
        assert_eq!(strip_tag("#a or (#b and not #a)", "a", false), "#b");
        assert_eq!(strip_tag("#a", "a", false), "");
        assert_eq!(strip_tag("python #a", "python", false), "python #a");
        assert_eq!(strip_tag("python #a", "python", true), "#a");
        assert_eq!(tag_names("#a not #b and c", false), vec!["a", "b"]);
        assert_eq!(tag_names("#a c", true), vec!["a", "c"]);
        assert!(tag_names("(broken", false).is_empty());
    }

    #[test]
    fn sql_uses_numbered_params() {
        let mut sql = Sql::default();
        let w = to_sql(&p("50% #rust or !untagged"), &mut sql, "0");
        assert_eq!(sql.params, vec![Value::from("%50\\%%"), "50%".into(), "rust".into()]);
        assert!(w.contains("b.title LIKE ?1") && w.contains("t.name = ?3") && w.contains("NOT EXISTS"));
    }
}
