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
pub fn parse_env_file(path: &Path) -> Result<HashMap<String, String>> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {}", path.display(), e))?;
    parse_env_contents(&contents)
}

/// Parse env file contents (separated for testability).
pub fn parse_env_contents(contents: &str) -> Result<HashMap<String, String>> {
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

        let value = strip_quotes(value.trim());
        env.insert(key.to_string(), value);
    }

    Ok(env)
}

/// Strip matching outer quotes (single or double) from a value.
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
        // Mismatched quotes are kept as-is
        let input = "FOO=\"bar'";
        let env = parse_env_contents(input).unwrap();
        assert_eq!(env.get("FOO").unwrap(), "\"bar'");
    }

    #[test]
    fn test_parse_env_file_nonexistent() {
        let result = parse_env_file(Path::new("/nonexistent/.dispatch/env"));
        assert!(result.is_err());
    }
}
