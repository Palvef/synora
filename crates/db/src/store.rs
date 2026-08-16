//! Store: all engine↔database interaction. M1 is SQLite-only; the Pg backend
//! (M3) implements the same methods.

use crate::sqlite::{DbError, DbResult, DbValue, Param, SqliteDb};
use synora_core::job::{JobSpec, JobStatus};

#[derive(Debug, Clone)]
pub struct ScheduleRow {
    pub schedule_json: String,
    pub timezone: String,
    pub misfire_policy: String,
    pub next_run: Option<i64>,
    pub anchor_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: String,
    pub job_id: String,
    pub worker_id: Option<String>,
    pub status: JobStatus,
    pub retry_count: u32,
    pub next_retry_at: Option<i64>,
    pub created_at: i64,
}

pub struct Store {
    db: SqliteDb,
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn cell<'a>(row: &'a [(String, DbValue)], name: &str) -> Option<&'a DbValue> {
    row.iter().find(|(n, _)| n == name).map(|(_, v)| v)
}

impl Store {
    pub fn new(db: SqliteDb) -> Self {
        Store { db }
    }

    pub fn db(&self) -> &SqliteDb {
        &self.db
    }

    // --- jobs / schedules -------------------------------------------------

    /// Insert or update a job definition from config (spec: config is the
    /// source of job definitions; DB mirrors it with runtime state).
    pub async fn sync_job(&self, job: &JobSpec) -> DbResult<()> {
        let provider = match &job.provider {
            synora_core::ProviderConfig::Rsync { .. } => "rsync",
            synora_core::ProviderConfig::Script { .. } => "script",
            synora_core::ProviderConfig::Docker { .. } => "docker",
        };
        let provider_config =
            serde_json::to_string(&job.provider).map_err(|e| DbError::Sql(e.to_string()))?;
        let success_codes =
            serde_json::to_string(&job.success_exit_codes).map_err(|e| DbError::Sql(e.to_string()))?;
        let resources =
            serde_json::to_string(&job.resources).map_err(|e| DbError::Sql(e.to_string()))?;
        let timeout_secs = job.timeout.whole_seconds().max(1);
        let retry_delay_secs = job.retry_delay.whole_seconds().max(1);
        self.db
            .execute(
                "INSERT INTO jobs (name, enabled, worker, provider, provider_config, upstream,
                                   storage_path, timeout_secs, retry, retry_delay_secs,
                                   retry_backoff, success_exit_codes, fail_on_match,
                                   max_concurrency, on_worker_lost, statistics, resources,
                                   priority, status, updated_at)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(name) DO UPDATE SET
                   enabled=excluded.enabled, worker=excluded.worker, provider=excluded.provider,
                   provider_config=excluded.provider_config, upstream=excluded.upstream,
                   storage_path=excluded.storage_path, timeout_secs=excluded.timeout_secs,
                   retry=excluded.retry, retry_delay_secs=excluded.retry_delay_secs,
                   retry_backoff=excluded.retry_backoff, success_exit_codes=excluded.success_exit_codes,
                   fail_on_match=excluded.fail_on_match, max_concurrency=excluded.max_concurrency,
                   on_worker_lost=excluded.on_worker_lost, statistics=excluded.statistics,
                   resources=excluded.resources, priority=excluded.priority,
                   updated_at=excluded.updated_at",
                &[
                    job.name.clone().into(),
                    (job.enabled as i64).into(),
                    job.worker.clone().into(),
                    provider.into(),
                    provider_config.into(),
                    job.upstream.clone().into(),
                    job.storage.display().to_string().into(),
                    timeout_secs.into(),
                    (job.retry as i64).into(),
                    retry_delay_secs.into(),
                    job.retry_backoff.into(),
                    success_codes.into(),
                    job.fail_on_match.clone().into(),
                    (job.max_concurrency as i64).into(),
                    match job.on_worker_lost {
                        synora_core::OnWorkerLost::Retry => "retry",
                        synora_core::OnWorkerLost::Fail => "fail",
                    }
                    .into(),
                    match job.statistics {
                        synora_core::StatisticsMode::Provider => "provider",
                        synora_core::StatisticsMode::Filesystem => "filesystem",
                    }
                    .into(),
                    resources.into(),
                    (job.priority as i64).into(),
                    JobStatus::Pending.to_db().into(),
                    unix_now().into(),
                ],
            )
            .await?;
        Ok(())
    }

    /// Upsert the schedule row, keeping the persisted anchor (interval
    /// alignment must survive restarts).
    pub async fn sync_schedule(
        &self,
        job_name: &str,
        schedule_json: &str,
        timezone: &str,
        misfire_policy: &str,
        next_run: Option<i64>,
        anchor: Option<i64>,
    ) -> DbResult<()> {
        self.db
            .execute(
                "INSERT INTO schedules (job_name, schedule_json, timezone, misfire_policy,
                                        next_run, anchor_at, created_at)
                 VALUES (?,?,?,?,?,?,?)
                 ON CONFLICT(job_name) DO UPDATE SET
                   schedule_json=excluded.schedule_json, timezone=excluded.timezone,
                   misfire_policy=excluded.misfire_policy, next_run=excluded.next_run,
                   anchor_at=COALESCE(excluded.anchor_at, schedules.anchor_at)",
                &[
                    job_name.into(),
                    schedule_json.into(),
                    timezone.into(),
                    misfire_policy.into(),
                    next_run.into(),
                    anchor.into(),
                    unix_now().into(),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn get_schedule(&self, job_name: &str) -> DbResult<Option<ScheduleRow>> {
        let rows = self
            .db
            .query(
                "SELECT schedule_json, timezone, misfire_policy, next_run, anchor_at
                 FROM schedules WHERE job_name = ?",
                &[job_name.into()],
            )
            .await?;
        Ok(rows.first().map(|r| ScheduleRow {
            schedule_json: cell(r, "schedule_json").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            timezone: cell(r, "timezone").and_then(|v| v.as_str()).unwrap_or("UTC").to_string(),
            misfire_policy: cell(r, "misfire_policy")
                .and_then(|v| v.as_str())
                .unwrap_or("skip")
                .to_string(),
            next_run: cell(r, "next_run").and_then(|v| v.as_i64()),
            anchor_at: cell(r, "anchor_at").and_then(|v| v.as_i64()),
        }))
    }

    pub async fn all_schedules(&self) -> DbResult<Vec<(String, ScheduleRow)>> {
        let rows = self
            .db
            .query(
                "SELECT job_name, schedule_json, timezone, misfire_policy, next_run, anchor_at
                 FROM schedules",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    cell(r, "job_name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    ScheduleRow {
                        schedule_json: cell(r, "schedule_json")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        timezone: cell(r, "timezone")
                            .and_then(|v| v.as_str())
                            .unwrap_or("UTC")
                            .to_string(),
                        misfire_policy: cell(r, "misfire_policy")
                            .and_then(|v| v.as_str())
                            .unwrap_or("skip")
                            .to_string(),
                        next_run: cell(r, "next_run").and_then(|v| v.as_i64()),
                        anchor_at: cell(r, "anchor_at").and_then(|v| v.as_i64()),
                    },
                )
            })
            .collect())
    }

    pub async fn set_next_run(
        &self,
        job_name: &str,
        next_run: Option<i64>,
        anchor: Option<i64>,
    ) -> DbResult<()> {
        self.db
            .execute(
                "UPDATE schedules SET next_run = ?, anchor_at = COALESCE(?, anchor_at)
                 WHERE job_name = ?",
                &[next_run.into(), anchor.into(), job_name.into()],
            )
            .await?;
        Ok(())
    }

    pub async fn set_job_status(&self, job_name: &str, status: JobStatus) -> DbResult<()> {
        self.db
            .execute(
                "UPDATE jobs SET status = ? WHERE name = ?",
                &[status.to_db().into(), job_name.into()],
            )
            .await?;
        Ok(())
    }

    // --- workers -------------------------------------------------------------

    /// Register/upsert a worker (spec §9). The standalone engine registers
    /// itself as worker "local" — job_runs.worker_id has a FK to workers.
    pub async fn upsert_worker(
        &self,
        id: &str,
        hostname: &str,
        address: &str,
        version: &str,
        labels: &[String],
    ) -> DbResult<()> {
        let labels_json = serde_json::to_string(labels).map_err(|e| DbError::Sql(e.to_string()))?;
        self.db
            .execute(
                "INSERT INTO workers (id, hostname, address, version, labels, status,
                                      last_heartbeat, registered_at)
                 VALUES (?,?,?,?,?, 'ONLINE', ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                   hostname=excluded.hostname, address=excluded.address,
                   version=excluded.version, labels=excluded.labels,
                   last_heartbeat=excluded.last_heartbeat",
                &[
                    id.into(),
                    hostname.into(),
                    address.into(),
                    version.into(),
                    labels_json.into(),
                    unix_now().into(),
                    unix_now().into(),
                ],
            )
            .await?;
        Ok(())
    }

    // --- runs ---------------------------------------------------------------

    pub async fn create_run(
        &self,
        id: &str,
        job_name: &str,
        worker: Option<&str>,
        status: JobStatus,
    ) -> DbResult<()> {
        self.db
            .execute(
                "INSERT INTO job_runs (id, job_id, worker_id, status, created_at)
                 VALUES (?,?,?,?,?)",
                &[
                    id.into(),
                    job_name.into(),
                    worker.map(|w| w.to_string()).into(),
                    status.to_db().into(),
                    unix_now().into(),
                ],
            )
            .await?;
        self.set_job_status(job_name, status).await?;
        Ok(())
    }

    /// Atomic claim: only a QUEUED run assigned to this worker (or unassigned)
    /// can be claimed. Returns true on success.
    pub async fn claim_run(&self, id: &str, worker: &str) -> DbResult<bool> {
        let n = self
            .db
            .execute(
                "UPDATE job_runs SET status = 'STARTING', worker_id = ?, started_at = ?
                 WHERE id = ? AND status = 'QUEUED' AND (worker_id IS NULL OR worker_id = ?)",
                &[worker.into(), unix_now().into(), id.into(), worker.into()],
            )
            .await?;
        if n > 0 {
            let job = self
                .db
                .query("SELECT job_id FROM job_runs WHERE id = ?", &[id.into()])
                .await?;
            if let Some(name) = job.first().and_then(|r| cell(r, "job_id").and_then(|v| v.as_str()))
            {
                self.set_job_status(name, JobStatus::Starting).await?;
            }
        }
        Ok(n > 0)
    }

    pub async fn set_run_status(&self, id: &str, status: JobStatus) -> DbResult<()> {
        self.db
            .execute(
                "UPDATE job_runs SET status = ? WHERE id = ?",
                &[status.to_db().into(), id.into()],
            )
            .await?;
        let job = self
            .db
            .query("SELECT job_id FROM job_runs WHERE id = ?", &[id.into()])
            .await?;
        if let Some(name) = job.first().and_then(|r| cell(r, "job_id").and_then(|v| v.as_str())) {
            self.set_job_status(name, status).await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn finish_run(
        &self,
        id: &str,
        status: JobStatus,
        exit_code: Option<i32>,
        size_before: Option<i64>,
        size_after: Option<i64>,
        bytes_transferred: Option<i64>,
        message: Option<&str>,
        duration_secs: i64,
    ) -> DbResult<()> {
        self.db
            .execute(
                "UPDATE job_runs SET status = ?, finished_at = ?, duration_secs = ?,
                        exit_code = ?, size_before = ?, size_after = ?,
                        bytes_transferred = ?, message = ?, lease_expires_at = NULL
                 WHERE id = ?",
                &[
                    status.to_db().into(),
                    unix_now().into(),
                    duration_secs.into(),
                    exit_code.map(|v| v as i64).into(),
                    size_before.into(),
                    size_after.into(),
                    bytes_transferred.into(),
                    message.map(|m| m.to_string()).into(),
                    id.into(),
                ],
            )
            .await?;
        self.set_job_status_for_run(id, status).await
    }

    async fn set_job_status_for_run(&self, id: &str, status: JobStatus) -> DbResult<()> {
        let job = self
            .db
            .query("SELECT job_id FROM job_runs WHERE id = ?", &[id.into()])
            .await?;
        if let Some(name) = job.first().and_then(|r| cell(r, "job_id").and_then(|v| v.as_str())) {
            self.set_job_status(name, status).await?;
        }
        Ok(())
    }

    /// Mark a failed run as RETRYING with the next attempt time.
    pub async fn set_retry(&self, id: &str, next_retry_at: i64, retry_count: u32) -> DbResult<()> {
        self.db
            .execute(
                "UPDATE job_runs SET status = 'RETRYING', next_retry_at = ?, retry_count = ?
                 WHERE id = ?",
                &[next_retry_at.into(), (retry_count as i64).into(), id.into()],
            )
            .await?;
        Ok(())
    }

    /// Retries whose wait elapsed: back to the queue.
    pub async fn due_retries(&self, now: i64) -> DbResult<Vec<RunRow>> {
        self.runs_where(
            "status = 'RETRYING' AND next_retry_at IS NOT NULL AND next_retry_at <= ?",
            &[now.into()],
        )
        .await
    }

    /// QUEUED runs for a worker to pick up (standalone: worker = "local").
    pub async fn queued_runs(&self, worker: &str) -> DbResult<Vec<RunRow>> {
        self.runs_where(
            "status = 'QUEUED' AND (worker_id IS NULL OR worker_id = ?)",
            &[worker.into()],
        )
        .await
    }

    async fn runs_where(&self, cond: &str, params: &[Param]) -> DbResult<Vec<RunRow>> {
        let sql = format!(
            "SELECT id, job_id, worker_id, status, retry_count, next_retry_at, created_at
             FROM job_runs WHERE {cond} ORDER BY created_at"
        );
        let rows = self.db.query(&sql, params).await?;
        Ok(rows
            .iter()
            .map(|r| RunRow {
                id: cell(r, "id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                job_id: cell(r, "job_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                worker_id: cell(r, "worker_id").and_then(|v| v.as_str()).map(|s| s.to_string()),
                status: JobStatus::from_db(
                    cell(r, "status").and_then(|v| v.as_str()).unwrap_or("PENDING"),
                ),
                retry_count: cell(r, "retry_count").and_then(|v| v.as_i64()).unwrap_or(0) as u32,
                next_retry_at: cell(r, "next_retry_at").and_then(|v| v.as_i64()),
                created_at: cell(r, "created_at").and_then(|v| v.as_i64()).unwrap_or(0),
            })
            .collect())
    }

    /// Run history for one job, newest first.
    pub async fn run_history(&self, job_name: &str, limit: u32) -> DbResult<Vec<RunRow>> {
        self.runs_where("job_id = ?", &[job_name.into()])
            .await
            .map(|mut v| {
                v.reverse();
                v.truncate(limit as usize);
                v.reverse();
                v
            })
    }

    pub async fn get_run(&self, id: &str) -> DbResult<Option<RunRow>> {
        let mut rows = self.runs_where("id = ?", &[id.into()]).await?;
        Ok(rows.pop())
    }

    // --- repositories / events / logs ---------------------------------------

    pub async fn set_repository_size(&self, path: &str, size: i64) -> DbResult<()> {
        self.db
            .execute(
                "INSERT INTO repositories (path, size_bytes, last_measured_at) VALUES (?,?,?)
                 ON CONFLICT(path) DO UPDATE SET size_bytes=excluded.size_bytes,
                   last_measured_at=excluded.last_measured_at",
                &[path.into(), size.into(), unix_now().into()],
            )
            .await?;
        Ok(())
    }

    pub async fn repository_size(&self, path: &str) -> DbResult<Option<i64>> {
        let rows = self
            .db
            .query(
                "SELECT size_bytes FROM repositories WHERE path = ?",
                &[path.into()],
            )
            .await?;
        Ok(rows.first().and_then(|r| cell(r, "size_bytes").and_then(|v| v.as_i64())))
    }

    pub async fn insert_event(
        &self,
        job_id: Option<&str>,
        run_id: Option<&str>,
        level: &str,
        message: &str,
    ) -> DbResult<()> {
        self.db
            .execute(
                "INSERT INTO events (id, ts, job_id, run_id, level, message) VALUES (?,?,?,?,?,?)",
                &[
                    synora_core::RunId::new().to_string().into(),
                    unix_now().into(),
                    job_id.map(|s| s.to_string()).into(),
                    run_id.map(|s| s.to_string()).into(),
                    level.into(),
                    message.into(),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn insert_log(&self, run_id: &str, job_name: &str, log_path: &str) -> DbResult<()> {
        self.db
            .execute(
                "INSERT INTO job_logs (run_id, job_id, log_path, created_at) VALUES (?,?,?,?)",
                &[run_id.into(), job_name.into(), log_path.into(), unix_now().into()],
            )
            .await?;
        Ok(())
    }

    /// Jobs with their denormalized status, for CLI/TUI listing.
    pub async fn job_status_list(&self) -> DbResult<Vec<(String, JobStatus)>> {
        let rows = self
            .db
            .query("SELECT name, status FROM jobs ORDER BY name", &[])
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    cell(r, "name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    JobStatus::from_db(
                        cell(r, "status").and_then(|v| v.as_str()).unwrap_or("PENDING"),
                    ),
                )
            })
            .collect())
    }
}

/// JobStatus ↔ DB string helpers.
pub trait JobStatusDb {
    fn to_db(&self) -> &'static str;
    fn from_db(s: &str) -> JobStatus;
}

impl JobStatusDb for JobStatus {
    fn to_db(&self) -> &'static str {
        match self {
            JobStatus::Pending => "PENDING",
            JobStatus::Scheduled => "SCHEDULED",
            JobStatus::Queued => "QUEUED",
            JobStatus::Starting => "STARTING",
            JobStatus::Running => "RUNNING",
            JobStatus::Success => "SUCCESS",
            JobStatus::Failed => "FAILED",
            JobStatus::Retrying => "RETRYING",
            JobStatus::Cancelling => "CANCELLING",
            JobStatus::Cancelled => "CANCELLED",
            JobStatus::Lost => "LOST",
        }
    }

    fn from_db(s: &str) -> JobStatus {
        match s {
            "PENDING" => JobStatus::Pending,
            "SCHEDULED" => JobStatus::Scheduled,
            "QUEUED" => JobStatus::Queued,
            "STARTING" => JobStatus::Starting,
            "RUNNING" => JobStatus::Running,
            "SUCCESS" => JobStatus::Success,
            "FAILED" => JobStatus::Failed,
            "RETRYING" => JobStatus::Retrying,
            "CANCELLING" => JobStatus::Cancelling,
            "CANCELLED" => JobStatus::Cancelled,
            "LOST" => JobStatus::Lost,
            _ => JobStatus::Pending,
        }
    }
}
