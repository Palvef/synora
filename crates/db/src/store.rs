//! Store: all engine↔database interaction. M1 is SQLite-only; the Pg backend
//! (M3) implements the same methods.

use crate::sqlite::{DbError, DbResult, DbValue, Param};
use crate::Db;
use synora_core::job::{JobSpec, JobStatus, RUN_LEASE_SECS, WORKER_HEARTBEAT_GRACE_SECS};

#[derive(Debug, Clone)]
pub struct ScheduleRow {
    pub schedule_json: String,
    pub timezone: String,
    pub misfire_policy: String,
    pub next_run: Option<i64>,
    pub anchor_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct JobRunStats {
    pub job_id: String,
    pub last_start: Option<i64>,
    pub last_end: Option<i64>,
    pub last_success: Option<i64>,
    pub last_finished_status: Option<JobStatus>,
    pub last_worker: Option<String>,
    pub duration_secs: Option<i64>,
    pub memory_bytes: Option<i64>,
    pub cpu_seconds: Option<f64>,
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
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub duration_secs: Option<i64>,
    pub exit_code: Option<i32>,
    pub message: Option<String>,
}

fn job_run_stats_entry(
    map: &mut std::collections::HashMap<String, JobRunStats>,
    job_id: String,
) -> &mut JobRunStats {
    map.entry(job_id.clone()).or_insert_with(|| JobRunStats {
        job_id,
        last_start: None,
        last_end: None,
        last_success: None,
        last_finished_status: None,
        last_worker: None,
        duration_secs: None,
        memory_bytes: None,
        cpu_seconds: None,
    })
}

pub struct Store {
    db: Db,
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
    pub fn new(db: Db) -> Self {
        Store { db }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    // --- jobs / schedules -------------------------------------------------

    /// Insert or update a job definition from config (spec: config is the
    /// source of job definitions; DB mirrors it with runtime state).
    pub async fn sync_job(&self, job: &JobSpec) -> DbResult<()> {
        let provider = match &job.provider {
            synora_core::ProviderConfig::Rsync { .. } => "rsync",
            synora_core::ProviderConfig::TwoStageRsync { .. } => "two-stage-rsync",
            synora_core::ProviderConfig::Script { .. } => "script",
            synora_core::ProviderConfig::Docker { .. } => "docker",
            synora_core::ProviderConfig::Git { .. } => "git",
            synora_core::ProviderConfig::Http { .. } => "http",
        };
        let provider_config =
            serde_json::to_string(&job.provider).map_err(|e| DbError::Sql(e.to_string()))?;
        let success_codes = serde_json::to_string(&job.success_exit_codes)
            .map_err(|e| DbError::Sql(e.to_string()))?;
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
                    cell(r, "job_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
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
        token_name: &str,
    ) -> DbResult<()> {
        let labels_json = serde_json::to_string(labels).map_err(|e| DbError::Sql(e.to_string()))?;
        self.db
            .execute(
                "INSERT INTO workers (id, hostname, address, version, labels, status,
                                      last_heartbeat, registered_at, token_name)
                 VALUES (?,?,?,?,?, 'ONLINE', ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                   hostname=excluded.hostname, address=excluded.address,
                   version=excluded.version, labels=excluded.labels,
                   last_heartbeat=excluded.last_heartbeat,
                   token_name = CASE WHEN workers.token_name = '' OR workers.token_name IS NULL
                                      THEN excluded.token_name ELSE workers.token_name END,
                   status = CASE WHEN workers.status IN ('DRAINING','MAINTENANCE')
                                 THEN workers.status ELSE 'ONLINE' END",
                &[
                    id.into(),
                    hostname.into(),
                    address.into(),
                    version.into(),
                    labels_json.into(),
                    unix_now().into(),
                    unix_now().into(),
                    token_name.into(),
                ],
            )
            .await?;
        Ok(())
    }

    // --- workers (M3: manager-side) -----------------------------------------

    pub async fn list_workers(&self) -> DbResult<Vec<Vec<(String, DbValue)>>> {
        self.db
            .query(
                "SELECT id, hostname, address, version, labels, capabilities, status,
                        jobs_running, last_heartbeat, registered_at
                 FROM workers ORDER BY id",
                &[],
            )
            .await
    }

    pub async fn get_worker(&self, id: &str) -> DbResult<Option<Vec<(String, DbValue)>>> {
        let mut rows = self
            .db
            .query(
                "SELECT id, hostname, address, version, labels, capabilities, status,
                        jobs_running, last_heartbeat, registered_at
                 FROM workers WHERE id = ?",
                &[id.into()],
            )
            .await?;
        Ok(rows.pop())
    }

    pub async fn touch_heartbeat(
        &self,
        id: &str,
        jobs_running: u32,
        status: &str,
        active_jobs: &[String],
    ) -> DbResult<()> {
        let _ = status; // heartbeat "idle/running" is informational; worker
                        // lifecycle status (ONLINE/OFFLINE/DRAINING/MAINTENANCE) is managed
                        // by register/reaper/drain only.
        self.db
            .execute(
                "UPDATE workers SET last_heartbeat = ?, jobs_running = ?,
                        status = CASE WHEN status = 'OFFLINE' THEN 'ONLINE'
                                      ELSE status END
                 WHERE id = ?",
                &[unix_now().into(), (jobs_running as i64).into(), id.into()],
            )
            .await?;
        // Refresh leases only for jobs this worker is actually running.
        // Refreshing every SYNCING/RUNNING row kept cancelled ghosts alive
        // after a missed complete_run (GXDE/ceph stayed syncing forever).
        let lease = unix_now() + RUN_LEASE_SECS;
        if !active_jobs.is_empty() {
            for job in active_jobs {
                self.db
                    .execute(
                        "UPDATE job_runs SET lease_expires_at = ?
                         WHERE worker_id = ? AND job_id = ? AND status IN ('STARTING','SYNCING','RUNNING')",
                        &[lease.into(), id.into(), job.clone().into()],
                    )
                    .await?;
            }
        } else if jobs_running > 0 {
            // Legacy worker without active_jobs: keep the old refresh so a
            // rolling upgrade does not lease-expire a live run.
            self.db
                .execute(
                    "UPDATE job_runs SET lease_expires_at = ?
                     WHERE worker_id = ? AND status IN ('STARTING','SYNCING','RUNNING')",
                    &[lease.into(), id.into()],
                )
                .await?;
        }
        Ok(())
    }

    pub async fn set_worker_status(&self, id: &str, status: &str) -> DbResult<()> {
        self.db
            .execute(
                "UPDATE workers SET status = ? WHERE id = ?",
                &[status.into(), id.into()],
            )
            .await?;
        Ok(())
    }

    pub async fn delete_worker(&self, id: &str) -> DbResult<()> {
        self.db
            .execute("DELETE FROM workers WHERE id = ?", &[id.into()])
            .await?;
        Ok(())
    }

    /// Workers eligible for dispatch: ONLINE, under their concurrency cap.
    pub async fn eligible_workers(&self, max_jobs: u32) -> DbResult<Vec<(String, Vec<String>)>> {
        let rows = self
            .db
            .query(
                "SELECT id, labels FROM workers
                 WHERE status = 'ONLINE' AND jobs_running < ? ORDER BY jobs_running, id",
                &[(max_jobs as i64).into()],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let labels: Vec<String> = cell(r, "labels")
                    .and_then(|v| v.as_str())
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                (
                    cell(r, "id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    labels,
                )
            })
            .collect())
    }

    /// QUEUED runs a specific worker should be offered (assigned to it).
    pub async fn assigned_runs(&self, worker: &str) -> DbResult<Vec<RunRow>> {
        // Offer runs one job at a time: skip jobs that already have an
        // active run, or a queued run behind an active one would be offered
        // forever and block everything after it.
        let sql = "SELECT id, job_id, worker_id, status, retry_count, next_retry_at, created_at,
                          started_at, finished_at, duration_secs, exit_code, message
                   FROM job_runs
                   WHERE status = 'QUEUED' AND worker_id = ?
                     AND job_id NOT IN (
                         SELECT job_id FROM job_runs
                         WHERE status IN ('STARTING','SYNCING','RUNNING')
                     )
                   ORDER BY priority DESC, created_at";
        let rows = self.db.query(sql, &[worker.into()]).await?;
        Ok(rows.iter().map(|r| run_row(r)).collect())
    }

    /// The token name that registered this worker (identity binding).
    pub async fn worker_token(&self, id: &str) -> DbResult<Option<String>> {
        let rows = self
            .db
            .query("SELECT token_name FROM workers WHERE id = ?", &[id.into()])
            .await?;
        Ok(rows
            .first()
            .and_then(|r| cell(r, "token_name").and_then(|v| v.as_str()))
            .map(String::from))
    }

    /// QUEUED runs with no worker assigned (queued while no worker was
    /// online, or unassigned by the reaper) — candidates for re-dispatch.
    pub async fn unassigned_runs(&self) -> DbResult<Vec<RunRow>> {
        self.runs_where("status = 'QUEUED' AND worker_id IS NULL", &[])
            .await
    }

    /// Any in-flight run of a job (queued or executing). Used as the
    /// per-job concurrency gate at dispatch time.
    pub async fn inflight_runs_of_job(&self, job: &str) -> DbResult<Vec<RunRow>> {
        self.runs_where(
            "job_id = ? AND status IN ('QUEUED','STARTING','SYNCING','RUNNING','RETRYING','CANCELLING')",
            &[job.into()],
        )
        .await
    }

    pub async fn count_runs_with_status(&self, status: &str) -> DbResult<i64> {
        let rows = self
            .db
            .query(
                "SELECT COUNT(*) AS n FROM job_runs WHERE status = ?",
                &[status.into()],
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|r| cell(r, "n"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0))
    }

    /// QUEUED runs that are actually waiting for a worker. Extra queued rows
    /// left behind a STARTING/SYNCING/RUNNING sibling are not waiting work.
    pub async fn count_waiting_runs(&self) -> DbResult<i64> {
        let rows = self
            .db
            .query(
                "SELECT COUNT(*) AS n FROM job_runs
                 WHERE status = 'QUEUED'
                   AND job_id NOT IN (
                       SELECT job_id FROM (
                           SELECT job_id FROM job_runs
                           WHERE status IN ('STARTING','SYNCING','RUNNING','CANCELLING')
                       )
                   )",
                &[],
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|r| cell(r, "n"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0))
    }

    /// Drop leftover QUEUED/RETRYING/CANCELLING rows once another run of the
    /// same job is already executing. Those leftovers flip `jobs.status` back
    /// to queued and inflate Grafana's queue count.
    pub async fn drop_superseded_runs(&self) -> DbResult<usize> {
        self.db
            .execute(
                "UPDATE job_runs
                 SET status = 'CANCELLED', finished_at = ?, message = 'superseded by an active run'
                 WHERE id IN (
                     SELECT id FROM (
                         SELECT q.id FROM job_runs q
                         WHERE q.status IN ('QUEUED','RETRYING','CANCELLING')
                           AND EXISTS (
                               SELECT 1 FROM job_runs a
                               WHERE a.job_id = q.job_id
                                 AND a.id != q.id
                                 AND a.status IN ('STARTING','SYNCING','RUNNING')
                           )
                     )
                 )",
                &[unix_now().into()],
            )
            .await
    }

    /// Atomically assign an unassigned QUEUED run to a worker. Returns true
    /// when this call did the assignment (a concurrent assign loses the race).
    pub async fn assign_queued_run(&self, id: &str, worker: &str) -> DbResult<bool> {
        let n = self
            .db
            .execute(
                "UPDATE job_runs SET worker_id = ?
                 WHERE id = ? AND status = 'QUEUED' AND worker_id IS NULL",
                &[worker.into(), id.into()],
            )
            .await?;
        Ok(n > 0)
    }

    /// Active runs of a job (SYNCING/RUNNING) — the per-job concurrency
    /// gate for claim.
    pub async fn active_runs_of_job(&self, job: &str) -> DbResult<Vec<RunRow>> {
        self.runs_where(
            "job_id = ? AND status IN ('STARTING','SYNCING','RUNNING')",
            &[job.into()],
        )
        .await
    }

    /// Active (claimed) runs of a worker.
    pub async fn active_runs_of(&self, worker: &str) -> DbResult<Vec<RunRow>> {
        self.runs_where(
            "worker_id = ? AND status IN ('STARTING','SYNCING','RUNNING')",
            &[worker.into()],
        )
        .await
    }

    /// Runs this worker holds that are marked CANCELLING (stop requests).
    pub async fn cancelling_runs_of(&self, worker: &str) -> DbResult<Vec<RunRow>> {
        self.runs_where("worker_id = ? AND status = 'CANCELLING'", &[worker.into()])
            .await
    }

    /// Runs whose lease expired (worker vanished) — the reaper marks LOST.
    pub async fn expired_runs(&self, now: i64) -> DbResult<Vec<RunRow>> {
        // A live worker (recent heartbeat) still owns the run even if a
        // lease row was not refreshed in time — do not mark LOST / re-dispatch
        // and docker-rm a still-running job.
        let alive_after = now - WORKER_HEARTBEAT_GRACE_SECS;
        self.runs_where(
            "status IN ('STARTING','SYNCING','RUNNING','CANCELLING')              AND lease_expires_at IS NOT NULL AND lease_expires_at <= ?              AND (worker_id IS NULL OR worker_id NOT IN (                  SELECT id FROM workers                  WHERE last_heartbeat IS NOT NULL                    AND last_heartbeat >= ?                    AND status IN ('ONLINE','DRAINING')              ))",
            &[now.into(), alive_after.into()],
        )
        .await
    }

    pub async fn set_run_lost(&self, id: &str) -> DbResult<()> {
        self.db
            .execute(
                "UPDATE job_runs SET status = 'LOST', finished_at = ?, message = 'lease expired (worker lost)'
                 WHERE id = ? AND status IN ('STARTING','SYNCING','RUNNING','CANCELLING')",
                &[unix_now().into(), id.into()],
            )
            .await?;
        self.set_job_status_for_run(id, JobStatus::Lost).await
    }

    /// Worker process restarted: every run it had claimed is gone.
    pub async fn mark_worker_runs_lost(&self, worker_id: &str) -> DbResult<Vec<RunRow>> {
        let runs = self.active_runs_of(worker_id).await?;
        for run in &runs {
            self.set_run_lost(&run.id).await?;
        }
        Ok(runs)
    }

    /// New run row on worker loss (LOST does not burn the retry budget, §29).
    pub async fn create_lost_requeue(
        &self,
        id: &str,
        job_name: &str,
        worker: Option<&str>,
    ) -> DbResult<Option<String>> {
        let new_id = synora_core::RunId::new().to_string();
        let n = self
            .db
            .execute(
                "INSERT INTO job_runs (id, job_id, worker_id, status, lost_count, created_at)
                 SELECT ?, job_id, ?, 'QUEUED',
                        COALESCE((SELECT lost_count FROM job_runs WHERE id = ?), 0) + 1,
                        ?
                 FROM job_runs WHERE id = ?
                   AND NOT EXISTS (
                       SELECT 1 FROM (
                           SELECT 1 FROM job_runs jr
                           WHERE jr.job_id = job_runs.job_id
                             AND jr.status IN ('QUEUED','STARTING','SYNCING','RUNNING','RETRYING','CANCELLING')
                       )
                   )",
                &[
                    new_id.clone().into(),
                    worker.map(|w| w.to_string()).into(),
                    id.into(),
                    unix_now().into(),
                    id.into(),
                ],
            )
            .await?;
        let _ = job_name;
        if n == 0 {
            return Ok(None);
        }
        self.set_job_status(job_name, JobStatus::Queued).await?;
        Ok(Some(new_id))
    }

    // --- runs ---------------------------------------------------------------

    pub async fn create_run(
        &self,
        id: &str,
        job_name: &str,
        worker: Option<&str>,
        status: JobStatus,
        priority: i64,
    ) -> DbResult<()> {
        self.db
            .execute(
                "INSERT INTO job_runs (id, job_id, worker_id, status, created_at, priority)
                 VALUES (?,?,?,?,?,?)",
                &[
                    id.into(),
                    job_name.into(),
                    worker.map(|w| w.to_string()).into(),
                    status.to_db().into(),
                    unix_now().into(),
                    priority.into(),
                ],
            )
            .await?;
        if matches!(status, JobStatus::Queued) {
            let active = self.active_runs_of_job(job_name).await?;
            if active.is_empty() {
                self.set_job_status(job_name, status).await?;
            }
        } else {
            self.set_job_status(job_name, status).await?;
        }
        Ok(())
    }

    /// Atomic claim: only a QUEUED run assigned to this worker (or unassigned)
    /// can be claimed. Returns true on success.
    pub async fn claim_run(&self, id: &str, worker: &str) -> DbResult<bool> {
        let n = self
            .db
            .execute(
                "UPDATE job_runs SET status = 'SYNCING', worker_id = ?, started_at = ?,
                        lease_expires_at = ?
                 WHERE id = ? AND status = 'QUEUED' AND (worker_id IS NULL OR worker_id = ?)
                   AND job_id NOT IN (
                       SELECT job_id FROM (
                           SELECT job_id FROM job_runs
                           WHERE status IN ('STARTING','SYNCING','RUNNING')
                       )
                   )",
                &[
                    worker.into(),
                    unix_now().into(),
                    (unix_now() + RUN_LEASE_SECS).into(),
                    id.into(),
                    worker.into(),
                ],
            )
            .await?;
        if n > 0 {
            let job = self
                .db
                .query("SELECT job_id FROM job_runs WHERE id = ?", &[id.into()])
                .await?;
            if let Some(name) = job
                .first()
                .and_then(|r| cell(r, "job_id").and_then(|v| v.as_str()))
            {
                self.set_job_status(name, JobStatus::Syncing).await?;
            }
        }
        Ok(n > 0)
    }

    /// Heartbeat promotion: a job the worker is actually executing is
    /// `RUNNING` (Grafana and the TUI both render this as syncing).
    pub async fn mark_jobs_running(&self, worker: &str, jobs: &[String]) -> DbResult<()> {
        for job in jobs {
            let n = self
                .db
                .execute(
                    "UPDATE job_runs SET status = 'RUNNING'
                     WHERE worker_id = ? AND job_id = ?
                       AND status IN ('STARTING','SYNCING')",
                    &[worker.into(), job.as_str().into()],
                )
                .await?;
            // Never overwrite RETRYING/FAILED with RUNNING. A heartbeat can
            // still list a job for one tick after complete_run scheduled a retry.
            if n > 0 {
                self.set_job_status(job, JobStatus::Running).await?;
                continue;
            }
            // Run already RUNNING: still refresh jobs.status. A later
            // create_run/retry can otherwise leave the job row on QUEUED
            // while the worker is still executing.
            let rows = self
                .db
                .query(
                    "SELECT 1 AS n FROM job_runs
                     WHERE worker_id = ? AND job_id = ?
                       AND status IN ('STARTING','SYNCING','RUNNING')
                     LIMIT 1",
                    &[worker.into(), job.as_str().into()],
                )
                .await?;
            if !rows.is_empty() {
                self.set_job_status(job, JobStatus::Running).await?;
            }
        }
        Ok(())
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
        if let Some(name) = job
            .first()
            .and_then(|r| cell(r, "job_id").and_then(|v| v.as_str()))
        {
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
        if let Some(name) = job
            .first()
            .and_then(|r| cell(r, "job_id").and_then(|v| v.as_str()))
        {
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
        self.set_job_status_for_run(id, JobStatus::Retrying).await
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
            "SELECT id, job_id, worker_id, status, retry_count, next_retry_at, created_at,
                    started_at, finished_at, duration_secs, exit_code, message
             FROM job_runs WHERE {cond} ORDER BY priority DESC, created_at"
        );
        let rows = self.db.query(&sql, params).await?;
        Ok(rows.iter().map(|r| run_row(r)).collect())
    }

    /// Run history for one job, newest first (by creation time only —
    /// priority must not reorder history).
    pub async fn run_history(&self, job_name: &str, limit: u32) -> DbResult<Vec<RunRow>> {
        let sql = "SELECT id, job_id, worker_id, status, retry_count, next_retry_at, created_at,
                          started_at, finished_at, duration_secs, exit_code, message
                   FROM job_runs WHERE job_id = ? ORDER BY created_at DESC LIMIT ?";
        let rows = self
            .db
            .query(sql, &[job_name.into(), (limit as i64).into()])
            .await?;
        let mut v: Vec<RunRow> = rows.iter().map(|r| run_row(r)).collect();
        v.truncate(limit as usize);
        Ok(v)
    }

    pub async fn get_run(&self, id: &str) -> DbResult<Option<RunRow>> {
        let mut rows = self.runs_where("id = ?", &[id.into()]).await?;
        Ok(rows.pop())
    }

    /// Mark a job's active runs CANCELLING (operator stop) — the worker picks
    /// the id up in its next heartbeat and cancels locally.
    pub async fn set_cancelling_by_job(&self, job_name: &str) -> DbResult<usize> {
        self.db
            .execute(
                "UPDATE job_runs SET status = 'CANCELLING'
                 WHERE job_id = ? AND status IN ('STARTING','SYNCING','RUNNING')",
                &[job_name.into()],
            )
            .await
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

    pub async fn list_repository_sizes(&self) -> DbResult<Vec<(String, i64)>> {
        let rows = self
            .db
            .query("SELECT path, size_bytes FROM repositories", &[])
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let path = cell(r, "path").and_then(|v| v.as_str()).map(String::from)?;
                let size = cell(r, "size_bytes").and_then(|v| v.as_i64())?;
                Some((path, size))
            })
            .collect())
    }

    /// Latest non-null `size_after` per job (any successful/failed run that
    /// reported a size). Used to backfill Grafana when repositories.path
    /// does not match the live job storage field.
    pub async fn latest_run_sizes(&self) -> DbResult<Vec<(String, i64)>> {
        let rows = self
            .db
            .query(
                "SELECT job_id, size_after FROM (
                     SELECT job_id, size_after,
                            ROW_NUMBER() OVER (
                                PARTITION BY job_id
                                ORDER BY COALESCE(finished_at, created_at) DESC
                            ) AS rn
                     FROM job_runs
                     WHERE size_after IS NOT NULL
                 ) WHERE rn = 1",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let job = cell(r, "job_id")
                    .and_then(|v| v.as_str())
                    .map(String::from)?;
                let size = cell(r, "size_after").and_then(|v| v.as_i64())?;
                Some((job, size))
            })
            .collect())
    }

    pub async fn repository_size(&self, path: &str) -> DbResult<Option<i64>> {
        let rows = self
            .db
            .query(
                "SELECT size_bytes FROM repositories WHERE path = ?",
                &[path.into()],
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|r| cell(r, "size_bytes").and_then(|v| v.as_i64())))
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
        self.insert_log_with(run_id, job_name, log_path, "").await
    }

    /// Insert a log row with content (remote workers report their log text).
    pub async fn insert_log_with(
        &self,
        run_id: &str,
        job_name: &str,
        log_path: &str,
        content: &str,
    ) -> DbResult<()> {
        self.db
            .execute(
                "INSERT INTO job_logs (run_id, job_id, log_path, created_at, content)
                 VALUES (?,?,?,?,?)
                 ON CONFLICT(run_id) DO UPDATE SET content = excluded.content",
                &[
                    run_id.into(),
                    job_name.into(),
                    log_path.into(),
                    unix_now().into(),
                    content.into(),
                ],
            )
            .await?;
        Ok(())
    }

    /// Latest stored log content for a job (remote-worker runs).
    pub async fn latest_log_content(&self, job_name: &str) -> DbResult<Option<String>> {
        let rows = self
            .db
            .query(
                "SELECT content FROM job_logs WHERE job_id = ?
                 ORDER BY created_at DESC LIMIT 1",
                &[job_name.into()],
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|r| cell(r, "content").and_then(|v| v.as_str()))
            .map(String::from))
    }

    /// Latest finished run per job: timestamps + last resource sample.
    pub async fn latest_run_stats(&self) -> DbResult<Vec<JobRunStats>> {
        let mut out: std::collections::HashMap<String, JobRunStats> =
            std::collections::HashMap::new();

        let rows = self
            .db
            .query(
                "SELECT job_id, started_at, finished_at, duration_secs, status,
                        memory_bytes, cpu_seconds FROM (
                     SELECT job_id, started_at, finished_at, duration_secs, status,
                            memory_bytes, cpu_seconds,
                            ROW_NUMBER() OVER (
                                PARTITION BY job_id
                                ORDER BY COALESCE(finished_at, created_at) DESC
                            ) AS rn
                     FROM job_runs
                     WHERE finished_at IS NOT NULL
                 ) WHERE rn = 1",
                &[],
            )
            .await?;
        for r in &rows {
            let Some(job_id) = cell(r, "job_id").and_then(|v| v.as_str()).map(String::from) else {
                continue;
            };
            let cpu = cell(r, "cpu_seconds").and_then(|v| match v {
                crate::sqlite::DbValue::Int(i) => Some(*i as f64),
                crate::sqlite::DbValue::Text(s) => s.parse().ok(),
                _ => None,
            });
            let stats = job_run_stats_entry(&mut out, job_id);
            stats.last_finished_status = cell(r, "status")
                .and_then(|v| v.as_str())
                .map(JobStatus::from_db);
            stats.last_start = cell(r, "started_at").and_then(|v| v.as_i64());
            stats.last_end = cell(r, "finished_at").and_then(|v| v.as_i64());
            stats.duration_secs = cell(r, "duration_secs").and_then(|v| v.as_i64());
            stats.memory_bytes = cell(r, "memory_bytes").and_then(|v| v.as_i64());
            stats.cpu_seconds = cpu;
        }

        let success_rows = self
            .db
            .query(
                "SELECT job_id, finished_at FROM (
                     SELECT job_id, finished_at,
                            ROW_NUMBER() OVER (
                                PARTITION BY job_id
                                ORDER BY finished_at DESC
                            ) AS rn
                     FROM job_runs
                     WHERE status = 'SUCCESS' AND finished_at IS NOT NULL
                 ) WHERE rn = 1",
                &[],
            )
            .await?;
        for r in &success_rows {
            if let (Some(job), Some(ts)) = (
                cell(r, "job_id").and_then(|v| v.as_str()).map(String::from),
                cell(r, "finished_at").and_then(|v| v.as_i64()),
            ) {
                job_run_stats_entry(&mut out, job).last_success = Some(ts);
            }
        }

        let start_rows = self
            .db
            .query(
                "SELECT job_id, started_at FROM (
                     SELECT job_id, started_at,
                            ROW_NUMBER() OVER (
                                PARTITION BY job_id
                                ORDER BY started_at DESC
                            ) AS rn
                     FROM job_runs
                     WHERE started_at IS NOT NULL
                 ) WHERE rn = 1",
                &[],
            )
            .await?;
        for r in &start_rows {
            if let (Some(job), Some(ts)) = (
                cell(r, "job_id").and_then(|v| v.as_str()).map(String::from),
                cell(r, "started_at").and_then(|v| v.as_i64()),
            ) {
                job_run_stats_entry(&mut out, job).last_start = Some(ts);
            }
        }

        let worker_rows = self
            .db
            .query(
                "SELECT job_id, worker_id FROM (
                     SELECT job_id, worker_id,
                            ROW_NUMBER() OVER (
                                PARTITION BY job_id
                                ORDER BY created_at DESC
                            ) AS rn
                     FROM job_runs
                     WHERE worker_id IS NOT NULL
                 ) WHERE rn = 1",
                &[],
            )
            .await?;
        for r in &worker_rows {
            if let (Some(job), Some(worker)) = (
                cell(r, "job_id").and_then(|v| v.as_str()).map(String::from),
                cell(r, "worker_id")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            ) {
                job_run_stats_entry(&mut out, job).last_worker = Some(worker);
            }
        }

        Ok(out.into_values().collect())
    }

    pub async fn set_run_resources(
        &self,
        id: &str,
        memory_bytes: Option<u64>,
        cpu_seconds: Option<f64>,
    ) -> DbResult<()> {
        self.db
            .execute(
                "UPDATE job_runs SET memory_bytes = COALESCE(?, memory_bytes),
                        cpu_seconds = COALESCE(?, cpu_seconds) WHERE id = ?",
                &[
                    memory_bytes.map(|v| v as i64).into(),
                    cpu_seconds.into(),
                    id.into(),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn last_run_worker(&self, job: &str) -> DbResult<Option<String>> {
        let rows = self
            .db
            .query(
                "SELECT worker_id FROM job_runs
                 WHERE job_id = ? AND worker_id IS NOT NULL
                 ORDER BY created_at DESC LIMIT 1",
                &[job.into()],
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|r| cell(r, "worker_id").and_then(|v| v.as_str()))
            .map(String::from))
    }

    /// Delete a job and every row that references it. Config is the source
    /// of truth; a removed job must not leave runs/logs/events/schedules.
    pub async fn purge_job(&self, name: &str) -> DbResult<()> {
        let storage_rows = self
            .db
            .query(
                "SELECT storage_path FROM jobs WHERE name = ?",
                &[name.into()],
            )
            .await?;
        let storage_path = storage_rows
            .first()
            .and_then(|r| cell(r, "storage_path").and_then(|v| v.as_str()))
            .map(str::to_string);
        self.db
            .execute("DELETE FROM job_logs WHERE job_id = ?", &[name.into()])
            .await?;
        self.db
            .execute("DELETE FROM events WHERE job_id = ?", &[name.into()])
            .await?;
        self.db
            .execute(
                "DELETE FROM config_history WHERE job_name = ?",
                &[name.into()],
            )
            .await?;
        self.db
            .execute("DELETE FROM job_runs WHERE job_id = ?", &[name.into()])
            .await?;
        self.db
            .execute("DELETE FROM schedules WHERE job_name = ?", &[name.into()])
            .await?;
        self.db
            .execute("DELETE FROM jobs WHERE name = ?", &[name.into()])
            .await?;
        if let Some(path) = storage_path {
            let others = self
                .db
                .query(
                    "SELECT name FROM jobs WHERE storage_path = ? LIMIT 1",
                    &[path.clone().into()],
                )
                .await?;
            if others.is_empty() {
                let _ = self
                    .db
                    .execute("DELETE FROM repositories WHERE path = ?", &[path.into()])
                    .await;
            }
        }
        Ok(())
    }

    /// Keep `jobs.status` aligned with executing runs. Leftover QUEUED rows
    /// used to win because they were newer than the live RUNNING row.
    pub async fn reconcile_stale_job_status(&self) -> DbResult<usize> {
        self.db
            .execute(
                "UPDATE jobs SET status = COALESCE(
                     (SELECT 'RUNNING' FROM job_runs jr
                       WHERE jr.job_id = jobs.name AND jr.status = 'RUNNING' LIMIT 1),
                     (SELECT 'SYNCING' FROM job_runs jr
                       WHERE jr.job_id = jobs.name AND jr.status IN ('STARTING','SYNCING') LIMIT 1),
                     (SELECT 'CANCELLING' FROM job_runs jr
                       WHERE jr.job_id = jobs.name AND jr.status = 'CANCELLING' LIMIT 1),
                     (SELECT 'QUEUED' FROM job_runs jr
                       WHERE jr.job_id = jobs.name AND jr.status = 'QUEUED' LIMIT 1),
                     (SELECT 'RETRYING' FROM job_runs jr
                       WHERE jr.job_id = jobs.name AND jr.status = 'RETRYING' LIMIT 1),
                     (SELECT jr.status FROM job_runs jr
                       WHERE jr.job_id = jobs.name
                       ORDER BY COALESCE(jr.finished_at, jr.created_at) DESC, jr.created_at DESC
                       LIMIT 1),
                     status
                 )",
                &[],
            )
            .await
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
                    cell(r, "name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    JobStatus::from_db(
                        cell(r, "status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("PENDING"),
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
            JobStatus::Syncing => "SYNCING",
            JobStatus::Running => "RUNNING",
            JobStatus::Success => "SUCCESS",
            JobStatus::Failed => "FAILED",
            JobStatus::Retrying => "RETRYING",
            JobStatus::Cancelling => "CANCELLING",
            JobStatus::Cancelled => "CANCELLED",
            JobStatus::Lost => "LOST",
            JobStatus::Skipped => "SKIPPED",
        }
    }

    fn from_db(s: &str) -> JobStatus {
        match s {
            "PENDING" => JobStatus::Pending,
            "SCHEDULED" => JobStatus::Scheduled,
            "QUEUED" => JobStatus::Queued,
            "STARTING" | "SYNCING" => JobStatus::Syncing,
            "RUNNING" => JobStatus::Running,
            "SUCCESS" => JobStatus::Success,
            "FAILED" => JobStatus::Failed,
            "RETRYING" => JobStatus::Retrying,
            "CANCELLING" => JobStatus::Cancelling,
            "CANCELLED" => JobStatus::Cancelled,
            "LOST" => JobStatus::Lost,
            "SKIPPED" => JobStatus::Skipped,
            _ => JobStatus::Pending,
        }
    }
}

/// Row → RunRow mapping shared by the run queries.
fn run_row(r: &[(String, DbValue)]) -> RunRow {
    RunRow {
        id: cell(r, "id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        job_id: cell(r, "job_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        worker_id: cell(r, "worker_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        status: JobStatus::from_db(
            cell(r, "status")
                .and_then(|v| v.as_str())
                .unwrap_or("PENDING"),
        ),
        retry_count: cell(r, "retry_count").and_then(|v| v.as_i64()).unwrap_or(0) as u32,
        next_retry_at: cell(r, "next_retry_at").and_then(|v| v.as_i64()),
        created_at: cell(r, "created_at").and_then(|v| v.as_i64()).unwrap_or(0),
        started_at: cell(r, "started_at").and_then(|v| v.as_i64()),
        finished_at: cell(r, "finished_at").and_then(|v| v.as_i64()),
        duration_secs: cell(r, "duration_secs").and_then(|v| v.as_i64()),
        exit_code: cell(r, "exit_code")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
        message: cell(r, "message")
            .and_then(|v| v.as_str())
            .map(String::from),
    }
}
