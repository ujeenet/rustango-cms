//! Public form renderer.
//!
//! Turns a [`schema::Form`](super::schema::Form) into the HTML a visitor fills in. The output is
//! the contract `form_runtime.js` enhances: pages are `<fieldset
//! data-rcms-page>` blocks the runtime shows one at a time; with JS off all
//! pages are visible and the form still submits + server-validates.
//!
//! Provenance + per-embed overrides: the form carries a hidden `_form`
//! (form id), `_embed` (the embedding block's uuid), and a cache-safe
//! `_csrf` field populated from the double-submit cookie by an inline
//! script (the rendered HTML may be page-cached, so no per-user token is
//! baked in). The submit handler reads the `Referer` for the source
//! page and resolves the embed's override from that page's stream by uuid.

use super::schema::{Field, FieldType, Form};

/// Honeypot field name — a hidden input bots tend to fill.
pub const HONEYPOT_FIELD: &str = "_hp";

/// Render a whole form to HTML. `embed_id` is the embedding block's uuid
/// (empty when previewed standalone). `submit_action` is the POST target.
#[must_use]
pub fn render_form_html(form: &Form, form_id: i64, embed_id: &str) -> String {
    let action = format!("/forms/submit/{form_id}");
    let mut out = String::new();
    // Standard form styles — opt out per-form to fully replace the look.
    if form.settings.builtin_css {
        out.push_str("<link rel=\"stylesheet\" href=\"/forms/forms.css\">\n");
    }
    out.push_str("<div class=\"rcms-form-thanks\" role=\"status\" hidden>");
    out.push_str(&html_escape(form.settings.success_text()));
    out.push_str("</div>\n");
    // Forms with file fields submit as multipart (and the runtime fetch-
    // submits them so the X-CSRF-Token header reaches the CSRF layer).
    let has_file = form
        .fields()
        .any(|f| matches!(f.field_type, FieldType::File));
    let multipart = if has_file {
        " enctype=\"multipart/form-data\" data-rcms-multipart"
    } else {
        ""
    };
    out.push_str(&format!(
        "<form class=\"rcms-form\" method=\"post\" action=\"{}\" data-rcms-form data-rcms-form-key=\"{}\" data-pages=\"{}\"{multipart} novalidate>\n",
        html_attr(&action),
        html_attr(&format!("{form_id}:{embed_id}")),
        form.pages.len().max(1)
    ));
    out.push_str(&format!(
        "  <input type=\"hidden\" name=\"_form\" value=\"{form_id}\">\n"
    ));
    out.push_str(&format!(
        "  <input type=\"hidden\" name=\"_embed\" value=\"{}\">\n",
        html_attr(embed_id)
    ));
    out.push_str(&format!(
        "  <input type=\"hidden\" name=\"{}\" value=\"\">\n",
        rustango::forms::csrf::CSRF_FORM_FIELD
    ));
    // Shown by the runtime when the server refused the submission.
    out.push_str(&format!(
        "  <div class=\"rcms-form-error\" role=\"alert\" hidden>{}</div>\n",
        html_escape(form.settings.error_text())
    ));
    // Honeypot — off the tab order, and hidden inline so it stays hidden
    // when a site turns off the built-in form styles.
    out.push_str(&format!(
        "  <div class=\"rcms-hp\" aria-hidden=\"true\" style=\"position:absolute;left:-10000px;width:1px;height:1px;overflow:hidden\"><label>Leave blank<input type=\"text\" name=\"{HONEYPOT_FIELD}\" tabindex=\"-1\" autocomplete=\"off\"></label></div>\n"
    ));

    // Progress (runtime fills it in for multi-page forms).
    if form.pages.len() > 1 {
        out.push_str(&format!(
            "  <div class=\"rcms-form-progress\" data-rcms-progress data-text=\"{}\" hidden></div>\n",
            html_attr(form.settings.progress_text())
        ));
    }

    for (pi, page) in form.pages.iter().enumerate() {
        out.push_str(&format!(
            "  <fieldset class=\"rcms-form-page\" data-rcms-page=\"{pi}\">\n"
        ));
        if !page.label.is_empty() && form.pages.len() > 1 {
            out.push_str(&format!(
                "    <legend>{}</legend>\n",
                html_escape(&page.label)
            ));
        }
        for section in &page.sections {
            out.push_str("    <div class=\"rcms-form-section\">\n");
            if !section.title.is_empty() {
                out.push_str(&format!(
                    "      <h3 class=\"rcms-form-section-title\">{}</h3>\n",
                    html_escape(&section.title)
                ));
            }
            for row in &section.rows {
                out.push_str("      <div class=\"rcms-form-row\">\n");
                for col in &row.columns {
                    out.push_str(&format!(
                        "        <div class=\"rcms-form-col\" style=\"flex:{} 1 0\">\n",
                        col.width.clamp(1, 12)
                    ));
                    for field in &col.fields {
                        out.push_str(&render_field(field));
                    }
                    out.push_str("        </div>\n");
                }
                out.push_str("      </div>\n");
            }
            out.push_str("    </div>\n");
        }
        out.push_str("  </fieldset>\n");
    }

    // Navigation — runtime toggles prev/next/submit; no-JS shows submit only.
    out.push_str("  <div class=\"rcms-form-nav\">\n");
    out.push_str(&format!(
        "    <button type=\"button\" class=\"rcms-form-prev\" data-rcms-prev hidden>{}</button>\n",
        html_escape(form.settings.back_text())
    ));
    out.push_str(&format!(
        "    <button type=\"button\" class=\"rcms-form-next\" data-rcms-next hidden>{}</button>\n",
        html_escape(form.settings.next_text())
    ));
    out.push_str(&format!(
        "    <button type=\"submit\" class=\"rcms-form-submit\" data-rcms-submit>{}</button>\n",
        html_escape(form.settings.submit_text())
    ));
    out.push_str("  </div>\n");

    out.push_str("</form>\n");
    // FB-10/FB-14 — the cacheable runtime (csrf-fill, thanks, paging,
    // conditional logic) enhances every form on the page. Guarded so
    // multiple embeds load it harmlessly once.
    out.push_str("<script src=\"/forms/runtime.js\" defer></script>\n");
    out
}

fn render_field(f: &Field) -> String {
    // FB-14 — conditional-logic hooks the runtime reads off each wrapper.
    let mut wattr = format!(" data-rcms-key=\"{}\"", html_attr(&f.key));
    if !f.rules.is_empty() {
        if let Ok(j) = serde_json::to_string(&f.rules) {
            wattr.push_str(&format!(" data-rcms-rules=\"{}\"", html_attr(&j)));
        }
    }

    // Presentational types carry no input.
    if matches!(f.field_type, FieldType::Static) {
        return format!(
            "          <div class=\"rcms-field rcms-field-static\"{wattr}>{}</div>\n",
            html_escape(&f.content)
        );
    }
    if matches!(f.field_type, FieldType::Richtext) {
        // Author-controlled rich text, sanitized here because every write
        // path (builder, publish, MCP, translations) can reach this render
        // with arbitrary HTML (#756).
        return format!(
            "          <div class=\"rcms-field rcms-field-richtext\"{wattr}>{}</div>\n",
            crate::markdown::sanitize_html(&f.content)
        );
    }
    if matches!(f.field_type, FieldType::Hidden) {
        return format!(
            "          <input type=\"hidden\" name=\"{}\" value=\"{}\">\n",
            html_attr(&f.key),
            html_attr(&f.default)
        );
    }

    let id = format!("rcms-f-{}", html_attr(&f.key));
    let req = if f.required { " required" } else { "" };
    let req_star = if f.required {
        " <span class=\"rcms-req\">*</span>"
    } else {
        ""
    };
    let ph = if f.placeholder.is_empty() {
        String::new()
    } else {
        format!(" placeholder=\"{}\"", html_attr(&f.placeholder))
    };

    // FB-13 — native HTML5 length/pattern attrs (client-side validation).
    let mut vattr = String::new();
    if let Some(min) = f.min_length {
        vattr.push_str(&format!(" minlength=\"{min}\""));
    }
    if let Some(max) = f.max_length {
        vattr.push_str(&format!(" maxlength=\"{max}\""));
    }
    if !f.pattern.is_empty() {
        vattr.push_str(&format!(" pattern=\"{}\"", html_attr(&f.pattern)));
    }

    let control = match f.field_type {
        FieldType::Textarea => format!(
            "<textarea id=\"{id}\" name=\"{name}\"{req}{ph}{vattr} rows=\"4\">{val}</textarea>",
            name = html_attr(&f.key),
            val = html_escape(&f.default),
        ),
        FieldType::Select | FieldType::Multiselect => {
            let multiple = if matches!(f.field_type, FieldType::Multiselect) {
                " multiple"
            } else {
                ""
            };
            let mut s = format!(
                "<select id=\"{id}\" name=\"{name}\"{req}{multiple}>",
                name = html_attr(&f.key)
            );
            if matches!(f.field_type, FieldType::Select) {
                // The field's placeholder (translatable) is the prompt.
                let prompt = if f.placeholder.is_empty() { "—" } else { f.placeholder.as_str() };
                s.push_str(&format!("<option value=\"\">{}</option>", html_escape(prompt)));
            }
            for o in &f.options {
                s.push_str(&format!(
                    "<option value=\"{}\">{}</option>",
                    html_attr(&o.value),
                    html_escape(if o.label.is_empty() { &o.value } else { &o.label })
                ));
            }
            s.push_str("</select>");
            s
        }
        FieldType::Radio | FieldType::Checkboxes => {
            let input_type = if matches!(f.field_type, FieldType::Radio) {
                "radio"
            } else {
                "checkbox"
            };
            // A required radio group is native (`required` on its inputs);
            // "at least one box" has no HTML attribute, so the runtime
            // enforces `data-rcms-required` on a checkbox group.
            let (group_req, input_req) = match (f.required, f.field_type) {
                (true, FieldType::Radio) => ("", " required"),
                (true, _) => (" data-rcms-required aria-required=\"true\"", ""),
                _ => ("", ""),
            };
            let mut s = format!(
                "<div class=\"rcms-choices\" role=\"group\" aria-label=\"{}\"{group_req}>",
                html_attr(&f.label)
            );
            for o in &f.options {
                s.push_str(&format!(
                    "<label class=\"rcms-choice\"><input type=\"{input_type}\" name=\"{name}\" value=\"{val}\"{input_req}> {lbl}</label>",
                    name = html_attr(&f.key),
                    val = html_attr(&o.value),
                    lbl = html_escape(if o.label.is_empty() { &o.value } else { &o.label })
                ));
            }
            s.push_str("</div>");
            s
        }
        FieldType::Checkbox => format!(
            "<label class=\"rcms-choice\"><input type=\"checkbox\" id=\"{id}\" name=\"{name}\" value=\"yes\"{req}> {lbl}</label>",
            name = html_attr(&f.key),
            lbl = html_escape(&f.label)
        ),
        FieldType::Rating => {
            let scale = f.scale.unwrap_or(5).clamp(1, 10);
            let mut s = format!("<div class=\"rcms-rating\" role=\"radiogroup\" aria-label=\"{}\">", html_attr(&f.label));
            for i in 1..=scale {
                s.push_str(&format!(
                    "<label class=\"rcms-rating-star\"><input type=\"radio\" name=\"{name}\" value=\"{i}\"{req} aria-label=\"{i}\"> <span aria-hidden=\"true\">★</span></label>",
                    name = html_attr(&f.key)
                ));
            }
            s.push_str("</div>");
            s
        }
        FieldType::File => format!(
            "<input type=\"file\" id=\"{id}\" name=\"{name}\"{req}{accept}>",
            name = html_attr(&f.key),
            accept = if f.accept.is_empty() {
                String::new()
            } else {
                format!(" accept=\"{}\"", html_attr(&f.accept))
            }
        ),
        _ => {
            // text-shaped + date/time/number map to native input types.
            let html_type = match f.field_type {
                FieldType::Email => "email",
                FieldType::Url => "url",
                FieldType::Tel => "tel",
                FieldType::Number => "number",
                FieldType::Date => "date",
                FieldType::Datetime => "datetime-local",
                FieldType::Time => "time",
                _ => "text",
            };
            let mut extra = String::new();
            if matches!(f.field_type, FieldType::Number) {
                if let Some(min) = f.min {
                    extra.push_str(&format!(" min=\"{min}\""));
                }
                if let Some(max) = f.max {
                    extra.push_str(&format!(" max=\"{max}\""));
                }
                if let Some(step) = f.step {
                    extra.push_str(&format!(" step=\"{step}\""));
                }
            }
            format!(
                "<input type=\"{html_type}\" id=\"{id}\" name=\"{name}\" value=\"{val}\"{req}{ph}{extra}{vattr}>",
                name = html_attr(&f.key),
                val = html_attr(&f.default),
            )
        }
    };

    // checkbox renders its own label
    if matches!(f.field_type, FieldType::Checkbox) {
        let help = if f.help.is_empty() {
            String::new()
        } else {
            format!(
                "<small class=\"rcms-help\">{}</small>",
                html_escape(&f.help)
            )
        };
        return format!("          <div class=\"rcms-field\"{wattr}>{control}{help}</div>\n");
    }

    let label = if f.label.is_empty() {
        String::new()
    } else {
        format!(
            "<label for=\"{id}\">{}{req_star}</label>",
            html_escape(&f.label)
        )
    };
    let help = if f.help.is_empty() {
        String::new()
    } else {
        format!(
            "<small class=\"rcms-help\">{}</small>",
            html_escape(&f.help)
        )
    };
    format!("          <div class=\"rcms-field\"{wattr}>{label}{control}{help}</div>\n")
}

pub(crate) fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn html_attr(s: &str) -> String {
    html_escape(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forms::schema::{Choice, Column, Page, Row, Section};

    fn form_with(fields: Vec<Field>) -> Form {
        Form {
            pages: vec![Page {
                id: "p1".into(),
                label: "Page 1".into(),
                sections: vec![Section {
                    id: "s1".into(),
                    title: String::new(),
                    rows: vec![Row {
                        id: "r1".into(),
                        columns: vec![Column {
                            id: "c1".into(),
                            width: 12,
                            fields,
                        }],
                    }],
                }],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn renders_core_inputs_and_hidden_meta() {
        let form = form_with(vec![
            Field {
                id: "f1".into(),
                key: "name".into(),
                field_type: FieldType::Text,
                label: "Name".into(),
                required: true,
                ..Default::default()
            },
            Field {
                id: "f2".into(),
                key: "email".into(),
                field_type: FieldType::Email,
                label: "Email".into(),
                ..Default::default()
            },
        ]);
        let html = render_form_html(&form, 7, "blk-123");
        assert!(html.contains("action=\"/forms/submit/7\""));
        assert!(html.contains("name=\"_form\" value=\"7\""));
        assert!(html.contains("name=\"_embed\" value=\"blk-123\""));
        assert!(html.contains("name=\"_csrf\" value=\"\""));
        assert!(html.contains("/forms/runtime.js")); // cacheable runtime
        assert!(html.contains("name=\"_hp\"")); // honeypot
        assert!(html.contains("class=\"rcms-form-error\" role=\"alert\" hidden")); // error banner
        // …hidden inline, so a site without the built-in styles doesn't show it.
        assert!(html.contains("class=\"rcms-hp\" aria-hidden=\"true\" style=\"position:absolute;left:-10000px"));
        assert!(html.contains("type=\"email\""));
        assert!(html.contains("name=\"name\""));
        assert!(html.contains("required"));
        assert!(html.contains("data-rcms-page=\"0\""));
    }

    #[test]
    fn rich_text_content_is_sanitized_on_render() {
        // #756 — every write path can store arbitrary HTML here.
        let form = form_with(vec![Field {
            id: "f1".into(),
            field_type: FieldType::Richtext,
            content: r#"<p>Hi <b>there</b></p><script>alert(1)</script><img src=x onerror=alert(2)>"#.into(),
            ..Default::default()
        }]);
        let html = render_form_html(&form, 7, "");
        assert!(html.contains("<b>there</b>"), "safe markup survives: {html}");
        // The form's own `<script src=/forms/runtime.js>` is expected; the
        // injected one is not.
        assert!(!html.contains("alert(1)"), "injected script stripped: {html}");
        assert!(!html.contains("onerror"), "event handler stripped: {html}");
    }

    #[test]
    fn sanitize_rich_text_cleans_stored_content() {
        let mut form = form_with(vec![Field {
            id: "f1".into(),
            field_type: FieldType::Richtext,
            content: "<p>ok</p><script>x()</script>".into(),
            ..Default::default()
        }]);
        form.sanitize_rich_text();
        let content = &form.fields().next().expect("one field").content;
        assert_eq!(content, "<p>ok</p>");
    }

    #[test]
    fn renders_choice_and_presentational() {
        let form = form_with(vec![
            Field {
                id: "f1".into(),
                key: "topic".into(),
                field_type: FieldType::Select,
                label: "Topic".into(),
                options: vec![Choice {
                    value: "a".into(),
                    label: "Apples".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            Field {
                id: "f2".into(),
                field_type: FieldType::Static,
                content: "Heading".into(),
                ..Default::default()
            },
        ]);
        let html = render_form_html(&form, 1, "");
        assert!(html.contains("<select"));
        assert!(html.contains("Apples"));
        assert!(html.contains("rcms-field-static"));
        assert!(html.contains("Heading"));
    }

    #[test]
    fn required_choice_groups_can_be_checked_in_the_browser() {
        let opts = vec![
            Choice { value: "a".into(), label: "A".into(), ..Default::default() },
            Choice { value: "b".into(), label: "B".into(), ..Default::default() },
        ];
        let form = form_with(vec![
            Field { id: "f1".into(), key: "pick".into(), field_type: FieldType::Radio, required: true, options: opts.clone(), ..Default::default() },
            Field { id: "f2".into(), key: "many".into(), field_type: FieldType::Checkboxes, required: true, options: opts, ..Default::default() },
            Field { id: "f3".into(), key: "stars".into(), field_type: FieldType::Rating, required: true, ..Default::default() },
        ]);
        let html = render_form_html(&form, 1, "");
        assert!(html.contains(r#"name="pick" value="a" required"#), "{html}");
        assert!(html.contains(r#"role="group" aria-label="" data-rcms-required"#), "{html}");
        assert!(!html.contains(r#"name="many" value="a" required"#), "no per-box required: {html}");
        assert!(html.contains(r#"name="stars" value="1" required"#), "{html}");
    }

    #[test]
    fn control_texts_come_from_the_settings() {
        let mut form = form_with(vec![]);
        form.pages.push(Page { id: "p2".into(), ..Default::default() });
        let html = render_form_html(&form, 1, "e1");
        assert!(html.contains(">Next</button>") && html.contains(">Back</button>"), "defaults: {html}");
        assert!(html.contains(r#"data-text="Step {n} of {total}""#));
        assert!(html.contains(r#"data-rcms-form-key="1:e1""#));
        form.settings.next_label = "Suivant".into();
        form.settings.back_label = "Retour".into();
        form.settings.progress_label = "Étape {n} sur {total}".into();
        form.settings.error_message = "Oups".into();
        form.settings.success_message = "Merci".into();
        let html = render_form_html(&form, 1, "e1");
        for want in [">Suivant</button>", ">Retour</button>", "Étape {n} sur {total}", ">Oups</div>", ">Merci</div>"] {
            assert!(html.contains(want), "{want}: {html}");
        }
    }

    #[test]
    fn multi_page_emits_progress_and_pages() {
        let mut form = form_with(vec![Field {
            id: "f1".into(),
            key: "a".into(),
            field_type: FieldType::Text,
            ..Default::default()
        }]);
        form.pages.push(Page {
            id: "p2".into(),
            label: "Two".into(),
            sections: vec![],
            ..Default::default()
        });
        let html = render_form_html(&form, 1, "");
        assert!(html.contains("data-rcms-progress"));
        assert!(html.contains("data-rcms-page=\"0\""));
        assert!(html.contains("data-rcms-page=\"1\""));
    }
}
