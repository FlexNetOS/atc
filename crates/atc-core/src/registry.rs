use crate::types::{DispatchRecord, HealthChecks, Status};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

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
}

#[async_trait]
pub trait Registry: Send + Sync {
    async fn insert(&self, record: &DispatchRecord) -> Result<()>;
    async fn update_status(&self, id: &str, status: Status) -> Result<()>;
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
    async fn increment_retries(
        &self,
        id: &str,
        new_session: &str,
        new_log_file: &Path,
        new_dispatched_at: DateTime<Utc>,
    ) -> Result<()>;

    /// Store full artifacts JSON blob.
    async fn set_artifacts(&self, id: &str, artifacts_json: &str) -> Result<()>;

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
  mode                      TEXT NOT NULL,
  retries                   INTEGER NOT NULL DEFAULT 0,
  resolver                  TEXT NOT NULL,
  pr_url                    TEXT,
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
];

impl SqliteRegistry {
    /// Apply DDL (create table + indexes) to the pool.
    async fn apply_ddl(pool: &sqlx::SqlitePool) -> Result<()> {
        sqlx::query(CREATE_TABLE_SQL).execute(pool).await?;
        for idx_sql in CREATE_INDEXES_SQL {
            sqlx::query(idx_sql).execute(pool).await?;
        }
        Ok(())
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
        let (has_artifacts,): (i32,) = sqlx::query_as(
            "SELECT COUNT(*) FROM pragma_table_info('dispatches') WHERE name = 'artifacts'",
        )
        .fetch_one(pool)
        .await?;
        if has_artifacts == 0 {
            sqlx::query("ALTER TABLE dispatches ADD COLUMN artifacts TEXT")
                .execute(pool)
                .await
                .ok(); // Ignore error if table doesn't exist yet (apply_ddl will create it)
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

    fn row_to_record(row: &sqlx::sqlite::SqliteRow) -> Result<DispatchRecord> {
        use sqlx::Row;

        let status_str: String = row.get("status");
        let mode_str: String = row.get("mode");
        let dispatched_at_str: String = row.get("dispatched_at");
        let updated_at_str: String = row.get("updated_at");
        let worktree_str: String = row.get("worktree_path");
        let log_file_str: String = row.get("log_file");

        Ok(DispatchRecord {
            id: row.get("id"),
            task_slug: row.get("task_slug"),
            branch: row.get("branch"),
            worktree_path: PathBuf::from(worktree_str),
            session: row.get("session"),
            log_file: PathBuf::from(log_file_str),
            status: status_str.parse()?,
            mode: mode_str.parse()?,
            retries: u32::try_from(row.get::<i32, _>("retries"))
                .map_err(|_| anyhow::anyhow!("invalid retries value in database"))?,
            resolver: row.get("resolver"),
            pr_url: row.get("pr_url"),
            checks: HealthChecks {
                agent_exited_clean: row.get::<i32, _>("check_agent_exited_clean") != 0,
                branch_pushed: row.get::<i32, _>("check_branch_pushed") != 0,
                pr_created: row.get::<i32, _>("check_pr_created") != 0,
                ci_passed: row.get::<i32, _>("check_ci_passed") != 0,
                reviews_approved: row.get::<i32, _>("check_reviews_approved") != 0,
                threads_resolved: row.get::<i32, _>("check_threads_resolved") != 0,
            },
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
            dispatched_at: DateTime::parse_from_rfc3339(&dispatched_at_str)?.with_timezone(&Utc),
            updated_at: DateTime::parse_from_rfc3339(&updated_at_str)?.with_timezone(&Utc),
        })
    }
}

#[async_trait]
impl Registry for SqliteRegistry {
    async fn insert(&self, record: &DispatchRecord) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO dispatches (
                id, task_slug, branch, worktree_path, session, log_file, status, mode, retries,
                resolver, pr_url, check_agent_exited_clean, check_branch_pushed, check_pr_created,
                check_ci_passed, check_reviews_approved, check_threads_resolved,
                cost_usd, num_turns, duration_ms, dispatched_at, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                ?18, ?19, ?20, ?21, ?22
            )"#,
        )
        .bind(&record.id)
        .bind(&record.task_slug)
        .bind(&record.branch)
        .bind(
            record
                .worktree_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("worktree_path must be valid UTF-8"))?,
        )
        .bind(&record.session)
        .bind(
            record
                .log_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("log_file must be valid UTF-8"))?,
        )
        .bind(record.status.as_str())
        .bind(record.mode.as_str())
        .bind(i32::try_from(record.retries).map_err(|_| anyhow::anyhow!("retries overflows i32"))?)
        .bind(&record.resolver)
        .bind(&record.pr_url)
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
        .bind(record.dispatched_at.to_rfc3339())
        .bind(record.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
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
        let now = Utc::now().to_rfc3339();
        let result =
            sqlx::query("UPDATE dispatches SET pr_url = ?1, updated_at = ?2 WHERE id = ?3")
                .bind(url)
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
                cost_usd = NULL,
                num_turns = NULL,
                duration_ms = NULL
            WHERE id = ?5"#,
        )
        .bind(new_session)
        .bind(
            new_log_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("new_log_file must be valid UTF-8"))?,
        )
        .bind(new_dispatched_at.to_rfc3339())
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
        let rows = sqlx::query(
            "SELECT * FROM dispatches WHERE pr_url = ?1 ORDER BY dispatched_at DESC, id DESC",
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DispatchRecord, HealthChecks, Mode, Status};
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
            mode: Mode::Implement,
            retries: 0,
            resolver: "task".to_string(),
            pr_url: None,
            checks: HealthChecks::default(),
            cost_usd: None,
            num_turns: None,
            duration_ms: None,
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
        assert_eq!(fetched.mode, Mode::Implement);
        assert_eq!(fetched.retries, 0);
        assert_eq!(fetched.resolver, "task");
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
        r3.mode = Mode::ReviewFix;

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
        assert_eq!(
            fetched.pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/1")
        );
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
        record.pr_url = Some("https://github.com/org/repo/pull/99".to_string());
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
        registry.insert(&record).await.unwrap();

        let fetched = registry.get("full-test").await.unwrap().unwrap();
        assert_eq!(fetched.pr_url, record.pr_url);
        assert_eq!(fetched.cost_usd, record.cost_usd);
        assert_eq!(fetched.num_turns, record.num_turns);
        assert_eq!(fetched.duration_ms, record.duration_ms);
        assert_eq!(fetched.checks, record.checks);
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
        r1.pr_url = Some("https://github.com/org/repo/pull/1".to_string());
        let mut r2 = sample_record("id-2");
        r2.pr_url = Some("https://github.com/org/repo/pull/1".to_string());
        let mut r3 = sample_record("id-3");
        r3.pr_url = None;

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
    async fn test_all_mode_variants_round_trip() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let modes = [
            Mode::Implement,
            Mode::Research,
            Mode::KbUpdate,
            Mode::ReviewFix,
            Mode::PrComments,
            Mode::Refine,
            Mode::CreateTask,
            Mode::Close,
        ];
        for (i, mode) in modes.iter().enumerate() {
            let id = format!("mode-{i}");
            let mut record = sample_record(&id);
            record.mode = mode.clone();
            registry.insert(&record).await.unwrap();
            let fetched = registry.get(&id).await.unwrap().unwrap();
            assert_eq!(&fetched.mode, mode);
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
        assert_eq!(fetched.pr_url, None);
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
}
