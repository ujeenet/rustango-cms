//! `routable_page` — routable pages. A single `Page` row can serve multiple URL patterns
//! under its canonical url_path.
//!
//! ## Why
//!
//! Archive / category index pages typically own a stable canonical
//! URL (`/blog/`) plus a family of sub-paths the framework's slug
//! resolver shouldn't have to materialize as real `cms_page` rows
//! — `/blog/archive/2026/`, `/blog/tag/rust/`, etc. The
//! routable-page surface lets the handler declare those patterns
//! and serve them through the same Tera template (with the
//! capture groups available in the context).
//!
//! ## API
//!
//! Handlers override [`crate::PageTypeHandler::routes`] to return
//! a list of [`RouteSpec`] entries. Each spec carries a regex
//! pattern (matched against the suffix BELOW the page's
//! `url_path`) and a stable name. When the public router can't
//! find an exact `url_path` match, it walks ancestor candidates
//! looking for a handler whose routes match the remaining suffix.
//! On match, the matched [`Page`](crate::page::Page) is rendered with two extra ctx
//! keys: `route_name` (the matched [`RouteSpec::name`]) and
//! `route_captures` (a `{name → value}` map of regex named groups).
//!
//! Handlers can also override [`crate::PageTypeHandler::route_context`]
//! to inject route-specific data (e.g. fetch the year's posts when
//! `archive_year` matched) — the returned map is merged into the
//! Tera context before render. Same precedence rule as
//! [`crate::PageTypeHandler::public_context`]: framework
//! keys land last and win every collision.

use std::collections::HashMap;

use regex::Regex;

/// A single named URL pattern. The pattern is anchored on both
/// ends (`^…$`) by the constructor; callers can lean on
/// `^` / `$` themselves but don't have to.
///
/// Suffix-relative — when the rendered page's `url_path` is
/// `/blog`, a request to `/blog/archive/2026` matches a route
/// pattern of `^archive/(?P<year>\d+)$`.
#[derive(Clone)]
pub struct RouteSpec {
    pub pattern: Regex,
    pub name: &'static str,
}

impl RouteSpec {
    /// Construct a route. The pattern is anchored automatically
    /// (`^…$`) so handlers don't have to remember the anchors. On
    /// invalid regex the call panics — callers are expected to
    /// declare these as static-shape strings at handler-build
    /// time, so a malformed pattern is a bug, not a runtime
    /// recoverable.
    #[must_use]
    pub fn new(pattern: &str, name: &'static str) -> Self {
        let anchored = anchor(pattern);
        Self {
            pattern: Regex::new(&anchored)
                .unwrap_or_else(|e| panic!("RouteSpec: invalid pattern `{pattern}`: {e}")),
            name,
        }
    }
}

impl std::fmt::Debug for RouteSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteSpec")
            .field("pattern", &self.pattern.as_str())
            .field("name", &self.name)
            .finish()
    }
}

fn anchor(pattern: &str) -> String {
    let lead = if pattern.starts_with('^') { "" } else { "^" };
    let trail = if pattern.ends_with('$') { "" } else { "$" };
    format!("{lead}{pattern}{trail}")
}

/// Result of attempting to match a route on a page.
#[derive(Debug, Clone)]
pub struct RouteMatch {
    pub name: &'static str,
    pub captures: HashMap<String, String>,
}

/// Walk a handler's routes against `suffix` (the request path with
/// the page's `url_path` stripped off — without the leading `/`).
/// Returns the first matching `RouteMatch`, or `None` when no
/// route accepts the suffix.
pub fn match_routes(routes: &[RouteSpec], suffix: &str) -> Option<RouteMatch> {
    let stripped = suffix.trim_start_matches('/').trim_end_matches('/');
    for route in routes {
        if let Some(captures) = route.pattern.captures(stripped) {
            let mut named: HashMap<String, String> = HashMap::new();
            for name in route.pattern.capture_names().flatten() {
                if let Some(m) = captures.name(name) {
                    named.insert(name.to_owned(), m.as_str().to_owned());
                }
            }
            return Some(RouteMatch {
                name: route.name,
                captures: named,
            });
        }
    }
    None
}

/// Reverse a named route into the URL suffix below the page's
/// `url_path`. Returns
/// `None` when no route has `name` or its pattern isn't reversible
/// (see [`reverse_pattern`]). Combine the suffix with the page's
/// `url_path` for a full URL — the `routable_url()` Tera function does
/// exactly that.
#[must_use]
pub fn reverse(
    routes: &[RouteSpec],
    name: &str,
    captures: &HashMap<String, String>,
) -> Option<String> {
    let route = routes.iter().find(|r| r.name == name)?;
    reverse_pattern(route.pattern.as_str(), captures)
}

/// Best-effort reverse of an anchored regex `pattern`: substitute
/// `captures` into each `(?P<name>…)` group and emit the literal
/// segments between them. Returns `None` when the pattern can't be
/// reversed unambiguously — a non-named group, a **quantified** group,
/// a character class / metacharacter in the literal text (`\d`, `.`,
/// `+`, `[`, `|`, …), or a capture with no supplied value.
///
/// This covers the routable shape people actually write —
/// `^archive/(?P<year>\d{4})/(?P<month>\d{2})$` — and bows out of the
/// genuinely-ambiguous ones rather than guessing.
#[must_use]
pub fn reverse_pattern(pattern: &str, captures: &HashMap<String, String>) -> Option<String> {
    let mut body = pattern;
    body = body.strip_prefix('^').unwrap_or(body);
    body = body.strip_suffix('$').unwrap_or(body);
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                // Escaped char: a metachar (`\.`, `\/`, `\-`) is a
                // literal; an escaped letter/digit (`\d`, `\w`, `\b`)
                // is a class — not reversible.
                i += 1;
                let e = *chars.get(i)?;
                if e.is_ascii_alphanumeric() {
                    return None;
                }
                out.push(e);
                i += 1;
            }
            '(' => {
                // Only named groups are reversible.
                if chars.get(i + 1) == Some(&'?')
                    && chars.get(i + 2) == Some(&'P')
                    && chars.get(i + 3) == Some(&'<')
                {
                    let mut j = i + 4;
                    let mut name = String::new();
                    while j < chars.len() && chars[j] != '>' {
                        name.push(chars[j]);
                        j += 1;
                    }
                    if j >= chars.len() {
                        return None; // unterminated group name
                    }
                    // Skip to the matching ')' (depth-counted, escapes
                    // skipped). The inner pattern is irrelevant — the
                    // supplied capture value replaces it.
                    let mut depth = 1;
                    let mut k = j + 1;
                    while k < chars.len() {
                        match chars[k] {
                            '\\' => {
                                k += 2;
                                continue;
                            }
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    k += 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    if depth != 0 {
                        return None; // unbalanced parens
                    }
                    // A quantifier on the group (`(…)?`, `(…)+`, `(…){2}`)
                    // makes the reverse ambiguous.
                    if matches!(chars.get(k), Some('?' | '*' | '+' | '{')) {
                        return None;
                    }
                    out.push_str(captures.get(&name)?);
                    i = k;
                } else {
                    return None; // non-named / non-capturing group
                }
            }
            // A bare metacharacter in the literal text means the
            // pattern isn't a fixed string — can't reverse it.
            '.' | '*' | '+' | '?' | '[' | ']' | '{' | '}' | '|' | ')' => return None,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(out)
}

/// Register the `routable_url(…)` Tera function. Two call shapes (extra kwargs are the
/// named captures):
///
/// ```tera
/// {# by page type + route name (looks up the handler's RouteSpec) #}
/// {{ routable_url(path=page.url_path, type=page.type_name, route="archive_year", year=2026) }}
/// {# or reverse a pattern directly #}
/// {{ routable_url(path=page.url_path, pattern="archive/(?P<year>\d{4})", year=2026) }}
/// ```
///
/// Returns the full URL (`{path}/{suffix}`), or null when the route /
/// pattern can't be reversed.
pub fn register_tera_function(tera: &mut tera::Tera) {
    tera.register_function("routable_url", routable_url_fn);
}

fn routable_url_fn(args: &HashMap<String, tera::Value>) -> tera::Result<tera::Value> {
    let path = args.get("path").and_then(tera::Value::as_str).unwrap_or("");
    // Captures = every kwarg except the reserved control keys.
    let reserved = ["path", "type", "route", "pattern"];
    let mut captures: HashMap<String, String> = HashMap::new();
    for (k, v) in args {
        if reserved.contains(&k.as_str()) {
            continue;
        }
        // Strings unwrap to their text; numbers/bools to their literal
        // (Value::to_string would JSON-quote a string).
        let s = match v {
            tera::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        captures.insert(k.clone(), s);
    }
    let suffix = if let Some(pattern) = args.get("pattern").and_then(tera::Value::as_str) {
        reverse_pattern(pattern, &captures)
    } else {
        let type_name = args.get("type").and_then(tera::Value::as_str).unwrap_or("");
        let route = args
            .get("route")
            .and_then(tera::Value::as_str)
            .unwrap_or("");
        if type_name.is_empty() || route.is_empty() {
            return Ok(tera::Value::Null);
        }
        match crate::page_type::find_handler(type_name) {
            Some(h) => reverse(&h.routes(), route, &captures),
            None => None,
        }
    };
    Ok(match suffix {
        Some(s) => tera::Value::String(format!("{}/{s}", path.trim_end_matches('/'))),
        None => tera::Value::Null,
    })
}

/// Compute the parent-prefix candidates from `request_path`,
/// ordered from longest (most specific) to shortest (root). Each
/// candidate is the prefix of `request_path` up to a path
/// boundary — i.e. `/blog/archive/2026` produces `/blog/archive`,
/// `/blog`, `/` (in that order).
///
/// Used by the resolver to walk up the tree when an exact
/// `url_path` lookup misses: try each ancestor's handler routes
/// before falling all the way through to a 404.
#[must_use]
pub fn ancestor_prefixes(request_path: &str) -> Vec<String> {
    let canon = crate::resolver::canonical_request_path(request_path);
    if canon == "/" {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut working = canon.as_str();
    // Strip the leaf segment first — we already missed on the full
    // path in the exact lookup, no point retrying it.
    while let Some(slash) = working.rfind('/') {
        let parent = if slash == 0 { "/" } else { &working[..slash] };
        out.push(parent.to_owned());
        if parent == "/" {
            break;
        }
        working = parent;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_pattern_when_not_anchored() {
        assert_eq!(anchor("foo"), "^foo$");
        assert_eq!(anchor("^foo"), "^foo$");
        assert_eq!(anchor("foo$"), "^foo$");
        assert_eq!(anchor("^foo$"), "^foo$");
    }

    #[test]
    fn match_routes_finds_named_captures() {
        let routes = vec![
            RouteSpec::new(r"archive/(?P<year>\d{4})", "archive_year"),
            RouteSpec::new(r"tag/(?P<slug>[a-z0-9-]+)", "tag"),
        ];
        let m = match_routes(&routes, "archive/2026").expect("match");
        assert_eq!(m.name, "archive_year");
        assert_eq!(m.captures.get("year"), Some(&"2026".to_owned()));
    }

    fn caps(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn reverse_substitutes_named_groups_and_literals() {
        let routes = vec![RouteSpec::new(
            r"archive/(?P<year>\d{4})/(?P<month>\d{2})",
            "archive_ym",
        )];
        let url = reverse(
            &routes,
            "archive_ym",
            &caps(&[("year", "2026"), ("month", "06")]),
        );
        assert_eq!(url.as_deref(), Some("archive/2026/06"));
    }

    #[test]
    fn reverse_round_trips_with_match() {
        // The suffix `reverse` produces must match the same route.
        let routes = vec![RouteSpec::new(r"tag/(?P<slug>[a-z0-9-]+)", "tag")];
        let suffix = reverse(&routes, "tag", &caps(&[("slug", "rust-lang")])).unwrap();
        assert_eq!(suffix, "tag/rust-lang");
        let m = match_routes(&routes, &suffix).expect("re-match");
        assert_eq!(m.name, "tag");
        assert_eq!(m.captures.get("slug"), Some(&"rust-lang".to_owned()));
    }

    #[test]
    fn reverse_unescapes_literal_metachars() {
        assert_eq!(
            reverse_pattern(r"^feed\.rss$", &caps(&[])).as_deref(),
            Some("feed.rss")
        );
    }

    #[test]
    fn reverse_unknown_route_or_missing_capture_is_none() {
        let routes = vec![RouteSpec::new(r"tag/(?P<slug>[a-z]+)", "tag")];
        assert!(reverse(&routes, "nope", &caps(&[("slug", "x")])).is_none());
        assert!(reverse(&routes, "tag", &caps(&[])).is_none()); // missing slug
    }

    #[test]
    fn reverse_bows_out_of_unreversible_patterns() {
        // Bare class outside a group, non-named group, alternation,
        // quantified group — all ambiguous.
        assert!(reverse_pattern(r"^posts/\d+$", &caps(&[])).is_none());
        assert!(reverse_pattern(r"^(?:a|b)/x$", &caps(&[])).is_none());
        assert!(reverse_pattern(r"^a.b$", &caps(&[])).is_none());
        assert!(reverse_pattern(r"^(?P<x>\d)+$", &caps(&[("x", "1")])).is_none());
    }

    fn render(src: &str) -> String {
        let mut tera = tera::Tera::default();
        register_tera_function(&mut tera);
        tera.add_raw_template("t.html", src).unwrap();
        tera.render("t.html", &tera::Context::new()).unwrap()
    }

    #[test]
    fn tera_routable_url_reverses_pattern_arg() {
        // `pattern` shape needs no registered handler. Path trailing
        // slash is normalised so we don't get a double slash. `| safe`
        // mirrors the other URL helpers — Tera would otherwise escape
        // the slashes (browsers decode them, but raw reads cleaner).
        let out = render(
            r#"{{ routable_url(path="/blog/", pattern="tag/(?P<slug>[a-z]+)", slug="rust") | safe }}"#,
        );
        assert_eq!(out, "/blog/tag/rust");
    }

    #[test]
    fn tera_routable_url_null_when_unreversible() {
        // Missing capture → null → empty render.
        let out = render(
            r#"{% set u = routable_url(path="/blog", pattern="tag/(?P<slug>[a-z]+)") %}{% if u %}{{ u }}{% else %}none{% endif %}"#,
        );
        assert_eq!(out, "none");
    }

    #[test]
    fn match_routes_tries_each_in_order() {
        let routes = vec![
            RouteSpec::new(r"archive/(?P<year>\d{4})", "archive_year"),
            RouteSpec::new(r"tag/(?P<slug>[a-z0-9-]+)", "tag"),
        ];
        let m = match_routes(&routes, "tag/rust").expect("match");
        assert_eq!(m.name, "tag");
        assert_eq!(m.captures.get("slug"), Some(&"rust".to_owned()));
    }

    #[test]
    fn match_routes_returns_none_when_no_pattern_accepts() {
        let routes = vec![RouteSpec::new(r"archive/(?P<year>\d{4})", "archive_year")];
        assert!(match_routes(&routes, "foo/bar").is_none());
    }

    #[test]
    fn match_routes_strips_leading_and_trailing_slashes() {
        let routes = vec![RouteSpec::new(r"archive/(?P<year>\d{4})", "archive_year")];
        // The resolver hands `/archive/2026/`; the handler's
        // pattern is anchored, so we strip the boundary slashes.
        let m = match_routes(&routes, "/archive/2026/").expect("match");
        assert_eq!(m.name, "archive_year");
    }

    #[test]
    fn ancestor_prefixes_walks_up_one_segment_at_a_time() {
        assert_eq!(
            ancestor_prefixes("/blog/archive/2026"),
            vec!["/blog/archive", "/blog", "/"],
        );
        assert_eq!(
            ancestor_prefixes("/blog/archive/2026/"),
            vec!["/blog/archive", "/blog", "/"],
        );
        assert_eq!(ancestor_prefixes("/blog"), vec!["/"]);
    }

    #[test]
    fn ancestor_prefixes_empty_for_root() {
        assert!(ancestor_prefixes("/").is_empty());
        assert!(ancestor_prefixes("").is_empty());
    }

    #[test]
    fn captures_without_named_groups_yields_empty_map() {
        let routes = vec![RouteSpec::new(r"archive/\d+", "archive_any")];
        let m = match_routes(&routes, "archive/2026").expect("match");
        assert!(m.captures.is_empty());
    }
}
