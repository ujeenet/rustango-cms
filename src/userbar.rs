//! On-site editor userbar (#148, Wagtail parity D4).
//!
//! Floating overlay that appears on the bottom-right of every
//! PUBLIC page when the visitor is logged into the CMS admin. Gives
//! editors one-click access to: Edit this page, View revisions,
//! Open admin, and plugin-contributed entries.
//!
//! ## Wiring
//!
//! Hosts call this from their public templates:
//!
//! ```tera
//! {# bottom of every public page #}
//! {{ rcms_userbar() | safe }}
//! ```
//!
//! Visibility check: the userbar renders only when the request
//! carries a CMS admin session cookie. Detection runs client-side
//! (the markup is always emitted; CSS hides it if the cookie is
//! absent) so server-side per-tenant caching of public pages stays
//! correct.

use std::collections::HashMap;

/// Register the `rcms_userbar(...)` Tera function. Call alongside
/// the other admin Tera helpers in the host's public-router setup.
pub fn register_tera_function(tera: &mut tera::Tera) {
    tera.register_function("rcms_userbar", rcms_userbar);
}

fn rcms_userbar(args: &HashMap<String, tera::Value>) -> tera::Result<tera::Value> {
    // The function takes optional `page_id` so hosts can hint which
    // page the userbar should target. When absent, the userbar
    // renders generic links (admin / no edit).
    let page_id = args.get("page_id").and_then(tera::Value::as_i64);
    // Admin prefix is a URL path supplied at registration time, not
    // user content — don't HTML-escape it (Tera's escape_html
    // encodes forward slashes which breaks the link href).
    let admin_prefix = args
        .get("admin_prefix")
        .and_then(tera::Value::as_str)
        .unwrap_or("/cms-admin")
        .to_owned();

    let mut plugin_items_html = String::new();
    for item in crate::hooks::userbar_items() {
        // The href template is plugin-supplied (compile-time string).
        // Don't escape it for HTML — same reason as admin_prefix.
        let href = match page_id {
            Some(id) => item.href_template.replace("{page_id}", &id.to_string()),
            None => item.href_template.replace("{page_id}", ""),
        };
        let icon = tera::escape_html(item.icon);
        let label = tera::escape_html(item.label);
        plugin_items_html.push_str("<a class=\"rcms-userbar-item\" href=\"");
        plugin_items_html.push_str(&href);
        plugin_items_html.push_str(
            "\" target=\"_blank\" rel=\"noopener\"><span class=\"material-symbols-rounded\">",
        );
        plugin_items_html.push_str(&icon);
        plugin_items_html.push_str("</span> ");
        plugin_items_html.push_str(&label);
        plugin_items_html.push_str("</a>");
    }

    let mut edit_html = String::new();
    if let Some(id) = page_id {
        edit_html.push_str("<a class=\"rcms-userbar-item\" href=\"");
        edit_html.push_str(&admin_prefix);
        edit_html.push_str("/pages/");
        edit_html.push_str(&id.to_string());
        edit_html.push_str("/edit\" target=\"_blank\" rel=\"noopener\"><span class=\"material-symbols-rounded\">edit</span> Edit page</a>");
        edit_html.push_str("<a class=\"rcms-userbar-item\" href=\"");
        edit_html.push_str(&admin_prefix);
        edit_html.push_str("/pages/");
        edit_html.push_str(&id.to_string());
        edit_html.push_str("/history\" target=\"_blank\" rel=\"noopener\"><span class=\"material-symbols-rounded\">history</span> History</a>");
    }

    // Render: a fixed-position overlay with a toggle button + a
    // panel of items. JS-side cookie check hides the whole thing
    // when there's no admin session.
    let html = format!(
        r#"<aside class="rcms-userbar" data-rcms-userbar data-rcms-cookie-name="cms_admin_session" aria-label="Editor toolbar">
    <button type="button" class="rcms-userbar-toggle" data-rcms-userbar-toggle aria-expanded="false">
        <span class="material-symbols-rounded">edit_note</span>
    </button>
    <div class="rcms-userbar-panel" data-rcms-userbar-panel hidden>
        {edit_html}{plugin_items_html}<a class="rcms-userbar-item" href="{prefix}" target="_blank" rel="noopener"><span class="material-symbols-rounded">dashboard</span> Open admin</a>
    </div>
</aside>
<style>
/* #314 — defensive reset. Outbound is already safe (every rule below is
   scoped to .rcms-userbar*), but the userbar lives in the HOST page's DOM,
   so a host theme's global element rules (a, button, aside, * , body
   inheritance) could otherwise leak font / text-transform / box-sizing /
   padding / margin into it. Re-assert safe values on every userbar element
   so the bar can't be broken by the host theme. (Full two-way isolation
   via shadow DOM remains a future option.) */
.rcms-userbar, .rcms-userbar * {{ box-sizing: border-box; }}
.rcms-userbar * {{
    margin: 0;
    padding: 0;
    font-family: inherit;
    line-height: normal;
    letter-spacing: normal;
    text-transform: none;
    text-align: left;
    white-space: normal;
}}
.rcms-userbar {{
    position: fixed; right: 16px; bottom: 16px;
    margin: 0;
    z-index: 2147483000;
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
    display: none;
}}
.rcms-userbar.is-visible {{ display: block; }}
.rcms-userbar-toggle {{
    width: 44px; height: 44px;
    border-radius: 50%;
    border: 0;
    background: #6750a4;
    color: #fff;
    cursor: pointer;
    box-shadow: 0 4px 12px rgba(0,0,0,0.18);
    display: flex; align-items: center; justify-content: center;
}}
.rcms-userbar-panel {{
    position: absolute; right: 0; bottom: 56px;
    min-width: 200px;
    background: #fff;
    color: #1c1b1f;
    border-radius: 12px;
    box-shadow: 0 8px 24px rgba(0,0,0,0.18);
    overflow: hidden;
    display: none;
}}
.rcms-userbar-panel[data-open] {{ display: block; }}
.rcms-userbar-item {{
    display: flex; align-items: center; gap: 8px;
    padding: 10px 14px;
    color: inherit; text-decoration: none;
    font-size: 14px;
}}
.rcms-userbar-item:hover {{ background: #f3f0f6; }}
.rcms-userbar-item .material-symbols-rounded {{ font-size: 18px; }}
@media (prefers-color-scheme: dark) {{
    .rcms-userbar-panel {{ background: #1c1b1f; color: #e6e1e5; }}
    .rcms-userbar-item:hover {{ background: #2a292e; }}
}}
</style>
<script>
(function () {{
    var bar = document.querySelector("[data-rcms-userbar]");
    if (!bar) return;
    // Show the bar only if the admin session cookie is present.
    var cookieName = bar.getAttribute("data-rcms-cookie-name") || "cms_admin_session";
    var hasSession = document.cookie.split(";").some(function (c) {{
        return c.trim().split("=")[0] === cookieName;
    }});
    // Also accept the framework's session cookie names.
    if (!hasSession) {{
        hasSession = ["rustango_tenant_session", "rustango_operator_session", "sessionid"].some(function (name) {{
            return document.cookie.split(";").some(function (c) {{
                return c.trim().split("=")[0] === name;
            }});
        }});
    }}
    if (hasSession) bar.classList.add("is-visible");
    var toggle = bar.querySelector("[data-rcms-userbar-toggle]");
    var panel = bar.querySelector("[data-rcms-userbar-panel]");
    if (toggle && panel) {{
        toggle.addEventListener("click", function () {{
            var open = panel.hasAttribute("data-open");
            if (open) {{ panel.removeAttribute("data-open"); }}
            else {{ panel.setAttribute("data-open", ""); }}
            toggle.setAttribute("aria-expanded", String(!open));
        }});
    }}
}})();
</script>"#,
        edit_html = edit_html,
        plugin_items_html = plugin_items_html,
        prefix = admin_prefix,
    );
    Ok(tera::Value::String(html))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_open_admin_link_even_without_page_id() {
        let mut tera = tera::Tera::default();
        register_tera_function(&mut tera);
        let args: HashMap<String, tera::Value> = HashMap::new();
        let v = rcms_userbar(&args).unwrap();
        let html = v.as_str().unwrap();
        assert!(html.contains("Open admin"));
        assert!(!html.contains("Edit page"));
    }

    #[test]
    fn renders_edit_link_with_page_id() {
        let mut args: HashMap<String, tera::Value> = HashMap::new();
        args.insert("page_id".to_owned(), tera::Value::from(42_i64));
        let v = rcms_userbar(&args).unwrap();
        let html = v.as_str().unwrap();
        assert!(html.contains("/cms-admin/pages/42/edit"));
        assert!(html.contains("/cms-admin/pages/42/history"));
    }

    #[test]
    fn respects_admin_prefix() {
        let mut args: HashMap<String, tera::Value> = HashMap::new();
        args.insert("page_id".to_owned(), tera::Value::from(7_i64));
        args.insert("admin_prefix".to_owned(), tera::Value::from("/admin"));
        let v = rcms_userbar(&args).unwrap();
        let html = v.as_str().unwrap();
        assert!(html.contains("/admin/pages/7/edit"));
    }
}
