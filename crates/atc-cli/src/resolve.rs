use anyhow::Result;
use atc_core::registry::Registry;
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
    anyhow::bail!("no dispatch record found for: {arg}")
}
