use anyhow::Result;
use async_trait::async_trait;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::types::{
    Directive, DispatchRecord, HealthChecks, Status, TerminalLocator, CLAUDE_AGENT_PROVIDER,
};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
        terminal_locator: None,
        dispatched_at: now,
        updated_at: now,
    }
}

pub(crate) struct MockRegistry {
    records: Mutex<Vec<DispatchRecord>>,
}

impl MockRegistry {
    pub(crate) fn new(records: Vec<DispatchRecord>) -> Self {
        Self {
            records: Mutex::new(records),
        }
    }

    fn with_record_mut<T>(&self, id: &str, f: impl FnOnce(&mut DispatchRecord) -> T) -> Result<T> {
        let mut records = self.records.lock().unwrap();
        let record = records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or_else(|| anyhow::anyhow!("no dispatch record found for id: {id}"))?;
        Ok(f(record))
    }
}

#[async_trait]
impl Registry for MockRegistry {
    async fn insert(&self, record: &DispatchRecord) -> Result<()> {
        self.records.lock().unwrap().push(record.clone());
        Ok(())
    }

    async fn insert_resume_reservation(&self, record: &DispatchRecord, _force: bool) -> Result<()> {
        self.insert(record).await
    }

    async fn update_status(&self, id: &str, status: Status) -> Result<()> {
        self.with_record_mut(id, |record| {
            record.status = status;
            record.updated_at = Utc::now();
        })
    }

    async fn update_session_locator(
        &self,
        id: &str,
        session: &str,
        terminal_locator: Option<&TerminalLocator>,
    ) -> Result<()> {
        self.with_record_mut(id, |record| {
            record.session = session.to_string();
            record.terminal_locator = terminal_locator.cloned();
            record.updated_at = Utc::now();
        })
    }

    async fn update_dispatch_work_unit(&self, id: &str, work_unit_id: Option<&str>) -> Result<()> {
        self.with_record_mut(id, |record| {
            record.work_unit_id = work_unit_id.map(str::to_string);
            record.updated_at = Utc::now();
        })
    }

    async fn update_cost(&self, id: &str, cost: f64, turns: u32, duration_ms: u64) -> Result<()> {
        self.with_record_mut(id, |record| {
            record.cost_usd = Some(cost);
            record.num_turns = Some(turns);
            record.duration_ms = Some(duration_ms);
            record.updated_at = Utc::now();
        })
    }

    async fn get(&self, id: &str) -> Result<Option<DispatchRecord>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .find(|record| record.id == id)
            .cloned())
    }

    async fn list(&self, filter: StatusFilter) -> Result<Vec<DispatchRecord>> {
        let records = self.records.lock().unwrap();
        let mut records = match filter {
            StatusFilter::All => records.clone(),
            StatusFilter::One(status) => records
                .iter()
                .filter(|record| record.status == status)
                .cloned()
                .collect(),
            StatusFilter::Any(ref statuses) => records
                .iter()
                .filter(|record| statuses.contains(&record.status))
                .cloned()
                .collect(),
            StatusFilter::AnyOrUpdatedSince {
                ref statuses,
                updated_since,
            } => records
                .iter()
                .filter(|record| {
                    statuses.contains(&record.status) || record.updated_at >= updated_since
                })
                .cloned()
                .collect(),
        };
        records.sort_by(|a, b| {
            b.dispatched_at
                .cmp(&a.dispatched_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(records)
    }

    async fn update_health(
        &self,
        id: &str,
        checks: &HealthChecks,
        status: Status,
        updated_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        self.with_record_mut(id, |record| {
            record.checks = checks.clone();
            record.status = status;
            record.updated_at = updated_at;
        })
    }

    async fn set_pr_url(&self, id: &str, url: &str) -> Result<()> {
        self.with_record_mut(id, |record| {
            record.pr_urls = vec![url.to_string()];
            record.updated_at = Utc::now();
        })
    }

    async fn add_pr_url(&self, id: &str, url: &str) -> Result<()> {
        self.with_record_mut(id, |record| {
            let url = url.to_string();
            if !record.pr_urls.contains(&url) {
                record.pr_urls.push(url);
            }
            record.updated_at = Utc::now();
        })
    }

    async fn increment_retries(
        &self,
        id: &str,
        new_session: &str,
        new_log_file: &Path,
        new_dispatched_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        self.with_record_mut(id, |record| {
            record.retries += 1;
            record.session = new_session.to_string();
            record.log_file = new_log_file.to_path_buf();
            record.dispatched_at = new_dispatched_at;
            record.updated_at = new_dispatched_at;
            record.status = Status::Running;
            record.checks = HealthChecks::default();
            record.pr_urls.clear();
            record.cost_usd = None;
            record.num_turns = None;
            record.duration_ms = None;
            record.terminal_locator = if new_session.trim().is_empty() {
                None
            } else {
                Some(TerminalLocator::atc_tmux(
                    new_session,
                    Some(record.worktree_path.clone()),
                    new_dispatched_at,
                ))
            };
        })
    }

    async fn set_artifacts(&self, id: &str, artifacts_json: &str) -> Result<()> {
        self.with_record_mut(id, |record| {
            record.artifacts = Some(artifacts_json.to_string());
            record.updated_at = Utc::now();
        })
    }

    async fn find_by_branch(&self, branch: &str) -> Result<Vec<DispatchRecord>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.branch == branch)
            .cloned()
            .collect())
    }

    async fn find_by_task_slug(&self, task_slug: &str) -> Result<Vec<DispatchRecord>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.task_slug.as_deref() == Some(task_slug))
            .cloned()
            .collect())
    }

    async fn find_by_pr_url(&self, pr_url: &str) -> Result<Vec<DispatchRecord>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.pr_urls.iter().any(|url| url == pr_url))
            .cloned()
            .collect())
    }

    async fn find_by_worktree(&self, worktree_path: &Path) -> Result<Vec<DispatchRecord>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.worktree_path == worktree_path)
            .cloned()
            .collect())
    }

    async fn find_latest_for_task(&self, task_slug: &str) -> Result<Option<DispatchRecord>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.task_slug.as_deref() == Some(task_slug))
            .max_by_key(|record| record.dispatched_at)
            .cloned())
    }

    async fn find_running_on_worktree(&self, worktree_path: &Path) -> Result<Vec<DispatchRecord>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| {
                record.worktree_path == worktree_path && record.status == Status::Running
            })
            .cloned()
            .collect())
    }
}
