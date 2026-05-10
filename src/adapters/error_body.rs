//! Truncate and sanitise raw HTTP response bodies before bubbling them
//! up into user-facing errors.
//!
//! Without this, an HTML maintenance page or a multi-kilobyte stack
//! trace from a misbehaving upstream lands straight in the LLM's
//! context window.

/// Maximum number of characters to keep from a non-HTML upstream error body.
/// Real GW2/wiki error bodies are tiny JSON (`{"text":"..."}`); 256 chars
/// is comfortably above the longest sensible upstream error.
pub const MAX_BODY_CHARS: usize = 256;

/// Sentinel returned in place of an HTML-shaped body. Surfaces enough
/// context for the user to know the upstream is having a bad day without
/// echoing kilobytes of styled markup.
pub const HTML_SENTINEL: &str = "<HTML response — likely a maintenance page or upstream error>";

/// Strip ASCII control characters (except plain space), detect HTML, and
/// truncate plain-text bodies to [`MAX_BODY_CHARS`] codepoints.
///
/// - Control chars (`\n`, `\t`, embedded NULs, ANSI escapes…) become
///   spaces. This stops a hostile body from inserting newlines into the
///   error message that misalign log lines or trick downstream parsers.
/// - HTML detection is intentionally cheap: if any of the first 64
///   non-whitespace characters look like the opening of a tag (`<`
///   followed by a letter or `!`), assume HTML. False positives just
///   produce a tighter error; false negatives still get truncated.
/// - Truncation cuts on character (codepoint) boundaries so we never
///   emit an invalid UTF-8 sequence.
#[must_use]
pub fn truncate_error_body(body: &str) -> String {
    if looks_like_html(body) {
        return HTML_SENTINEL.to_owned();
    }
    let cleaned: String = body
        .chars()
        .map(|c| {
            if c == ' ' || !c.is_ascii_control() {
                c
            } else {
                ' '
            }
        })
        .collect();

    let mut out: String = cleaned.chars().take(MAX_BODY_CHARS).collect();
    if out.chars().count() < cleaned.chars().count() {
        out.push_str("… (truncated)");
    }
    out
}

fn looks_like_html(body: &str) -> bool {
    // Look at the first ~64 leading bytes after whitespace. HTML pages
    // (DOCTYPE, <html>, <!--) all start with `<` followed by a tag-name
    // character or `!`.
    let snippet = body.trim_start();
    let head: String = snippet.chars().take(64).collect();
    let mut chars = head.chars();
    while let Some(c) = chars.next() {
        if c == '<' {
            return matches!(chars.next(), Some(n) if n == '!' || n.is_ascii_alphabetic());
        }
        if !c.is_ascii_whitespace() {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_doctype_collapses_to_sentinel() {
        let body = "<!DOCTYPE html><html><body>Maintenance, please come back later</body></html>";
        assert_eq!(truncate_error_body(body), HTML_SENTINEL);
    }

    #[test]
    fn html_tag_collapses_to_sentinel() {
        let body = "<html>...</html>";
        assert_eq!(truncate_error_body(body), HTML_SENTINEL);
    }

    #[test]
    fn html_with_leading_whitespace_collapses_to_sentinel() {
        let body = "   \n\n  <html>...</html>";
        assert_eq!(truncate_error_body(body), HTML_SENTINEL);
    }

    #[test]
    fn html_with_comment_prefix_collapses_to_sentinel() {
        let body = "<!-- header -->\n<html>";
        assert_eq!(truncate_error_body(body), HTML_SENTINEL);
    }

    #[test]
    fn short_plain_text_is_returned_verbatim() {
        let body = "no such character";
        assert_eq!(truncate_error_body(body), "no such character");
    }

    #[test]
    fn json_error_is_returned_verbatim() {
        let body = r#"{"text":"invalid key"}"#;
        assert_eq!(truncate_error_body(body), body);
    }

    #[test]
    fn long_plain_text_is_truncated_with_marker() {
        let body = "x".repeat(MAX_BODY_CHARS + 100);
        let out = truncate_error_body(&body);
        assert!(out.starts_with(&"x".repeat(MAX_BODY_CHARS)));
        assert!(out.ends_with("(truncated)"));
        assert!(out.len() < body.len());
    }

    #[test]
    fn boundary_at_exact_max_keeps_full_body() {
        let body = "x".repeat(MAX_BODY_CHARS);
        let out = truncate_error_body(&body);
        assert_eq!(out, body);
        assert!(!out.contains("truncated"));
    }

    #[test]
    fn control_chars_become_spaces() {
        let body = "hello\nworld\t\x07!";
        assert_eq!(truncate_error_body(body), "hello world  !");
    }

    #[test]
    fn unicode_truncation_respects_boundaries() {
        // Emoji is 4 bytes but 1 codepoint — make sure we count codepoints.
        let body = "🎮".repeat(MAX_BODY_CHARS + 10);
        let out = truncate_error_body(&body);
        // Must still be valid UTF-8 (no panic) and end with the marker.
        assert!(out.ends_with("(truncated)"));
        assert!(out.chars().filter(|c| *c == '🎮').count() == MAX_BODY_CHARS);
    }

    #[test]
    fn lone_lt_char_is_not_html() {
        // A body that starts with `<3` (heart, not a tag) shouldn't be
        // mis-detected as HTML.
        let body = "<3 not html";
        assert_eq!(truncate_error_body(body), "<3 not html");
    }
}
