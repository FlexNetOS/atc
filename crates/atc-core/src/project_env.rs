//! Parse `.dispatch/env` files into key-value pairs for agent environment injection.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use crate::executor::validate_env_key;

/// Parse a `.dispatch/env` file into key-value pairs.
///
/// Format: shell-compatible `KEY=VALUE` lines.
/// - Skips empty lines and comments (lines starting with `#`).
/// - Strips optional `export ` prefix.
/// - Handles double-quoted and single-quoted values.
/// - Does NOT evaluate shell expressions (`$()`, backticks).
/// - Validates all keys via `validate_env_key()`.
///
/// Rejects files larger than 1 MiB to prevent OOM.
pub fn parse_env_file(path: &Path) -> Result<HashMap<String, String>> {
    const MAX_ENV_FILE_SIZE: u64 = 1024 * 1024;
    let meta = std::fs::metadata(path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {}", path.display(), e))?;
    if meta.len() > MAX_ENV_FILE_SIZE {
        anyhow::bail!(
            "{} is too large ({} bytes, max {})",
            path.display(),
            meta.len(),
            MAX_ENV_FILE_SIZE
        );
    }
    let contents = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {}", path.display(), e))?;
    parse_env_contents(&contents)
}

/// Parse env file contents (separated for testability).
pub fn parse_env_contents(contents: &str) -> Result<HashMap<String, String>> {
    // Strip optional UTF-8 BOM (U+FEFF) that some editors prepend
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    const MAX_ENV_ENTRIES: usize = 256;
    let mut env = HashMap::new();

    for (line_num, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Strip optional `export ` prefix
        let line = line.strip_prefix("export ").unwrap_or(line);

        // Split on first `=`
        let Some((key, value)) = line.split_once('=') else {
            anyhow::bail!(
                "line {}: invalid format (expected KEY=VALUE): {:?}",
                line_num + 1,
                raw_line,
            );
        };

        let key = key.trim();
        validate_env_key(key).map_err(|e| anyhow::anyhow!("line {}: {}", line_num + 1, e))?;

        let value = strip_inline_comment(value);
        let value = value.trim();

        // Detect unclosed quotes before stripping — a common typo that would
        // silently corrupt values (e.g. FOO="hello # world → "hello).
        let has_open_double = value.starts_with('"');
        let has_close_double = value.ends_with('"');
        let has_open_single = value.starts_with('\'');
        let has_close_single = value.ends_with('\'');
        if (has_open_double && (!has_close_double || value.len() < 2))
            || (has_open_single && (!has_close_single || value.len() < 2))
        {
            anyhow::bail!(
                "line {}: unclosed quote in value: {:?}",
                line_num + 1,
                raw_line,
            );
        }

        let value = strip_quotes(value);

        if !env.contains_key(key) && env.len() >= MAX_ENV_ENTRIES {
            anyhow::bail!("env file exceeds maximum of {} entries", MAX_ENV_ENTRIES);
        }
        env.insert(key.to_string(), value);
    }

    Ok(env)
}

/// Strip trailing inline comments from unquoted values.
///
/// Per POSIX shell convention, an unquoted ` #` (space then hash) starts a
/// comment. If the value starts with a quote, we find the matching closing
/// quote first and only search for ` #` after that boundary — this correctly
/// handles values like `"val # ue" # comment` where `#` appears inside quotes.
fn strip_inline_comment(s: &str) -> String {
    // If the value starts with a quote, find the matching closing quote and
    // only search for ` #` after that boundary.
    let quote_end = if let Some(rest) = s.strip_prefix('"') {
        rest.find('"').map(|i| i + 2)
    } else if let Some(rest) = s.strip_prefix('\'') {
        rest.find('\'').map(|i| i + 2)
    } else {
        None
    };
    let search_from = quote_end.unwrap_or(0);
    if let Some(pos) = s[search_from..].find(" #") {
        s[..search_from + pos].trim_end().to_string()
    } else {
        s.to_string()
    }
}

/// Strip matching outer quotes (single or double) from a value.
///
/// Note: escape sequences (e.g. `\"`, `\\`) inside quoted values are **not**
/// interpreted — they are preserved literally. This is intentional: we avoid
/// evaluating shell semantics for safety (see also the `$()` / backtick note
/// in `parse_env_contents`).
fn strip_quotes(s: &str) -> String {
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        return s[1..s.len() - 1].to_string();
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_key_value() {
        let input = "FOO=bar\nBAZ=qux";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "bar");
        assert_eq!(env.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn test_comments_and_empty_lines() {
        let input = "# This is a comment\n\nFOO=bar\n  # indented comment\n\nBAR=baz\n";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.len(), 2);
        assert_eq!(env.get("FOO").unwrap(), "bar");
        assert_eq!(env.get("BAR").unwrap(), "baz");
    }

    #[test]
    fn test_export_prefix() {
        let input = "export FOO=bar\nexport BAZ=qux\nNORMAL=val";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "bar");
        assert_eq!(env.get("BAZ").unwrap(), "qux");
        assert_eq!(env.get("NORMAL").unwrap(), "val");
    }

    #[test]
    fn test_double_quoted_values() {
        let input = r#"FOO="value with spaces""#;
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "value with spaces");
    }

    #[test]
    fn test_single_quoted_values() {
        let input = "FOO='value with spaces'";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "value with spaces");
    }

    #[test]
    fn test_unquoted_value_with_equals() {
        let input = "FOO=bar=baz";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "bar=baz");
    }

    #[test]
    fn test_empty_value() {
        let input = "FOO=\nBAR=\"\"";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "");
        assert_eq!(env.get("BAR").unwrap(), "");
    }

    #[test]
    fn test_invalid_key_rejected() {
        let input = "GOOD=ok\n1BAD=no";
        let err = parse_env_contents(input).unwrap_err();
        assert!(err.to_string().contains("line 2"));
    }

    #[test]
    fn test_injection_key_rejected() {
        let input = "x; rm -rf /=val";
        assert!(parse_env_contents(input).is_err());
    }

    #[test]
    fn test_missing_equals() {
        let input = "JUST_A_KEY";
        let err = parse_env_contents(input).unwrap_err();
        assert!(err.to_string().contains("invalid format"));
    }

    #[test]
    fn test_dollar_in_key_rejected() {
        let input = "$FOO=bar";
        assert!(parse_env_contents(input).is_err());
    }

    #[test]
    fn test_shell_expression_not_evaluated() {
        // Shell expressions are stored literally, not evaluated
        let input = "FOO=$(whoami)\nBAR=`hostname`";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "$(whoami)");
        assert_eq!(env.get("BAR").unwrap(), "`hostname`");
    }

    #[test]
    fn test_strip_quotes_mismatched() {
        // Mismatched quotes (opening without closing) are now detected as errors
        let input = "FOO=\"bar'";
        let err = parse_env_contents(input).unwrap_err();
        assert!(err.to_string().contains("unclosed quote"));
    }

    #[test]
    fn test_utf8_bom_stripped() {
        let input = "\u{feff}FOO=bar\nBAZ=qux";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "bar");
        assert_eq!(env.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn test_inline_comments_stripped() {
        let input = "FOO=bar # this is a comment\nBAZ=qux";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "bar");
        assert_eq!(env.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn test_inline_comment_preserved_in_quotes() {
        let input = "FOO=\"value # not a comment\"";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "value # not a comment");
    }

    #[test]
    fn test_hash_without_space_not_stripped() {
        let input = "FOO=bar#baz";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "bar#baz");
    }

    #[test]
    fn test_unclosed_double_quote() {
        // FOO="hello # world — missing closing quote should error, not silently truncate
        let input = r#"FOO="hello # world"#;
        let err = parse_env_contents(input).unwrap_err();
        assert!(err.to_string().contains("unclosed quote"));
    }

    #[test]
    fn test_unclosed_single_quote() {
        let input = "FOO='hello # world";
        let err = parse_env_contents(input).unwrap_err();
        assert!(err.to_string().contains("unclosed quote"));
    }

    #[test]
    fn test_quoted_value_with_trailing_comment() {
        // Quoted value followed by an inline comment outside the quotes
        let input = r#"FOO="value" # trailing comment"#;
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "value");
    }

    #[test]
    fn test_single_quote_char_not_treated_as_quoted() {
        // A lone quote character should not be treated as a quoted value
        let input = "FOO=\"";
        let err = parse_env_contents(input).unwrap_err();
        assert!(err.to_string().contains("unclosed quote"));
    }

    #[test]
    fn test_empty_value_with_trailing_comment() {
        // FOO= # comment should yield empty string, not "# comment"
        let input = "FOO= # comment\nBAR=val # note";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "");
        assert_eq!(env.get("BAR").unwrap(), "val");
    }

    #[test]
    fn test_quoted_value_with_hash_inside_and_trailing_comment() {
        // FOO="val # ue" # comment → value should be `val # ue`
        let input = r#"FOO="val # ue" # comment"#;
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "val # ue");
    }

    #[test]
    fn test_single_quoted_value_with_hash_inside_and_trailing_comment() {
        let input = "FOO='val # ue' # comment";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "val # ue");
    }

    #[test]
    fn test_duplicate_keys_last_wins() {
        let input = "FOO=first\nFOO=second";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "second");
    }

    #[test]
    fn test_windows_line_endings() {
        let input = "FOO=bar\r\nBAZ=qux\r\n";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "bar");
        assert_eq!(env.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn test_max_entries_exceeded() {
        let lines: Vec<String> = (0..257).map(|i| format!("K{}=v{}", i, i)).collect();
        let input = lines.join("\n");
        let err = parse_env_contents(&input).unwrap_err();
        assert!(err.to_string().contains("maximum of 256 entries"));
    }

    #[test]
    fn test_max_entries_at_limit_succeeds() {
        let lines: Vec<String> = (0..256).map(|i| format!("K{}=v{}", i, i)).collect();
        let input = lines.join("\n");
        let env = parse_env_contents(&input).unwrap();
        assert_eq!(env.len(), 256);
    }

    #[test]
    fn test_max_entries_duplicate_keys_do_not_bypass_limit() {
        // 256 unique keys fill the limit, then a 257th *new* key should fail
        // even if interspersed with duplicate-key reassignments.
        let mut lines: Vec<String> = (0..256).map(|i| format!("K{}=v{}", i, i)).collect();
        // Reassign existing keys (should be allowed — map stays at 256)
        lines.push("K0=override".to_string());
        lines.push("K1=override".to_string());
        let input = lines.join("\n");
        let env = parse_env_contents(&input).unwrap();
        assert_eq!(env.len(), 256);
        assert_eq!(env.get("K0").unwrap(), "override");

        // Now add a truly new key — should fail
        lines.push("KNEW=boom".to_string());
        let input = lines.join("\n");
        let err = parse_env_contents(&input).unwrap_err();
        assert!(err.to_string().contains("maximum of 256 entries"));
    }

    #[test]
    fn test_parse_env_file_nonexistent() {
        let result = parse_env_file(Path::new("/nonexistent/.dispatch/env"));
        assert!(result.is_err());
    }
}
