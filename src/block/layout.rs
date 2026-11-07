//! Layout blocks: **Cards**, **Slider** and **Gallery**, each a list of
//! items an editor adds one by one (`BlockField::Repeat` over an item
//! block: `card`, `slide`, `gallery_image`).
//!
//! The templates (`blocks/<type>.html`) emit plain markup with
//! `rcms-block-*` classes and no styles, so a site's own stylesheet lays
//! them out; the starter themes style all three. A slider works without
//! JavaScript (it scrolls sideways with snap points); a theme may add
//! previous/next buttons on top.
//!
//! Item links go through the `safe_href` filter ([`safe_href`]), so an
//! editor's link can be a site path or a web, mail or phone address,
//! never `javascript:` or another scheme.

use crate::block::BlockFieldMeta;
use crate::{register_block, Block, BlockField};

/// A link fit for an `href`: a site path (`/about`, not `//host`), a
/// fragment, or an `http(s)`, `mailto` or `tel` address. Anything else
/// (`javascript:`, `data:`, a bare word) is `None`.
#[must_use]
pub fn safe_href(link: &str) -> Option<&str> {
    let link = link.trim();
    let lower = link.to_ascii_lowercase();
    let ok = (link.starts_with('/') && !link.starts_with("//") && !link.starts_with("/\\"))
        || link.starts_with('#')
        || ["https://", "http://", "mailto:", "tel:"].iter().any(|s| lower.starts_with(s));
    ok.then_some(link)
}

/// `{{ value.url | safe_href }}`: the link when [`safe_href`] accepts it,
/// else an empty string (so `{% if href %}` drops the link).
pub(crate) fn register_tera_filter(tera: &mut tera::Tera) {
    tera.register_filter(
        "safe_href",
        |value: &tera::Value, _: &std::collections::HashMap<String, tera::Value>| {
            Ok(tera::Value::String(
                value.as_str().and_then(safe_href).unwrap_or_default().to_owned(),
            ))
        },
    );
}

/// A select that always has a value: `default` until the editor picks.
fn choice<const N: usize>(name: &str, label: &str, default: &str, options: [(&str, &str); N]) -> BlockField {
    BlockField::choice(name, label, options)
        .required()
        .with_meta(BlockFieldMeta::default().default_value(default))
}

const LINK_HELP: &str = "A page on this site like /about, or a full link (https://…, mailto:…, tel:…).";

fn link_fields(label_help: &str) -> [BlockField; 2] {
    [
        BlockField::char("link_label", "Link text").with_help(label_help),
        BlockField::char("url", "Link").with_help(LINK_HELP),
    ]
}

/// One card of a **Cards** block.
#[derive(Default)]
pub struct CardBlock;

impl Block for CardBlock {
    fn type_name(&self) -> &'static str {
        "card"
    }
    fn verbose_name(&self) -> &'static str {
        "Card"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("crop_portrait")
    }
    fn label_format(&self) -> Option<&'static str> {
        Some("{title}")
    }
    fn fields(&self) -> Vec<BlockField> {
        let mut fields = vec![
            BlockField::widget("image", "Image", crate::widget::WidgetKind::MediaPicker),
            BlockField::char("title", "Title").required(),
            BlockField::text("text", "Text"),
        ];
        fields.extend(link_fields("Shown under the text. Without it, the title is the link."));
        fields
    }
}

/// `cards`: a grid of cards, each with an image, a title, text and a link.
#[derive(Default)]
pub struct CardsBlock;

impl Block for CardsBlock {
    fn type_name(&self) -> &'static str {
        "cards"
    }
    fn verbose_name(&self) -> &'static str {
        "Cards"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("view_module")
    }
    fn description(&self) -> Option<&'static str> {
        Some("A grid of cards, each with an image, a title, text and a link.")
    }
    fn label_format(&self) -> Option<&'static str> {
        Some("{heading}")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::char("heading", "Heading"),
            choice("columns", "Cards per row", "3", [("2", "2"), ("3", "3"), ("4", "4")]),
            BlockField::repeat("cards", "Cards", "card").with_min(1).with_max(12),
        ]
    }
}

/// One slide of a **Slider** block.
#[derive(Default)]
pub struct SlideBlock;

impl Block for SlideBlock {
    fn type_name(&self) -> &'static str {
        "slide"
    }
    fn verbose_name(&self) -> &'static str {
        "Slide"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("view_carousel")
    }
    fn label_format(&self) -> Option<&'static str> {
        Some("{heading}")
    }
    fn fields(&self) -> Vec<BlockField> {
        let mut fields = vec![
            BlockField::widget("image", "Image", crate::widget::WidgetKind::MediaPicker),
            BlockField::char("alt", "Image description")
                .with_help("What the image shows, for people who can't see it."),
            BlockField::char("heading", "Heading"),
            BlockField::text("text", "Text"),
        ];
        fields.extend(link_fields("A button on the slide. Leave empty for no button."));
        fields
    }
}

/// `slider`: slides that scroll sideways, one at a time or several in a row.
#[derive(Default)]
pub struct SliderBlock;

impl Block for SliderBlock {
    fn type_name(&self) -> &'static str {
        "slider"
    }
    fn verbose_name(&self) -> &'static str {
        "Slider"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("view_carousel")
    }
    fn description(&self) -> Option<&'static str> {
        Some("Slides that scroll sideways: one large slide at a time, or a row of several.")
    }
    fn label_format(&self) -> Option<&'static str> {
        Some("{heading}")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::char("heading", "Heading"),
            choice(
                "style",
                "Style",
                "slides",
                [("slides", "One slide at a time"), ("scroller", "A row of several")],
            ),
            BlockField::repeat("slides", "Slides", "slide").with_min(1).with_max(20),
        ]
    }
}

/// One picture of a **Gallery** block.
#[derive(Default)]
pub struct GalleryImageBlock;

impl Block for GalleryImageBlock {
    fn type_name(&self) -> &'static str {
        "gallery_image"
    }
    fn verbose_name(&self) -> &'static str {
        "Picture"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("image")
    }
    fn label_format(&self) -> Option<&'static str> {
        Some("{caption}")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            BlockField::widget("image", "Image", crate::widget::WidgetKind::MediaPicker).required(),
            BlockField::char("alt", "Image description")
                .with_help("What the image shows, for people who can't see it."),
            BlockField::char("caption", "Caption"),
        ]
    }
}

/// `gallery`: any number of pictures in a grid; each opens full size.
#[derive(Default)]
pub struct GalleryBlock;

impl Block for GalleryBlock {
    fn type_name(&self) -> &'static str {
        "gallery"
    }
    fn verbose_name(&self) -> &'static str {
        "Gallery"
    }
    fn icon(&self) -> Option<&'static str> {
        Some("photo_library")
    }
    fn description(&self) -> Option<&'static str> {
        Some("Pictures in a grid, each opening full size.")
    }
    fn label_format(&self) -> Option<&'static str> {
        Some("{caption}")
    }
    fn fields(&self) -> Vec<BlockField> {
        vec![
            choice("columns", "Pictures per row", "3", [("2", "2"), ("3", "3"), ("4", "4")]),
            BlockField::repeat("images", "Pictures", "gallery_image").with_min(1).with_max(48),
            BlockField::char("caption", "Caption").with_help("Shown under the whole gallery."),
        ]
    }
}

register_block!(CardBlock);
register_block!(CardsBlock);
register_block!(SlideBlock);
register_block!(SliderBlock);
register_block!(GalleryImageBlock);
register_block!(GalleryBlock);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_site_paths_and_web_mail_phone_links_pass() {
        for ok in ["/contact", "/a/b?c=1#d", "#top", "https://x.org", "HTTP://X.ORG", "mailto:a@b.c", "tel:+1 555", " /padded "] {
            assert!(safe_href(ok).is_some(), "{ok}");
        }
        for bad in ["javascript:alert(1)", "JaVaScRiPt:x", "data:text/html,x", "//evil.org", "/\\evil.org", "vbscript:x", "contact", ""] {
            assert!(safe_href(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn every_list_names_a_registered_item_block() {
        for (list, item) in [("cards", "card"), ("slider", "slide"), ("gallery", "gallery_image")] {
            assert!(crate::block::find_block(list).is_some(), "{list}");
            assert!(crate::block::find_block(item).is_some(), "{item}");
        }
    }
}
