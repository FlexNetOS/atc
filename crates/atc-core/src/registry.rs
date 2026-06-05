use crate::queue::{
    DispatchQueue, EnqueueItem, EnqueueResult, QueueInputType, QueueItemStatus, QueueRow,
};
use crate::types::{
    claude_agent_capabilities, AgentCapabilities, AgentSessionId, DispatchRecord, HealthChecks,
    Status, TerminalLocator, CLAUDE_AGENT_PROVIDER,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use tracing::warn;

const ACTIVE_DISPATCH_STATUSES: &[Status] = &[Status::Running, Status::Retrying];
const ACTIVE_DISPATCH_STATUS_SQL: &str = "status IN ('running', 'retrying')";

fn is_active_dispatch_status(status: Status) -> bool {
    ACTIVE_DISPATCH_STATUSES.contains(&status)
}

fn active_agent_session_index_sql() -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS idx_dispatches_active_agent_session \
         ON dispatches(agent_provider, agent_session_id, dispatched_at DESC, id DESC) \
         WHERE {ACTIVE_DISPATCH_STATUS_SQL} AND agent_session_id IS NOT NULL;"
    )
}

fn active_agent_session_query_sql(select: &str) -> String {
    format!(
        "{select}
         WHERE agent_provider = ?1
           AND agent_session_id = ?2
           AND {ACTIVE_DISPATCH_STATUS_SQL}
         ORDER BY dispatched_at DESC, id DESC
         LIMIT 1"
    )
}

fn active_task_count_query_sql() -> String {
    format!("SELECT COUNT(*) FROM dispatches WHERE task_slug = ?1 AND {ACTIVE_DISPATCH_STATUS_SQL}")
}

/// Filter passed to `Registry::list`.
#[derive(Debug, Default)]
pub enum StatusFilter {
    /// No filter — return all records.
    #[default]
    All,
    /// Exactly one status.
    One(Status),
    /// Any of the given statuses (generates `WHERE status IN (...)`).
    Any(Vec<Status>),
    /// Records with any of the given statuses, plus records updated at or after
    /// the timestamp. Useful for live views that show active records and a
    /// bounded recent tail without loading the entire registry.
    AnyOrUpdatedSince {
        statuses: Vec<Status>,
        updated_since: DateTime<Utc>,
    },
}

impl StatusFilter {
    pub fn all() -> Self {
        Self::All
    }
    pub fn by_status(status: Status) -> Self {
        Self::One(status)
    }
    pub fn any(statuses: Vec<Status>) -> Self {
        Self::Any(statuses)
    }
    pub fn any_or_updated_since(statuses: Vec<Status>, updated_since: DateTime<Utc>) -> Self {
        Self::AnyOrUpdatedSince {
            statuses,
            updated_since,
        }
    }
}

#[async_trait]
pub trait Registry: Send + Sync {
    async fn insert(&self, record: &DispatchRecord) -> Result<()>;
    /// Insert a resumed dispatch as a pre-spawn reservation.
    ///
    /// Implementations with concurrent writers must make the active-session
    /// check and insert atomic when `force` is false.
    async fn insert_resume_reservation(&self, record: &DispatchRecord, force: bool) -> Result<()>;
    /// Find a non-terminal dispatch using the provider-native session.
    async fn find_active_by_agent_session(
        &self,
        provider: &str,
        session_id: AgentSessionId,
    ) -> Result<Option<DispatchRecord>> {
        Ok(self
            .list(StatusFilter::All)
            .await?
            .into_iter()
            .find(|record| {
                record.agent_provider == provider
                    && record.agent_session_id == Some(session_id)
                    && is_active_dispatch_status(record.status)
            }))
    }
    async fn update_status(&self, id: &str, status: Status) -> Result<()>;
    async fn update_session_locator(
        &self,
        id: &str,
        session: &str,
        terminal_locator: Option<&TerminalLocator>,
    ) -> Result<()>;
    async fn update_dispatch_work_unit(
        &self,
        _id: &str,
        _work_unit_id: Option<&str>,
    ) -> Result<()> {
        anyhow::bail!("dispatch work-unit updates are not implemented for this registry backend")
    }
    async fn update_cost(&self, id: &str, cost: f64, turns: u32, duration_ms: u64) -> Result<()>;
    async fn get(&self, id: &str) -> Result<Option<DispatchRecord>>;
    async fn list(&self, filter: StatusFilter) -> Result<Vec<DispatchRecord>>;
    /// Atomically update health checks, status, and updated_at in a single write.
    async fn update_health(
        &self,
        id: &str,
        checks: &HealthChecks,
        status: Status,
        updated_at: DateTime<Utc>,
    ) -> Result<()>;
    async fn set_pr_url(&self, id: &str, url: &str) -> Result<()>;
    /// Append a PR URL to the JSON array of tracked PR URLs.
    /// Deduplicates: if the URL is already present, this is a no-op.
    async fn add_pr_url(&self, id: &str, url: &str) -> Result<()>;
    async fn increment_retries(
        &self,
        id: &str,
        new_session: &str,
        new_log_file: &Path,
        new_dispatched_at: DateTime<Utc>,
    ) -> Result<()>;

    /// Store full artifacts JSON blob.
    async fn set_artifacts(&self, _id: &str, _artifacts_json: &str) -> Result<()> {
        anyhow::bail!("artifacts persistence is not implemented for this registry backend")
    }

    // --- Work unit methods ---
    async fn insert_work_unit(&self, _unit: &crate::types::WorkUnit) -> Result<()> {
        anyhow::bail!("work units not implemented for this registry backend")
    }
    async fn get_work_unit(&self, _id: &str) -> Result<Option<crate::types::WorkUnit>> {
        Ok(None)
    }
    async fn find_work_unit_by_task(
        &self,
        _task_slug: &str,
    ) -> Result<Option<crate::types::WorkUnit>> {
        Ok(None)
    }
    async fn find_work_unit_by_branch(
        &self,
        _branch: &str,
    ) -> Result<Option<crate::types::WorkUnit>> {
        Ok(None)
    }
    async fn find_work_unit_by_pr(&self, _pr_url: &str) -> Result<Option<crate::types::WorkUnit>> {
        Ok(None)
    }
    async fn update_work_unit_status(
        &self,
        _id: &str,
        _status: crate::types::WorkUnitStatus,
    ) -> Result<()> {
        anyhow::bail!("work units not implemented for this registry backend")
    }
    /// Atomically update work unit status only if no non-terminal dispatches exist.
    /// Returns Ok(true) if status was updated, Ok(false) if a live dispatch blocked it.
    async fn update_work_unit_status_if_idle(
        &self,
        _id: &str,
        _status: crate::types::WorkUnitStatus,
    ) -> Result<bool> {
        anyhow::bail!("work units not implemented for this registry backend")
    }
    async fn add_work_unit_pr(&self, _id: &str, _pr_url: &str) -> Result<()> {
        anyhow::bail!("work units not implemented for this registry backend")
    }
    async fn add_work_unit_repo(&self, _id: &str, _repo_path: &str) -> Result<()> {
        anyhow::bail!("work units not implemented for this registry backend")
    }
    /// Promote a work unit by setting its task_slug (e.g., when a branch-only unit
    /// is later associated with a task).
    async fn update_work_unit_task_slug(&self, _id: &str, _task_slug: &str) -> Result<()> {
        anyhow::bail!("work units not implemented for this registry backend")
    }
    async fn list_work_units(&self) -> Result<Vec<crate::types::WorkUnit>> {
        Ok(Vec::new())
    }
    /// List only the requested work units. Default implementations may scan
    /// `list_work_units`; SQLite overrides this to keep live views bounded.
    async fn list_work_units_by_ids(&self, ids: &[String]) -> Result<Vec<crate::types::WorkUnit>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
        Ok(self
            .list_work_units()
            .await?
            .into_iter()
            .filter(|unit| wanted.contains(unit.id.as_str()))
            .collect())
    }
    async fn list_active_work_units(&self) -> Result<Vec<crate::types::WorkUnit>> {
        Ok(Vec::new())
    }
    /// Find work unit by task slug across all statuses (for history lookups).
    async fn find_work_unit_by_task_any_status(
        &self,
        _task_slug: &str,
    ) -> Result<Option<crate::types::WorkUnit>> {
        Ok(None)
    }
    /// Find work unit by branch across all statuses (for history lookups).
    async fn find_work_unit_by_branch_any_status(
        &self,
        _branch: &str,
    ) -> Result<Option<crate::types::WorkUnit>> {
        Ok(None)
    }
    async fn list_dispatches_for_work_unit(
        &self,
        _work_unit_id: &str,
    ) -> Result<Vec<DispatchRecord>> {
        Ok(Vec::new())
    }

    // --- New query methods ---
    async fn find_by_branch(&self, branch: &str) -> Result<Vec<DispatchRecord>>;
    async fn find_by_task_slug(&self, task_slug: &str) -> Result<Vec<DispatchRecord>>;
    async fn find_by_pr_url(&self, pr_url: &str) -> Result<Vec<DispatchRecord>>;
    async fn find_by_worktree(&self, worktree_path: &Path) -> Result<Vec<DispatchRecord>>;
    async fn find_latest_for_task(&self, task_slug: &str) -> Result<Option<DispatchRecord>>;
    async fn find_running_on_worktree(&self, worktree_path: &Path) -> Result<Vec<DispatchRecord>>;
}

pub struct SqliteRegistry {
    pool: sqlx::SqlitePool,
}

const CREATE_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS dispatches (
  id                        TEXT PRIMARY KEY,
  task_slug                 TEXT,
  branch                    TEXT NOT NULL,
  worktree_path             TEXT NOT NULL,
  session                   TEXT NOT NULL,
  log_file                  TEXT NOT NULL,
  status                    TEXT NOT NULL DEFAULT 'running',
  directive                 TEXT NOT NULL,
  retries                   INTEGER NOT NULL DEFAULT 0,
  resolver                  TEXT NOT NULL,
  pr_url                    TEXT,
  pr_urls                   TEXT NOT NULL DEFAULT '[]',
  no_worktree               INTEGER NOT NULL DEFAULT 0,
  original_input            TEXT,
  kb_root                   TEXT,
  check_agent_exited_clean  INTEGER NOT NULL DEFAULT 0,
  check_branch_pushed       INTEGER NOT NULL DEFAULT 0,
  check_pr_created          INTEGER NOT NULL DEFAULT 0,
  check_ci_passed           INTEGER NOT NULL DEFAULT 0,
  check_reviews_approved    INTEGER NOT NULL DEFAULT 0,
  check_threads_resolved    INTEGER NOT NULL DEFAULT 0,
  cost_usd                  REAL,
  num_turns                 INTEGER,
  duration_ms               INTEGER,
  artifacts                 TEXT,
  work_unit_id              TEXT,
  agent_provider            TEXT NOT NULL DEFAULT 'claude',
  agent_session_id          TEXT,
  agent_transcript_cwd      TEXT,
  resume_of_dispatch_id     TEXT,
  agent_capabilities_json   TEXT,
  terminal_locator_json     TEXT,
  dispatched_at             TEXT NOT NULL,
  updated_at                TEXT NOT NULL
);
"#;

const CREATE_INDEXES_SQL: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_dispatches_status ON dispatches(status);",
    "CREATE INDEX IF NOT EXISTS idx_dispatches_task_slug ON dispatches(task_slug);",
    "CREATE INDEX IF NOT EXISTS idx_dispatches_branch ON dispatches(branch);",
    "CREATE INDEX IF NOT EXISTS idx_dispatches_worktree ON dispatches(worktree_path);",
    "CREATE INDEX IF NOT EXISTS idx_dispatches_pr_url ON dispatches(pr_url);",
    "CREATE INDEX IF NOT EXISTS idx_dispatches_updated_at ON dispatches(updated_at);",
];

const CREATE_WORK_UNITS_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS work_units (
    id          TEXT PRIMARY KEY,
    task_slug   TEXT,
    branch      TEXT,
    repos       TEXT NOT NULL DEFAULT '[]',
    pr_urls     TEXT NOT NULL DEFAULT '[]',
    status      TEXT NOT NULL DEFAULT 'active',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
"#;

const CREATE_WORK_UNITS_INDEXES_SQL: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_work_units_task ON work_units(task_slug);",
    "CREATE INDEX IF NOT EXISTS idx_work_units_branch ON work_units(branch);",
    "CREATE INDEX IF NOT EXISTS idx_work_units_status ON work_units(status);",
    "CREATE INDEX IF NOT EXISTS idx_dispatches_work_unit ON dispatches(work_unit_id);",
    // Enforce at most one active work unit per (task_slug, branch) pair.
    // This prevents TOCTOU races where two concurrent resolve_work_unit calls
    // both observe "no active unit" and both insert.
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_work_units_active_task ON work_units(task_slug) WHERE status = 'active' AND task_slug IS NOT NULL;",
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_work_units_active_branch ON work_units(branch) WHERE status = 'active' AND branch IS NOT NULL;",
];

const CREATE_QUEUE_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS dispatch_queue (
    id          TEXT PRIMARY KEY,
    queue_name  TEXT NOT NULL DEFAULT 'default',
    input_type  TEXT NOT NULL,
    input_value TEXT NOT NULL,
    mode        TEXT,
    priority    INTEGER NOT NULL DEFAULT 50,
    params      TEXT,
    status      TEXT NOT NULL DEFAULT 'pending',
    dispatch_id TEXT,
    enqueued_at TEXT NOT NULL,
    enqueued_by TEXT,
    claimed_at  TEXT,
    dispatched_at TEXT,
    error       TEXT
);
"#;

const CREATE_QUEUE_INDEXES_SQL: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_queue_pending ON dispatch_queue(queue_name, status, priority, enqueued_at);",
];

impl SqliteRegistry {
    /// Apply DDL (create table + indexes) to the pool.
    async fn apply_ddl(pool: &sqlx::SqlitePool) -> Result<()> {
        sqlx::query(CREATE_TABLE_SQL).execute(pool).await?;
        for idx_sql in CREATE_INDEXES_SQL {
            sqlx::query(idx_sql).execute(pool).await?;
        }
        let active_session_index_sql = active_agent_session_index_sql();
        sqlx::query(&active_session_index_sql).execute(pool).await?;
        // Work units table
        sqlx::query(CREATE_WORK_UNITS_TABLE_SQL)
            .execute(pool)
            .await?;
        for idx_sql in CREATE_WORK_UNITS_INDEXES_SQL {
            sqlx::query(idx_sql).execute(pool).await?;
        }
        // Queue table
        sqlx::query(CREATE_QUEUE_TABLE_SQL).execute(pool).await?;
        for idx_sql in CREATE_QUEUE_INDEXES_SQL {
            sqlx::query(idx_sql).execute(pool).await?;
        }
        Ok(())
    }

    /// Expose the pool for DispatchQueue impl.
    pub fn pool(&self) -> &sqlx::SqlitePool {
        &self.pool
    }

    /// Migrate from old schema (slug PK) to new schema (id PK).
    /// DROP + CREATE is fine — no production state to preserve.
    async fn migrate_if_needed(pool: &sqlx::SqlitePool) -> Result<()> {
        // Check if old schema exists (has 'slug' column but no 'id' column)
        let row: Option<(i32,)> = sqlx::query_as(
            "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = 'slug'",
        )
        .fetch_optional(pool)
        .await?;

        if let Some((count,)) = row {
            if count > 0 {
                // Old schema detected, drop and recreate
                sqlx::query("DROP TABLE IF EXISTS dispatches")
                    .execute(pool)
                    .await?;
            }
        }

        // Add artifacts TEXT column if missing (Phase 2 migration — safe ALTER TABLE ADD COLUMN)
        let (table_exists,): (i32,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'dispatches'",
        )
        .fetch_one(pool)
        .await?;
        if table_exists > 0 {
            let (has_artifacts,): (i32,) = sqlx::query_as(
                "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = 'artifacts'",
            )
            .fetch_one(pool)
            .await?;
            if has_artifacts == 0 {
                sqlx::query("ALTER TABLE dispatches ADD COLUMN artifacts TEXT")
                    .execute(pool)
                    .await?;
            }

            // Add no_worktree column if missing (Phase 4B migration)
            let (has_no_worktree,): (i32,) = sqlx::query_as(
                "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = 'no_worktree'",
            )
            .fetch_one(pool)
            .await?;
            if has_no_worktree == 0 {
                sqlx::query(
                    "ALTER TABLE dispatches ADD COLUMN no_worktree INTEGER NOT NULL DEFAULT 0",
                )
                .execute(pool)
                .await?;
            }

            // Add original_input column if missing (retry fidelity migration)
            let (has_original_input,): (i32,) = sqlx::query_as(
                "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = 'original_input'",
            )
            .fetch_one(pool)
            .await?;
            if has_original_input == 0 {
                sqlx::query("ALTER TABLE dispatches ADD COLUMN original_input TEXT")
                    .execute(pool)
                    .await?;
            }

            // Add kb_root column if missing (multi-KB discovery migration)
            let (has_kb_root,): (i32,) = sqlx::query_as(
                "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = 'kb_root'",
            )
            .fetch_one(pool)
            .await?;
            if has_kb_root == 0 {
                sqlx::query("ALTER TABLE dispatches ADD COLUMN kb_root TEXT")
                    .execute(pool)
                    .await?;
            }

            // Add pr_urls JSON array column if missing (multi-repo PR tracking migration)
            let (has_pr_urls,): (i32,) = sqlx::query_as(
                "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = 'pr_urls'",
            )
            .fetch_one(pool)
            .await?;
            if has_pr_urls == 0 {
                sqlx::query("ALTER TABLE dispatches ADD COLUMN pr_urls TEXT NOT NULL DEFAULT '[]'")
                    .execute(pool)
                    .await?;
            }
            // Always backfill: handles crash between ALTER TABLE and UPDATE on first run,
            // and is a no-op when all rows are already populated.
            sqlx::query(
                "UPDATE dispatches
                 SET pr_urls = json_array(pr_url)
                 WHERE pr_url IS NOT NULL
                   AND (pr_urls IS NULL OR pr_urls = '[]')",
            )
            .execute(pool)
            .await?;

            // Rename mode → directive column (Mode→Directive migration)
            let (has_mode_col,): (i32,) = sqlx::query_as(
                "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = 'mode'",
            )
            .fetch_one(pool)
            .await?;
            let (has_directive_col,): (i32,) = sqlx::query_as(
                "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = 'directive'",
            )
            .fetch_one(pool)
            .await?;
            if has_mode_col > 0 && has_directive_col == 0 {
                sqlx::query("ALTER TABLE dispatches RENAME COLUMN mode TO directive")
                    .execute(pool)
                    .await?;
            }

            // Add work_unit_id column if missing (work unit grouping migration)
            let (has_work_unit_id,): (i32,) = sqlx::query_as(
                "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = 'work_unit_id'",
            )
            .fetch_one(pool)
            .await?;
            if has_work_unit_id == 0 {
                sqlx::query("ALTER TABLE dispatches ADD COLUMN work_unit_id TEXT")
                    .execute(pool)
                    .await?;
            }

            let agent_columns = [
                (
                    "agent_provider",
                    "ALTER TABLE dispatches ADD COLUMN agent_provider TEXT NOT NULL DEFAULT 'claude'",
                ),
                (
                    "agent_session_id",
                    "ALTER TABLE dispatches ADD COLUMN agent_session_id TEXT",
                ),
                (
                    "agent_transcript_cwd",
                    "ALTER TABLE dispatches ADD COLUMN agent_transcript_cwd TEXT",
                ),
                (
                    "resume_of_dispatch_id",
                    "ALTER TABLE dispatches ADD COLUMN resume_of_dispatch_id TEXT",
                ),
                (
                    "agent_capabilities_json",
                    "ALTER TABLE dispatches ADD COLUMN agent_capabilities_json TEXT",
                ),
            ];
            for (column, ddl) in agent_columns {
                let (has_column,): (i32,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = ?1",
                )
                .bind(column)
                .fetch_one(pool)
                .await?;
                if has_column == 0 {
                    sqlx::query(ddl).execute(pool).await?;
                }
            }

            let (has_terminal_locator,): (i32,) = sqlx::query_as(
                "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = 'terminal_locator_json'",
            )
            .fetch_one(pool)
            .await?;
            if has_terminal_locator == 0 {
                sqlx::query("ALTER TABLE dispatches ADD COLUMN terminal_locator_json TEXT")
                    .execute(pool)
                    .await?;
            }

            // Backfill provider capability snapshots for legacy Claude rows.
            // Capabilities describe the provider contract rather than per-run
            // state; per-action checks still require the needed session/log data.
            let claude_capabilities_json = serde_json::to_string(&claude_agent_capabilities())?;
            sqlx::query(
                "UPDATE dispatches
                 SET agent_capabilities_json = ?1
                 WHERE agent_provider = ?2
                   AND (agent_capabilities_json IS NULL OR trim(agent_capabilities_json) = '')",
            )
            .bind(&claude_capabilities_json)
            .bind(CLAUDE_AGENT_PROVIDER)
            .execute(pool)
            .await?;
        }

        Ok(())
    }

    /// Open (or create) the SQLite database at `path`.
    /// Applies DDL on first open. Enables WAL mode on every open.
    pub async fn open(path: &std::path::Path) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        let url = format!("sqlite:{}?mode=rwc", path.display());
        let pool = sqlx::SqlitePool::connect(&url).await?;

        let mode: (String,) = sqlx::query_as("PRAGMA journal_mode=WAL")
            .fetch_one(&pool)
            .await?;
        anyhow::ensure!(
            mode.0 == "wal",
            "failed to enable WAL mode, got: {}",
            mode.0
        );

        Self::migrate_if_needed(&pool).await?;
        Self::apply_ddl(&pool).await?;

        Ok(Self { pool })
    }

    /// In-memory instance for unit tests.
    /// Uses a shared-cache URI so all pool connections share the same database.
    /// Each call gets a unique name to avoid cross-test interference.
    pub async fn in_memory() -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let url = format!("sqlite:file:atc_test_{id}?mode=memory&cache=shared");
        let pool = sqlx::SqlitePool::connect(&url).await?;

        // WAL mode is not supported for in-memory databases; skip verification
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await?;
        Self::apply_ddl(&pool).await?;

        Ok(Self { pool })
    }

    async fn insert_dispatch<'e, E>(
        executor: E,
        record: &DispatchRecord,
    ) -> Result<sqlx::sqlite::SqliteQueryResult>
    where
        E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
    {
        let pr_urls_json = serde_json::to_string(&record.pr_urls)?;
        let pr_url_compat = record.pr_urls.first().cloned();
        let agent_capabilities_json = record
            .agent_capabilities
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let terminal_locator_json = record
            .terminal_locator
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let worktree_path = record
            .worktree_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("worktree_path must be valid UTF-8"))?;
        let log_file = record
            .log_file
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("log_file must be valid UTF-8"))?;
        let kb_root = record
            .kb_root
            .as_ref()
            .map(|p| {
                p.to_str()
                    .ok_or_else(|| anyhow::anyhow!("kb_root must be valid UTF-8"))
            })
            .transpose()?;
        let agent_session_id = record.agent_session_id.map(|id| id.to_string());
        let agent_transcript_cwd = record
            .agent_transcript_cwd
            .as_ref()
            .map(|p| {
                p.to_str()
                    .ok_or_else(|| anyhow::anyhow!("agent_transcript_cwd must be valid UTF-8"))
            })
            .transpose()?;

        let result = sqlx::query(
            r#"INSERT INTO dispatches (
                id, task_slug, branch, worktree_path, session, log_file, status, directive, retries,
                resolver, pr_url, pr_urls, no_worktree, original_input, kb_root,
                check_agent_exited_clean, check_branch_pushed, check_pr_created,
                check_ci_passed, check_reviews_approved, check_threads_resolved,
                cost_usd, num_turns, duration_ms, work_unit_id,
                agent_provider, agent_session_id, agent_transcript_cwd, resume_of_dispatch_id,
                agent_capabilities_json, terminal_locator_json, artifacts, dispatched_at, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34
            )"#,
        )
        .bind(&record.id)
        .bind(&record.task_slug)
        .bind(&record.branch)
        .bind(worktree_path)
        .bind(&record.session)
        .bind(log_file)
        .bind(record.status.as_str())
        .bind(record.directive.as_str())
        .bind(i32::try_from(record.retries).map_err(|_| anyhow::anyhow!("retries overflows i32"))?)
        .bind(&record.resolver)
        .bind(&pr_url_compat)
        .bind(&pr_urls_json)
        .bind(record.no_worktree as i32)
        .bind(&record.original_input)
        .bind(kb_root)
        .bind(record.checks.agent_exited_clean as i32)
        .bind(record.checks.branch_pushed as i32)
        .bind(record.checks.pr_created as i32)
        .bind(record.checks.ci_passed as i32)
        .bind(record.checks.reviews_approved as i32)
        .bind(record.checks.threads_resolved as i32)
        .bind(record.cost_usd)
        .bind(
            record
                .num_turns
                .map(i32::try_from)
                .transpose()
                .map_err(|_| anyhow::anyhow!("num_turns overflows i32"))?,
        )
        .bind(
            record
                .duration_ms
                .map(i64::try_from)
                .transpose()
                .map_err(|_| anyhow::anyhow!("duration_ms overflows i64"))?,
        )
        .bind(&record.work_unit_id)
        .bind(&record.agent_provider)
        .bind(agent_session_id)
        .bind(agent_transcript_cwd)
        .bind(&record.resume_of_dispatch_id)
        .bind(&agent_capabilities_json)
        .bind(&terminal_locator_json)
        .bind(&record.artifacts)
        .bind(record.dispatched_at.to_rfc3339())
        .bind(record.updated_at.to_rfc3339())
        .execute(executor)
        .await?;

        Ok(result)
    }

    fn row_to_record(row: &sqlx::sqlite::SqliteRow) -> Result<DispatchRecord> {
        use sqlx::Row;

        let status_str: String = row.get("status");
        let directive_str: String = row.get("directive");
        let dispatched_at_str: String = row.get("dispatched_at");
        let updated_at_str: String = row.get("updated_at");
        let worktree_str: String = row.get("worktree_path");
        let log_file_str: String = row.get("log_file");
        let id: String = row.get("id");
        let agent_session_id = match row
            .get::<Option<String>, _>("agent_session_id")
            .as_deref()
            .map(AgentSessionId::parse_str)
            .transpose()
        {
            Ok(session_id) => session_id,
            Err(e) => {
                warn!(dispatch_id = %id, error = %e, "ignoring invalid agent_session_id");
                None
            }
        };
        let agent_capabilities = match row
            .get::<Option<String>, _>("agent_capabilities_json")
            .as_deref()
            .map(serde_json::from_str::<AgentCapabilities>)
            .transpose()
        {
            Ok(capabilities) => capabilities,
            Err(e) => {
                warn!(dispatch_id = %id, error = %e, "ignoring invalid agent_capabilities_json");
                None
            }
        };
        let terminal_locator = match row
            .get::<Option<String>, _>("terminal_locator_json")
            .as_deref()
            .map(serde_json::from_str::<TerminalLocator>)
            .transpose()
        {
            Ok(locator) => locator,
            Err(e) => {
                warn!(dispatch_id = %id, error = %e, "ignoring invalid terminal_locator_json");
                None
            }
        };

        Ok(DispatchRecord {
            id,
            task_slug: row.get("task_slug"),
            branch: row.get("branch"),
            worktree_path: PathBuf::from(worktree_str),
            session: row.get("session"),
            log_file: PathBuf::from(log_file_str),
            status: status_str.parse()?,
            directive: directive_str.parse()?,
            retries: u32::try_from(row.get::<i32, _>("retries"))
                .map_err(|_| anyhow::anyhow!("invalid retries value in database"))?,
            resolver: row.get("resolver"),
            pr_urls: {
                let json_str: String = row.get("pr_urls");
                serde_json::from_str(&json_str).unwrap_or_default()
            },
            no_worktree: row.get::<i32, _>("no_worktree") != 0,
            original_input: row.get("original_input"),
            checks: HealthChecks {
                agent_exited_clean: row.get::<i32, _>("check_agent_exited_clean") != 0,
                branch_pushed: row.get::<i32, _>("check_branch_pushed") != 0,
                pr_created: row.get::<i32, _>("check_pr_created") != 0,
                ci_passed: row.get::<i32, _>("check_ci_passed") != 0,
                reviews_approved: row.get::<i32, _>("check_reviews_approved") != 0,
                threads_resolved: row.get::<i32, _>("check_threads_resolved") != 0,
            },
            kb_root: row.get::<Option<String>, _>("kb_root").map(PathBuf::from),
            cost_usd: row.get("cost_usd"),
            num_turns: row
                .get::<Option<i32>, _>("num_turns")
                .map(u32::try_from)
                .transpose()
                .map_err(|_| anyhow::anyhow!("invalid num_turns value in database"))?,
            duration_ms: row
                .get::<Option<i64>, _>("duration_ms")
                .map(u64::try_from)
                .transpose()
                .map_err(|_| anyhow::anyhow!("invalid duration_ms value in database"))?,
            artifacts: row.get("artifacts"),
            work_unit_id: row.get("work_unit_id"),
            agent_provider: row.get("agent_provider"),
            agent_session_id,
            agent_transcript_cwd: row
                .get::<Option<String>, _>("agent_transcript_cwd")
                .map(PathBuf::from),
            resume_of_dispatch_id: row.get("resume_of_dispatch_id"),
            agent_capabilities,
            terminal_locator,
            dispatched_at: DateTime::parse_from_rfc3339(&dispatched_at_str)?.with_timezone(&Utc),
            updated_at: DateTime::parse_from_rfc3339(&updated_at_str)?.with_timezone(&Utc),
        })
    }

    fn row_to_work_unit(row: &sqlx::sqlite::SqliteRow) -> Result<crate::types::WorkUnit> {
        use sqlx::Row;
        let status_str: String = row.get("status");
        let created_at_str: String = row.get("created_at");
        let updated_at_str: String = row.get("updated_at");
        Ok(crate::types::WorkUnit {
            id: row.get("id"),
            task_slug: row.get("task_slug"),
            branch: row.get("branch"),
            repos: {
                let json_str: String = row.get("repos");
                serde_json::from_str(&json_str).unwrap_or_default()
            },
            pr_urls: {
                let json_str: String = row.get("pr_urls");
                serde_json::from_str(&json_str).unwrap_or_default()
            },
            status: status_str.parse()?,
            created_at: DateTime::parse_from_rfc3339(&created_at_str)?.with_timezone(&Utc),
            updated_at: DateTime::parse_from_rfc3339(&updated_at_str)?.with_timezone(&Utc),
        })
    }

    /// Convert an optional row to an optional WorkUnit (reduces repetition in find_* methods).
    fn optional_work_unit(
        row: Option<&sqlx::sqlite::SqliteRow>,
    ) -> Result<Option<crate::types::WorkUnit>> {
        match row {
            Some(r) => Ok(Some(Self::row_to_work_unit(r)?)),
            None => Ok(None),
        }
    }
}

#[async_trait]
impl Registry for SqliteRegistry {
    async fn insert(&self, record: &DispatchRecord) -> Result<()> {
        Self::insert_dispatch(&self.pool, record).await?;
        Ok(())
    }

    async fn insert_resume_reservation(&self, record: &DispatchRecord, force: bool) -> Result<()> {
        anyhow::ensure!(
            record.resume_of_dispatch_id.is_some(),
            "resume reservations require resume_of_dispatch_id"
        );

        let Some(session_id) = record.agent_session_id else {
            anyhow::bail!("resume reservations require agent_session_id");
        };

        anyhow::ensure!(
            record.status == Status::Running,
            "resume reservations must be inserted with running status, got {}",
            record.status
        );

        if force {
            return self.insert(record).await;
        }

        let session_id = session_id.to_string();
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

        let result: Result<()> = async {
            let active_session_query =
                active_agent_session_query_sql("SELECT id, status FROM dispatches");
            let conflict: Option<(String, String)> = sqlx::query_as(&active_session_query)
                .bind(&record.agent_provider)
                .bind(&session_id)
                .fetch_optional(&mut *conn)
                .await?;

            if let Some((id, status)) = conflict {
                anyhow::bail!(
                    "provider session {session_id} is already active in dispatch {id} (status {status})"
                );
            }

            Self::insert_dispatch(&mut *conn, record).await?;
            Ok(())
        }
        .await;

        let finalize = if result.is_ok() { "COMMIT" } else { "ROLLBACK" };
        if let Err(e) = sqlx::query(finalize).execute(&mut *conn).await {
            if result.is_ok() {
                return Err(e.into());
            }
            warn!(error = %e, "failed to roll back resume reservation transaction");
        }

        result
    }

    async fn find_active_by_agent_session(
        &self,
        provider: &str,
        session_id: AgentSessionId,
    ) -> Result<Option<DispatchRecord>> {
        let active_session_query = active_agent_session_query_sql("SELECT * FROM dispatches");
        let row = sqlx::query(&active_session_query)
            .bind(provider)
            .bind(session_id.to_string())
            .fetch_optional(&self.pool)
            .await?;

        match row {
            Some(ref r) => Ok(Some(Self::row_to_record(r)?)),
            None => Ok(None),
        }
    }

    async fn update_status(&self, id: &str, status: Status) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let result =
            sqlx::query("UPDATE dispatches SET status = ?1, updated_at = ?2 WHERE id = ?3")
                .bind(status.as_str())
                .bind(&now)
                .bind(id)
                .execute(&self.pool)
                .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for id: {id}"
        );
        Ok(())
    }

    async fn update_session_locator(
        &self,
        id: &str,
        session: &str,
        terminal_locator: Option<&TerminalLocator>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let terminal_locator_json = terminal_locator.map(serde_json::to_string).transpose()?;
        let result = sqlx::query(
            "UPDATE dispatches SET session = ?1, terminal_locator_json = ?2, updated_at = ?3 WHERE id = ?4",
        )
        .bind(session)
        .bind(&terminal_locator_json)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for id: {id}"
        );
        Ok(())
    }

    async fn update_dispatch_work_unit(&self, id: &str, work_unit_id: Option<&str>) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let result =
            sqlx::query("UPDATE dispatches SET work_unit_id = ?1, updated_at = ?2 WHERE id = ?3")
                .bind(work_unit_id)
                .bind(&now)
                .bind(id)
                .execute(&self.pool)
                .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for id: {id}"
        );
        Ok(())
    }

    async fn update_cost(&self, id: &str, cost: f64, turns: u32, duration_ms: u64) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE dispatches SET cost_usd = ?1, num_turns = ?2, duration_ms = ?3, updated_at = ?4 WHERE id = ?5",
        )
        .bind(cost)
        .bind(i32::try_from(turns).map_err(|_| anyhow::anyhow!("turns overflows i32"))?)
        .bind(i64::try_from(duration_ms).map_err(|_| anyhow::anyhow!("duration_ms overflows i64"))?)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for id: {id}"
        );
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<DispatchRecord>> {
        let row = sqlx::query("SELECT * FROM dispatches WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        match row {
            Some(ref r) => Ok(Some(Self::row_to_record(r)?)),
            None => Ok(None),
        }
    }

    async fn list(&self, filter: StatusFilter) -> Result<Vec<DispatchRecord>> {
        let rows = match &filter {
            StatusFilter::All => {
                sqlx::query("SELECT * FROM dispatches ORDER BY dispatched_at DESC, id DESC")
                    .fetch_all(&self.pool)
                    .await?
            }
            StatusFilter::One(status) => sqlx::query(
                "SELECT * FROM dispatches WHERE status = ?1 ORDER BY dispatched_at DESC, id DESC",
            )
            .bind(status.as_str())
            .fetch_all(&self.pool)
            .await?,
            StatusFilter::Any(statuses) => {
                if statuses.is_empty() {
                    return Ok(Vec::new());
                }
                // Build parameterised IN clause: WHERE status IN (?1, ?2, ...)
                let placeholders: Vec<String> =
                    (1..=statuses.len()).map(|i| format!("?{i}")).collect();
                let sql = format!(
                    "SELECT * FROM dispatches WHERE status IN ({}) ORDER BY dispatched_at DESC, id DESC",
                    placeholders.join(", ")
                );
                let mut query = sqlx::query(&sql);
                for s in statuses {
                    query = query.bind(s.as_str());
                }
                query.fetch_all(&self.pool).await?
            }
            StatusFilter::AnyOrUpdatedSince {
                statuses,
                updated_since,
            } => {
                let updated_since = updated_since.to_rfc3339();
                if statuses.is_empty() {
                    sqlx::query(
                        "SELECT * FROM dispatches WHERE updated_at >= ?1 ORDER BY dispatched_at DESC, id DESC",
                    )
                    .bind(updated_since)
                    .fetch_all(&self.pool)
                    .await?
                } else {
                    let placeholders: Vec<String> =
                        (1..=statuses.len()).map(|i| format!("?{i}")).collect();
                    let updated_param = statuses.len() + 1;
                    let sql = format!(
                        "SELECT * FROM dispatches \
                         WHERE status IN ({}) OR updated_at >= ?{} \
                         ORDER BY dispatched_at DESC, id DESC",
                        placeholders.join(", "),
                        updated_param
                    );
                    let mut query = sqlx::query(&sql);
                    for s in statuses {
                        query = query.bind(s.as_str());
                    }
                    query.bind(updated_since).fetch_all(&self.pool).await?
                }
            }
        };

        rows.iter().map(Self::row_to_record).collect()
    }

    async fn update_health(
        &self,
        id: &str,
        checks: &HealthChecks,
        status: Status,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        let result = sqlx::query(
            r#"UPDATE dispatches SET
                check_agent_exited_clean = ?1,
                check_branch_pushed = ?2,
                check_pr_created = ?3,
                check_ci_passed = ?4,
                check_reviews_approved = ?5,
                check_threads_resolved = ?6,
                status = ?7,
                updated_at = ?8
            WHERE id = ?9"#,
        )
        .bind(checks.agent_exited_clean as i32)
        .bind(checks.branch_pushed as i32)
        .bind(checks.pr_created as i32)
        .bind(checks.ci_passed as i32)
        .bind(checks.reviews_approved as i32)
        .bind(checks.threads_resolved as i32)
        .bind(status.as_str())
        .bind(updated_at.to_rfc3339())
        .bind(id)
        .execute(&self.pool)
        .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for id: {id}"
        );
        Ok(())
    }

    async fn set_pr_url(&self, id: &str, url: &str) -> Result<()> {
        // For backward compat, set_pr_url replaces the pr_urls array with a single URL
        let pr_urls_json = serde_json::to_string(&vec![url])?;
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE dispatches SET pr_url = ?1, pr_urls = ?2, updated_at = ?3 WHERE id = ?4",
        )
        .bind(url)
        .bind(&pr_urls_json)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for id: {id}"
        );
        Ok(())
    }

    async fn add_pr_url(&self, id: &str, url: &str) -> Result<()> {
        // Atomic read-modify-write: BEGIN IMMEDIATE prevents concurrent readers
        // from seeing stale pr_urls between SELECT and UPDATE.
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

        let result: Result<()> = async {
            let now = Utc::now().to_rfc3339();
            let (current_json,): (String,) =
                sqlx::query_as("SELECT pr_urls FROM dispatches WHERE id = ?1")
                    .bind(id)
                    .fetch_one(&mut *conn)
                    .await?;

            let mut urls: Vec<String> = serde_json::from_str(&current_json)?;
            if !urls.iter().any(|existing| existing == url) {
                urls.push(url.to_string());
            }

            let pr_url_compat = urls.first().cloned();
            sqlx::query(
                "UPDATE dispatches SET pr_url = ?1, pr_urls = ?2, updated_at = ?3 WHERE id = ?4",
            )
            .bind(&pr_url_compat)
            .bind(serde_json::to_string(&urls)?)
            .bind(&now)
            .bind(id)
            .execute(&mut *conn)
            .await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *conn).await?;
                Ok(())
            }
            Err(e) => {
                sqlx::query("ROLLBACK").execute(&mut *conn).await.ok();
                Err(e)
            }
        }
    }

    async fn set_artifacts(&self, id: &str, artifacts_json: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let result =
            sqlx::query("UPDATE dispatches SET artifacts = ?1, updated_at = ?2 WHERE id = ?3")
                .bind(artifacts_json)
                .bind(&now)
                .bind(id)
                .execute(&self.pool)
                .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for id: {id}"
        );
        Ok(())
    }

    async fn increment_retries(
        &self,
        id: &str,
        new_session: &str,
        new_log_file: &Path,
        new_dispatched_at: DateTime<Utc>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let terminal_locator_json = match sqlx::query_as::<_, (String,)>(
            "SELECT worktree_path FROM dispatches WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        {
            Some((worktree_path,)) if !new_session.trim().is_empty() => {
                let locator = TerminalLocator::atc_tmux(
                    new_session,
                    Some(PathBuf::from(worktree_path)),
                    new_dispatched_at,
                );
                Some(serde_json::to_string(&locator)?)
            }
            _ => None,
        };
        let result = sqlx::query(
            r#"UPDATE dispatches SET
                retries = retries + 1,
                session = ?1,
                log_file = ?2,
                status = 'running',
                dispatched_at = ?3,
                updated_at = ?4,
                check_agent_exited_clean = 0,
                check_branch_pushed = 0,
                check_pr_created = 0,
                check_ci_passed = 0,
                check_reviews_approved = 0,
                check_threads_resolved = 0,
                pr_url = NULL,
                pr_urls = '[]',
                cost_usd = NULL,
                num_turns = NULL,
                duration_ms = NULL,
                terminal_locator_json = ?5
            WHERE id = ?6"#,
        )
        .bind(new_session)
        .bind(
            new_log_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("new_log_file must be valid UTF-8"))?,
        )
        .bind(new_dispatched_at.to_rfc3339())
        .bind(&now)
        .bind(&terminal_locator_json)
        .bind(id)
        .execute(&self.pool)
        .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for id: {id}"
        );
        Ok(())
    }

    async fn find_by_branch(&self, branch: &str) -> Result<Vec<DispatchRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM dispatches WHERE branch = ?1 ORDER BY dispatched_at DESC, id DESC",
        )
        .bind(branch)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_record).collect()
    }

    async fn find_by_task_slug(&self, task_slug: &str) -> Result<Vec<DispatchRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM dispatches WHERE task_slug = ?1 ORDER BY dispatched_at DESC, id DESC",
        )
        .bind(task_slug)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_record).collect()
    }

    async fn find_by_pr_url(&self, pr_url: &str) -> Result<Vec<DispatchRecord>> {
        // Search within the pr_urls JSON array using SQLite json_each
        let rows = sqlx::query(
            "SELECT DISTINCT d.* FROM dispatches d, json_each(d.pr_urls) j WHERE j.value = ?1 ORDER BY d.dispatched_at DESC, d.id DESC",
        )
        .bind(pr_url)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_record).collect()
    }

    async fn find_by_worktree(&self, worktree_path: &Path) -> Result<Vec<DispatchRecord>> {
        let path_str = worktree_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("worktree_path must be valid UTF-8"))?;
        let rows = sqlx::query(
            "SELECT * FROM dispatches WHERE worktree_path = ?1 ORDER BY dispatched_at DESC, id DESC",
        )
        .bind(path_str)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_record).collect()
    }

    async fn find_latest_for_task(&self, task_slug: &str) -> Result<Option<DispatchRecord>> {
        let row = sqlx::query(
            "SELECT * FROM dispatches WHERE task_slug = ?1 ORDER BY dispatched_at DESC, id DESC LIMIT 1",
        )
        .bind(task_slug)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(ref r) => Ok(Some(Self::row_to_record(r)?)),
            None => Ok(None),
        }
    }

    async fn find_running_on_worktree(&self, worktree_path: &Path) -> Result<Vec<DispatchRecord>> {
        let path_str = worktree_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("worktree_path must be valid UTF-8"))?;
        let rows = sqlx::query(
            "SELECT * FROM dispatches WHERE worktree_path = ?1 AND status = ?2 ORDER BY dispatched_at DESC, id DESC",
        )
        .bind(path_str)
        .bind(Status::Running.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_record).collect()
    }

    // --- Work unit methods ---

    async fn insert_work_unit(&self, unit: &crate::types::WorkUnit) -> Result<()> {
        let repos_json = serde_json::to_string(&unit.repos)?;
        let pr_urls_json = serde_json::to_string(&unit.pr_urls)?;
        // Use OR IGNORE so that the unique partial indexes on active (task_slug, branch)
        // silently reject duplicates instead of erroring. The caller (resolve_work_unit)
        // will find the existing row on retry via the lookup path.
        sqlx::query(
            r#"INSERT OR IGNORE INTO work_units (id, task_slug, branch, repos, pr_urls, status, created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
        )
        .bind(&unit.id)
        .bind(&unit.task_slug)
        .bind(&unit.branch)
        .bind(&repos_json)
        .bind(&pr_urls_json)
        .bind(unit.status.as_str())
        .bind(unit.created_at.to_rfc3339())
        .bind(unit.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_work_unit(&self, id: &str) -> Result<Option<crate::types::WorkUnit>> {
        let row = sqlx::query("SELECT * FROM work_units WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Self::optional_work_unit(row.as_ref())
    }

    async fn find_work_unit_by_task(
        &self,
        task_slug: &str,
    ) -> Result<Option<crate::types::WorkUnit>> {
        let row = sqlx::query(
            "SELECT * FROM work_units WHERE task_slug = ?1 AND status = 'active' ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(task_slug)
        .fetch_optional(&self.pool)
        .await?;
        Self::optional_work_unit(row.as_ref())
    }

    async fn find_work_unit_by_branch(
        &self,
        branch: &str,
    ) -> Result<Option<crate::types::WorkUnit>> {
        let row = sqlx::query(
            "SELECT * FROM work_units WHERE branch = ?1 AND status = 'active' ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(branch)
        .fetch_optional(&self.pool)
        .await?;
        Self::optional_work_unit(row.as_ref())
    }

    async fn find_work_unit_by_pr(&self, pr_url: &str) -> Result<Option<crate::types::WorkUnit>> {
        let row = sqlx::query(
            "SELECT w.* FROM work_units w, json_each(w.pr_urls) je WHERE je.value = ?1 ORDER BY w.created_at DESC, w.id DESC LIMIT 1",
        )
        .bind(pr_url)
        .fetch_optional(&self.pool)
        .await?;
        Self::optional_work_unit(row.as_ref())
    }

    async fn update_work_unit_status(
        &self,
        id: &str,
        status: crate::types::WorkUnitStatus,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let result =
            sqlx::query("UPDATE work_units SET status = ?1, updated_at = ?2 WHERE id = ?3")
                .bind(status.as_str())
                .bind(&now)
                .bind(id)
                .execute(&self.pool)
                .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no work unit found for id: {id}"
        );
        Ok(())
    }

    async fn update_work_unit_status_if_idle(
        &self,
        id: &str,
        status: crate::types::WorkUnitStatus,
    ) -> Result<bool> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
        let result: Result<bool> = async {
            // Check for non-terminal dispatches under the write lock
            let terminal_statuses = ["done", "failed", "stopped", "needs-human", "needs-review"];
            let has_live: (i64,) =
                sqlx::query_as(&format!(
                "SELECT COUNT(*) FROM dispatches WHERE work_unit_id = ?1 AND status NOT IN ({})",
                terminal_statuses.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ")
            ))
                .bind(id)
                .fetch_one(&mut *conn)
                .await?;
            if has_live.0 > 0 {
                return Ok(false);
            }
            let now = Utc::now().to_rfc3339();
            let r = sqlx::query("UPDATE work_units SET status = ?1, updated_at = ?2 WHERE id = ?3")
                .bind(status.as_str())
                .bind(&now)
                .bind(id)
                .execute(&mut *conn)
                .await?;
            anyhow::ensure!(r.rows_affected() > 0, "no work unit found for id: {id}");
            Ok(true)
        }
        .await;
        match result {
            Ok(updated) => {
                sqlx::query("COMMIT").execute(&mut *conn).await?;
                Ok(updated)
            }
            Err(e) => {
                sqlx::query("ROLLBACK").execute(&mut *conn).await.ok();
                Err(e)
            }
        }
    }

    async fn add_work_unit_pr(&self, id: &str, pr_url: &str) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
        let result: Result<()> = async {
            let now = Utc::now().to_rfc3339();
            let (current_json,): (String,) =
                sqlx::query_as("SELECT pr_urls FROM work_units WHERE id = ?1")
                    .bind(id)
                    .fetch_one(&mut *conn)
                    .await?;
            let mut urls: Vec<String> = serde_json::from_str(&current_json)?;
            if !urls.iter().any(|existing| existing == pr_url) {
                urls.push(pr_url.to_string());
            }
            sqlx::query("UPDATE work_units SET pr_urls = ?1, updated_at = ?2 WHERE id = ?3")
                .bind(serde_json::to_string(&urls)?)
                .bind(&now)
                .bind(id)
                .execute(&mut *conn)
                .await?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *conn).await?;
                Ok(())
            }
            Err(e) => {
                sqlx::query("ROLLBACK").execute(&mut *conn).await.ok();
                Err(e)
            }
        }
    }

    async fn add_work_unit_repo(&self, id: &str, repo_path: &str) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
        let result: Result<()> = async {
            let now = Utc::now().to_rfc3339();
            let (current_json,): (String,) =
                sqlx::query_as("SELECT repos FROM work_units WHERE id = ?1")
                    .bind(id)
                    .fetch_one(&mut *conn)
                    .await?;
            let mut repos: Vec<String> = serde_json::from_str(&current_json)?;
            if !repos.iter().any(|existing| existing == repo_path) {
                repos.push(repo_path.to_string());
            }
            sqlx::query("UPDATE work_units SET repos = ?1, updated_at = ?2 WHERE id = ?3")
                .bind(serde_json::to_string(&repos)?)
                .bind(&now)
                .bind(id)
                .execute(&mut *conn)
                .await?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *conn).await?;
                Ok(())
            }
            Err(e) => {
                sqlx::query("ROLLBACK").execute(&mut *conn).await.ok();
                Err(e)
            }
        }
    }

    async fn update_work_unit_task_slug(&self, id: &str, task_slug: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let result =
            sqlx::query("UPDATE work_units SET task_slug = ?1, updated_at = ?2 WHERE id = ?3")
                .bind(task_slug)
                .bind(&now)
                .bind(id)
                .execute(&self.pool)
                .await;
        match result {
            Ok(r) => {
                anyhow::ensure!(r.rows_affected() > 0, "no work unit found for id: {id}");
                Ok(())
            }
            Err(e) => {
                // If the update hits the unique partial index (another dispatch
                // already created an active work unit for this task_slug), that's
                // fine — the winning row exists. Treat as success.
                let msg = e.to_string();
                if msg.contains("UNIQUE constraint failed") || msg.contains("unique") {
                    tracing::debug!(
                        work_unit = %id,
                        task_slug = %task_slug,
                        "task-slug promotion hit unique constraint — another active unit exists, treating as success"
                    );
                    Ok(())
                } else {
                    Err(e.into())
                }
            }
        }
    }

    async fn list_work_units(&self) -> Result<Vec<crate::types::WorkUnit>> {
        let rows = sqlx::query("SELECT * FROM work_units ORDER BY updated_at DESC")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(Self::row_to_work_unit).collect()
    }

    async fn list_work_units_by_ids(&self, ids: &[String]) -> Result<Vec<crate::types::WorkUnit>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT * FROM work_units WHERE id IN ({}) ORDER BY updated_at DESC",
            placeholders.join(", ")
        );
        let mut query = sqlx::query(&sql);
        for id in ids {
            query = query.bind(id);
        }
        let rows = query.fetch_all(&self.pool).await?;
        rows.iter().map(Self::row_to_work_unit).collect()
    }

    async fn list_active_work_units(&self) -> Result<Vec<crate::types::WorkUnit>> {
        let rows = sqlx::query(
            "SELECT * FROM work_units WHERE status = 'active' ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_work_unit).collect()
    }

    async fn find_work_unit_by_task_any_status(
        &self,
        task_slug: &str,
    ) -> Result<Option<crate::types::WorkUnit>> {
        let row = sqlx::query(
            "SELECT * FROM work_units WHERE task_slug = ?1 ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(task_slug)
        .fetch_optional(&self.pool)
        .await?;
        Self::optional_work_unit(row.as_ref())
    }

    async fn find_work_unit_by_branch_any_status(
        &self,
        branch: &str,
    ) -> Result<Option<crate::types::WorkUnit>> {
        let row = sqlx::query(
            "SELECT * FROM work_units WHERE branch = ?1 ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(branch)
        .fetch_optional(&self.pool)
        .await?;
        Self::optional_work_unit(row.as_ref())
    }

    async fn list_dispatches_for_work_unit(
        &self,
        work_unit_id: &str,
    ) -> Result<Vec<DispatchRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM dispatches WHERE work_unit_id = ?1 ORDER BY dispatched_at ASC, id ASC",
        )
        .bind(work_unit_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_record).collect()
    }
}

impl SqliteRegistry {
    fn queue_row_from_sqlite(row: &sqlx::sqlite::SqliteRow) -> Result<QueueRow> {
        use sqlx::Row;
        let enqueued_at_str: String = row.get("enqueued_at");
        let dispatched_at_str: Option<String> = row.get("dispatched_at");
        let input_type_str: String = row.get("input_type");
        let status_str: String = row.get("status");
        Ok(QueueRow {
            id: row.get("id"),
            queue_name: row.get("queue_name"),
            input_type: input_type_str.parse()?,
            input_value: row.get("input_value"),
            mode: row.get("mode"),
            priority: row.get("priority"),
            params: row.get("params"),
            status: status_str.parse()?,
            dispatch_id: row.get("dispatch_id"),
            enqueued_at: DateTime::parse_from_rfc3339(&enqueued_at_str)?.with_timezone(&Utc),
            enqueued_by: row.get("enqueued_by"),
            dispatched_at: dispatched_at_str
                .as_deref()
                .map(|s| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
                .transpose()?,
            error: row.get("error"),
        })
    }

    /// Generate a ULID-like ID for queue rows.
    ///
    /// Uses an atomic counter combined with the timestamp to guarantee
    /// uniqueness even when called multiple times within the same millisecond.
    fn generate_queue_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let ts = Utc::now().timestamp_millis();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mix = count
            .wrapping_mul(0x517cc1b727220a95) // stafford mix constant
            ^ (std::process::id() as u64);
        format!("{:013x}-{:08x}", ts, (mix & 0xFFFF_FFFF) as u32)
    }
}

#[async_trait]
impl DispatchQueue for SqliteRegistry {
    async fn enqueue(&self, item: EnqueueItem) -> Result<EnqueueResult> {
        // Use a raw IMMEDIATE transaction so the dedup check + insert are atomic.
        // sqlx's begin() opens DEFERRED which allows two concurrent enqueues to
        // both pass the dedup SELECT before either writes. BEGIN IMMEDIATE acquires
        // a write lock upfront, serializing the critical section.
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

        // Helper to rollback on any early return
        let result: Result<EnqueueResult> = async {
            // Dedup: already pending/dispatching in this queue?
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM dispatch_queue WHERE queue_name = ?1 AND input_value = ?2 AND status IN ('pending', 'dispatching')",
            )
            .bind(&item.queue_name)
            .bind(&item.input_value)
            .fetch_one(&mut *conn)
            .await?;

            if count > 0 {
                return Ok(EnqueueResult::Skipped(
                    "already pending in queue".to_string(),
                ));
            }

            // Dedup: already running in registry?
            if item.input_type == QueueInputType::Task {
                let active_task_count_query = active_task_count_query_sql();
                let active_count: i64 = sqlx::query_scalar(&active_task_count_query)
                    .bind(&item.input_value)
                    .fetch_one(&mut *conn)
                    .await?;

                if active_count > 0 {
                    return Ok(EnqueueResult::Skipped(
                        "already running in registry".to_string(),
                    ));
                }
            }

            let id = Self::generate_queue_id();
            let now = Utc::now().to_rfc3339();

            sqlx::query(
                r#"INSERT INTO dispatch_queue (
                    id, queue_name, input_type, input_value, mode, priority, params,
                    status, enqueued_at, enqueued_by
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
            )
            .bind(&id)
            .bind(&item.queue_name)
            .bind(item.input_type.as_str())
            .bind(&item.input_value)
            .bind(&item.mode)
            .bind(item.priority.as_i32())
            .bind(&item.params)
            .bind(QueueItemStatus::Pending.as_str())
            .bind(&now)
            .bind(&item.enqueued_by)
            .execute(&mut *conn)
            .await?;

            Ok(EnqueueResult::Enqueued { id })
        }
        .await;

        match &result {
            Ok(EnqueueResult::Enqueued { .. }) => {
                sqlx::query("COMMIT").execute(&mut *conn).await?;
            }
            _ => {
                sqlx::query("ROLLBACK").execute(&mut *conn).await.ok();
            }
        }

        result
    }

    async fn queue_list(&self, queue_name: &str) -> Result<Vec<QueueRow>> {
        let rows = sqlx::query(
            "SELECT * FROM dispatch_queue WHERE queue_name = ?1 AND status = 'pending' ORDER BY priority ASC, enqueued_at ASC",
        )
        .bind(queue_name)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::queue_row_from_sqlite).collect()
    }

    async fn queue_peek(&self, queue_name: &str, limit: u32) -> Result<Vec<QueueRow>> {
        let rows = sqlx::query(
            "SELECT * FROM dispatch_queue WHERE queue_name = ?1 AND status = 'pending' ORDER BY priority ASC, enqueued_at ASC LIMIT ?2",
        )
        .bind(queue_name)
        .bind(limit as i32)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::queue_row_from_sqlite).collect()
    }

    async fn queue_claim(&self, id: &str) -> Result<Option<String>> {
        let claim_token = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE dispatch_queue SET status = 'dispatching', claimed_at = ?1 WHERE id = ?2 AND status = 'pending'",
        )
        .bind(&claim_token)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok((result.rows_affected() > 0).then_some(claim_token))
    }

    async fn queue_set_dispatch_id(
        &self,
        id: &str,
        claim_token: &str,
        dispatch_id: &str,
    ) -> Result<()> {
        let result = sqlx::query(
            "UPDATE dispatch_queue SET dispatch_id = ?1 WHERE id = ?2 AND status = 'dispatching' AND claimed_at = ?3",
        )
        .bind(dispatch_id)
        .bind(id)
        .bind(claim_token)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            anyhow::bail!(
                "queue_set_dispatch_id: no matching row for id={} (claim may have been stolen)",
                id
            );
        }
        Ok(())
    }

    async fn queue_mark_dispatched(
        &self,
        id: &str,
        claim_token: &str,
        dispatch_id: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE dispatch_queue SET status = 'dispatched', dispatch_id = ?1, dispatched_at = ?2 WHERE id = ?3 AND status = 'dispatching' AND claimed_at = ?4",
        )
        .bind(dispatch_id)
        .bind(&now)
        .bind(id)
        .bind(claim_token)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            anyhow::bail!(
                "queue_mark_dispatched: no matching row for id={} (claim may have been stolen)",
                id
            );
        }
        Ok(())
    }

    async fn queue_mark_failed(&self, id: &str, claim_token: &str, error: &str) -> Result<()> {
        let result = sqlx::query(
            "UPDATE dispatch_queue SET status = 'failed', error = ?1 WHERE id = ?2 AND status = 'dispatching' AND claimed_at = ?3",
        )
        .bind(error)
        .bind(id)
        .bind(claim_token)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            anyhow::bail!(
                "queue_mark_failed: no matching row for id={} (claim may have been stolen)",
                id
            );
        }
        Ok(())
    }

    async fn queue_clear(&self, queue_name: &str) -> Result<u64> {
        let result =
            sqlx::query("DELETE FROM dispatch_queue WHERE queue_name = ?1 AND status = 'pending'")
                .bind(queue_name)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected())
    }

    async fn queue_pending_count(&self, queue_name: &str) -> Result<u64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM dispatch_queue WHERE queue_name = ?1 AND status = 'pending'",
        )
        .bind(queue_name)
        .fetch_one(&self.pool)
        .await?;
        Ok(count as u64)
    }

    async fn queue_has_pending(&self, queue_name: &str, input_value: &str) -> Result<bool> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM dispatch_queue WHERE queue_name = ?1 AND input_value = ?2 AND status IN ('pending', 'dispatching')",
        )
        .bind(queue_name)
        .bind(input_value)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    async fn queue_recover(&self, queue_names: &[&str]) -> Result<(u64, u64)> {
        if queue_names.is_empty() {
            return Ok((0, 0));
        }

        // Check dispatching rows that have been stuck longer than the staleness
        // cutoff (60s). This avoids stealing rows from workers that are legitimately
        // mid-dispatch. If they have a matching dispatch_id in registry, mark
        // dispatched. Otherwise, reset to pending.
        let staleness_cutoff = (chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339();
        let placeholders: Vec<String> =
            (1..=queue_names.len()).map(|i| format!("?{}", i)).collect();
        let sql = format!(
            "SELECT * FROM dispatch_queue WHERE status = 'dispatching' AND queue_name IN ({}) AND (claimed_at <= ?{} OR claimed_at IS NULL)",
            placeholders.join(", "),
            queue_names.len() + 1,
        );
        let mut query = sqlx::query(&sql);
        for name in queue_names {
            query = query.bind(*name);
        }
        query = query.bind(&staleness_cutoff);
        let dispatching_rows = query.fetch_all(&self.pool).await?;

        let mut recovered = 0u64;
        let mut completed = 0u64;

        for row in &dispatching_rows {
            use sqlx::Row;
            let id: String = row.get("id");
            let dispatch_id: Option<String> = row.get("dispatch_id");

            if let Some(ref did) = dispatch_id {
                // Check if dispatch exists in registry
                let (exists,): (i64,) =
                    sqlx::query_as("SELECT COUNT(*) FROM dispatches WHERE id = ?1")
                        .bind(did)
                        .fetch_one(&self.pool)
                        .await?;

                if exists > 0 {
                    let now = Utc::now().to_rfc3339();
                    sqlx::query("UPDATE dispatch_queue SET status = 'dispatched', dispatched_at = ?1 WHERE id = ?2 AND status = 'dispatching'")
                        .bind(&now)
                        .bind(&id)
                        .execute(&self.pool)
                        .await?;
                    completed += 1;
                } else {
                    sqlx::query(
                        "UPDATE dispatch_queue SET status = 'pending', dispatch_id = NULL, claimed_at = NULL WHERE id = ?1 AND status = 'dispatching'",
                    )
                    .bind(&id)
                    .execute(&self.pool)
                    .await?;
                    recovered += 1;
                }
            } else {
                // No dispatch_id means it was never dispatched — reset to pending
                sqlx::query("UPDATE dispatch_queue SET status = 'pending', claimed_at = NULL WHERE id = ?1 AND status = 'dispatching'")
                    .bind(&id)
                    .execute(&self.pool)
                    .await?;
                recovered += 1;
            }
        }

        Ok((recovered, completed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        claude_agent_capabilities, AgentSessionId, Directive, DispatchRecord, HealthChecks, Status,
        CLAUDE_AGENT_PROVIDER,
    };
    use chrono::{DateTime, Utc};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn sample_record(id: &str) -> DispatchRecord {
        DispatchRecord {
            id: id.to_string(),
            task_slug: Some("tasks/gitkb-42".to_string()),
            branch: "tasks--gitkb-42".to_string(),
            worktree_path: PathBuf::from("/tmp/test-worktree"),
            session: format!("{}@implement@1234567890", id),
            log_file: PathBuf::from("/tmp/test.jsonl"),
            status: Status::Running,
            directive: Directive::Implement,
            retries: 0,
            resolver: "task".to_string(),
            pr_urls: vec![],
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
            agent_session_id: Some(
                AgentSessionId::parse_str("00000000-0000-4000-8000-000000000100").unwrap(),
            ),
            agent_transcript_cwd: Some(PathBuf::from("/tmp/test-worktree")),
            resume_of_dispatch_id: None,
            agent_capabilities: Some(claude_agent_capabilities()),
            terminal_locator: None,
            dispatched_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_insert_and_get_round_trip() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let record = sample_record("tasks--gitkb-42@implement@1234567890");
        registry.insert(&record).await.unwrap();
        let fetched = registry
            .get("tasks--gitkb-42@implement@1234567890")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.id, "tasks--gitkb-42@implement@1234567890");
        assert_eq!(fetched.task_slug.as_deref(), Some("tasks/gitkb-42"));
        assert_eq!(fetched.status, Status::Running);
        assert_eq!(fetched.directive, Directive::Implement);
        assert_eq!(fetched.retries, 0);
        assert_eq!(fetched.resolver, "task");
    }

    #[tokio::test]
    async fn test_terminal_locator_round_trip() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut record = sample_record("locator-round-trip");
        record.terminal_locator = Some(TerminalLocator::atc_tmux(
            "safe-session",
            Some(PathBuf::from("/tmp/test-worktree")),
            record.dispatched_at,
        ));

        registry.insert(&record).await.unwrap();
        let fetched = registry.get("locator-round-trip").await.unwrap().unwrap();

        assert_eq!(fetched.terminal_locator, record.terminal_locator);
    }

    #[tokio::test]
    async fn test_malformed_optional_terminal_locator_does_not_break_reads() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let record = sample_record("bad-terminal-locator");
        registry.insert(&record).await.unwrap();

        sqlx::query("UPDATE dispatches SET terminal_locator_json = ?1 WHERE id = ?2")
            .bind("{not-json")
            .bind("bad-terminal-locator")
            .execute(&registry.pool)
            .await
            .unwrap();

        let fetched = registry.get("bad-terminal-locator").await.unwrap().unwrap();

        assert!(fetched.terminal_locator.is_none());
    }

    #[tokio::test]
    async fn test_malformed_optional_agent_metadata_does_not_break_reads() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let record = sample_record("bad-agent-metadata");
        registry.insert(&record).await.unwrap();

        sqlx::query(
            "UPDATE dispatches SET agent_session_id = ?1, agent_capabilities_json = ?2 WHERE id = ?3",
        )
        .bind("not-a-uuid")
        .bind("{not-json")
        .bind("bad-agent-metadata")
        .execute(&registry.pool)
        .await
        .unwrap();

        let fetched = registry.get("bad-agent-metadata").await.unwrap().unwrap();

        assert_eq!(fetched.agent_provider, "claude");
        assert!(fetched.agent_session_id.is_none());
        assert!(fetched.agent_capabilities.is_none());
    }

    #[tokio::test]
    async fn test_multiple_dispatches_per_task() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut r1 = sample_record("tasks--gitkb-42@implement@1000");
        r1.task_slug = Some("tasks/gitkb-42".to_string());
        let mut r2 = sample_record("tasks--gitkb-42@implement@2000");
        r2.task_slug = Some("tasks/gitkb-42".to_string());
        let mut r3 = sample_record("tasks--gitkb-42@review-fix@3000");
        r3.task_slug = Some("tasks/gitkb-42".to_string());
        r3.directive = Directive::ReviewFix;

        registry.insert(&r1).await.unwrap();
        registry.insert(&r2).await.unwrap();
        registry.insert(&r3).await.unwrap();

        // All three should be retrievable
        let all = registry.list(StatusFilter::all()).await.unwrap();
        assert_eq!(all.len(), 3);

        // find_by_task_slug should return all 3
        let by_slug = registry.find_by_task_slug("tasks/gitkb-42").await.unwrap();
        assert_eq!(by_slug.len(), 3);
    }

    #[tokio::test]
    async fn test_update_status() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let id = "test@implement@1000";
        registry.insert(&sample_record(id)).await.unwrap();
        registry.update_status(id, Status::Done).await.unwrap();
        let fetched = registry.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.status, Status::Done);
    }

    #[tokio::test]
    async fn test_update_session_locator_updates_session_and_locator() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let id = "test@implement@locator-update";
        let record = sample_record(id);
        registry.insert(&record).await.unwrap();

        let locator =
            TerminalLocator::atc_tmux("final-session", Some(record.worktree_path), Utc::now());
        registry
            .update_session_locator(id, "final-session", Some(&locator))
            .await
            .unwrap();

        let fetched = registry.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.session, "final-session");
        assert_eq!(fetched.terminal_locator, Some(locator));
    }

    #[tokio::test]
    async fn test_update_dispatch_work_unit() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let id = "work-unit-link-dispatch";
        registry.insert(&sample_record(id)).await.unwrap();

        registry
            .update_dispatch_work_unit(id, Some("wu-linked"))
            .await
            .unwrap();
        let fetched = registry.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.work_unit_id.as_deref(), Some("wu-linked"));

        registry.update_dispatch_work_unit(id, None).await.unwrap();
        let fetched = registry.get(id).await.unwrap().unwrap();
        assert!(fetched.work_unit_id.is_none());
    }

    #[tokio::test]
    async fn test_wal_mode_pragma() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let registry = SqliteRegistry::open(&db_path).await.unwrap();
        let mode: (String,) = sqlx::query_as("PRAGMA journal_mode")
            .fetch_one(&registry.pool)
            .await
            .unwrap();
        assert_eq!(mode.0, "wal");
    }

    #[tokio::test]
    async fn test_list_with_filter() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        registry.insert(&sample_record("id-1")).await.unwrap();
        registry.insert(&sample_record("id-2")).await.unwrap();
        registry.update_status("id-2", Status::Done).await.unwrap();

        let all = registry.list(StatusFilter::all()).await.unwrap();
        assert_eq!(all.len(), 2);

        let running = registry
            .list(StatusFilter::by_status(Status::Running))
            .await
            .unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, "id-1");
    }

    #[tokio::test]
    async fn test_update_cost() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let id = "cost-test";
        registry.insert(&sample_record(id)).await.unwrap();

        registry.update_cost(id, 1.23, 15, 45000).await.unwrap();

        let fetched = registry.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.cost_usd, Some(1.23));
        assert_eq!(fetched.num_turns, Some(15));
        assert_eq!(fetched.duration_ms, Some(45000));
    }

    #[tokio::test]
    async fn test_set_pr_url() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let id = "pr-test";
        registry.insert(&sample_record(id)).await.unwrap();

        registry
            .set_pr_url(id, "https://github.com/org/repo/pull/1")
            .await
            .unwrap();

        let fetched = registry.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.pr_urls, vec!["https://github.com/org/repo/pull/1"]);
    }

    #[tokio::test]
    async fn test_update_health_round_trip() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let id = "health-test";
        registry.insert(&sample_record(id)).await.unwrap();

        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: false,
            reviews_approved: false,
            threads_resolved: false,
        };
        let status = Status::NeedsReview;
        let updated_at = DateTime::parse_from_rfc3339("2025-06-15T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        registry
            .update_health(id, &checks, status, updated_at)
            .await
            .unwrap();

        let fetched = registry.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.checks, checks);
        assert_eq!(fetched.status, Status::NeedsReview);
        assert_eq!(fetched.updated_at, updated_at);
    }

    #[tokio::test]
    async fn test_update_health_nonexistent_errors() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let err = registry
            .update_health(
                "no-such-id",
                &HealthChecks::default(),
                Status::Running,
                Utc::now(),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no dispatch record found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_round_trip_with_all_optional_fields() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut record = sample_record("full-test");
        record.pr_urls = vec!["https://github.com/org/repo/pull/99".to_string()];
        record.cost_usd = Some(4.56);
        record.num_turns = Some(42);
        record.duration_ms = Some(120_000);
        record.checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: true,
            reviews_approved: true,
            threads_resolved: true,
        };
        record.no_worktree = true;
        record.original_input = Some("review".to_string());
        record.agent_session_id =
            Some(AgentSessionId::parse_str("00000000-0000-4000-8000-000000000101").unwrap());
        record.agent_transcript_cwd = Some(PathBuf::from("/tmp/transcripts"));
        record.resume_of_dispatch_id = Some("previous-dispatch".to_string());
        registry.insert(&record).await.unwrap();

        let fetched = registry.get("full-test").await.unwrap().unwrap();
        assert_eq!(fetched.pr_urls, record.pr_urls);
        assert_eq!(fetched.cost_usd, record.cost_usd);
        assert_eq!(fetched.num_turns, record.num_turns);
        assert_eq!(fetched.duration_ms, record.duration_ms);
        assert_eq!(fetched.checks, record.checks);
        assert_eq!(fetched.no_worktree, record.no_worktree);
        assert_eq!(fetched.original_input, record.original_input);
        assert_eq!(fetched.agent_provider, record.agent_provider);
        assert_eq!(fetched.agent_session_id, record.agent_session_id);
        assert_eq!(fetched.agent_transcript_cwd, record.agent_transcript_cwd);
        assert_eq!(fetched.resume_of_dispatch_id, record.resume_of_dispatch_id);
        assert_eq!(fetched.agent_capabilities, record.agent_capabilities);
    }

    // --- Error path tests ---

    #[tokio::test]
    async fn test_get_nonexistent_returns_none() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let result = registry.get("does-not-exist").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_update_status_nonexistent_errors() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let err = registry
            .update_status("no-such-id", Status::Done)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no dispatch record found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_update_cost_nonexistent_errors() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let err = registry
            .update_cost("no-such-id", 1.0, 1, 1000)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no dispatch record found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_set_pr_url_nonexistent_errors() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let err = registry
            .set_pr_url("no-such-id", "https://example.com")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no dispatch record found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_increment_retries_nonexistent_errors() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let err = registry
            .increment_retries(
                "no-such-id",
                "session",
                &PathBuf::from("/tmp/log.jsonl"),
                Utc::now(),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no dispatch record found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_duplicate_insert_errors() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let id = "dup-test";
        registry.insert(&sample_record(id)).await.unwrap();
        let err = registry.insert(&sample_record(id)).await.unwrap_err();
        assert!(
            err.to_string().contains("UNIQUE constraint failed"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_resume_reservation_rejects_active_session_unless_forced() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut source = sample_record("source-session");
        source.status = Status::Done;
        registry.insert(&source).await.unwrap();

        let mut first_resume = sample_record("resume-1");
        first_resume.resume_of_dispatch_id = Some(source.id.clone());
        registry
            .insert_resume_reservation(&first_resume, false)
            .await
            .unwrap();

        let mut second_resume = sample_record("resume-2");
        second_resume.resume_of_dispatch_id = Some(source.id.clone());
        let err = registry
            .insert_resume_reservation(&second_resume, false)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("already active"),
            "unexpected reservation conflict error: {err}"
        );

        let mut forced_resume = sample_record("resume-3");
        forced_resume.resume_of_dispatch_id = Some(source.id);
        registry
            .insert_resume_reservation(&forced_resume, true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_resume_reservation_requires_running_status_even_when_forced() {
        let registry = SqliteRegistry::in_memory().await.unwrap();

        for force in [false, true] {
            let mut record = sample_record(&format!("terminal-reservation-{force}"));
            record.status = Status::Done;
            record.resume_of_dispatch_id = Some("source-session".to_string());

            let err = registry
                .insert_resume_reservation(&record, force)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("running status"),
                "unexpected terminal reservation error: {err}"
            );
        }
    }

    #[tokio::test]
    async fn test_find_active_by_agent_session_returns_only_non_terminal_dispatch() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let session_id = AgentSessionId::parse_str("00000000-0000-4000-8000-000000000099").unwrap();

        for status in [
            Status::Done,
            Status::Failed,
            Status::NeedsReview,
            Status::NeedsHuman,
            Status::Stopped,
        ] {
            let mut terminal = sample_record(&format!("active-session-terminal-{status}"));
            terminal.status = status;
            terminal.agent_session_id = Some(session_id);
            registry.insert(&terminal).await.unwrap();
        }

        let mut retrying = sample_record("active-session-retrying");
        retrying.status = Status::Retrying;
        retrying.agent_session_id = Some(session_id);
        registry.insert(&retrying).await.unwrap();

        let found = registry
            .find_active_by_agent_session("claude", session_id)
            .await
            .unwrap()
            .expect("retrying dispatch should be returned");
        assert_eq!(found.id, "active-session-retrying");

        registry
            .update_status("active-session-retrying", Status::Done)
            .await
            .unwrap();
        assert!(registry
            .find_active_by_agent_session("claude", session_id)
            .await
            .unwrap()
            .is_none());

        let mut running = sample_record("active-session-running");
        running.status = Status::Running;
        running.agent_session_id = Some(session_id);
        registry.insert(&running).await.unwrap();

        let found = registry
            .find_active_by_agent_session("claude", session_id)
            .await
            .unwrap()
            .expect("running dispatch should be returned");
        assert_eq!(found.id, "active-session-running");

        registry
            .update_status("active-session-running", Status::Stopped)
            .await
            .unwrap();
        assert!(registry
            .find_active_by_agent_session("claude", session_id)
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_active_dispatch_statuses_match_terminal_semantics() {
        for status in [
            Status::Running,
            Status::Retrying,
            Status::Done,
            Status::Failed,
            Status::NeedsReview,
            Status::NeedsHuman,
            Status::Stopped,
        ] {
            assert_eq!(
                is_active_dispatch_status(status),
                !status.is_terminal(),
                "active status drift for {status}"
            );
        }
    }

    #[tokio::test]
    async fn test_resume_reservation_requires_resume_metadata_even_when_forced() {
        let registry = SqliteRegistry::in_memory().await.unwrap();

        let missing_resume_link = sample_record("forced-missing-resume-link");
        let err = registry
            .insert_resume_reservation(&missing_resume_link, true)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("resume_of_dispatch_id"),
            "unexpected missing resume link error: {err}"
        );

        let mut missing_session = sample_record("forced-missing-session");
        missing_session.resume_of_dispatch_id = Some("source".to_string());
        missing_session.agent_session_id = None;
        let err = registry
            .insert_resume_reservation(&missing_session, true)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("agent_session_id"),
            "unexpected missing session error: {err}"
        );
    }

    #[tokio::test]
    async fn test_list_empty_returns_empty_vec() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let all = registry.list(StatusFilter::all()).await.unwrap();
        assert!(all.is_empty());
        let filtered = registry
            .list(StatusFilter::by_status(Status::Running))
            .await
            .unwrap();
        assert!(filtered.is_empty());
    }

    // --- StatusFilter::Any tests ---

    #[tokio::test]
    async fn test_list_with_any_filter() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut r1 = sample_record("running-1");
        r1.status = Status::Running;
        let mut r2 = sample_record("done-1");
        r2.status = Status::Done;
        let mut r3 = sample_record("needs-review-1");
        r3.status = Status::NeedsReview;
        let mut r4 = sample_record("failed-1");
        r4.status = Status::Failed;
        let mut r5 = sample_record("needs-human-1");
        r5.status = Status::NeedsHuman;

        for r in [&r1, &r2, &r3, &r4, &r5] {
            registry.insert(r).await.unwrap();
        }

        let active = registry
            .list(StatusFilter::any(vec![
                Status::Running,
                Status::NeedsReview,
            ]))
            .await
            .unwrap();
        assert_eq!(active.len(), 2);
        let ids: Vec<&str> = active.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"running-1"));
        assert!(ids.contains(&"needs-review-1"));

        let empty = registry.list(StatusFilter::any(vec![])).await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_list_with_any_or_updated_since_filter() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let cutoff = DateTime::parse_from_rfc3339("2026-06-03T12:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc);

        let mut running_old = sample_record("running-old");
        running_old.status = Status::Running;
        running_old.updated_at = cutoff - chrono::Duration::hours(48);
        running_old.dispatched_at = running_old.updated_at;

        let mut done_recent = sample_record("done-recent");
        done_recent.status = Status::Done;
        done_recent.updated_at = cutoff + chrono::Duration::minutes(1);
        done_recent.dispatched_at = done_recent.updated_at;

        let mut done_old = sample_record("done-old");
        done_old.status = Status::Done;
        done_old.updated_at = cutoff - chrono::Duration::hours(48);
        done_old.dispatched_at = done_old.updated_at;

        for record in [&running_old, &done_recent, &done_old] {
            registry.insert(record).await.unwrap();
        }

        let records = registry
            .list(StatusFilter::any_or_updated_since(
                vec![Status::Running],
                cutoff,
            ))
            .await
            .unwrap();
        let ids: Vec<&str> = records.iter().map(|record| record.id.as_str()).collect();

        assert_eq!(ids, vec!["done-recent", "running-old"]);

        let recent_only = registry
            .list(StatusFilter::any_or_updated_since(vec![], cutoff))
            .await
            .unwrap();
        let ids: Vec<&str> = recent_only
            .iter()
            .map(|record| record.id.as_str())
            .collect();
        assert_eq!(ids, vec!["done-recent"]);
    }

    // --- New query method tests ---

    #[tokio::test]
    async fn test_find_by_branch() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut r1 = sample_record("id-1");
        r1.branch = "feature-a".to_string();
        let mut r2 = sample_record("id-2");
        r2.branch = "feature-b".to_string();
        let mut r3 = sample_record("id-3");
        r3.branch = "feature-a".to_string();

        for r in [&r1, &r2, &r3] {
            registry.insert(r).await.unwrap();
        }

        let results = registry.find_by_branch("feature-a").await.unwrap();
        assert_eq!(results.len(), 2);
        let results = registry.find_by_branch("feature-b").await.unwrap();
        assert_eq!(results.len(), 1);
        let results = registry.find_by_branch("nonexistent").await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_find_by_task_slug() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut r1 = sample_record("id-1");
        r1.task_slug = Some("tasks/gitkb-42".to_string());
        let mut r2 = sample_record("id-2");
        r2.task_slug = Some("tasks/gitkb-42".to_string());
        let mut r3 = sample_record("id-3");
        r3.task_slug = Some("tasks/gitkb-99".to_string());
        let mut r4 = sample_record("id-4");
        r4.task_slug = None;

        for r in [&r1, &r2, &r3, &r4] {
            registry.insert(r).await.unwrap();
        }

        let results = registry.find_by_task_slug("tasks/gitkb-42").await.unwrap();
        assert_eq!(results.len(), 2);
        let results = registry.find_by_task_slug("tasks/gitkb-99").await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_find_by_pr_url() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut r1 = sample_record("id-1");
        r1.pr_urls = vec!["https://github.com/org/repo/pull/1".to_string()];
        let mut r2 = sample_record("id-2");
        r2.pr_urls = vec!["https://github.com/org/repo/pull/1".to_string()];
        let mut r3 = sample_record("id-3");
        r3.pr_urls = vec![];

        for r in [&r1, &r2, &r3] {
            registry.insert(r).await.unwrap();
        }

        let results = registry
            .find_by_pr_url("https://github.com/org/repo/pull/1")
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_add_pr_url_appends_to_json_array() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let id = "multi-pr-test";
        registry.insert(&sample_record(id)).await.unwrap();

        // Add first PR URL
        registry
            .add_pr_url(id, "https://github.com/org/repo-a/pull/1")
            .await
            .unwrap();
        let fetched = registry.get(id).await.unwrap().unwrap();
        assert_eq!(
            fetched.pr_urls,
            vec!["https://github.com/org/repo-a/pull/1"]
        );

        // Add second PR URL
        registry
            .add_pr_url(id, "https://github.com/org/repo-b/pull/2")
            .await
            .unwrap();
        let fetched = registry.get(id).await.unwrap().unwrap();
        assert_eq!(
            fetched.pr_urls,
            vec![
                "https://github.com/org/repo-a/pull/1",
                "https://github.com/org/repo-b/pull/2",
            ]
        );

        // Dedup: adding same URL again should be a no-op
        registry
            .add_pr_url(id, "https://github.com/org/repo-a/pull/1")
            .await
            .unwrap();
        let fetched = registry.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.pr_urls.len(), 2);
    }

    #[tokio::test]
    async fn test_find_by_pr_url_searches_json_array() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut r1 = sample_record("id-1");
        r1.pr_urls = vec![
            "https://github.com/org/core/pull/1".to_string(),
            "https://github.com/org/api/pull/10".to_string(),
        ];
        registry.insert(&r1).await.unwrap();

        // Should find by either URL in the array
        let results = registry
            .find_by_pr_url("https://github.com/org/core/pull/1")
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "id-1");

        let results = registry
            .find_by_pr_url("https://github.com/org/api/pull/10")
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "id-1");

        // Should not find by URL not in the array
        let results = registry
            .find_by_pr_url("https://github.com/org/other/pull/99")
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_multi_pr_urls_round_trip() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut record = sample_record("multi-pr-rt");
        record.pr_urls = vec![
            "https://github.com/org/core/pull/62".to_string(),
            "https://github.com/org/api/pull/136".to_string(),
            "https://github.com/org/ui/pull/35".to_string(),
        ];
        registry.insert(&record).await.unwrap();

        let fetched = registry.get("multi-pr-rt").await.unwrap().unwrap();
        assert_eq!(fetched.pr_urls, record.pr_urls);
    }

    #[tokio::test]
    async fn test_set_pr_url_replaces_pr_urls_array() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let id = "replace-test";
        let mut record = sample_record(id);
        record.pr_urls = vec![
            "https://github.com/org/a/pull/1".to_string(),
            "https://github.com/org/b/pull/2".to_string(),
        ];
        registry.insert(&record).await.unwrap();

        // set_pr_url replaces the entire array
        registry
            .set_pr_url(id, "https://github.com/org/c/pull/3")
            .await
            .unwrap();
        let fetched = registry.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.pr_urls, vec!["https://github.com/org/c/pull/3"]);
    }

    #[tokio::test]
    async fn test_pr_urls_migration_backfill() {
        // Test that migration from pr_url → pr_urls works correctly
        // by simulating a legacy database with only pr_url column
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("migration-test.db");

        // Create a database with the old schema (no pr_urls column)
        {
            let url = format!("sqlite:{}?mode=rwc", db_path.display());
            let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
            sqlx::query("PRAGMA journal_mode=WAL")
                .execute(&pool)
                .await
                .unwrap();
            // Create dispatches table WITHOUT pr_urls column
            sqlx::query(
                r#"CREATE TABLE dispatches (
                    id TEXT PRIMARY KEY,
                    task_slug TEXT,
                    branch TEXT NOT NULL,
                    worktree_path TEXT NOT NULL,
                    session TEXT NOT NULL,
                    log_file TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'running',
                    directive TEXT NOT NULL,
                    retries INTEGER NOT NULL DEFAULT 0,
                    resolver TEXT NOT NULL,
                    pr_url TEXT,
                    no_worktree INTEGER NOT NULL DEFAULT 0,
                    original_input TEXT,
                    kb_root TEXT,
                    check_agent_exited_clean INTEGER NOT NULL DEFAULT 0,
                    check_branch_pushed INTEGER NOT NULL DEFAULT 0,
                    check_pr_created INTEGER NOT NULL DEFAULT 0,
                    check_ci_passed INTEGER NOT NULL DEFAULT 0,
                    check_reviews_approved INTEGER NOT NULL DEFAULT 0,
                    check_threads_resolved INTEGER NOT NULL DEFAULT 0,
                    cost_usd REAL,
                    num_turns INTEGER,
                    duration_ms INTEGER,
                    artifacts TEXT,
                    dispatched_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )"#,
            )
            .execute(&pool)
            .await
            .unwrap();
            // Insert a record with a pr_url
            sqlx::query(
                r#"INSERT INTO dispatches (id, task_slug, branch, worktree_path, session, log_file, status, directive, retries, resolver, pr_url, dispatched_at, updated_at)
                   VALUES ('legacy-1', 'tasks/old', 'branch', '/tmp/wt', 'session', '/tmp/log', 'done', 'implement', 0, 'task', 'https://github.com/org/repo/pull/42', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        // Open with the new schema — migration should backfill pr_urls
        let registry = SqliteRegistry::open(&db_path).await.unwrap();
        let record = registry.get("legacy-1").await.unwrap().unwrap();
        assert_eq!(
            record.pr_urls,
            vec!["https://github.com/org/repo/pull/42"],
            "migration should backfill pr_urls from pr_url"
        );
        assert_eq!(record.agent_provider, "claude");
        assert!(record.agent_session_id.is_none());
        assert!(record.agent_transcript_cwd.is_none());
    }

    #[tokio::test]
    async fn test_agent_metadata_migration_handles_partial_columns() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("partial-agent-columns.db");

        {
            let url = format!("sqlite:{}?mode=rwc", db_path.display());
            let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
            sqlx::query(
                r#"CREATE TABLE dispatches (
                    id TEXT PRIMARY KEY,
                    task_slug TEXT,
                    branch TEXT NOT NULL,
                    worktree_path TEXT NOT NULL,
                    session TEXT NOT NULL,
                    log_file TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'running',
                    directive TEXT NOT NULL,
                    retries INTEGER NOT NULL DEFAULT 0,
                    resolver TEXT NOT NULL,
                    pr_url TEXT,
                    pr_urls TEXT NOT NULL DEFAULT '[]',
                    no_worktree INTEGER NOT NULL DEFAULT 0,
                    original_input TEXT,
                    kb_root TEXT,
                    check_agent_exited_clean INTEGER NOT NULL DEFAULT 0,
                    check_branch_pushed INTEGER NOT NULL DEFAULT 0,
                    check_pr_created INTEGER NOT NULL DEFAULT 0,
                    check_ci_passed INTEGER NOT NULL DEFAULT 0,
                    check_reviews_approved INTEGER NOT NULL DEFAULT 0,
                    check_threads_resolved INTEGER NOT NULL DEFAULT 0,
                    cost_usd REAL,
                    num_turns INTEGER,
                    duration_ms INTEGER,
                    artifacts TEXT,
                    work_unit_id TEXT,
                    agent_provider TEXT NOT NULL DEFAULT 'claude',
                    agent_session_id TEXT,
                    dispatched_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )"#,
            )
            .execute(&pool)
            .await
            .unwrap();

            let now = Utc::now().to_rfc3339();
            sqlx::query(
                r#"INSERT INTO dispatches (
                    id, task_slug, branch, worktree_path, session, log_file, status,
                    directive, retries, resolver, pr_urls, agent_provider,
                    agent_session_id, dispatched_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"#,
            )
            .bind("partial-agent-id")
            .bind("tasks/partial")
            .bind("tasks--partial")
            .bind("/tmp/partial")
            .bind("partial-session")
            .bind("/tmp/partial.jsonl")
            .bind("running")
            .bind("implement")
            .bind(0i32)
            .bind("task")
            .bind("[]")
            .bind("claude")
            .bind("00000000-0000-4000-8000-000000000777")
            .bind(&now)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

            pool.close().await;
        }

        let registry = SqliteRegistry::open(&db_path).await.unwrap();
        let record = registry.get("partial-agent-id").await.unwrap().unwrap();

        assert_eq!(record.agent_provider, "claude");
        assert_eq!(
            record
                .agent_session_id
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("00000000-0000-4000-8000-000000000777")
        );
        assert!(record.agent_transcript_cwd.is_none());
        assert!(record.resume_of_dispatch_id.is_none());
        assert_eq!(record.agent_capabilities, Some(claude_agent_capabilities()));
        assert!(record.terminal_locator.is_none());
        let (has_terminal_locator,): (i32,) = sqlx::query_as(
            "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = 'terminal_locator_json'",
        )
        .fetch_one(registry.pool())
        .await
        .unwrap();
        assert_eq!(has_terminal_locator, 1);
    }

    #[tokio::test]
    async fn test_find_by_worktree() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut r1 = sample_record("id-1");
        r1.worktree_path = PathBuf::from("/tmp/wt-a");
        let mut r2 = sample_record("id-2");
        r2.worktree_path = PathBuf::from("/tmp/wt-a");
        let mut r3 = sample_record("id-3");
        r3.worktree_path = PathBuf::from("/tmp/wt-b");

        for r in [&r1, &r2, &r3] {
            registry.insert(r).await.unwrap();
        }

        let results = registry
            .find_by_worktree(Path::new("/tmp/wt-a"))
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_find_latest_for_task() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut r1 = sample_record("id-1");
        r1.task_slug = Some("tasks/gitkb-42".to_string());
        r1.dispatched_at = DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut r2 = sample_record("id-2");
        r2.task_slug = Some("tasks/gitkb-42".to_string());
        r2.dispatched_at = DateTime::parse_from_rfc3339("2025-06-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        registry.insert(&r1).await.unwrap();
        registry.insert(&r2).await.unwrap();

        let latest = registry
            .find_latest_for_task("tasks/gitkb-42")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.id, "id-2");

        let none = registry
            .find_latest_for_task("tasks/nonexistent")
            .await
            .unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn test_find_running_on_worktree() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut r1 = sample_record("id-1");
        r1.worktree_path = PathBuf::from("/tmp/wt-a");
        r1.status = Status::Running;
        let mut r2 = sample_record("id-2");
        r2.worktree_path = PathBuf::from("/tmp/wt-a");
        r2.status = Status::Done;
        let mut r3 = sample_record("id-3");
        r3.worktree_path = PathBuf::from("/tmp/wt-b");
        r3.status = Status::Running;

        for r in [&r1, &r2, &r3] {
            registry.insert(r).await.unwrap();
        }

        let results = registry
            .find_running_on_worktree(Path::new("/tmp/wt-a"))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "id-1");
    }

    // --- Security / red-team tests ---

    #[tokio::test]
    async fn test_sql_injection_in_id() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let malicious_id = "'; DROP TABLE dispatches; --";
        let mut record = sample_record(malicious_id);
        record.branch = "safe-branch".to_string();
        registry.insert(&record).await.unwrap();

        let fetched = registry.get(malicious_id).await.unwrap().unwrap();
        assert_eq!(fetched.id, malicious_id);

        let all = registry.list(StatusFilter::all()).await.unwrap();
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn test_nullable_task_slug() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut record = sample_record("prompt-dispatch");
        record.task_slug = None;
        record.resolver = "prompt".to_string();
        registry.insert(&record).await.unwrap();

        let fetched = registry.get("prompt-dispatch").await.unwrap().unwrap();
        assert!(fetched.task_slug.is_none());
        assert_eq!(fetched.resolver, "prompt");
    }

    #[tokio::test]
    async fn test_all_status_variants_round_trip() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let statuses = [
            Status::Running,
            Status::Done,
            Status::Failed,
            Status::NeedsReview,
            Status::NeedsHuman,
            Status::Stopped,
            Status::Retrying,
        ];
        for (i, status) in statuses.iter().enumerate() {
            let id = format!("status-{i}");
            let mut record = sample_record(&id);
            record.status = *status;
            registry.insert(&record).await.unwrap();
            let fetched = registry.get(&id).await.unwrap().unwrap();
            assert_eq!(&fetched.status, status);
        }
    }

    #[tokio::test]
    async fn test_all_directive_variants_round_trip() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let all_directives = [
            Directive::Implement,
            Directive::Research,
            Directive::KbUpdate,
            Directive::ReviewFix,
            Directive::PrComments,
            Directive::Refine,
            Directive::CreateTask,
            Directive::Close,
        ];
        for (i, d) in all_directives.iter().enumerate() {
            let id = format!("directive-{i}");
            let mut record = sample_record(&id);
            record.directive = d.clone();
            registry.insert(&record).await.unwrap();
            let fetched = registry.get(&id).await.unwrap().unwrap();
            assert_eq!(&fetched.directive, d);
        }
    }

    #[tokio::test]
    async fn test_increment_retries() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let id = "retry-test";
        registry.insert(&sample_record(id)).await.unwrap();
        registry.update_status(id, Status::Failed).await.unwrap();

        let new_time = Utc::now();
        registry
            .increment_retries(
                id,
                "new-session",
                &PathBuf::from("/tmp/retry.jsonl"),
                new_time,
            )
            .await
            .unwrap();

        let fetched = registry.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.retries, 1);
        assert_eq!(fetched.status, Status::Running);
        assert_eq!(fetched.session, "new-session");
        assert!(!fetched.checks.agent_exited_clean);
        assert!(fetched.pr_urls.is_empty());
        assert_eq!(fetched.cost_usd, None);
    }

    #[tokio::test]
    async fn test_list_ordered_by_dispatched_at_desc() {
        let registry = SqliteRegistry::in_memory().await.unwrap();

        let mut older = sample_record("older");
        older.dispatched_at = DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut newer = sample_record("newer");
        newer.dispatched_at = DateTime::parse_from_rfc3339("2025-06-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        registry.insert(&older).await.unwrap();
        registry.insert(&newer).await.unwrap();

        let all = registry.list(StatusFilter::all()).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "newer");
        assert_eq!(all[1].id, "older");
    }

    #[tokio::test]
    async fn test_concurrent_inserts() {
        let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
        let mut handles = Vec::new();

        for i in 0..10 {
            let reg = Arc::clone(&registry);
            handles.push(tokio::spawn(async move {
                let id = format!("concurrent-{i}");
                reg.insert(&sample_record(&id)).await.unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let all = registry.list(StatusFilter::all()).await.unwrap();
        assert_eq!(all.len(), 10);
    }

    /// Regression test: open a legacy database that lacks `no_worktree` and
    /// `original_input` columns and verify `migrate_if_needed()` adds them so
    /// records round-trip with the correct defaults.
    #[tokio::test]
    async fn test_migration_adds_no_worktree_and_original_input_columns() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("legacy.db");

        // Create a legacy schema WITHOUT no_worktree and original_input columns.
        {
            let url = format!("sqlite:{}?mode=rwc", db_path.display());
            let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
            sqlx::query(
                r#"CREATE TABLE dispatches (
                    id TEXT PRIMARY KEY,
                    task_slug TEXT,
                    branch TEXT NOT NULL,
                    worktree_path TEXT NOT NULL,
                    session TEXT NOT NULL,
                    log_file TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'running',
                    mode TEXT NOT NULL,
                    retries INTEGER NOT NULL DEFAULT 0,
                    resolver TEXT NOT NULL,
                    pr_url TEXT,
                    check_agent_exited_clean INTEGER NOT NULL DEFAULT 0,
                    check_branch_pushed INTEGER NOT NULL DEFAULT 0,
                    check_pr_created INTEGER NOT NULL DEFAULT 0,
                    check_ci_passed INTEGER NOT NULL DEFAULT 0,
                    check_reviews_approved INTEGER NOT NULL DEFAULT 0,
                    check_threads_resolved INTEGER NOT NULL DEFAULT 0,
                    cost_usd REAL,
                    num_turns INTEGER,
                    duration_ms INTEGER,
                    artifacts TEXT,
                    dispatched_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )"#,
            )
            .execute(&pool)
            .await
            .unwrap();

            // Insert a record using the old schema
            let now = Utc::now().to_rfc3339();
            sqlx::query(
                r#"INSERT INTO dispatches (
                    id, task_slug, branch, worktree_path, session, log_file,
                    status, mode, retries, resolver, dispatched_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"#,
            )
            .bind("legacy-id")
            .bind("tasks/old-task")
            .bind("tasks--old-task")
            .bind("/tmp/old")
            .bind("old-session")
            .bind("/tmp/old.jsonl")
            .bind("running")
            .bind("implement")
            .bind(0i32)
            .bind("task")
            .bind(&now)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

            pool.close().await;
        }

        // Re-open through SqliteRegistry::open() which runs migrate_if_needed()
        let registry = SqliteRegistry::open(&db_path).await.unwrap();
        let fetched = registry.get("legacy-id").await.unwrap().unwrap();

        // Migrated columns should have sensible defaults
        assert!(!fetched.no_worktree, "no_worktree should default to false");
        assert_eq!(
            fetched.original_input, None,
            "original_input should default to None"
        );
        assert_eq!(fetched.agent_provider, "claude");
        assert!(fetched.agent_session_id.is_none());
        assert!(fetched.agent_transcript_cwd.is_none());
    }

    // ========== Queue tests ==========

    use crate::queue::{DispatchQueue, EnqueueItem, EnqueueResult, Priority, QueueInputType};

    fn sample_enqueue_item(input_value: &str) -> EnqueueItem {
        EnqueueItem {
            queue_name: "default".to_string(),
            input_type: QueueInputType::Task,
            input_value: input_value.to_string(),
            mode: None,
            priority: Priority::Medium,
            params: None,
            enqueued_by: Some("test".to_string()),
        }
    }

    #[tokio::test]
    async fn test_enqueue_and_list() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let result = registry
            .enqueue(sample_enqueue_item("tasks/foo"))
            .await
            .unwrap();
        assert!(result.is_enqueued());

        let items = registry.queue_list("default").await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].input_value, "tasks/foo");
        assert_eq!(items[0].input_type, QueueInputType::Task);
        assert_eq!(items[0].priority, 50); // Medium
    }

    #[tokio::test]
    async fn test_enqueue_dedup_pending() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let r1 = registry
            .enqueue(sample_enqueue_item("tasks/foo"))
            .await
            .unwrap();
        assert!(r1.is_enqueued());

        // Enqueue same item — should be skipped
        let r2 = registry
            .enqueue(sample_enqueue_item("tasks/foo"))
            .await
            .unwrap();
        assert!(!r2.is_enqueued());
        match r2 {
            EnqueueResult::Skipped(reason) => assert!(reason.contains("already pending")),
            _ => panic!("expected Skipped"),
        }

        // Different item should succeed
        let r3 = registry
            .enqueue(sample_enqueue_item("tasks/bar"))
            .await
            .unwrap();
        assert!(r3.is_enqueued());

        let items = registry.queue_list("default").await.unwrap();
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn test_enqueue_dedup_running_in_registry() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        // Insert a running dispatch for tasks/foo
        let mut record = sample_record("running-foo");
        record.task_slug = Some("tasks/foo".to_string());
        record.status = Status::Running;
        registry.insert(&record).await.unwrap();

        // Enqueue the same task — should be skipped (already running)
        let result = registry
            .enqueue(sample_enqueue_item("tasks/foo"))
            .await
            .unwrap();
        assert!(!result.is_enqueued());
        match result {
            EnqueueResult::Skipped(reason) => assert!(reason.contains("already running")),
            _ => panic!("expected Skipped"),
        }
    }

    #[tokio::test]
    async fn test_priority_ordering() {
        let registry = SqliteRegistry::in_memory().await.unwrap();

        // Enqueue in reverse priority order
        let mut low = sample_enqueue_item("tasks/low");
        low.priority = Priority::Low;
        let mut critical = sample_enqueue_item("tasks/critical");
        critical.priority = Priority::Critical;
        let mut high = sample_enqueue_item("tasks/high");
        high.priority = Priority::High;

        registry.enqueue(low).await.unwrap();
        registry.enqueue(critical).await.unwrap();
        registry.enqueue(high).await.unwrap();

        let items = registry.queue_list("default").await.unwrap();
        assert_eq!(items.len(), 3);
        // Should be ordered: critical (0) → high (25) → low (75)
        assert_eq!(items[0].input_value, "tasks/critical");
        assert_eq!(items[1].input_value, "tasks/high");
        assert_eq!(items[2].input_value, "tasks/low");
    }

    #[tokio::test]
    async fn test_queue_claim_and_mark() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let result = registry
            .enqueue(sample_enqueue_item("tasks/foo"))
            .await
            .unwrap();
        let id = match result {
            EnqueueResult::Enqueued { id } => id,
            _ => panic!("expected Enqueued"),
        };

        // Claim
        let claim_token = registry
            .queue_claim(&id)
            .await
            .unwrap()
            .expect("claim should succeed");
        // Double claim should fail
        assert!(registry.queue_claim(&id).await.unwrap().is_none());

        // Mark dispatched
        registry
            .queue_mark_dispatched(&id, &claim_token, "dispatch-123")
            .await
            .unwrap();

        // Should no longer appear in pending list
        let items = registry.queue_list("default").await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn test_queue_clear() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        registry
            .enqueue(sample_enqueue_item("tasks/a"))
            .await
            .unwrap();
        registry
            .enqueue(sample_enqueue_item("tasks/b"))
            .await
            .unwrap();
        registry
            .enqueue(sample_enqueue_item("tasks/c"))
            .await
            .unwrap();

        let count = registry.queue_clear("default").await.unwrap();
        assert_eq!(count, 3);

        let items = registry.queue_list("default").await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn test_queue_pending_count() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        assert_eq!(registry.queue_pending_count("default").await.unwrap(), 0);

        registry
            .enqueue(sample_enqueue_item("tasks/a"))
            .await
            .unwrap();
        registry
            .enqueue(sample_enqueue_item("tasks/b"))
            .await
            .unwrap();
        assert_eq!(registry.queue_pending_count("default").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_named_queues() {
        let registry = SqliteRegistry::in_memory().await.unwrap();

        let mut item_ci = sample_enqueue_item("tasks/ci-fix");
        item_ci.queue_name = "ci-fixes".to_string();
        registry.enqueue(item_ci).await.unwrap();

        registry
            .enqueue(sample_enqueue_item("tasks/default-work"))
            .await
            .unwrap();

        // Default queue should have 1 item
        assert_eq!(registry.queue_pending_count("default").await.unwrap(), 1);
        // ci-fixes queue should have 1 item
        assert_eq!(registry.queue_pending_count("ci-fixes").await.unwrap(), 1);

        // List each queue
        let default_items = registry.queue_list("default").await.unwrap();
        assert_eq!(default_items[0].input_value, "tasks/default-work");

        let ci_items = registry.queue_list("ci-fixes").await.unwrap();
        assert_eq!(ci_items[0].input_value, "tasks/ci-fix");
    }

    #[tokio::test]
    async fn test_queue_recover_stale_dispatching() {
        let registry = SqliteRegistry::in_memory().await.unwrap();

        // Enqueue and claim an item (simulating mid-dispatch crash)
        let result = registry
            .enqueue(sample_enqueue_item("tasks/crashed"))
            .await
            .unwrap();
        let id = match result {
            EnqueueResult::Enqueued { id } => id,
            _ => panic!("expected Enqueued"),
        };
        registry
            .queue_claim(&id)
            .await
            .unwrap()
            .expect("claim should succeed");

        // Back-date claimed_at so recovery considers it stale
        let old_time = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        sqlx::query("UPDATE dispatch_queue SET claimed_at = ?1 WHERE id = ?2")
            .bind(&old_time)
            .bind(&id)
            .execute(&registry.pool)
            .await
            .unwrap();

        // Recover should reset to pending (no dispatch_id means it never completed)
        let (recovered, completed) = registry.queue_recover(&["default"]).await.unwrap();
        assert_eq!(recovered, 1);
        assert_eq!(completed, 0);

        // Should be back in pending
        let items = registry.queue_list("default").await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].input_value, "tasks/crashed");
    }

    #[tokio::test]
    async fn test_queue_recover_skips_recent_claims() {
        let registry = SqliteRegistry::in_memory().await.unwrap();

        // Enqueue and claim an item (simulating active dispatch)
        let result = registry
            .enqueue(sample_enqueue_item("tasks/active"))
            .await
            .unwrap();
        let id = match result {
            EnqueueResult::Enqueued { id } => id,
            _ => panic!("expected Enqueued"),
        };
        registry
            .queue_claim(&id)
            .await
            .unwrap()
            .expect("claim should succeed");

        // Recent claim should NOT be recovered (staleness cutoff)
        let (recovered, completed) = registry.queue_recover(&["default"]).await.unwrap();
        assert_eq!(recovered, 0);
        assert_eq!(completed, 0);
    }

    #[tokio::test]
    async fn test_queue_recover_empty_queues() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        // Empty queue_names should return (0, 0) without SQL error
        let (recovered, completed) = registry.queue_recover(&[]).await.unwrap();
        assert_eq!(recovered, 0);
        assert_eq!(completed, 0);
    }

    #[tokio::test]
    async fn test_queue_peek_limit() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        for i in 0..5 {
            registry
                .enqueue(sample_enqueue_item(&format!("tasks/item-{}", i)))
                .await
                .unwrap();
        }

        let peeked = registry.queue_peek("default", 3).await.unwrap();
        assert_eq!(peeked.len(), 3);
    }

    #[tokio::test]
    async fn test_queue_mark_failed() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let result = registry
            .enqueue(sample_enqueue_item("tasks/will-fail"))
            .await
            .unwrap();
        let id = match result {
            EnqueueResult::Enqueued { id } => id,
            _ => panic!("expected Enqueued"),
        };

        let claim_token = registry
            .queue_claim(&id)
            .await
            .unwrap()
            .expect("claim should succeed");
        registry
            .queue_mark_failed(&id, &claim_token, "pipeline error")
            .await
            .unwrap();

        // Should not appear in pending list
        let items = registry.queue_list("default").await.unwrap();
        assert!(items.is_empty());
    }

    // ---- Work Unit tests ----

    use crate::types::{WorkUnit, WorkUnitStatus};

    fn sample_work_unit(id: &str, task_slug: Option<&str>, branch: Option<&str>) -> WorkUnit {
        WorkUnit {
            id: id.to_string(),
            task_slug: task_slug.map(|s| s.to_string()),
            branch: branch.map(|s| s.to_string()),
            repos: vec!["open-source/atc".to_string()],
            pr_urls: vec![],
            status: WorkUnitStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_work_unit_insert_and_get() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let wu = sample_work_unit(
            "wu-001",
            Some("tasks/harmony-370"),
            Some("tasks-harmony-370"),
        );
        registry.insert_work_unit(&wu).await.unwrap();

        let fetched = registry.get_work_unit("wu-001").await.unwrap().unwrap();
        assert_eq!(fetched.id, "wu-001");
        assert_eq!(fetched.task_slug.as_deref(), Some("tasks/harmony-370"));
        assert_eq!(fetched.branch.as_deref(), Some("tasks-harmony-370"));
        assert_eq!(fetched.status, WorkUnitStatus::Active);
    }

    #[tokio::test]
    async fn test_work_unit_find_by_task() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let wu = sample_work_unit(
            "wu-002",
            Some("tasks/harmony-370"),
            Some("tasks-harmony-370"),
        );
        registry.insert_work_unit(&wu).await.unwrap();

        let found = registry
            .find_work_unit_by_task("tasks/harmony-370")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, "wu-002");

        // Non-existent task returns None
        let not_found = registry
            .find_work_unit_by_task("tasks/nonexistent")
            .await
            .unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_work_unit_find_by_branch() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let wu = sample_work_unit("wu-003", None, Some("fix/rebase-msg"));
        registry.insert_work_unit(&wu).await.unwrap();

        let found = registry
            .find_work_unit_by_branch("fix/rebase-msg")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, "wu-003");
    }

    #[tokio::test]
    async fn test_work_unit_find_by_pr() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut wu = sample_work_unit("wu-004", Some("tasks/harmony-370"), None);
        wu.pr_urls = vec!["https://github.com/org/repo/pull/42".to_string()];
        registry.insert_work_unit(&wu).await.unwrap();

        let found = registry
            .find_work_unit_by_pr("https://github.com/org/repo/pull/42")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, "wu-004");

        // Non-existent PR returns None
        let not_found = registry
            .find_work_unit_by_pr("https://github.com/org/repo/pull/999")
            .await
            .unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_work_unit_status_transition() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let wu = sample_work_unit("wu-005", Some("tasks/harmony-370"), None);
        registry.insert_work_unit(&wu).await.unwrap();

        registry
            .update_work_unit_status("wu-005", WorkUnitStatus::Merged)
            .await
            .unwrap();

        let fetched = registry.get_work_unit("wu-005").await.unwrap().unwrap();
        assert_eq!(fetched.status, WorkUnitStatus::Merged);
    }

    #[tokio::test]
    async fn test_work_unit_non_active_not_found_by_task() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let wu = sample_work_unit("wu-006", Some("tasks/harmony-370"), None);
        registry.insert_work_unit(&wu).await.unwrap();

        // Transition to merged
        registry
            .update_work_unit_status("wu-006", WorkUnitStatus::Merged)
            .await
            .unwrap();

        // find_work_unit_by_task only returns active work units
        let not_found = registry
            .find_work_unit_by_task("tasks/harmony-370")
            .await
            .unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_work_unit_add_pr() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let wu = sample_work_unit("wu-007", None, None);
        registry.insert_work_unit(&wu).await.unwrap();

        registry
            .add_work_unit_pr("wu-007", "https://github.com/org/repo/pull/1")
            .await
            .unwrap();
        registry
            .add_work_unit_pr("wu-007", "https://github.com/org/repo/pull/2")
            .await
            .unwrap();
        // Dedup
        registry
            .add_work_unit_pr("wu-007", "https://github.com/org/repo/pull/1")
            .await
            .unwrap();

        let fetched = registry.get_work_unit("wu-007").await.unwrap().unwrap();
        assert_eq!(fetched.pr_urls.len(), 2);
        assert!(fetched
            .pr_urls
            .contains(&"https://github.com/org/repo/pull/1".to_string()));
        assert!(fetched
            .pr_urls
            .contains(&"https://github.com/org/repo/pull/2".to_string()));
    }

    #[tokio::test]
    async fn test_work_unit_add_repo() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let wu = sample_work_unit("wu-008", None, None);
        registry.insert_work_unit(&wu).await.unwrap();

        registry
            .add_work_unit_repo("wu-008", "platform/api")
            .await
            .unwrap();
        // Dedup existing repo
        registry
            .add_work_unit_repo("wu-008", "open-source/atc")
            .await
            .unwrap();

        let fetched = registry.get_work_unit("wu-008").await.unwrap().unwrap();
        assert_eq!(fetched.repos.len(), 2);
        assert!(fetched.repos.contains(&"open-source/atc".to_string()));
        assert!(fetched.repos.contains(&"platform/api".to_string()));
    }

    #[tokio::test]
    async fn test_work_unit_list_dispatches() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let wu = sample_work_unit("wu-009", Some("tasks/harmony-370"), None);
        registry.insert_work_unit(&wu).await.unwrap();

        let mut record = sample_record("dispatch-1");
        record.work_unit_id = Some("wu-009".to_string());
        registry.insert(&record).await.unwrap();

        let mut record2 = sample_record("dispatch-2");
        record2.work_unit_id = Some("wu-009".to_string());
        registry.insert(&record2).await.unwrap();

        // Orphan dispatch (no work unit)
        let record3 = sample_record("dispatch-3");
        registry.insert(&record3).await.unwrap();

        let dispatches = registry
            .list_dispatches_for_work_unit("wu-009")
            .await
            .unwrap();
        assert_eq!(dispatches.len(), 2);
        assert_eq!(dispatches[0].id, "dispatch-1");
        assert_eq!(dispatches[1].id, "dispatch-2");
    }

    #[tokio::test]
    async fn test_work_unit_list() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let wu1 = sample_work_unit("wu-010", Some("tasks/harmony-370"), None);
        let wu2 = sample_work_unit("wu-011", None, Some("fix/bug"));
        registry.insert_work_unit(&wu1).await.unwrap();
        registry.insert_work_unit(&wu2).await.unwrap();

        let all = registry.list_work_units().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_work_unit_list_by_ids_is_bounded_to_requested_units() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let wu1 = sample_work_unit("wu-010", Some("tasks/harmony-370"), None);
        let wu2 = sample_work_unit("wu-011", None, Some("fix/bug"));
        let wu3 = sample_work_unit("wu-012", Some("tasks/other"), None);
        registry.insert_work_unit(&wu1).await.unwrap();
        registry.insert_work_unit(&wu2).await.unwrap();
        registry.insert_work_unit(&wu3).await.unwrap();

        let bounded = registry
            .list_work_units_by_ids(&["wu-011".to_string(), "wu-010".to_string()])
            .await
            .unwrap();
        let ids: std::collections::HashSet<&str> =
            bounded.iter().map(|unit| unit.id.as_str()).collect();

        assert_eq!(ids.len(), 2);
        assert!(ids.contains("wu-010"));
        assert!(ids.contains("wu-011"));
        assert!(!ids.contains("wu-012"));
        assert!(registry
            .list_work_units_by_ids(&[])
            .await
            .unwrap()
            .is_empty());
    }
}
