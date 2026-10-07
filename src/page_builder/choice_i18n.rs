//! Per-language labels for a page type's choice options.
//!
//! A select / radio / checkboxes field stores the chosen option's *value*
//! on the page; visitors see the option's label. The labels live in the
//! page type's schema document, so their translations do too: each option
//! may carry `"labels": {"<locale code>": "<label>"}` (see
//! [`crate::forms::schema::Choice::label_in`] for how they are read).
//!
//! These helpers work on the raw document JSON, not the typed
//! [`super::Document`], so saving translations never rewrites anything
//! else the builder stored.

use std::collections::HashMap;

use serde_json::{Map, Value};

/// A choice field as the translation screen lists it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ChoiceField {
    /// Dotted key path, as in `builder.*` (`glaze`, `details.finish`).
    pub path: String,
    /// The field's label (its key when it has none).
    pub label: String,
    pub options: Vec<ChoiceOption>,
}

/// One option of a [`ChoiceField`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ChoiceOption {
    pub value: String,
    /// The label in the default language (the value when it has none).
    pub label: String,
    /// The label in the language being translated; empty when missing.
    pub translation: String,
}

/// `(value, label)` of an option in either stored shape: an object or the
/// older `[value, label]` pair.
fn option_parts(opt: &Value) -> (String, String) {
    match opt {
        Value::Array(a) => (
            a.first().and_then(Value::as_str).unwrap_or_default().to_owned(),
            a.get(1).and_then(Value::as_str).unwrap_or_default().to_owned(),
        ),
        Value::Object(o) => (
            o.get("value").and_then(Value::as_str).unwrap_or_default().to_owned(),
            o.get("label").and_then(Value::as_str).unwrap_or_default().to_owned(),
        ),
        _ => (String::new(), String::new()),
    }
}

fn child_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

/// Every choice field of `doc` (top level, rows and groups, in order),
/// with each option's label in `locale`.
#[must_use]
pub fn choice_fields(doc: &Value, locale: &str) -> Vec<ChoiceField> {
    fn walk(nodes: &[Value], prefix: &str, locale: &str, out: &mut Vec<ChoiceField>) {
        for node in nodes {
            let kind = node.get("kind").and_then(Value::as_str).unwrap_or_default();
            let key = node.get("key").and_then(Value::as_str).unwrap_or_default();
            let children = node.get("children").and_then(Value::as_array);
            match kind {
                "field" => {
                    let Some(opts) = node.get("options").and_then(Value::as_array).filter(|o| !o.is_empty()) else {
                        continue;
                    };
                    let label = node.get("label").and_then(Value::as_str).filter(|l| !l.is_empty()).unwrap_or(key);
                    out.push(ChoiceField {
                        path: child_path(prefix, key),
                        label: label.to_owned(),
                        options: opts
                            .iter()
                            .map(|o| {
                                let (value, label) = option_parts(o);
                                let translation = o
                                    .get("labels")
                                    .and_then(|l| l.get(locale))
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned();
                                let label = if label.is_empty() { value.clone() } else { label };
                                ChoiceOption { value, label, translation }
                            })
                            .collect(),
                    });
                }
                "row" => walk(children.map_or(&[][..], Vec::as_slice), prefix, locale, out),
                "group" => walk(children.map_or(&[][..], Vec::as_slice), &child_path(prefix, key), locale, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(doc.get("nodes").and_then(Value::as_array).map_or(&[][..], Vec::as_slice), "", locale, &mut out);
    out
}

/// Store `labels` — `{(field path, option value) -> label}` — as the
/// `locale` labels of `doc`'s options. An empty label removes the
/// translation. Options are matched by value, so a draft whose options
/// were reordered still gets the right labels. Returns how many options
/// changed.
pub fn set_choice_labels(doc: &mut Value, locale: &str, labels: &HashMap<(String, String), String>) -> usize {
    fn walk(nodes: &mut [Value], prefix: &str, locale: &str, labels: &HashMap<(String, String), String>) -> usize {
        let mut changed = 0;
        for node in nodes {
            let kind = node.get("kind").and_then(Value::as_str).unwrap_or_default().to_owned();
            let key = node.get("key").and_then(Value::as_str).unwrap_or_default().to_owned();
            match kind.as_str() {
                "field" => {
                    let path = child_path(prefix, &key);
                    let Some(opts) = node.get_mut("options").and_then(Value::as_array_mut) else {
                        continue;
                    };
                    for opt in opts {
                        let (value, label) = option_parts(opt);
                        let Some(new) = labels.get(&(path.clone(), value.clone())) else {
                            continue;
                        };
                        let new = new.trim();
                        if opt.is_array() {
                            // Upgrade the pair to the object shape to hold labels.
                            let mut o = Map::new();
                            o.insert("value".into(), Value::String(value));
                            o.insert("label".into(), Value::String(label));
                            *opt = Value::Object(o);
                        }
                        let Some(o) = opt.as_object_mut() else {
                            continue;
                        };
                        let map = o.entry("labels").or_insert_with(|| Value::Object(Map::new()));
                        let Some(map) = map.as_object_mut() else {
                            continue;
                        };
                        let before = map.get(locale).and_then(Value::as_str).unwrap_or_default().to_owned();
                        if new.is_empty() {
                            map.remove(locale);
                        } else {
                            map.insert(locale.to_owned(), Value::String(new.to_owned()));
                        }
                        if map.is_empty() {
                            o.remove("labels");
                        }
                        if before != new {
                            changed += 1;
                        }
                    }
                }
                "row" | "group" => {
                    let next = if kind == "group" { child_path(prefix, &key) } else { prefix.to_owned() };
                    if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
                        changed += walk(children, &next, locale, labels);
                    }
                }
                _ => {}
            }
        }
        changed
    }
    match doc.get_mut("nodes").and_then(Value::as_array_mut) {
        Some(nodes) => walk(nodes, "", locale, labels),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Value {
        json!({ "nodes": [
            { "kind": "field", "key": "price", "label": "Price", "widget": "number" },
            { "kind": "field", "key": "glaze", "label": "Glaze", "widget": "select", "id": "n1",
              "options": [["moon", "Moon blue"], { "value": "white", "label": "Satin white", "labels": { "de": "Seidenweiß" } }] },
            { "kind": "group", "key": "details", "label": "Details", "children": [
                { "kind": "row", "children": [
                    { "kind": "field", "key": "finish", "widget": "radio", "options": [["matte", ""]] }
                ]}
            ]}
        ]})
    }

    #[test]
    fn lists_choice_fields_with_paths_and_translations() {
        let fields = choice_fields(&doc(), "de");
        let paths: Vec<&str> = fields.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["glaze", "details.finish"]);
        assert_eq!(fields[0].options[1].translation, "Seidenweiß");
        assert_eq!(fields[0].options[0].translation, "");
        // No label: the value and the key stand in.
        assert_eq!(fields[1].label, "finish");
        assert_eq!(fields[1].options[0].label, "matte");
    }

    #[test]
    fn sets_and_clears_labels_by_value_keeping_everything_else() {
        let mut d = doc();
        let mut labels = HashMap::new();
        labels.insert(("glaze".to_owned(), "moon".to_owned()), " Bleu lune ".to_owned());
        labels.insert(("details.finish".to_owned(), "matte".to_owned()), "Mat".to_owned());
        assert_eq!(set_choice_labels(&mut d, "fr", &labels), 2);
        assert_eq!(d["nodes"][1]["options"][0], json!({ "value": "moon", "label": "Moon blue", "labels": { "fr": "Bleu lune" } }));
        assert_eq!(d["nodes"][1]["id"], "n1");
        assert_eq!(d["nodes"][2]["children"][0]["children"][0]["options"][0]["labels"]["fr"], "Mat");
        // The other language's label is untouched.
        assert_eq!(d["nodes"][1]["options"][1]["labels"], json!({ "de": "Seidenweiß" }));

        let mut clear = HashMap::new();
        clear.insert(("glaze".to_owned(), "moon".to_owned()), String::new());
        assert_eq!(set_choice_labels(&mut d, "fr", &clear), 1);
        assert_eq!(d["nodes"][1]["options"][0], json!({ "value": "moon", "label": "Moon blue" }));
        // The typed schema still parses what was written.
        assert!(crate::page_builder::parse_schema(&d).is_ok());
    }
}
