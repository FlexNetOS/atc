//! Helpers for rendering untrusted values in terminal output.

/// Escape terminal controls and invisible formatting characters so registry or
/// log data cannot move the cursor, write OSC sequences, or visually spoof rows.
pub fn display_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() || is_dangerous_format_control(ch) => {
                let code = ch as u32;
                if code <= 0xff {
                    out.push_str(&format!("\\x{code:02x}"));
                } else {
                    out.push_str(&format!("\\u{{{code:x}}}"));
                }
            }
            ch => out.push(ch),
        }
    }
    out
}

/// Pretty-serialize JSON while escaping Unicode format controls that can spoof
/// terminal display. JSON control characters such as ESC and BEL are already
/// escaped by serde_json; this post-processing covers bidi/invisible controls
/// that serde_json is allowed to emit as raw UTF-8.
pub fn terminal_safe_json_pretty<T>(value: &T) -> Result<String, serde_json::Error>
where
    T: serde::Serialize + ?Sized,
{
    serde_json::to_string_pretty(value).map(|json| escape_json_format_controls(&json))
}

/// Serialize compact JSON while applying the same terminal-spoofing escapes as
/// [`terminal_safe_json_pretty`].
pub fn terminal_safe_json<T>(value: &T) -> Result<String, serde_json::Error>
where
    T: serde::Serialize + ?Sized,
{
    serde_json::to_string(value).map(|json| escape_json_format_controls(&json))
}

/// Escape dangerous raw Unicode format controls in already-serialized JSON.
pub fn escape_json_format_controls(value: &str) -> String {
    if !value.chars().any(is_dangerous_format_control) {
        return value.to_string();
    }

    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if is_dangerous_format_control(ch) {
            push_json_unicode_escape(&mut out, ch);
        } else {
            out.push(ch);
        }
    }
    out
}

fn is_dangerous_format_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}'
            | '\u{feff}'
    )
}

fn push_json_unicode_escape(out: &mut String, ch: char) {
    let code = ch as u32;
    if code <= 0xffff {
        out.push_str(&format!("\\u{code:04x}"));
    } else {
        let code = code - 0x1_0000;
        let high = 0xd800 + ((code >> 10) & 0x3ff);
        let low = 0xdc00 + (code & 0x3ff);
        out.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_text_escapes_terminal_control_sequences() {
        let value = "task\x1b[31mred\x1b[0m\r\n\t\x07\u{9b}done";
        let escaped = display_text(value);
        assert_eq!(escaped, "task\\x1b[31mred\\x1b[0m\\r\\n\\t\\x07\\x9bdone");
        assert!(!escaped.contains('\x1b'));
        assert!(!escaped.contains('\x07'));
        assert!(!escaped.contains('\u{9b}'));
    }

    #[test]
    fn display_text_escapes_bidi_and_invisible_format_controls() {
        let value = "safe\u{202e}gpj.exe\u{2066}visible\u{2069}\u{200b}";
        let escaped = display_text(value);
        assert_eq!(
            escaped,
            "safe\\u{202e}gpj.exe\\u{2066}visible\\u{2069}\\u{200b}"
        );
        assert!(!escaped.contains('\u{202e}'));
        assert!(!escaped.contains('\u{2066}'));
        assert!(!escaped.contains('\u{2069}'));
        assert!(!escaped.contains('\u{200b}'));
    }

    #[test]
    fn terminal_safe_json_pretty_escapes_bidi_but_preserves_decoded_data() {
        let value = serde_json::json!({
            "text": "safe\u{202e}gpj.exe\u{2066}visible\u{2069}",
            "control": "bell\u{7}"
        });

        let json = terminal_safe_json_pretty(&value).unwrap();

        assert!(!json.contains('\u{202e}'));
        assert!(!json.contains('\u{2066}'));
        assert!(!json.contains('\u{2069}'));
        assert!(!json.contains('\u{7}'));
        assert!(json.contains("\\u202e"));
        assert!(json.contains("\\u2066"));
        assert!(json.contains("\\u2069"));
        assert!(json.contains("\\u0007"));

        let decoded: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn terminal_safe_json_escapes_bidi_but_preserves_decoded_data() {
        let value = serde_json::json!({
            "text": "safe\u{202e}gpj.exe\u{2066}visible\u{2069}",
            "control": "bell\u{7}"
        });

        let json = terminal_safe_json(&value).unwrap();

        assert!(!json.contains('\u{202e}'));
        assert!(!json.contains('\u{2066}'));
        assert!(!json.contains('\u{2069}'));
        assert!(!json.contains('\u{7}'));
        assert!(json.contains("\\u202e"));
        assert!(json.contains("\\u2066"));
        assert!(json.contains("\\u2069"));
        assert!(json.contains("\\u0007"));

        let decoded: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn display_text_escapes_unicode_line_separators() {
        let value = "first\u{2028}second\u{2029}third";
        let escaped = display_text(value);

        assert_eq!(escaped, "first\\u{2028}second\\u{2029}third");
        assert!(!escaped.contains('\u{2028}'));
        assert!(!escaped.contains('\u{2029}'));
    }

    #[test]
    fn terminal_safe_json_escapes_unicode_line_separators() {
        let value = serde_json::json!({
            "text": "first\u{2028}second\u{2029}third"
        });

        let json = terminal_safe_json(&value).unwrap();

        assert!(!json.contains('\u{2028}'));
        assert!(!json.contains('\u{2029}'));
        assert!(json.contains("\\u2028"));
        assert!(json.contains("\\u2029"));

        let decoded: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, value);
    }
}
