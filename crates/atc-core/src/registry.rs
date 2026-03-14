use crate::types::{DispatchRecord, HealthChecks, Status};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::path::PathBuf;

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
    async fn update_status(&self, slug: &str, status: Status) -> Result<()>;
    async fn update_checks(&self, slug: &str, checks: &HealthChecks) -> Result<()>;
    async fn update_cost(&self, slug: &str, cost: f64, turns: u32, duration_ms: u64) -> Result<()>;
    async fn get(&self, slug: &str) -> Result<Option<DispatchRecord>>;
    async fn list(&self, filter: StatusFilter) -> Result<Vec<DispatchRecord>>;
    /// Atomically update health checks, status, and updated_at in a single write.
    async fn update_health(
        &self,
        slug: &str,
        checks: &HealthChecks,
        status: Status,
        updated_at: DateTime<Utc>,
    ) -> Result<()>;
    async fn set_pr_url(&self, slug: &str, url: &str) -> Result<()>;
    async fn increment_retries(
        &self,
        slug: &str,
        new_session: &str,
        new_log_file: &std::path::Path,
        new_dispatched_at: DateTime<Utc>,
    ) -> Result<()>;
}

pub struct SqliteRegistry {
    pool: sqlx::SqlitePool,
}

const CREATE_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS dispatches (
  slug                      TEXT PRIMARY KEY,
  branch                    TEXT NOT NULL,
  worktree_path             TEXT NOT NULL,
  session                   TEXT NOT NULL,
  log_file                  TEXT NOT NULL,
  status                    TEXT NOT NULL DEFAULT 'running',
  mode                      TEXT NOT NULL,
  retries                   INTEGER NOT NULL DEFAULT 0,
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
  dispatched_at             TEXT NOT NULL,
  updated_at                TEXT NOT NULL
);
"#;

const CREATE_INDEX_SQL: &str =
    "CREATE INDEX IF NOT EXISTS idx_dispatches_status ON dispatches(status);";

impl SqliteRegistry {
    /// Apply DDL (create table + index) to the pool.
    async fn apply_ddl(pool: &sqlx::SqlitePool) -> Result<()> {
        sqlx::query(CREATE_TABLE_SQL).execute(pool).await?;
        sqlx::query(CREATE_INDEX_SQL).execute(pool).await?;
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
            slug: row.get("slug"),
            branch: row.get("branch"),
            worktree_path: PathBuf::from(worktree_str),
            session: row.get("session"),
            log_file: PathBuf::from(log_file_str),
            status: status_str.parse()?,
            mode: mode_str.parse()?,
            retries: u32::try_from(row.get::<i32, _>("retries"))
                .map_err(|_| anyhow::anyhow!("invalid retries value in database"))?,
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
                slug, branch, worktree_path, session, log_file, status, mode, retries,
                pr_url, check_agent_exited_clean, check_branch_pushed, check_pr_created,
                check_ci_passed, check_reviews_approved, check_threads_resolved,
                cost_usd, num_turns, duration_ms, dispatched_at, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20
            )"#,
        )
        .bind(&record.slug)
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

    async fn update_status(&self, slug: &str, status: Status) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let result =
            sqlx::query("UPDATE dispatches SET status = ?1, updated_at = ?2 WHERE slug = ?3")
                .bind(status.as_str())
                .bind(&now)
                .bind(slug)
                .execute(&self.pool)
                .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for slug: {slug}"
        );
        Ok(())
    }

    async fn update_checks(&self, slug: &str, checks: &HealthChecks) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            r#"UPDATE dispatches SET
                check_agent_exited_clean = ?1,
                check_branch_pushed = ?2,
                check_pr_created = ?3,
                check_ci_passed = ?4,
                check_reviews_approved = ?5,
                check_threads_resolved = ?6,
                updated_at = ?7
            WHERE slug = ?8"#,
        )
        .bind(checks.agent_exited_clean as i32)
        .bind(checks.branch_pushed as i32)
        .bind(checks.pr_created as i32)
        .bind(checks.ci_passed as i32)
        .bind(checks.reviews_approved as i32)
        .bind(checks.threads_resolved as i32)
        .bind(&now)
        .bind(slug)
        .execute(&self.pool)
        .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for slug: {slug}"
        );
        Ok(())
    }

    async fn update_cost(&self, slug: &str, cost: f64, turns: u32, duration_ms: u64) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE dispatches SET cost_usd = ?1, num_turns = ?2, duration_ms = ?3, updated_at = ?4 WHERE slug = ?5",
        )
        .bind(cost)
        .bind(i32::try_from(turns).map_err(|_| anyhow::anyhow!("turns overflows i32"))?)
        .bind(i64::try_from(duration_ms).map_err(|_| anyhow::anyhow!("duration_ms overflows i64"))?)
        .bind(&now)
        .bind(slug)
        .execute(&self.pool)
        .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for slug: {slug}"
        );
        Ok(())
    }

    async fn get(&self, slug: &str) -> Result<Option<DispatchRecord>> {
        let row = sqlx::query("SELECT * FROM dispatches WHERE slug = ?1")
            .bind(slug)
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
                sqlx::query("SELECT * FROM dispatches ORDER BY dispatched_at DESC")
                    .fetch_all(&self.pool)
                    .await?
            }
            StatusFilter::One(status) => {
                sqlx::query(
                    "SELECT * FROM dispatches WHERE status = ?1 ORDER BY dispatched_at DESC",
                )
                .bind(status.as_str())
                .fetch_all(&self.pool)
                .await?
            }
            StatusFilter::Any(statuses) => {
                if statuses.is_empty() {
                    return Ok(Vec::new());
                }
                // Build parameterised IN clause: WHERE status IN (?1, ?2, ...)
                let placeholders: Vec<String> =
                    (1..=statuses.len()).map(|i| format!("?{i}")).collect();
                let sql = format!(
                    "SELECT * FROM dispatches WHERE status IN ({}) ORDER BY dispatched_at DESC",
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
        slug: &str,
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
            WHERE slug = ?9"#,
        )
        .bind(checks.agent_exited_clean as i32)
        .bind(checks.branch_pushed as i32)
        .bind(checks.pr_created as i32)
        .bind(checks.ci_passed as i32)
        .bind(checks.reviews_approved as i32)
        .bind(checks.threads_resolved as i32)
        .bind(status.as_str())
        .bind(updated_at.to_rfc3339())
        .bind(slug)
        .execute(&self.pool)
        .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for slug: {slug}"
        );
        Ok(())
    }

    async fn set_pr_url(&self, slug: &str, url: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let result =
            sqlx::query("UPDATE dispatches SET pr_url = ?1, updated_at = ?2 WHERE slug = ?3")
                .bind(url)
                .bind(&now)
                .bind(slug)
                .execute(&self.pool)
                .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for slug: {slug}"
        );
        Ok(())
    }

    async fn increment_retries(
        &self,
        slug: &str,
        new_session: &str,
        new_log_file: &std::path::Path,
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
            WHERE slug = ?5"#,
        )
        .bind(new_session)
        .bind(
            new_log_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("new_log_file must be valid UTF-8"))?,
        )
        .bind(new_dispatched_at.to_rfc3339())
        .bind(&now)
        .bind(slug)
        .execute(&self.pool)
        .await?;
        anyhow::ensure!(
            result.rows_affected() > 0,
            "no dispatch record found for slug: {slug}"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DispatchRecord, HealthChecks, Mode, Status};
    use chrono::{DateTime, Utc};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn sample_record(slug: &str) -> DispatchRecord {
        DispatchRecord {
            slug: slug.to_string(),
            branch: format!("{}-branch", slug.replace('/', "-")),
            worktree_path: PathBuf::from("/tmp/test-worktree"),
            session: format!("{}@implement@1234567890", slug.replace('/', "-")),
            log_file: PathBuf::from("/tmp/test.jsonl"),
            status: Status::Running,
            mode: Mode::Implement,
            retries: 0,
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
        let record = sample_record("tasks/gitkb-42");
        registry.insert(&record).await.unwrap();
        let fetched = registry.get("tasks/gitkb-42").await.unwrap().unwrap();
        assert_eq!(fetched.slug, "tasks/gitkb-42");
        assert_eq!(fetched.status, Status::Running);
        assert_eq!(fetched.mode, Mode::Implement);
        assert_eq!(fetched.retries, 0);
    }

    #[tokio::test]
    async fn test_update_status() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        registry
            .insert(&sample_record("tasks/gitkb-42"))
            .await
            .unwrap();
        registry
            .update_status("tasks/gitkb-42", Status::Done)
            .await
            .unwrap();
        let fetched = registry.get("tasks/gitkb-42").await.unwrap().unwrap();
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
        registry
            .insert(&sample_record("tasks/gitkb-1"))
            .await
            .unwrap();
        registry
            .insert(&sample_record("tasks/gitkb-2"))
            .await
            .unwrap();
        registry
            .update_status("tasks/gitkb-2", Status::Done)
            .await
            .unwrap();

        let all = registry.list(StatusFilter::all()).await.unwrap();
        assert_eq!(all.len(), 2);

        let running = registry
            .list(StatusFilter::by_status(Status::Running))
            .await
            .unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].slug, "tasks/gitkb-1");
    }

    #[tokio::test]
    async fn test_update_checks() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        registry
            .insert(&sample_record("tasks/gitkb-42"))
            .await
            .unwrap();

        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: false,
            reviews_approved: false,
            threads_resolved: false,
        };
        registry
            .update_checks("tasks/gitkb-42", &checks)
            .await
            .unwrap();

        let fetched = registry.get("tasks/gitkb-42").await.unwrap().unwrap();
        assert!(fetched.checks.agent_exited_clean);
        assert!(fetched.checks.branch_pushed);
        assert!(fetched.checks.pr_created);
        assert!(!fetched.checks.ci_passed);
        assert!(!fetched.checks.reviews_approved);
        assert!(!fetched.checks.threads_resolved);
    }

    #[tokio::test]
    async fn test_update_cost() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        registry
            .insert(&sample_record("tasks/gitkb-42"))
            .await
            .unwrap();

        registry
            .update_cost("tasks/gitkb-42", 1.23, 15, 45000)
            .await
            .unwrap();

        let fetched = registry.get("tasks/gitkb-42").await.unwrap().unwrap();
        assert_eq!(fetched.cost_usd, Some(1.23));
        assert_eq!(fetched.num_turns, Some(15));
        assert_eq!(fetched.duration_ms, Some(45000));
    }

    #[tokio::test]
    async fn test_set_pr_url() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        registry
            .insert(&sample_record("tasks/gitkb-42"))
            .await
            .unwrap();

        registry
            .set_pr_url("tasks/gitkb-42", "https://github.com/org/repo/pull/1")
            .await
            .unwrap();

        let fetched = registry.get("tasks/gitkb-42").await.unwrap().unwrap();
        assert_eq!(
            fetched.pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/1")
        );
    }

    #[tokio::test]
    async fn test_round_trip_with_all_optional_fields() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut record = sample_record("tasks/gitkb-42");
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

        let fetched = registry.get("tasks/gitkb-42").await.unwrap().unwrap();
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
        let result = registry.get("tasks/does-not-exist").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_update_status_nonexistent_errors() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let err = registry
            .update_status("tasks/no-such-slug", Status::Done)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no dispatch record found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_update_checks_nonexistent_errors() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let err = registry
            .update_checks("tasks/no-such-slug", &HealthChecks::default())
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
            .update_cost("tasks/no-such-slug", 1.0, 1, 1000)
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
            .set_pr_url("tasks/no-such-slug", "https://example.com")
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
                "tasks/no-such-slug",
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
        registry
            .insert(&sample_record("tasks/gitkb-42"))
            .await
            .unwrap();
        let err = registry
            .insert(&sample_record("tasks/gitkb-42"))
            .await
            .unwrap_err();
        // SQLite UNIQUE constraint violation
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
        // Insert records with different statuses
        let mut r1 = sample_record("tasks/running-1");
        r1.status = Status::Running;
        let mut r2 = sample_record("tasks/done-1");
        r2.status = Status::Done;
        let mut r3 = sample_record("tasks/needs-review-1");
        r3.status = Status::NeedsReview;
        let mut r4 = sample_record("tasks/failed-1");
        r4.status = Status::Failed;
        let mut r5 = sample_record("tasks/needs-human-1");
        r5.status = Status::NeedsHuman;

        for r in [&r1, &r2, &r3, &r4, &r5] {
            registry.insert(r).await.unwrap();
        }

        // Query running + needs-review (the health check query)
        let active = registry
            .list(StatusFilter::any(vec![
                Status::Running,
                Status::NeedsReview,
            ]))
            .await
            .unwrap();
        assert_eq!(active.len(), 2);
        let slugs: Vec<&str> = active.iter().map(|r| r.slug.as_str()).collect();
        assert!(slugs.contains(&"tasks/running-1"));
        assert!(slugs.contains(&"tasks/needs-review-1"));

        // Query single status via Any
        let done_only = registry
            .list(StatusFilter::any(vec![Status::Done]))
            .await
            .unwrap();
        assert_eq!(done_only.len(), 1);
        assert_eq!(done_only[0].slug, "tasks/done-1");

        // Query all terminal statuses
        let terminal = registry
            .list(StatusFilter::any(vec![Status::Done, Status::Failed]))
            .await
            .unwrap();
        assert_eq!(terminal.len(), 2);

        // Empty vec returns empty
        let empty = registry.list(StatusFilter::any(vec![])).await.unwrap();
        assert!(empty.is_empty());
    }

    // --- Security / red-team tests ---

    #[tokio::test]
    async fn test_sql_injection_in_slug() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let malicious_slug = "'; DROP TABLE dispatches; --";
        let mut record = sample_record(malicious_slug);
        record.branch = "safe-branch".to_string();
        registry.insert(&record).await.unwrap();

        // Table still exists and record round-trips
        let fetched = registry.get(malicious_slug).await.unwrap().unwrap();
        assert_eq!(fetched.slug, malicious_slug);

        // Other operations still work
        let all = registry.list(StatusFilter::all()).await.unwrap();
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn test_unicode_slug_round_trip() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let unicode_slug = "tasks/日本語-émojis-🚀";
        registry.insert(&sample_record(unicode_slug)).await.unwrap();
        let fetched = registry.get(unicode_slug).await.unwrap().unwrap();
        assert_eq!(fetched.slug, unicode_slug);
    }

    #[tokio::test]
    async fn test_very_long_slug() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let long_slug: String = "a".repeat(4096);
        registry.insert(&sample_record(&long_slug)).await.unwrap();
        let fetched = registry.get(&long_slug).await.unwrap().unwrap();
        assert_eq!(fetched.slug, long_slug);
    }

    #[tokio::test]
    async fn test_empty_slug() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        registry.insert(&sample_record("")).await.unwrap();
        let fetched = registry.get("").await.unwrap().unwrap();
        assert_eq!(fetched.slug, "");
    }

    #[tokio::test]
    async fn test_path_traversal_stored_literally() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        let mut record = sample_record("tasks/traversal");
        record.worktree_path = PathBuf::from("../../etc/passwd");
        registry.insert(&record).await.unwrap();
        let fetched = registry.get("tasks/traversal").await.unwrap().unwrap();
        // Path should be stored as-is, no resolution
        assert_eq!(fetched.worktree_path, PathBuf::from("../../etc/passwd"));
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
        ];
        for (i, status) in statuses.iter().enumerate() {
            let slug = format!("tasks/status-{i}");
            let mut record = sample_record(&slug);
            record.status = status.clone();
            registry.insert(&record).await.unwrap();
            let fetched = registry.get(&slug).await.unwrap().unwrap();
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
        ];
        for (i, mode) in modes.iter().enumerate() {
            let slug = format!("tasks/mode-{i}");
            let mut record = sample_record(&slug);
            record.mode = mode.clone();
            registry.insert(&record).await.unwrap();
            let fetched = registry.get(&slug).await.unwrap().unwrap();
            assert_eq!(&fetched.mode, mode);
        }
    }

    #[tokio::test]
    async fn test_increment_retries() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        registry
            .insert(&sample_record("tasks/gitkb-42"))
            .await
            .unwrap();
        registry
            .update_status("tasks/gitkb-42", Status::Failed)
            .await
            .unwrap();

        let new_time = Utc::now();
        registry
            .increment_retries(
                "tasks/gitkb-42",
                "new-session",
                &PathBuf::from("/tmp/retry.jsonl"),
                new_time,
            )
            .await
            .unwrap();

        let fetched = registry.get("tasks/gitkb-42").await.unwrap().unwrap();
        assert_eq!(fetched.retries, 1);
        assert_eq!(fetched.status, Status::Running);
        assert_eq!(fetched.session, "new-session");
        // Verify per-attempt state was reset
        assert!(!fetched.checks.agent_exited_clean);
        assert!(!fetched.checks.branch_pushed);
        assert!(!fetched.checks.pr_created);
        assert!(!fetched.checks.ci_passed);
        assert!(!fetched.checks.reviews_approved);
        assert!(!fetched.checks.threads_resolved);
        assert_eq!(fetched.pr_url, None);
        assert_eq!(fetched.cost_usd, None);
        assert_eq!(fetched.num_turns, None);
        assert_eq!(fetched.duration_ms, None);
    }

    #[tokio::test]
    async fn test_increment_retries_clears_pr_url() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        registry
            .insert(&sample_record("tasks/gitkb-42"))
            .await
            .unwrap();
        // Set a pr_url from a previous attempt
        registry
            .set_pr_url("tasks/gitkb-42", "https://github.com/org/repo/pull/1")
            .await
            .unwrap();
        let before = registry.get("tasks/gitkb-42").await.unwrap().unwrap();
        assert!(before.pr_url.is_some());

        // Retry should clear pr_url
        registry
            .increment_retries(
                "tasks/gitkb-42",
                "retry-session",
                &PathBuf::from("/tmp/retry.jsonl"),
                Utc::now(),
            )
            .await
            .unwrap();

        let after = registry.get("tasks/gitkb-42").await.unwrap().unwrap();
        assert_eq!(after.pr_url, None, "pr_url should be cleared on retry");
    }

    // --- Timestamp advancement tests ---

    #[tokio::test]
    async fn test_updated_at_advances_on_mutation() {
        let registry = SqliteRegistry::in_memory().await.unwrap();
        registry
            .insert(&sample_record("tasks/gitkb-42"))
            .await
            .unwrap();
        let before = registry.get("tasks/gitkb-42").await.unwrap().unwrap();

        // Small sleep to ensure clock advances
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        registry
            .update_status("tasks/gitkb-42", Status::Done)
            .await
            .unwrap();
        let after = registry.get("tasks/gitkb-42").await.unwrap().unwrap();

        assert!(
            after.updated_at > before.updated_at,
            "updated_at should advance: before={}, after={}",
            before.updated_at,
            after.updated_at
        );
    }

    // --- List ordering tests ---

    #[tokio::test]
    async fn test_list_ordered_by_dispatched_at_desc() {
        let registry = SqliteRegistry::in_memory().await.unwrap();

        // Insert records with known ordering: older first
        let mut older = sample_record("tasks/older");
        older.dispatched_at = DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut newer = sample_record("tasks/newer");
        newer.dispatched_at = DateTime::parse_from_rfc3339("2025-06-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        // Insert older first, newer second
        registry.insert(&older).await.unwrap();
        registry.insert(&newer).await.unwrap();

        let all = registry.list(StatusFilter::all()).await.unwrap();
        assert_eq!(all.len(), 2);
        // DESC order: newer should be first
        assert_eq!(all[0].slug, "tasks/newer");
        assert_eq!(all[1].slug, "tasks/older");
    }

    // --- Serde JSON round-trip tests ---

    #[tokio::test]
    async fn test_dispatch_record_serde_json_round_trip() {
        let mut record = sample_record("tasks/gitkb-42");
        record.pr_url = Some("https://github.com/org/repo/pull/1".to_string());
        record.cost_usd = Some(2.50);
        record.num_turns = Some(10);
        record.duration_ms = Some(60_000);

        let json = serde_json::to_string(&record).unwrap();
        let deserialized: DispatchRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.slug, record.slug);
        assert_eq!(deserialized.status, record.status);
        assert_eq!(deserialized.mode, record.mode);
        assert_eq!(deserialized.pr_url, record.pr_url);
        assert_eq!(deserialized.cost_usd, record.cost_usd);
        assert_eq!(deserialized.num_turns, record.num_turns);
        assert_eq!(deserialized.duration_ms, record.duration_ms);
        assert_eq!(deserialized.checks, record.checks);
    }

    #[test]
    fn test_status_serde_kebab_case() {
        let json = serde_json::to_string(&Status::NeedsReview).unwrap();
        assert_eq!(json, "\"needs-review\"");
        let deserialized: Status = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, Status::NeedsReview);
    }

    #[test]
    fn test_mode_serde_kebab_case() {
        let json = serde_json::to_string(&Mode::KbUpdate).unwrap();
        assert_eq!(json, "\"kb-update\"");
        let deserialized: Mode = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, Mode::KbUpdate);
    }

    // --- Concurrent access test ---

    #[tokio::test]
    async fn test_concurrent_inserts() {
        let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
        let mut handles = Vec::new();

        for i in 0..10 {
            let reg = Arc::clone(&registry);
            handles.push(tokio::spawn(async move {
                let slug = format!("tasks/concurrent-{i}");
                reg.insert(&sample_record(&slug)).await.unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let all = registry.list(StatusFilter::all()).await.unwrap();
        assert_eq!(all.len(), 10);
    }
}
