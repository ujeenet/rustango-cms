//! Template inspector: shows a signed-in admin which templates rendered a
//! public page, and where each one comes from.
//!
//! An admin turns it on under **Preferences → Show templates on the site**
//! (stored on their user, `data.show_templates`). While it is on and they
//! browse the site:
//!
//! - every content block's HTML is wrapped in comment markers naming its
//!   template (`<!--rcms-template:blocks/hero.html-->…`), which change
//!   nothing about the layout;
//! - a small panel lists the page template, the templates it extends, and
//!   each block template, with its source — this site's own copy, the
//!   project's `templates/` folder, or built into Rustango-CMS — and a link
//!   to the **Templates** editor, opened from the admin bar's **Templates**
//!   button ([`crate::admin_bar`]); hovering a block outlines it and names
//!   its template.
//!
//! Visitors never get any of it: it needs an admin session, the page is
//! sent `no-store`, and the page cache already skips signed-in requests.
//! The flag travels with the request's task ([`scope`]), so the render
//! code needs no extra parameter.

use std::future::Future;

use rustango::tenancy::auth::User;

rustango::__private_runtime::tokio::task_local! {
    static ACTIVE: bool;
}

/// The user-data key holding the preference.
pub const PREFERENCE: &str = "show_templates";

/// Whether this signed-in admin wants templates shown.
#[must_use]
pub fn wanted(user: &User) -> bool {
    user.is_superuser && user.data.get(PREFERENCE).and_then(serde_json::Value::as_bool) == Some(true)
}

/// Run `fut` (a page render) with the inspector on or off.
pub async fn scope<F: Future>(on: bool, fut: F) -> F::Output {
    ACTIVE.scope(on, fut).await
}

/// Whether the current render is being inspected.
#[must_use]
pub fn active() -> bool {
    ACTIVE.try_with(|on| *on).unwrap_or(false)
}

/// A block's HTML, marked with the template that rendered it.
#[must_use]
pub fn mark(template: &str, html: &str) -> String {
    format!("<!--rcms-template:{}-->{html}<!--/rcms-template-->", template.replace("--", "-"))
}

/// `&`, `<`, `>` and `"` escaped; template names stay readable.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Where a template comes from.
fn source(tenant_slug: &str, name: &str) -> (&'static str, bool) {
    let tt = crate::tenant_templates::installed();
    if tt.as_ref().is_some_and(|t| t.read_override(tenant_slug, name).is_some()) {
        ("This site's copy", true)
    } else if tt.as_ref().is_some_and(|t| t.read_global(name).is_some()) {
        ("Project templates/ folder", true)
    } else {
        ("Built into Rustango-CMS", false)
    }
}

/// The page with the inspector panel added before `</body>`.
#[must_use]
pub fn annotate(html: String, tera: &tera::Tera, page_template: &str, tenant_slug: &str) -> String {
    // The page template and the chain it extends.
    let mut layout = vec![page_template.to_owned()];
    if let Some(t) = tera.templates.get(page_template) {
        layout.extend(t.parents.iter().cloned());
    }
    // Block templates, in page order, once each.
    let mut blocks: Vec<String> = Vec::new();
    let mut rest = html.as_str();
    while let Some(at) = rest.find("<!--rcms-template:") {
        rest = &rest[at + "<!--rcms-template:".len()..];
        let name = rest.split("-->").next().unwrap_or_default().to_owned();
        if !blocks.contains(&name) {
            blocks.push(name);
        }
    }
    let row = |name: &str| {
        let (from, editable) = source(tenant_slug, name);
        let link = if editable {
            format!(
                "<a href=\"/cms-admin/templates/edit?name={}\" target=\"_blank\" rel=\"noopener\">Edit</a>",
                escape(name)
            )
        } else {
            String::new()
        };
        format!(
            "<li><code>{}</code><span>{}</span>{link}</li>",
            escape(name),
            escape(from)
        )
    };
    let layout_rows: String = layout.iter().map(|n| row(n)).collect();
    let block_rows: String = if blocks.is_empty() {
        "<li><span>No content blocks on this page.</span></li>".to_owned()
    } else {
        blocks.iter().map(|n| row(n)).collect()
    };
    let panel = format!(
        "{STYLE}<aside id=\"rcms-tpl-panel\" aria-label=\"Templates on this page\" hidden>\
         <div id=\"rcms-tpl-body\">\
         <h2>Page</h2><ul>{layout_rows}</ul>\
         <h2>Blocks</h2><ul>{block_rows}</ul>\
         <p>Hover a block on the page to see its template. Only you see this; \
         turn it off under Preferences in the admin.</p>\
         </div></aside>{SCRIPT}"
    );
    match html.rfind("</body>") {
        Some(at) => {
            let mut out = html;
            out.insert_str(at, &panel);
            out
        }
        None => html + &panel,
    }
}

const STYLE: &str = "<style>\
#rcms-tpl-panel{position:fixed;right:16px;bottom:68px;z-index:2147483646;width:min(420px,calc(100vw - 32px));\
font:13px/1.45 system-ui,sans-serif;color:#1b1b1f;background:#fff;border:1px solid #d0d3dc;border-radius:12px;\
box-shadow:0 12px 32px rgb(0 0 0/.18);overflow:hidden}\
#rcms-tpl-panel[hidden]{display:none}\
#rcms-tpl-body{padding:4px 12px 10px;max-height:50vh;overflow:auto}\
#rcms-tpl-panel h2{font-size:11px;letter-spacing:.06em;text-transform:uppercase;color:#6b6c75;margin:10px 0 4px}\
#rcms-tpl-panel ul{list-style:none;margin:0;padding:0}\
#rcms-tpl-panel li{display:flex;flex-wrap:wrap;gap:2px 8px;align-items:baseline;padding:4px 0;border-top:1px solid #eef0f4}\
#rcms-tpl-panel li:first-child{border-top:0}\
#rcms-tpl-panel code{font:12px ui-monospace,monospace;color:#1b1b1f}\
#rcms-tpl-panel span{color:#6b6c75;font-size:12px}\
#rcms-tpl-panel a{margin-left:auto;color:#7c3aed;font-weight:600;text-decoration:none}\
#rcms-tpl-panel p{margin:8px 0 0;color:#6b6c75;font-size:12px}\
.rcms-tpl-hover{outline:2px dashed #7c3aed!important;outline-offset:2px}\
#rcms-tpl-label{position:fixed;z-index:2147483647;pointer-events:none;font:12px ui-monospace,monospace;\
background:#7c3aed;color:#fff;padding:2px 6px;border-radius:4px}\
</style>";

const SCRIPT: &str = "<script>(()=>{\
const panel=document.getElementById('rcms-tpl-panel');\
const show=(open)=>{panel.hidden=!open;document.querySelectorAll('[data-rcms-templates]').forEach((b)=>b.setAttribute('aria-expanded',String(open)));};\
document.addEventListener('click',(ev)=>{if(!ev.target.closest('[data-rcms-templates]'))return;\
show(panel.hidden);try{localStorage.setItem('rcms-tpl-open',panel.hidden?'0':'1');}catch(e){}});\
document.addEventListener('DOMContentLoaded',()=>{try{show(localStorage.getItem('rcms-tpl-open')==='1');}catch(e){}});\
const owners=new Map();\
const walk=document.createTreeWalker(document.body,NodeFilter.SHOW_COMMENT);\
const open=[];\
for(let n=walk.nextNode();n;n=walk.nextNode()){\
const v=n.nodeValue;\
if(v.startsWith('rcms-template:')){open.push({name:v.slice(14),start:n});}\
else if(v==='/rcms-template'&&open.length){const m=open.pop();\
for(let e=m.start.nextSibling;e&&e!==n;e=e.nextSibling){if(e.nodeType===1&&!owners.has(e))owners.set(e,m.name);}}}\
const label=document.createElement('div');label.id='rcms-tpl-label';label.hidden=true;document.body.appendChild(label);\
let current=null;\
document.addEventListener('mouseover',(ev)=>{\
let el=ev.target;while(el&&!owners.has(el))el=el.parentElement;\
if(current&&current!==el)current.classList.remove('rcms-tpl-hover');\
current=el;if(!el){label.hidden=true;return;}\
el.classList.add('rcms-tpl-hover');label.textContent=owners.get(el);\
const r=el.getBoundingClientRect();label.style.left=Math.max(4,r.left)+'px';label.style.top=Math.max(4,r.top-22)+'px';label.hidden=false;});\
})();</script>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_marked_and_listed_once_in_page_order() {
        let html = format!(
            "<html><body><main>{}{}{}</main></body></html>",
            mark("blocks/hero.html", "<section>a</section>"),
            mark("blocks/cards.html", "<section>b</section>"),
            mark("blocks/hero.html", "<section>c</section>"),
        );
        let out = annotate(html, &tera::Tera::default(), "page.html", "demo");
        let panel = &out[out.find("rcms-tpl-panel").unwrap()..];
        let hero = panel.find("<code>blocks/hero.html</code>").unwrap();
        let cards = panel.find("<code>blocks/cards.html</code>").unwrap();
        assert!(hero < cards, "page order");
        assert_eq!(panel.matches("<code>blocks/hero.html</code>").count(), 1, "listed once");
        assert!(panel.contains("<code>page.html</code>"));
        assert!(out.find("rcms-tpl-panel").unwrap() < out.find("</body>").unwrap(), "before </body>");
    }

    #[tokio::test]
    async fn the_flag_lives_only_inside_its_scope() {
        assert!(!active());
        assert!(scope(true, async { active() }).await);
        assert!(!scope(false, async { active() }).await);
        assert!(!active());
    }
}
