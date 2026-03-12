use crate::types::{DispatchRecord, HealthChecks, Status};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::path::PathBuf;

/// Filter passed to `Registry::list`.
#[derive(Debug, Default)]
pub struct StatusFilter {
    pub status: Option<Status>,
}

impl StatusFilter {
    pub fn all() -> Self {
        Self { status: None }
    }
    pub fn by_status(status: Status) -> Self {
        Self {
            status: Some(status),
        }
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
    /// Open (or create) the SQLite database at `path`.
    /// Applies DDL on first open. Enables WAL mode on every open.
    pub async fn open(path: &std::path::Path) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let url = format!("sqlite:{}?mode=rwc", path.display());
        let pool = sqlx::SqlitePool::connect(&url).await?;

        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await?;
        sqlx::query(CREATE_TABLE_SQL).execute(&pool).await?;
        sqlx::query(CREATE_INDEX_SQL).execute(&pool).await?;

        Ok(Self { pool })
    }

    /// In-memory instance for unit tests.
    pub async fn in_memory() -> Result<Self> {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await?;

        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await?;
        sqlx::query(CREATE_TABLE_SQL).execute(&pool).await?;
        sqlx::query(CREATE_INDEX_SQL).execute(&pool).await?;

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
            retries: row.get::<i32, _>("retries") as u32,
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
            num_turns: row.get::<Option<i32>, _>("num_turns").map(|v| v as u32),
            duration_ms: row.get::<Option<i64>, _>("duration_ms").map(|v| v as u64),
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
        .bind(record.worktree_path.to_string_lossy().as_ref())
        .bind(&record.session)
        .bind(record.log_file.to_string_lossy().as_ref())
        .bind(record.status.as_str())
        .bind(record.mode.as_str())
        .bind(record.retries as i32)
        .bind(&record.pr_url)
        .bind(record.checks.agent_exited_clean as i32)
        .bind(record.checks.branch_pushed as i32)
        .bind(record.checks.pr_created as i32)
        .bind(record.checks.ci_passed as i32)
        .bind(record.checks.reviews_approved as i32)
        .bind(record.checks.threads_resolved as i32)
        .bind(record.cost_usd)
        .bind(record.num_turns.map(|v| v as i32))
        .bind(record.duration_ms.map(|v| v as i64))
        .bind(record.dispatched_at.to_rfc3339())
        .bind(record.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update_status(&self, slug: &str, status: Status) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE dispatches SET status = ?1, updated_at = ?2 WHERE slug = ?3")
            .bind(status.as_str())
            .bind(&now)
            .bind(slug)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_checks(&self, slug: &str, checks: &HealthChecks) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
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
        Ok(())
    }

    async fn update_cost(&self, slug: &str, cost: f64, turns: u32, duration_ms: u64) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE dispatches SET cost_usd = ?1, num_turns = ?2, duration_ms = ?3, updated_at = ?4 WHERE slug = ?5",
        )
        .bind(cost)
        .bind(turns as i32)
        .bind(duration_ms as i64)
        .bind(&now)
        .bind(slug)
        .execute(&self.pool)
        .await?;
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
        let rows = if let Some(ref status) = filter.status {
            sqlx::query("SELECT * FROM dispatches WHERE status = ?1 ORDER BY dispatched_at DESC")
                .bind(status.as_str())
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query("SELECT * FROM dispatches ORDER BY dispatched_at DESC")
                .fetch_all(&self.pool)
                .await?
        };

        rows.iter().map(Self::row_to_record).collect()
    }

    async fn set_pr_url(&self, slug: &str, url: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE dispatches SET pr_url = ?1, updated_at = ?2 WHERE slug = ?3")
            .bind(url)
            .bind(&now)
            .bind(slug)
            .execute(&self.pool)
            .await?;
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
        sqlx::query(
            r#"UPDATE dispatches SET
                retries = retries + 1,
                session = ?1,
                log_file = ?2,
                status = 'running',
                dispatched_at = ?3,
                updated_at = ?4
            WHERE slug = ?5"#,
        )
        .bind(new_session)
        .bind(new_log_file.to_string_lossy().as_ref())
        .bind(new_dispatched_at.to_rfc3339())
        .bind(&now)
        .bind(slug)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DispatchRecord, HealthChecks, Mode, Status};
    use chrono::Utc;
    use std::path::PathBuf;

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
    }
}
