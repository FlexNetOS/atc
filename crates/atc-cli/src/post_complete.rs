//! `atc post-complete` — Run post-completion pipeline for a dispatch.
//!
//! Called automatically by the tmux pipeline after the agent exits,
//! or manually by the user for recovery.

use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::post_completion::{self, PostCompleteInput};
use atc_core::registry::Registry;
use std::path::PathBuf;

/// Run the `atc post-complete` command.
///
/// If `--id` is provided, uses that dispatch ID. Otherwise, finds the most
/// recent Running dispatch.
///
/// If `--exit-code` is not provided, infers from the result event in the log.
///
/// If `--log` is not provided, resolves from the registry record.
pub async fn run_post_complete(
    config: &AtcConfig,
    registry: &dyn Registry,
    id: Option<&str>,
    exit_code: Option<i32>,
    log_file: Option<PathBuf>,
) -> Result<()> {
    // Resolve dispatch ID
    let dispatch_id = match id {
        Some(id) => id.to_string(),
        None => {
            // Find most recent Running dispatch
            let records = registry
                .list(atc_core::registry::StatusFilter::by_status(
                    atc_core::types::Status::Running,
                ))
                .await?;
            records
                .first()
                .map(|r| r.id.clone())
                .ok_or_else(|| anyhow::anyhow!("no running dispatches found; specify --id"))?
        }
    };

    let input = PostCompleteInput {
        dispatch_id: dispatch_id.clone(),
        exit_code,
        log_file,
        skip_cleanup: false,
    };

    let result = post_completion::run_post_completion(&input, registry, config).await?;

    eprintln!(
        "post-complete: {} → {} (PR: {})",
        dispatch_id,
        result.status,
        result.pr_url.as_deref().unwrap_or("none"),
    );

    Ok(())
}
