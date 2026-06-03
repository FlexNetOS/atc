//! Helpers for rendering untrusted values in human terminal output.

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

fn is_dangerous_format_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}'
            | '\u{feff}'
    )
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
}
