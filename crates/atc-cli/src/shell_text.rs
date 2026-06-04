use atc_core::terminal_text::display_text;

pub(crate) fn shell_display_arg(value: &str) -> String {
    match shlex::try_quote(value) {
        Ok(quoted) => display_text(&quoted),
        Err(_) => "<unrepresentable>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_display_arg_shell_quotes_special_characters() {
        let value = "tasks/foo; rm -rf /";
        let quoted = shell_display_arg(value);
        assert_ne!(quoted, value);
        assert!(quoted.contains("rm -rf"));
    }

    #[test]
    fn shell_display_arg_escapes_terminal_controls_after_quoting() {
        let quoted = shell_display_arg("tasks/foo\x1b[31m\u{202e}");
        assert!(quoted.contains("\\x1b"));
        assert!(quoted.contains("\\u{202e}"));
        assert!(!quoted.contains('\x1b'));
        assert!(!quoted.contains('\u{202e}'));
    }

    #[test]
    fn shell_display_arg_marks_values_that_cannot_be_shell_arguments() {
        assert_eq!(shell_display_arg("tasks/foo\0bar"), "<unrepresentable>");
    }
}
