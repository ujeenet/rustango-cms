//! The admin bar: a small floating bar on public pages for someone signed
//! in to the CMS admin, with **Edit this page** and **Admin** links.
//!
//! It shows for an admin session only (not a member, not a visitor), in
//! the admin's interface language, and can be collapsed to its icon (the
//! choice is remembered in the browser). Pages shown with it are sent
//! `no-store`; the page cache already skips signed-in requests. The labels
//! travel with the request's task ([`scope`]), like the template
//! inspector's flag, so the render code needs no extra parameter.

use std::future::Future;

rustango::__private_runtime::tokio::task_local! {
    static LABELS: Option<Labels>;
}

/// The bar's text, in the admin's language.
#[derive(Debug, Clone)]
pub struct Labels {
    pub edit: String,
    pub admin: String,
    pub hide: String,
    pub templates: String,
}

/// Run `fut` (a page render) with the bar on (`Some`) or off.
pub async fn scope<F: Future>(labels: Option<Labels>, fut: F) -> F::Output {
    LABELS.scope(labels, fut).await
}

fn current() -> Option<Labels> {
    LABELS.try_with(Clone::clone).ok().flatten()
}

/// Whether the current render shows the bar.
#[must_use]
pub fn active() -> bool {
    current().is_some()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The page with the bar added before `</body>`. `page_id` 0 (a page not
/// stored, such as a built-in error page) leaves out **Edit this page**.
#[must_use]
pub fn add(html: String, page_id: i64) -> String {
    let Some(labels) = current() else { return html };
    let edit = if page_id > 0 {
        format!(
            "<a href=\"/cms-admin/pages/{page_id}/edit\" class=\"rcms-adminbar-link\">\
             <svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><path d=\"M3 17.25V21h3.75L17.81 9.94l-3.75-3.75L3 17.25zm17.71-10.21a1 1 0 0 0 0-1.41l-2.34-2.34a1 1 0 0 0-1.41 0l-1.83 1.83 3.75 3.75 1.83-1.83z\"/></svg>\
             {}</a>",
            escape(&labels.edit)
        )
    } else {
        String::new()
    };
    let bar = format!(
        "{STYLE}<div id=\"rcms-adminbar\" role=\"navigation\" aria-label=\"Rustango-CMS\">\
         <button type=\"button\" id=\"rcms-adminbar-toggle\" aria-expanded=\"true\" title=\"{hide}\">\
         <img src=\"/cms-admin/static/brand/icon-light.png\" alt=\"Rustango-CMS\" width=\"28\" height=\"28\"></button>\
         <div id=\"rcms-adminbar-links\">{edit}\
         <a href=\"/cms-admin/\" class=\"rcms-adminbar-link\">\
         <svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><path d=\"M3 13h8V3H3v10zm0 8h8v-6H3v6zm10 0h8V11h-8v10zm0-18v6h8V3h-8z\"/></svg>\
         {admin}</a>{templates}</div></div>{SCRIPT}",
        hide = escape(&labels.hide),
        admin = escape(&labels.admin),
        templates = if crate::template_inspector::active() {
            // Opens the template inspector's panel (see that module).
            format!(
                "<button type=\"button\" class=\"rcms-adminbar-link\" data-rcms-templates aria-controls=\"rcms-tpl-panel\" aria-expanded=\"false\">\
                 <svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><path d=\"M9.4 16.6 4.8 12l4.6-4.6L8 6l-6 6 6 6 1.4-1.4zm5.2 0 4.6-4.6-4.6-4.6L16 6l6 6-6 6-1.4-1.4z\"/></svg>\
                 {}</button>",
                escape(&labels.templates)
            )
        } else {
            String::new()
        },
    );
    match html.rfind("</body>") {
        Some(at) => {
            let mut out = html;
            out.insert_str(at, &bar);
            out
        }
        None => html + &bar,
    }
}

const STYLE: &str = "<style>\
#rcms-adminbar{position:fixed;right:16px;bottom:16px;z-index:2147483645;display:flex;align-items:center;gap:4px;\
padding:4px;font:600 13px/1 system-ui,sans-serif;background:#1b1b1f;color:#fff;border-radius:999px;\
box-shadow:0 10px 30px rgb(0 0 0/.25)}\
#rcms-adminbar-toggle{all:unset;display:block;width:28px;height:28px;border-radius:50%;cursor:pointer;background:#fff}\
#rcms-adminbar-toggle img{display:block;width:28px;height:28px}\
#rcms-adminbar-toggle:focus-visible,.rcms-adminbar-link:focus-visible{outline:2px solid #a78bfa;outline-offset:2px}\
#rcms-adminbar-links{display:flex;gap:2px}\
#rcms-adminbar[data-collapsed] #rcms-adminbar-links{display:none}\
.rcms-adminbar-link{display:inline-flex;align-items:center;gap:6px;padding:8px 12px;border-radius:999px;\
color:#fff!important;text-decoration:none!important;border:0;background:none;font:inherit;cursor:pointer}\
.rcms-adminbar-link[aria-expanded=\"true\"]{background:rgb(255 255 255/.2)}\
.rcms-adminbar-link:hover{background:rgb(255 255 255/.14)}\
.rcms-adminbar-link svg{width:16px;height:16px;fill:currentColor}\
@media print{#rcms-adminbar{display:none}}\
</style>";

const SCRIPT: &str = "<script>(()=>{\
const bar=document.getElementById('rcms-adminbar');\
const toggle=document.getElementById('rcms-adminbar-toggle');\
const set=(collapsed)=>{bar.toggleAttribute('data-collapsed',collapsed);toggle.setAttribute('aria-expanded',String(!collapsed));};\
try{set(localStorage.getItem('rcms-adminbar')==='collapsed');}catch(e){}\
toggle.addEventListener('click',()=>{const c=!bar.hasAttribute('data-collapsed');set(c);\
try{localStorage.setItem('rcms-adminbar',c?'collapsed':'open');}catch(e){}});\
})();</script>";

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> Labels {
        Labels { edit: "Edit this page".into(), admin: "Admin".into(), hide: "Hide".into(), templates: "Templates".into() }
    }

    #[tokio::test]
    async fn the_bar_links_to_the_page_editor_and_the_admin() {
        let out = scope(Some(labels()), async { add("<body><p>x</p></body>".into(), 7) }).await;
        assert!(out.contains("href=\"/cms-admin/pages/7/edit\""), "{out}");
        assert!(out.contains("href=\"/cms-admin/\""));
        assert!(out.find("rcms-adminbar").unwrap() < out.find("</body>").unwrap());
        let no_page = scope(Some(labels()), async { add("<body></body>".into(), 0) }).await;
        assert!(!no_page.contains("/edit\""), "no edit link without a page");
    }

    #[tokio::test]
    async fn without_a_scope_the_page_is_unchanged() {
        assert_eq!(add("<body></body>".into(), 7), "<body></body>");
        assert!(!scope(None, async { active() }).await);
    }
}
