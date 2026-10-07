//! Content metrics — word count, estimated reading time, and a
//! readability score.
//!
//! Computed server-side at editor-open from the **content-bearing
//! extension widget values** (the markdown / text fields the author is
//! editing), so the editor shows metrics without opening the preview
//! Pure functions — [`analyze`] takes a string and
//! [`collect_widget_text`] gathers the text to feed it.

use crate::widget::{Widget, WidgetKind};

/// Average adult reading speed (words per minute) for the reading-time
/// estimate. Common estimates use ~200–265; 200 is the conservative
/// end so the estimate doesn't undersell length.
const WORDS_PER_MINUTE: usize = 200;

/// Word count + reading time + readability for a chunk of content.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ContentMetrics {
    pub words: usize,
    /// Minutes, `ceil(words / 200)`. Zero when there are no words.
    pub reading_time_min: usize,
    /// Flesch Reading Ease (higher = easier; ~0–100 typical). Zero when
    /// empty.
    pub flesch_reading_ease: f64,
    /// Coarse band for the score: `Easy` / `Standard` / `Difficult`,
    /// or `—` when empty.
    pub reading_ease_label: String,
}

/// Concatenate the text of content-bearing widgets — `Text`,
/// `Textarea`, `Markdown`, `RichText` — skipping choosers, numbers,
/// booleans, and the like. The result feeds [`analyze`].
#[must_use]
pub fn collect_widget_text(widgets: &[Widget]) -> String {
    widgets
        .iter()
        .filter(|w| {
            matches!(
                w.kind,
                WidgetKind::Text
                    | WidgetKind::Textarea
                    | WidgetKind::Markdown
                    | WidgetKind::RichText
            )
        })
        .map(|w| w.value.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Word count + reading time + Flesch reading ease for `text`. HTML
/// tags and light markdown punctuation are stripped first, so the same
/// function works on markdown source, rendered HTML, or plain text.
#[must_use]
pub fn analyze(text: &str) -> ContentMetrics {
    let plain = strip_markup(text);
    let words: Vec<&str> = plain
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .collect();
    let word_count = words.len();
    if word_count == 0 {
        return ContentMetrics {
            words: 0,
            reading_time_min: 0,
            flesch_reading_ease: 0.0,
            reading_ease_label: "—".to_owned(),
        };
    }
    let reading_time_min = word_count.div_ceil(WORDS_PER_MINUTE);
    // Sentence terminators; at least one so the ratio is finite.
    let sentences = plain
        .chars()
        .filter(|c| matches!(c, '.' | '!' | '?'))
        .count()
        .max(1);
    let syllables: usize = words.iter().map(|w| syllables_in(w)).sum();

    let wps = word_count as f64 / sentences as f64;
    let spw = syllables as f64 / word_count as f64;
    let flesch = 206.835 - 1.015 * wps - 84.6 * spw;
    let flesch = (flesch * 10.0).round() / 10.0;

    let label = if flesch >= 70.0 {
        "Easy"
    } else if flesch >= 50.0 {
        "Standard"
    } else {
        "Difficult"
    };
    ContentMetrics {
        words: word_count,
        reading_time_min,
        flesch_reading_ease: flesch,
        reading_ease_label: label.to_owned(),
    }
}

/// Strip HTML tags and replace common markdown punctuation with spaces
/// so word splitting sees content, not syntax. Deliberately light — an
/// exact parse isn't needed for a count/estimate.
fn strip_markup(text: &str) -> String {
    let mut no_tags = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => no_tags.push(c),
            _ => {}
        }
    }
    no_tags
        .chars()
        .map(|c| match c {
            '#' | '*' | '_' | '`' | '[' | ']' | '(' | ')' | '!' | '|' => ' ',
            other => other,
        })
        .collect()
}

/// Rough English syllable count — number of vowel groups, with a silent
/// trailing-`e` correction, floored at 1. Good enough for a readability
/// estimate, not a dictionary.
fn syllables_in(word: &str) -> usize {
    let chars: Vec<char> = word
        .chars()
        .filter(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if chars.is_empty() {
        return 0;
    }
    let is_vowel = |c: char| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y');
    let mut count = 0usize;
    let mut prev_vowel = false;
    for &c in &chars {
        let v = is_vowel(c);
        if v && !prev_vowel {
            count += 1;
        }
        prev_vowel = v;
    }
    // Silent trailing 'e' (e.g. "make", "code") — drop one group.
    if chars.len() > 2
        && *chars.last().unwrap() == 'e'
        && !is_vowel(chars[chars.len() - 2])
        && count > 1
    {
        count -= 1;
    }
    count.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_words_and_reading_time() {
        let m = analyze("Hello world. This is a short test.");
        assert_eq!(m.words, 7);
        assert_eq!(m.reading_time_min, 1); // <200 words → 1 min
    }

    #[test]
    fn reading_time_rounds_up_per_200_words() {
        let text = "word ".repeat(250);
        assert_eq!(analyze(&text).reading_time_min, 2); // ceil(250/200)
        let text2 = "word ".repeat(200);
        assert_eq!(analyze(&text2).reading_time_min, 1);
    }

    #[test]
    fn strips_html_and_markdown() {
        let m = analyze("<p>Hello <strong>world</strong></p>");
        assert_eq!(m.words, 2);
        let md = analyze("# Heading\n\n**Bold** and `code` words here");
        // "Heading Bold and code words here" → 6 words (syntax stripped).
        assert_eq!(md.words, 6);
    }

    #[test]
    fn empty_text_is_all_zero_with_dash_label() {
        let m = analyze("   \n  ");
        assert_eq!(m.words, 0);
        assert_eq!(m.reading_time_min, 0);
        assert_eq!(m.reading_ease_label, "—");
    }

    #[test]
    fn readability_label_bands() {
        // Simple short sentences read "Easy"; long Latinate ones harder.
        let easy = analyze("The cat sat on the mat. The dog ran.");
        assert_eq!(easy.reading_ease_label, "Easy", "{easy:?}");
        let hard = analyze(
            "Consequently, the aforementioned institutional methodologies \
             necessitated comprehensive reconsideration.",
        );
        assert!(
            hard.flesch_reading_ease < easy.flesch_reading_ease,
            "dense prose should score lower: {hard:?} vs {easy:?}"
        );
    }

    #[test]
    fn collect_widget_text_only_content_kinds() {
        let widgets = vec![
            Widget::new(WidgetKind::Text, "title", "Title").with_value("Hello there"),
            Widget::new(WidgetKind::Markdown, "body", "Body").with_value("more words"),
            Widget::new(WidgetKind::Number, "rank", "Rank").with_value("5"),
            Widget::new(WidgetKind::MediaPicker, "img", "Image").with_value("42"),
        ];
        let text = collect_widget_text(&widgets);
        assert!(text.contains("Hello there"));
        assert!(text.contains("more words"));
        assert!(!text.contains('5'), "number widget excluded: {text}");
        assert!(!text.contains("42"), "chooser widget excluded: {text}");
        assert_eq!(analyze(&text).words, 4);
    }
}
