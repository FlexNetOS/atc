use anyhow::Result;
use atc_core::registry::Registry;
use atc_core::terminal_text::display_text;
use atc_core::types::DispatchRecord;

/// Resolve a dispatch record by ID or task slug.
/// Tries get(arg) first (by ID), then find_latest_for_task(arg) (by slug).
pub async fn resolve_record(registry: &dyn Registry, arg: &str) -> Result<DispatchRecord> {
    if let Some(record) = registry.get(arg).await? {
        return Ok(record);
    }
    if let Some(record) = registry.find_latest_for_task(arg).await? {
        return Ok(record);
    }
    anyhow::bail!("no dispatch record found for: {}", display_text(arg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockRegistry;

    #[tokio::test]
    async fn resolve_record_missing_arg_escapes_terminal_controls() {
        let registry = MockRegistry::new(Vec::new());
        let err = resolve_record(&registry, "missing-\x1b[2J\u{202e}gpj")
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("missing-\\x1b[2J\\u{202e}gpj"));
        assert!(!err.contains('\x1b'));
        assert!(!err.contains('\u{202e}'));
    }
}
