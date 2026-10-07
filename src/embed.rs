//! Embed provider discovery + responsive render.
//!
//! Naively emitting a pasted URL as
//! `<iframe src="{the raw url}">` is broken for the URLs
//! people actually paste: a YouTube *watch* URL or a Vimeo page URL
//! won't load in an `<iframe>` (you need the provider's *embed* URL),
//! and an arbitrary URL in an iframe is both broken and a clickjacking
//! surface.
//!
//! This module recognises the common video providers from a public
//! URL, rewrites it to the embeddable `src`, and the block template
//! wraps it in a CSS `aspect-ratio` box so the player stays
//! responsive. Detection is a **pure, synchronous URL transformation**
//! (no network) so it runs inside the synchronous block-render path
//! ([`crate::block::BlockRenderCtx`] carries no pool). Providers are
//! an explicit **whitelist**; an unrecognised URL renders as a safe
//! link-out rather than an arbitrary iframe.
//!
//! oEmbed *metadata* (title / thumbnail fetched from the provider's
//! oEmbed endpoint) needs outbound HTTP + a cache and is out of scope
//! here — the render path is synchronous, so that belongs in a
//! save-time or prefetch-time resolver, tracked separately.
//!
//! Adding a provider is a match arm + an id extractor + a test.

/// A recognised embed provider. The whitelist — anything not here
/// falls back to a link-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    YouTube,
    Vimeo,
}

impl Provider {
    /// Human label — also used as the iframe `title` for a11y.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::YouTube => "YouTube",
            Self::Vimeo => "Vimeo",
        }
    }

    /// CSS `aspect-ratio` value for the responsive wrapper. Both
    /// current providers default to widescreen; split this out per
    /// provider if one ever needs a different intrinsic ratio.
    #[must_use]
    pub fn aspect_ratio(self) -> &'static str {
        "16 / 9"
    }
}

/// Recognising a public URL: the provider, the embeddable iframe
/// `src`, and the responsive aspect ratio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedTarget {
    pub provider: Provider,
    pub src: String,
    pub aspect_ratio: &'static str,
}

/// Recognise `url` and return its embeddable target, or `None` for an
/// unrecognised provider (the caller links out rather than iframing an
/// arbitrary URL).
#[must_use]
pub fn resolve(url: &str) -> Option<EmbedTarget> {
    let trimmed = url.trim();
    if let Some(id) = youtube_id(trimmed) {
        return Some(EmbedTarget {
            provider: Provider::YouTube,
            src: format!("https://www.youtube.com/embed/{id}"),
            aspect_ratio: Provider::YouTube.aspect_ratio(),
        });
    }
    if let Some(id) = vimeo_id(trimmed) {
        return Some(EmbedTarget {
            provider: Provider::Vimeo,
            src: format!("https://player.vimeo.com/video/{id}"),
            aspect_ratio: Provider::Vimeo.aspect_ratio(),
        });
    }
    None
}

/// Split a URL into `(lowercased host, rest)` where `rest` begins at
/// the path/query/fragment. Scheme-relative + bare inputs are handled;
/// inputs with no host return `None`.
fn split_host(url: &str) -> Option<(String, &str)> {
    let after = url.split_once("://").map_or(url, |(_, r)| r);
    let (host, rest) = match after.find(['/', '?', '#']) {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, ""),
    };
    // Strip an optional `user@` and `:port` so host matching is clean.
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some((host.to_ascii_lowercase(), rest))
}

/// Exact host or any subdomain of `domain`.
fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Extract a YouTube video id from watch / `youtu.be` / embed / shorts
/// / live URLs.
fn youtube_id(url: &str) -> Option<String> {
    let (host, rest) = split_host(url)?;
    if host_matches(&host, "youtu.be") {
        return first_segment(rest).filter(|s| is_id(s));
    }
    if host_matches(&host, "youtube.com") || host_matches(&host, "youtube-nocookie.com") {
        if let Some(v) = query_param(rest, "v") {
            return Some(v).filter(|s| is_id(s));
        }
        let path = rest.split(['?', '#']).next().unwrap_or("");
        let mut segs = path.split('/').filter(|s| !s.is_empty());
        if let Some(prefix) = segs.next() {
            if matches!(prefix, "embed" | "shorts" | "v" | "live") {
                return segs.next().map(str::to_owned).filter(|s| is_id(s));
            }
        }
    }
    None
}

/// Extract a Vimeo numeric video id from a page or player URL.
fn vimeo_id(url: &str) -> Option<String> {
    let (host, rest) = split_host(url)?;
    if !host_matches(&host, "vimeo.com") {
        return None;
    }
    let path = rest.split(['?', '#']).next().unwrap_or("");
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let candidate = match segs.as_slice() {
        // player.vimeo.com/video/<id>
        ["video", id, ..] => *id,
        // vimeo.com/<id>
        [id, ..] => *id,
        _ => return None,
    };
    if !candidate.is_empty() && candidate.chars().all(|c| c.is_ascii_digit()) {
        Some(candidate.to_owned())
    } else {
        None
    }
}

/// First non-empty path segment (query/fragment stripped).
fn first_segment(rest: &str) -> Option<String> {
    let path = rest.split(['?', '#']).next().unwrap_or("");
    path.split('/').find(|s| !s.is_empty()).map(str::to_owned)
}

/// Value of query parameter `key`, if present.
fn query_param(rest: &str, key: &str) -> Option<String> {
    let q = rest.split_once('?')?.1;
    let q = q.split('#').next().unwrap_or(q);
    q.split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_owned())
}

/// YouTube ids are 11 chars of `[A-Za-z0-9_-]`. Validated loosely
/// (non-empty, url-safe charset) so a future id-format change doesn't
/// silently drop valid embeds, while still rejecting path noise.
fn is_id(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_watch_url() {
        let t = resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();
        assert_eq!(t.provider, Provider::YouTube);
        assert_eq!(t.src, "https://www.youtube.com/embed/dQw4w9WgXcQ");
        assert_eq!(t.aspect_ratio, "16 / 9");
    }

    #[test]
    fn youtube_watch_with_extra_params() {
        let t = resolve("https://youtube.com/watch?v=dQw4w9WgXcQ&t=42s&feature=share").unwrap();
        assert_eq!(t.src, "https://www.youtube.com/embed/dQw4w9WgXcQ");
    }

    #[test]
    fn youtube_short_link_and_shorts_and_embed() {
        assert_eq!(
            resolve("https://youtu.be/dQw4w9WgXcQ?t=10").unwrap().src,
            "https://www.youtube.com/embed/dQw4w9WgXcQ"
        );
        assert_eq!(
            resolve("https://www.youtube.com/shorts/abc123XYZ_-")
                .unwrap()
                .src,
            "https://www.youtube.com/embed/abc123XYZ_-"
        );
        // Already an embed URL → normalised to the same canonical form.
        assert_eq!(
            resolve("https://www.youtube.com/embed/dQw4w9WgXcQ")
                .unwrap()
                .src,
            "https://www.youtube.com/embed/dQw4w9WgXcQ"
        );
    }

    #[test]
    fn youtube_nocookie_and_subdomain() {
        assert_eq!(
            resolve("https://m.youtube.com/watch?v=dQw4w9WgXcQ")
                .unwrap()
                .src,
            "https://www.youtube.com/embed/dQw4w9WgXcQ"
        );
        assert_eq!(
            resolve("https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ")
                .unwrap()
                .src,
            "https://www.youtube.com/embed/dQw4w9WgXcQ"
        );
    }

    #[test]
    fn vimeo_page_and_player_urls() {
        let t = resolve("https://vimeo.com/123456789").unwrap();
        assert_eq!(t.provider, Provider::Vimeo);
        assert_eq!(t.src, "https://player.vimeo.com/video/123456789");
        assert_eq!(
            resolve("https://player.vimeo.com/video/123456789?h=abc")
                .unwrap()
                .src,
            "https://player.vimeo.com/video/123456789"
        );
    }

    #[test]
    fn vimeo_rejects_non_numeric_path() {
        // /channels/staffpicks etc. aren't a video id.
        assert!(resolve("https://vimeo.com/channels/staffpicks").is_none());
    }

    #[test]
    fn unknown_provider_is_none() {
        assert!(resolve("https://example.com/video/abc").is_none());
        assert!(resolve("https://twitter.com/jack/status/20").is_none());
        assert!(resolve("not a url").is_none());
        assert!(resolve("").is_none());
    }

    #[test]
    fn watch_without_v_param_is_none() {
        assert!(resolve("https://www.youtube.com/watch?feature=share").is_none());
    }

    #[test]
    fn scheme_relative_and_no_scheme() {
        // Scheme is optional — people paste bare hosts too.
        assert_eq!(
            resolve("youtube.com/watch?v=dQw4w9WgXcQ").unwrap().src,
            "https://www.youtube.com/embed/dQw4w9WgXcQ"
        );
    }
}
