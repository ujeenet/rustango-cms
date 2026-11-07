//! Stockists: the shops that sell Clay & Kiln pieces.
//!
//! The developer chapter "Your own data in the admin" walks through it:
//!
//! - `Stockist` is a plain table with an admin editor from
//!   `#[derive(CmsAdmin)]` (list, search, add, edit, delete);
//! - `StockistImport` is a custom admin page with a form, for pasting
//!   many stockists at once;
//! - the shop page serves `/shop/stockists` and `/shop/stockists/<city>`
//!   from this table (see `ShopPage` in `models.rs`).

use async_trait::async_trait;
use axum::response::Response;
use rustango::sql::{Auto, FetcherPool as _, Pool};
use rustango_cms::{
    AdminForm, AdminPageCtx, AdminPageHandler, CmsAdminOverrides, FieldError, MessageLevel, SidebarSection,
};
use serde::{Deserialize, Serialize};

/// A shop that sells our pieces.
#[derive(rustango::Model, rustango_cms::CmsAdmin, Debug, Clone, Serialize, Deserialize)]
#[rustango(table = "shop_stockist", app = "shop", display = "name")]
#[cms_admin(
    label = "Stockists",
    singular = "stockist",
    slug = "stockists",
    icon = "store",
    section = "Shop",
    list = "name, city, website, is_active",
    search = "name, city",
    ordering = "city, name"
)]
pub struct Stockist {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    #[field(widget = Text, label = "Shop name", required, max_length = 120)]
    pub name: String,
    #[rustango(max_length = 80)]
    #[field(widget = Text, label = "City", required, max_length = 80)]
    pub city: String,
    #[rustango(max_length = 200, default = "''")]
    #[field(widget = Url, label = "Website", placeholder = "https://")]
    pub website: String,
    #[field(widget = MediaPicker, label = "Photo")]
    pub photo: Option<i64>,
    #[rustango(default = "true")]
    #[field(widget = Boolean, label = "Show on the site")]
    pub is_active: bool,
}

/// A new stockist is shown on the site.
impl Default for Stockist {
    fn default() -> Self {
        Self {
            id: Auto::Unset,
            name: String::new(),
            city: String::new(),
            website: String::new(),
            photo: None,
            is_active: true,
        }
    }
}

impl Stockist {
    /// The city as it appears in URLs: `Le Mans` → `le-mans`.
    #[must_use]
    pub fn city_slug(&self) -> String {
        slug(&self.city)
    }
}

/// Lowercase letters and digits, words joined by `-`.
#[must_use]
pub fn slug(text: &str) -> String {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn bad_website(url: &str) -> bool {
    !url.is_empty() && !(url.starts_with("https://") || url.starts_with("http://"))
}

#[async_trait]
impl CmsAdminOverrides for Stockist {
    async fn validate(&self, _pool: &Pool) -> Vec<FieldError> {
        if bad_website(&self.website) {
            return vec![FieldError::new("website", "Start the address with https://")];
        }
        Vec::new()
    }
}

/// The active stockists, ordered by city then name.
pub async fn active(pool: &Pool) -> Vec<Stockist> {
    use rustango::core::Column as _;
    Stockist::objects()
        .where_(Stockist::is_active.eq(true))
        .order_by(&[("city", false), ("name", false)])
        .fetch(pool)
        .await
        .unwrap_or_default()
}

// ---------------------------------------------------------------- import

/// **Import stockists**: paste one stockist per line,
/// `Shop name, City, Website` (the website is optional).
#[derive(Default)]
pub struct StockistImport;

/// One pasted line as a stockist, or why it can't be one.
fn parse_line(line: &str) -> Result<Stockist, String> {
    let parts: Vec<&str> = line.split(',').map(str::trim).collect();
    let (name, city) = match parts.as_slice() {
        [name, city, ..] if !name.is_empty() && !city.is_empty() => (*name, *city),
        _ => return Err(format!("“{line}”: write the shop name, then the city.")),
    };
    let website = parts.get(2).copied().unwrap_or_default();
    if bad_website(website) {
        return Err(format!("“{line}”: start the website with https://"));
    }
    Ok(Stockist { name: name.to_owned(), city: city.to_owned(), website: website.to_owned(), ..Stockist::default() })
}

#[async_trait]
impl AdminPageHandler for StockistImport {
    fn slug(&self) -> &'static str {
        "stockist-import"
    }

    fn label(&self) -> &'static str {
        "Import stockists"
    }

    fn icon(&self) -> Option<&'static str> {
        Some("upload")
    }

    fn section(&self) -> SidebarSection {
        SidebarSection::Custom("Shop")
    }

    fn sort_order(&self) -> i32 {
        110
    }

    /// Adds a "Import stockists" row to the role matrix: `view` opens the
    /// page, `edit` may send the form.
    fn permission_resource(&self) -> Option<&'static str> {
        Some("shop_stockist_import")
    }

    async fn render(&self, ctx: AdminPageCtx<'_>) -> Response {
        let mut tctx = tera::Context::new();
        tctx.insert("lines", "");
        tctx.insert("errors", &Vec::<String>::new());
        ctx.render("stockist_import.html", &mut tctx).await
    }

    async fn post(&self, ctx: AdminPageCtx<'_>, form: AdminForm) -> Response {
        let lines = form.text("lines");
        let parsed: Vec<Result<Stockist, String>> =
            lines.lines().map(str::trim).filter(|l| !l.is_empty()).map(parse_line).collect();
        let errors: Vec<String> = parsed.iter().filter_map(|r| r.as_ref().err().cloned()).collect();
        if !errors.is_empty() || parsed.is_empty() {
            // Show the form again with the text, so nothing is lost.
            let mut tctx = tera::Context::new();
            tctx.insert("lines", &lines);
            tctx.insert("errors", &if parsed.is_empty() { vec!["Paste at least one line.".to_owned()] } else { errors });
            return ctx.render("stockist_import.html", &mut tctx).await;
        }
        let pool = ctx.tenant.pool();
        let mut added = 0;
        for mut stockist in parsed.into_iter().flatten() {
            if let Err(e) = stockist.save(pool).await {
                return ctx.redirect(&ctx.page_url(), MessageLevel::Error, &format!("Stopped after {added}: {e}"));
            }
            added += 1;
        }
        let noun = if added == 1 { "stockist" } else { "stockists" };
        ctx.redirect("/cms-admin/x/stockists", MessageLevel::Success, &format!("Imported {added} {noun}."))
    }
}

rustango_cms::register_admin_page!(StockistImport);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse_or_explain() {
        let s = parse_line("Café Lune, Le Mans, https://cafe-lune.example").expect("valid");
        assert_eq!((s.name.as_str(), s.city_slug().as_str()), ("Café Lune", "le-mans"));
        assert!(parse_line("Only a name").is_err());
        assert!(parse_line("Shop, Paris, www.shop.example").is_err());
    }
}
