//! The values a page edit form was loaded with, so a save can tell what
//! this editor changed from what someone else changed meanwhile (#903).
//!
//! The form carries a signed token, [`BASELINE_KEY`], with a short hash
//! of each form-owned `cms_page` column as rendered. On save, a field
//! the editor left as loaded keeps whatever is stored now; a field only
//! the editor changed is written; a field both changed is a
//! [`FieldConflict`]. A post without a valid token (API, MCP, an old
//! form, a tampered one) is saved as before: posted values win.
//!
//! Only the page's own columns are covered. Tags, categories and the
//! type's content (extension row, inline panels, page-builder body) are
//! still last-write-wins.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::page::Page;

/// The form key the token travels under.
pub const BASELINE_KEY: &str = "_baseline";

/// The response header autosave returns a refreshed token in.
pub const BASELINE_HEADER: &str = "x-rcms-baseline";

/// Purpose for [`crate::signing::derived_key`].
const KEY_PURPOSE: &str = "rcms:page-baseline";

/// Hex digits kept per field hash (48 bits).
const HASH_HEX: usize = 12;

/// Hex digits kept of the signature (128 bits).
const SIG_HEX: usize = 32;

/// The form-owned columns the baseline covers, in token order. Changing
/// this list changes the token length, so old tokens stop parsing and
/// their forms save as if they had none.
pub const FIELDS: &[&str] = &[
    "title",
    "slug",
    "status",
    "seo_title",
    "seo_description",
    "robots_index",
    "sitemap_priority",
    "go_live_at",
    "expire_at",
    "show_in_menus",
    "preview_path",
    "template_override",
    "og_title",
    "og_description",
    "og_image_media_id",
    "twitter_card",
];

/// A field both this editor and someone else changed since the form
/// loaded. Values are as [`value`] renders them.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FieldConflict {
    pub field: &'static str,
    /// The editor's label for the field (English, a catalog key).
    pub label: &'static str,
    /// What is stored now.
    pub theirs: String,
    /// What this editor posted.
    pub yours: String,
}

/// `field` of `page` as text: compared through its hash, and shown on
/// the conflict screen. Times are kept to the minute (the admin's
/// schedule inputs), the f32 priority to two decimals.
#[must_use]
pub fn value(page: &Page, field: &str) -> String {
    let time = |t: Option<chrono::DateTime<chrono::Utc>>| {
        t.map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()
    };
    let id = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_default();
    match field {
        "title" => page.title.clone(),
        "slug" => page.slug.clone(),
        "status" => page.status.clone(),
        "seo_title" => page.seo_title.clone(),
        "seo_description" => page.seo_description.clone(),
        "robots_index" => page.robots_index.to_string(),
        "sitemap_priority" => format!("{:.2}", page.sitemap_priority),
        "go_live_at" => time(page.go_live_at),
        "expire_at" => time(page.expire_at),
        "show_in_menus" => page.show_in_menus.to_string(),
        "preview_path" => page.preview_path.clone(),
        "template_override" => page.template_override.clone(),
        "og_title" => page.og_title.clone(),
        "og_description" => page.og_description.clone(),
        "og_image_media_id" => id(page.og_image_media_id),
        "twitter_card" => page.twitter_card.clone(),
        _ => String::new(),
    }
}

/// The page form's label for `field`.
#[must_use]
pub fn label(field: &str) -> &'static str {
    match field {
        "title" => "Title",
        "slug" => "Slug",
        "status" => "Status",
        "seo_title" => "SEO title",
        "seo_description" => "SEO description",
        "robots_index" => "Allow search engines to index this page",
        "sitemap_priority" => "Sitemap priority",
        "go_live_at" => "Go live at",
        "expire_at" => "Expire at",
        "show_in_menus" => "Show in menus",
        "preview_path" => "Frontend route",
        "template_override" => "Template",
        "og_title" => "OG title",
        "og_description" => "OG description",
        "og_image_media_id" => "OG image",
        "twitter_card" => "Twitter card",
        _ => "",
    }
}

/// Set `field` of `dst` to `src`'s.
fn copy_field(dst: &mut Page, src: &Page, field: &str) {
    match field {
        "title" => dst.title.clone_from(&src.title),
        "slug" => dst.slug.clone_from(&src.slug),
        "status" => dst.status.clone_from(&src.status),
        "seo_title" => dst.seo_title.clone_from(&src.seo_title),
        "seo_description" => dst.seo_description.clone_from(&src.seo_description),
        "robots_index" => dst.robots_index = src.robots_index,
        "sitemap_priority" => dst.sitemap_priority = src.sitemap_priority,
        "go_live_at" => dst.go_live_at = src.go_live_at,
        "expire_at" => dst.expire_at = src.expire_at,
        "show_in_menus" => dst.show_in_menus = src.show_in_menus,
        "preview_path" => dst.preview_path.clone_from(&src.preview_path),
        "template_override" => dst.template_override.clone_from(&src.template_override),
        "og_title" => dst.og_title.clone_from(&src.og_title),
        "og_description" => dst.og_description.clone_from(&src.og_description),
        "og_image_media_id" => dst.og_image_media_id = src.og_image_media_id,
        "twitter_card" => dst.twitter_card.clone_from(&src.twitter_card),
        _ => {}
    }
}

fn hash(field: &str, value: &str) -> String {
    let mut h = Sha256::new();
    h.update(field.as_bytes());
    h.update([0]);
    h.update(value.as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(HASH_HEX);
    for b in &digest[..HASH_HEX / 2] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn sign(tenant: &str, page_id: i64, hashes: &str) -> String {
    let key = crate::signing::derived_key(KEY_PURPOSE);
    let msg = format!("v1\0{tenant}\0{page_id}\0{hashes}");
    let mut sig = crate::signing::hmac_sha256_hex(&key, msg.as_bytes());
    sig.truncate(SIG_HEX);
    sig
}

/// A verified baseline: one hash per [`FIELDS`] entry, for one page of
/// one tenant.
#[derive(Debug, Clone)]
pub struct Baseline {
    tenant: String,
    page_id: i64,
    hashes: HashMap<&'static str, String>,
}

impl Baseline {
    /// The baseline of `page`, of tenant `tenant`, as stored.
    #[must_use]
    pub fn of(tenant: &str, page: &Page) -> Self {
        let hashes = FIELDS.iter().map(|f| (*f, hash(f, &value(page, f)))).collect();
        Self { tenant: tenant.to_owned(), page_id: page.id.get().copied().unwrap_or_default(), hashes }
    }

    /// The token for the form's hidden field.
    #[must_use]
    pub fn token(&self) -> String {
        let hashes: String = FIELDS
            .iter()
            .map(|f| self.hashes.get(f).map_or("", String::as_str))
            .collect();
        let sig = sign(&self.tenant, self.page_id, &hashes);
        format!("{}.{hashes}.{sig}", self.page_id)
    }

    /// Parse and verify a posted token for page `page_id` of `tenant`.
    /// `None` for a token of another page or tenant, a malformed one or
    /// a bad signature.
    #[must_use]
    pub fn parse(token: &str, tenant: &str, page_id: i64) -> Option<Self> {
        let mut parts = token.trim().splitn(3, '.');
        let id: i64 = parts.next()?.parse().ok()?;
        let hashes = parts.next()?;
        let sig = parts.next()?;
        if id != page_id || hashes.len() != FIELDS.len() * HASH_HEX || !hashes.is_ascii() {
            return None;
        }
        if !crate::signing::constant_time_eq(sign(tenant, id, hashes).as_bytes(), sig.as_bytes()) {
            return None;
        }
        let hashes = FIELDS
            .iter()
            .enumerate()
            .map(|(i, f)| (*f, hashes[i * HASH_HEX..(i + 1) * HASH_HEX].to_owned()))
            .collect();
        Some(Self { tenant: tenant.to_owned(), page_id, hashes })
    }

    /// The baseline in a posted form, for page `page_id` of `tenant`. A token that
    /// doesn't verify is ignored (and logged): the save then goes ahead
    /// as one without a baseline.
    #[must_use]
    pub fn from_form(form: &HashMap<String, String>, tenant: &str, page_id: i64) -> Option<Self> {
        let token = form.get(BASELINE_KEY).filter(|t| !t.is_empty())?;
        let parsed = Self::parse(token, tenant, page_id);
        if parsed.is_none() {
            tracing::warn!(page_id, "page edit baseline did not verify; saving without it");
        }
        parsed
    }

    /// Whether `field` was `value` when the form loaded.
    #[must_use]
    pub fn matches(&self, field: &str, value: &str) -> bool {
        self.hashes.get(field).is_some_and(|h| *h == hash(field, value))
    }

    /// This baseline with `fields` taken from `page`: what the form now
    /// holds after autosave wrote those fields.
    pub fn rebase(&mut self, page: &Page, fields: &[&'static str]) {
        for f in fields {
            self.hashes.insert(f, hash(f, &value(page, f)));
        }
    }

    /// Merge an edit three ways. `edited` holds the editor's values,
    /// `stored` the row as it is now, `posted_status` the status the form
    /// posted (before any derivation). A field the editor left as loaded
    /// takes the stored value; a field both sides changed to different
    /// values is returned as a conflict, left at the editor's value.
    #[must_use]
    pub fn merge(&self, stored: &Page, edited: &mut Page, posted_status: &str) -> Vec<FieldConflict> {
        let mut conflicts = Vec::new();
        for field in FIELDS {
            let posted = if *field == "status" {
                posted_status.to_owned()
            } else {
                value(edited, field)
            };
            if self.matches(field, &posted) {
                copy_field(edited, stored, field);
                continue;
            }
            let theirs = value(stored, field);
            let yours = value(edited, field);
            if !self.matches(field, &theirs) && yours != theirs {
                conflicts.push(FieldConflict { field, label: label(field), theirs, yours });
            }
        }
        conflicts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Page {
        use rustango::sql::Auto;
        Page {
            id: Auto::Set(7),
            page_type_id: 1,
            title: "Old".to_owned(),
            slug: "old".to_owned(),
            path: "0001/".to_owned(),
            url_path: "/old".to_owned(),
            preview_path: String::new(),
            template_override: String::new(),
            depth: 1,
            parent_id: None,
            locale_variant_of: None,
            alias_of: None,
            theme_id: None,
            sort_order: 0,
            status: "draft".to_owned(),
            published_at: None,
            last_published_at: None,
            go_live_at: None,
            expire_at: None,
            seo_title: String::new(),
            seo_description: String::new(),
            robots_index: true,
            sitemap_priority: 0.5,
            show_in_menus: false,
            og_title: String::new(),
            og_description: String::new(),
            og_image_media_id: None,
            twitter_card: "summary_large_image".to_owned(),
            notification_pre_published_sent: false,
            created_at: Auto::Unset,
            updated_at: Auto::Unset,
        }
    }

    #[test]
    fn a_token_round_trips_for_its_page_only() {
        let p = page();
        let token = Baseline::of("acme", &p).token();
        let parsed = Baseline::parse(&token, "acme", 7).expect("verifies");
        assert!(parsed.matches("title", &p.title));
        assert!(!parsed.matches("title", "New"));
        assert!(Baseline::parse(&token, "acme", 8).is_none(), "bound to the page");
        assert!(Baseline::parse(&token, "globex", 7).is_none(), "bound to the tenant");
    }

    #[test]
    fn a_tampered_token_is_rejected() {
        let p = page();
        let token = Baseline::of("acme", &p).token();
        let (id, rest) = token.split_once('.').expect("id");
        let flipped = match rest.strip_prefix('0') {
            Some(tail) => format!("1{tail}"),
            None => format!("0{}", &rest[1..]),
        };
        assert!(Baseline::parse(&format!("{id}.{flipped}"), "acme", 7).is_none());
        assert!(Baseline::parse("7.abc.def", "acme", 7).is_none());
        assert!(Baseline::parse("", "acme", 7).is_none());
    }

    #[test]
    fn merge_keeps_their_change_and_mine_and_flags_both() {
        let loaded = page();
        let base = Baseline::of("acme", &loaded);
        let mut stored = loaded.clone();
        stored.seo_title = "Theirs".into();
        stored.title = "Their title".into();
        let mut edited = loaded.clone();
        edited.og_title = "Mine".into();
        edited.title = "My title".into();
        let conflicts = base.merge(&stored, &mut edited, &loaded.status);
        assert_eq!(edited.seo_title, "Theirs", "their field survives");
        assert_eq!(edited.og_title, "Mine", "my field is kept");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].field, "title");
        assert_eq!(conflicts[0].theirs, "Their title");
        assert_eq!(conflicts[0].yours, "My title");
    }

    /// After autosave writes a field and rebases, the editor's own save
    /// isn't a conflict with itself.
    #[test]
    fn a_rebased_field_is_not_a_conflict_with_yourself() {
        let loaded = page();
        let mut base = Baseline::of("acme", &loaded);
        let mut stored = loaded.clone();
        stored.title = "Autosaved".into();
        base.rebase(&stored, &["title"]);
        let base = Baseline::parse(&base.token(), "acme", 7).expect("re-signed");
        let mut edited = loaded.clone();
        edited.title = "Typed on".into();
        assert!(base.merge(&stored, &mut edited, &loaded.status).is_empty());
        assert_eq!(edited.title, "Typed on");
    }
}
