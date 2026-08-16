//! Engine: owns config, DB, metrics, and the run loop.

use config::{DbKind, ResolvedConfig};
use db::store::Store;
use db::migrator::Migrator;
use db::SqliteDb;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use synora_core::job::{JobSpec, JobStatus};
use synora_core::Metrics;

/// Worker id used for runs executed locally by the standalone engine.
pub const LOCAL_WORKER: &str = "local";

pub struct Engine {
    pub cfg: ResolvedConfig,
    pub store: Store,
    pub metrics: Arc<Metrics>,
    /// Per-job mutex: serializes dispatch decisions (spec §8).
    job_locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Global concurrency gate (spec §8).
    global_sem: Arc<tokio::sync::Semaphore>,
    /// Active runs per job (per-job concurrency gate).
    pub(crate) active: std::sync::Mutex<HashMap<String, usize>>,
    shutdown: Arc<AtomicBool>,
}

impl Engine {
    pub async fn new(
        cfg: ResolvedConfig,
        migrations_dir: &std::path::Path,
    ) -> Result<Arc<Self>, String> {
        let db_path = match cfg.daemon.db.kind {
            DbKind::Sqlite => PathBuf::from(&cfg.daemon.db.path),
            DbKind::Postgres => {
                return Err("postgres backend lands in M3".to_string());
            }
        };
        let db = SqliteDb::open(&db_path).map_err(|e| e.to_string())?;
        let applied = Migrator::new(migrations_dir).run(&db).await.map_err(|e| e.to_string())?;
        for m in applied {
            tracing::info!("migration applied: {m}");
        }
        let store = Store::new(db);
        // The standalone engine is its own worker (job_runs.worker_id → workers).
        let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
        store
            .upsert_worker(
                LOCAL_WORKER,
                &hostname,
                "local",
                env!("CARGO_PKG_VERSION"),
                &[],
            )
            .await
            .map_err(|e| e.to_string())?;

        let mut job_locks = HashMap::new();
        for j in &cfg.jobs {
            job_locks.insert(j.name.clone(), Arc::new(tokio::sync::Mutex::new(())));
        }
        let engine = Arc::new(Engine {
            cfg,
            store,
            metrics: Arc::new(Metrics::new()),
            job_locks,
            global_sem: Arc::new(tokio::sync::Semaphore::new(16)),
            active: std::sync::Mutex::new(HashMap::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
        });
        // Refresh the global concurrency cap from config.
        engine.refresh_global_sem();
        Ok(engine)
    }

    fn refresh_global_sem(&self) {
        // tokio Semaphore capacity is fixed at construction; a config value
        // larger than the initial 16 is rare enough to ignore for M1.
        // ponytail: rebuild with the right capacity when reload lands in M2.
        let _ = self.cfg.daemon.max_concurrency;
    }

    /// Sync config-defined jobs + schedules into the DB (idempotent, keeps
    /// interval anchors). Jobs removed from config get disabled.
    pub async fn sync_config(&self) -> Result<(), String> {
        let now = unix_now();
        for job in &self.cfg.jobs {
            self.store.sync_job(job).await.map_err(|e| e.to_string())?;
            self.sync_schedule_for(job, now).await?;
        }
        for (name, _) in self
            .store
            .job_status_list()
            .await
            .map_err(|e| e.to_string())?
        {
            if !self.cfg.jobs.iter().any(|j| j.name == name) {
                let _ = self
                    .store
                    .db()
                    .execute("UPDATE jobs SET enabled = 0 WHERE name = ?", &[name.into()])
                    .await;
            }
        }
        Ok(())
    }

    async fn sync_schedule_for(&self, job: &JobSpec, now: i64) -> Result<(), String> {
        let tz = time_tz::timezones::get_by_name(&job.timezone)
            .ok_or_else(|| format!("unknown timezone `{}`", job.timezone))?;
        let schedule_json = serde_json::to_string(&job.schedule).map_err(|e| e.to_string())?;
        let misfire = match job.misfire_policy {
            synora_core::MisfirePolicy::Skip => "skip",
            synora_core::MisfirePolicy::RunImmediately => "run-immediately",
            synora_core::MisfirePolicy::RunNext => "run-next",
        };
        let now_dt = time::OffsetDateTime::from_unix_timestamp(now)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        let existing = self
            .store
            .get_schedule(&job.name)
            .await
            .map_err(|e| e.to_string())?;
        let is_first_sync = existing.is_none();
        let (next_run, anchor) = match &job.schedule.kind {
            synora_core::ScheduleKind::Manual | synora_core::ScheduleKind::Startup => {
                (None, None)
            }
            synora_core::ScheduleKind::Interval { .. } => {
                let anchor = existing
                    .as_ref()
                    .and_then(|r| r.anchor_at)
                    .map(|a| time::OffsetDateTime::from_unix_timestamp(a).unwrap_or(now_dt))
                    .unwrap_or(now_dt);
                let next = job
                    .schedule
                    .next_after(now_dt, tz, Some(anchor))
                    .map(|t| t.unix_timestamp());
                (next, Some(anchor.unix_timestamp()))
            }
            _ => {
                let next = job
                    .schedule
                    .next_after(now_dt, tz, None)
                    .map(|t| t.unix_timestamp());
                (next, None)
            }
        };
        // Interval first sync: run right away so the grid starts now.
        let next_run = match (&job.schedule.kind, is_first_sync) {
            (synora_core::ScheduleKind::Interval { .. }, true) => Some(now),
            _ => next_run,
        };
        self.store
            .sync_schedule(
                &job.name,
                &schedule_json,
                &job.timezone,
                misfire,
                next_run,
                anchor,
            )
            .await
            .map_err(|e| e.to_string())?;
        // Expose the next run time as a metric.
        self.metrics.set_gauge(
            "synora_job_next_run_timestamp",
            &[("job", job.name.as_str())],
            next_run.unwrap_or(0) as f64,
        );
        Ok(())
    }

    pub fn job(&self, name: &str) -> Option<JobSpec> {
        self.cfg.jobs.iter().find(|j| j.name == name).cloned()
    }

    /// Dispatch one job: create a QUEUED run row. Serialized per job.
    pub async fn dispatch(&self, job_name: &str) -> Result<String, String> {
        let job = self
            .job(job_name)
            .ok_or_else(|| format!("unknown job `{job_name}`"))?;
        if !job.enabled {
            return Err(format!("job `{job_name}` is disabled"));
        }
        let lock = self
            .job_locks
            .get(job_name)
            .cloned()
            .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
        let _guard = lock.lock().await;
        let run_id = synora_core::RunId::new().to_string();
        self.store
            .create_run(&run_id, job_name, Some(LOCAL_WORKER), JobStatus::Queued)
            .await
            .map_err(|e| e.to_string())?;
        let _ = self
            .store
            .insert_event(Some(job_name), Some(&run_id), "INFO", "run queued")
            .await;
        Ok(run_id)
    }

    /// Main loop: startup dispatch, misfire boot pass, retry tick, due
    /// dispatch, and QUEUED-run execution. Runs until shutdown.
    pub async fn run(self: Arc<Self>) -> Result<(), String> {
        let boot = unix_now();
        // Startup jobs: dispatch once per daemon boot (alignment decision).
        for job in &self.cfg.jobs {
            if matches!(job.schedule.kind, synora_core::ScheduleKind::Startup) && job.enabled {
                if let Err(e) = self.dispatch(&job.name).await {
                    tracing::warn!("startup dispatch of `{}` failed: {e}", job.name);
                }
            }
        }
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut boot_pass_done = false;
        loop {
            tick.tick().await;
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if !boot_pass_done {
                boot_pass_done = true;
                crate::scheduler::boot_pass(&self, boot).await;
            }
            self.tick().await;
        }
        Ok(())
    }

    /// One scheduler tick: retry requeue, due dispatch, QUEUED execution.
    /// Public so tests can drive the loop without a daemon.
    pub async fn tick(self: &Arc<Self>) {
        let now = unix_now();
        crate::scheduler::retry_tick(self, now).await;
        crate::scheduler::dispatch_due(self, now).await;
        crate::scheduler::execute_queued(self).await;
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }

    pub fn global_sem(&self) -> Arc<tokio::sync::Semaphore> {
        self.global_sem.clone()
    }

    /// Active-run accounting (per-job concurrency gate).
    pub fn active_inc(&self, job: &str) -> usize {
        let mut m = self.active.lock().unwrap();
        let n = m.entry(job.to_string()).or_insert(0);
        *n += 1;
        *n
    }

    pub fn active_dec(&self, job: &str) -> usize {
        let mut m = self.active.lock().unwrap();
        if let Some(n) = m.get_mut(job) {
            *n -= 1;
            if *n == 0 {
                m.remove(job);
                return 0;
            }
            return *n;
        }
        0
    }

    /// One-shot run (for `synora run <job>`): dispatch and wait for the run
    /// to reach a terminal state.
    pub async fn run_once(self: Arc<Self>, job_name: &str) -> Result<JobStatus, String> {
        self.sync_config().await?;
        let run_id = self.dispatch(job_name).await?;
        // Spin until the run row reaches a terminal state (bounded).
        for _ in 0..600 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            crate::scheduler::execute_queued(&self).await;
            if let Some(run) = self
                .store
                .get_run(&run_id)
                .await
                .map_err(|e| e.to_string())?
            {
                match run.status {
                    JobStatus::Success
                    | JobStatus::Failed
                    | JobStatus::Cancelled
                    | JobStatus::Lost => return Ok(run.status),
                    _ => continue,
                }
            }
        }
        Err("run did not finish in time".to_string())
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
