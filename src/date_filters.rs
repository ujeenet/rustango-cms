//! Tera date filters — parse stringified dates from extension /
//! form values and re-format them for output, fail-soft on
//! unparseable / null / empty input (#255).
//!
//! Tera's built-in `| date` filter DOES parse `YYYY-MM-DD` and
//! RFC3339 strings — but it errors hard on anything else
//! (empty, null, partial datetime, missing zone). For extension
//! fields backed by a `Date` widget the storage shape IS a string,
//! but the value can be empty if the editor left the field blank;
//! piping a blank through `| date` 500s the whole page render.
//!
//! Two filters:
//!
//! - `| date_parse` — accepts the same shapes the built-in `| date`
//!   does PLUS a handful of common variants (RFC3339 without `Z`,
//!   naive datetime, naive date) and returns a normalized RFC3339
//!   string Tera's `date` can chew. Null / empty / unparseable
//!   collapses to `Value::Null`, which Tera's `date` skips via the
//!   author's `| default(value="")` (or a surrounding
//!   `{% if … %}`).
//! - `| format_date(format="…")` — fused parse+format. Single call
//!   site, returns the formatted string or empty on any failure.
//!   Most blog/feed templates use this shape.
//!
//! Both filters are marked `is_safe = false` — output is plain
//! text, autoescape applies normally.

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, Utc};
use tera::{Filter, Tera, Value};

/// Register `| date_parse` + `| format_date` on `tera`. Called once
/// at Tera setup from [`crate::urls::register_tera_helpers`].
pub fn register_tera_filters(tera: &mut Tera) {
    tera.register_filter("date_parse", DateParseFilter);
    tera.register_filter("format_date", FormatDateFilter);
}

struct DateParseFilter;

impl Filter for DateParseFilter {
    fn filter(&self, value: &Value, _args: &HashMap<String, Value>) -> tera::Result<Value> {
        match parse_value_to_utc(value) {
            Some(dt) => Ok(Value::String(dt.to_rfc3339())),
            None => Ok(Value::Null),
        }
    }
}

struct FormatDateFilter;

impl Filter for FormatDateFilter {
    fn filter(&self, value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
        let Some(dt) = parse_value_to_utc(value) else {
            return Ok(Value::String(String::new()));
        };
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("%Y-%m-%d");
        // chrono panics on a malformed strftime; defensively try a
        // probe-format to surface a tera error instead of crashing
        // the render. The probe uses the same format on a known-good
        // datetime so any parse-time issue with the spec lights up.
        let probe = chrono::format::strftime::StrftimeItems::new(format)
            .find(|i| matches!(i, chrono::format::Item::Error))
            .is_some();
        if probe {
            return Err(tera::Error::msg(format!(
                "format_date: invalid strftime `{format}`"
            )));
        }
        Ok(Value::String(dt.format(format).to_string()))
    }
}

/// Try every accepted shape and return a UTC datetime, or `None`
/// when the input was null / empty / unparseable.
fn parse_value_to_utc(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::Null => None,
        Value::Number(n) => {
            let secs = n.as_i64()?;
            DateTime::<Utc>::from_timestamp(secs, 0)
        }
        Value::String(s) => parse_str(s),
        _ => None,
    }
}

fn parse_str(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // RFC3339 with offset (the canonical chrono::DateTime serde shape).
    if let Ok(dt) = DateTime::<FixedOffset>::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // RFC3339-ish without explicit zone (`2026-05-24T10:30:00`).
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }
    // Same without seconds (HTML5 datetime-local minute precision).
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }
    // Space-separated naive (`2026-05-24 10:30:00` — common SQL
    // dump shape).
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }
    // Plain date — anchor at midnight UTC so `| date` consumers
    // get a usable instant.
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let naive = date.and_hms_opt(0, 0, 0)?;
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tera::{Context, Tera};

    fn fresh_tera() -> Tera {
        let mut tera = Tera::default();
        register_tera_filters(&mut tera);
        tera
    }

    fn render(tera: &Tera, src: &str, value: Value) -> String {
        let mut t = tera.clone();
        t.add_raw_template("t.html", src).unwrap();
        let mut ctx = Context::new();
        ctx.insert("v", &value);
        t.render("t.html", &ctx).unwrap()
    }

    // ----- format_date -----

    #[test]
    fn format_date_parses_yyyy_mm_dd() {
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{{ v | format_date(format="%b %d, %Y") }}"#,
            Value::String("2026-05-24".to_owned()),
        );
        assert_eq!(out, "May 24, 2026");
    }

    #[test]
    fn format_date_parses_rfc3339() {
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{{ v | format_date(format="%Y-%m-%d") }}"#,
            Value::String("2026-05-24T10:30:00Z".to_owned()),
        );
        assert_eq!(out, "2026-05-24");
    }

    #[test]
    fn format_date_parses_naive_datetime() {
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{{ v | format_date(format="%Y-%m-%d %H:%M") }}"#,
            Value::String("2026-05-24T10:30:00".to_owned()),
        );
        assert_eq!(out, "2026-05-24 10:30");
    }

    #[test]
    fn format_date_handles_html5_datetime_local() {
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{{ v | format_date(format="%Y-%m-%d") }}"#,
            // datetime-local without seconds — same shape the
            // page form's go_live_at input emits.
            Value::String("2026-05-24T10:30".to_owned()),
        );
        assert_eq!(out, "2026-05-24");
    }

    #[test]
    fn format_date_handles_space_separated_naive() {
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{{ v | format_date(format="%Y-%m-%d") }}"#,
            Value::String("2026-05-24 10:30:00".to_owned()),
        );
        assert_eq!(out, "2026-05-24");
    }

    #[test]
    fn format_date_empty_string_returns_empty() {
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"x{{ v | format_date(format="%Y-%m-%d") }}y"#,
            Value::String("".to_owned()),
        );
        // Empty value → empty output, surrounding text intact.
        assert_eq!(out, "xy");
    }

    #[test]
    fn format_date_null_returns_empty() {
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"x{{ v | format_date(format="%Y-%m-%d") }}y"#,
            Value::Null,
        );
        assert_eq!(out, "xy");
    }

    #[test]
    fn format_date_unparseable_returns_empty() {
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"x{{ v | format_date(format="%Y-%m-%d") }}y"#,
            Value::String("not a date".to_owned()),
        );
        assert_eq!(out, "xy");
    }

    #[test]
    fn format_date_unix_timestamp_number() {
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{{ v | format_date(format="%Y-%m-%d") }}"#,
            // Unix epoch — 1970-01-01 00:00:00 UTC. Self-checking
            // anchor value: any drift in chrono / `from_timestamp`
            // surfaces immediately.
            Value::Number(serde_json::Number::from(0i64)),
        );
        assert_eq!(out, "1970-01-01");
    }

    #[test]
    fn format_date_default_format_is_ymd() {
        let tera = fresh_tera();
        let out = render(
            &tera,
            r#"{{ v | format_date }}"#,
            Value::String("2026-05-24T10:30:00Z".to_owned()),
        );
        assert_eq!(out, "2026-05-24");
    }

    #[test]
    fn format_date_invalid_strftime_errors() {
        let mut tera = fresh_tera();
        tera.add_raw_template("t.html", r#"{{ v | format_date(format="%Q") }}"#)
            .unwrap();
        let mut ctx = Context::new();
        ctx.insert("v", "2026-05-24");
        let err = tera.render("t.html", &ctx).unwrap_err();
        let chain = format!("{err:?}");
        assert!(
            chain.contains("invalid strftime"),
            "expected strftime error, got `{chain}`"
        );
    }

    // ----- date_parse (normalization for piping into `| date`) -----

    #[test]
    fn date_parse_normalizes_to_rfc3339() {
        let tera = fresh_tera();
        let mut t = tera.clone();
        t.add_raw_template("t.html", r#"{{ v | date_parse }}"#)
            .unwrap();
        let mut ctx = Context::new();
        ctx.insert("v", "2026-05-24");
        let out = t.render("t.html", &ctx).unwrap();
        // RFC3339 starts with the date; the exact suffix is timezone
        // (`+00:00`). Pin the prefix so format drift in chrono doesn't
        // wreck the test.
        assert!(out.starts_with("2026-05-24T00:00:00"), "got `{out}`");
        // Confirm it chains into Tera's built-in `| date`.
        let mut t2 = tera.clone();
        t2.add_raw_template(
            "t.html",
            r#"{{ v | date_parse | date(format="%b %d, %Y") }}"#,
        )
        .unwrap();
        let out2 = t2.render("t.html", &ctx).unwrap();
        assert_eq!(out2, "May 24, 2026");
    }

    #[test]
    fn date_parse_empty_returns_null() {
        // Empty / unparseable input collapses to `Value::Null`. Tera
        // serializes Null as the empty string in `{{ … }}`, so a
        // surrounding `{% if … %}` is the idiomatic guard.
        let tera = fresh_tera();
        let mut t = tera.clone();
        t.add_raw_template(
            "t.html",
            r#"x{% if v | date_parse %}got{% else %}empty{% endif %}y"#,
        )
        .unwrap();
        let mut ctx = Context::new();
        ctx.insert("v", "");
        assert_eq!(t.render("t.html", &ctx).unwrap(), "xemptyy");
        let mut ctx2 = Context::new();
        ctx2.insert("v", "2026-05-24");
        assert_eq!(t.render("t.html", &ctx2).unwrap(), "xgoty");
    }
}
