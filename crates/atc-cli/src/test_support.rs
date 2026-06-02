use atc_core::types::{Directive, DispatchRecord, HealthChecks, Status, CLAUDE_AGENT_PROVIDER};
use chrono::Utc;
use std::path::PathBuf;

pub(crate) fn dispatch_record_fixture() -> DispatchRecord {
    let now = Utc::now();
    DispatchRecord {
        id: String::new(),
        task_slug: None,
        branch: String::new(),
        worktree_path: PathBuf::new(),
        session: String::new(),
        log_file: PathBuf::new(),
        status: Status::Running,
        directive: Directive::Implement,
        retries: 0,
        resolver: String::new(),
        pr_urls: Vec::new(),
        no_worktree: false,
        original_input: None,
        checks: HealthChecks::default(),
        kb_root: None,
        cost_usd: None,
        num_turns: None,
        duration_ms: None,
        artifacts: None,
        work_unit_id: None,
        agent_provider: CLAUDE_AGENT_PROVIDER.to_string(),
        agent_session_id: None,
        agent_transcript_cwd: None,
        resume_of_dispatch_id: None,
        agent_capabilities: None,
        dispatched_at: now,
        updated_at: now,
    }
}
