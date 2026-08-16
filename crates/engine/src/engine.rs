//! Engine: owns config, DB, metrics, and the run loop.

use config::{CliOverrides, ConfigLoader, DbKind, ResolvedConfig};
use db::store::Store;
use db::migrator::Migrator;
use db::SqliteDb;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use synora_core::job::{JobSpec, JobStatus};
use synora_core::Metrics;

/// Storage/snapshot context passed to every run (built from config).
#[derive(Clone)]
pub struct RunStorageCtx {
    pub manager: Option<std::sync::Arc<storage::StorageManager>>,
    pub storages: std::collections::HashMap<String, config::StorageConfig>,
    pub retention: synora_core::RetentionPolicy,
    pub min_free_bytes: Option<u64>,
}

impl RunStorageCtx {
    pub fn from_config(cfg: &ResolvedConfig) -> Option<RunStorageCtx> {
        if cfg.storages.is_empty()
            && cfg.snapshot_retention == synora_core::RetentionPolicy::default()
            && cfg.min_free_bytes.is_none()
        {
            return None;
        }
        Some(RunStorageCtx {
            manager: Some(std::sync::Arc::new(storage::StorageManager::new(&cfg.storages))),
            storages: cfg.storages.clone(),
            retention: cfg.snapshot_retention.clone(),
            min_free_bytes: cfg.min_free_bytes,
        })
    }

    /// The storage entry whose mountpoint matches the job's storage path.
    pub fn storage_for(&self, job: &JobSpec) -> Option<(&String, &config::StorageConfig)> {
        let path = job.storage.to_string_lossy();
        self.storages
            .iter()
            .find(|(_, c)| {
                c.mountpoint
                    .as_ref()
                    .map(|m| m.to_string_lossy() == path)
                    .unwrap_or(false)
            })
    }
}

/// Worker id used for runs executed locally by the standalone engine.
pub const LOCAL_WORKER: &str = "local";

/// Worker picker for distributed dispatch.
pub type WorkerPlanner = Arc<dyn Fn(&JobSpec) -> Option<String> + Send + Sync>;

pub struct Engine {
    /// Static daemon config: NOT hot-reloadable (spec §85). Reloadable job
    /// definitions live in `live_jobs`.
    pub cfg: ResolvedConfig,
    pub store: Store,
    pub metrics: Arc<Metrics>,
    /// Live job definitions — swapped by `reload()`.
    live_jobs: std::sync::RwLock<HashMap<String, JobSpec>>,
    /// Per-job mutex: serializes dispatch decisions (spec §8).
    job_locks: std::sync::RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Global concurrency gate (spec §8).
    global_sem: Arc<tokio::sync::Semaphore>,
    /// Active runs per job (per-job concurrency gate).
    pub(crate) active: std::sync::Mutex<HashMap<String, usize>>,
    /// Cancel tokens for running runs, keyed by job name.
    active_runs: std::sync::Mutex<HashMap<String, tokio_util::sync::CancellationToken>>,
    /// Source path + overrides for `reload()`.
    config_source: std::sync::RwLock<Option<(PathBuf, CliOverrides)>>,
    /// Storage/snapshot runtime (None when no storage sections configured).
    pub run_storage: Option<RunStorageCtx>,
    /// Consecutive-failure counters per job (alert dedup, spec §91).
    pub failure_streak: std::sync::Mutex<std::collections::HashMap<String, u32>>,
    /// Network routing (proxies/egress). Built when any proxy/egress config
    /// exists; None = pure direct mode.
    pub netroute: std::sync::RwLock<Option<std::sync::Arc<netroute::NetRoute>>>,
    /// Worker picker for distributed dispatch (None = standalone/local).
    planner: std::sync::RwLock<Option<WorkerPlanner>>,
    shutdown: Arc<AtomicBool>,
}

impl Engine {
    pub async fn new(
        cfg: ResolvedConfig,
        migrations_dir: &std::path::Path,
        register_local: bool,
    ) -> Result<Arc<Self>, String> {
        let db = match cfg.daemon.db.kind {
            DbKind::Sqlite => {
                db::Db::Sqlite(std::sync::Arc::new(
                    SqliteDb::open(&PathBuf::from(&cfg.daemon.db.path)).map_err(|e| e.to_string())?,
                ))
            }
            DbKind::Postgres => {
                let url = cfg
                    .daemon
                    .db
                    .url
                    .as_deref()
                    .ok_or("db.kind = \"postgres\" requires db.url")?;
                db::Db::Pg(std::sync::Arc::new(
                    db::PgDb::connect(url).await.map_err(|e| e.to_string())?,
                ))
            }
        };
        let applied = Migrator::new(migrations_dir).run(&db).await.map_err(|e| e.to_string())?;
        for m in applied {
            tracing::info!("migration applied: {m}");
        }
        let store = Store::new(db);
        // The standalone engine is its own worker (job_runs.worker_id → workers).
        // The manager does not register itself — it only orchestrates.
        if register_local {
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
        }

        let run_storage = RunStorageCtx::from_config(&cfg);
        let netroute = std::sync::RwLock::new(
            netroute::NetRoute::build_optional(
                &cfg.proxies,
                &cfg.proxy_groups,
                &cfg.egresses,
                &cfg.egress_groups,
                cfg.daemon.default_proxy.as_deref(),
            ),
        );
        let mut job_locks = HashMap::new();
        let mut live_jobs = HashMap::new();
        for j in &cfg.jobs {
            job_locks.insert(j.name.clone(), Arc::new(tokio::sync::Mutex::new(())));
            live_jobs.insert(j.name.clone(), j.clone());
        }
        let engine = Arc::new(Engine {
            cfg,
            store,
            metrics: Arc::new(Metrics::new()),
            live_jobs: std::sync::RwLock::new(live_jobs),
            job_locks: std::sync::RwLock::new(job_locks),
            global_sem: Arc::new(tokio::sync::Semaphore::new(16)),
            active: std::sync::Mutex::new(HashMap::new()),
            active_runs: std::sync::Mutex::new(HashMap::new()),
            config_source: std::sync::RwLock::new(None),
            planner: std::sync::RwLock::new(None),
            run_storage,
            netroute,
            failure_streak: std::sync::Mutex::new(std::collections::HashMap::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
        });
        Ok(engine)
    }

    /// Sync config-defined jobs + schedules into the DB (idempotent, keeps
    /// interval anchors). Jobs removed from config get disabled.
    pub async fn sync_config(&self) -> Result<(), String> {
        let now = unix_now();
        let jobs: Vec<JobSpec> = self.live_jobs.read().unwrap().values().cloned().collect();
        for job in &jobs {
            self.store.sync_job(job).await.map_err(|e| e.to_string())?;
            self.sync_schedule_for(job, now).await?;
        }
        for (name, _) in self
            .store
            .job_status_list()
            .await
            .map_err(|e| e.to_string())?
        {
            if !jobs.iter().any(|j| j.name == name) {
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
        self.live_jobs.read().unwrap().get(name).cloned()
    }

    pub fn jobs(&self) -> Vec<JobSpec> {
        self.live_jobs.read().unwrap().values().cloned().collect()
    }

    /// Dispatch one job: create a QUEUED run row. Serialized per job.
    /// Dependencies (spec §93): a dep whose latest run is not Success marks
    /// this run SKIPPED (a terminal status — it never executes).
    pub async fn dispatch(&self, job_name: &str) -> Result<String, String> {
        let job = self
            .job(job_name)
            .ok_or_else(|| format!("unknown job `{job_name}`"))?;
        if !job.enabled {
            return Err(format!("job `{job_name}` is disabled"));
        }
        for dep in &job.depends_on {
            let dep_ok = self
                .store
                .run_history(dep, 1)
                .await
                .map(|runs| runs.first().map(|r| r.status == JobStatus::Success).unwrap_or(false))
                .unwrap_or(false);
            if !dep_ok {
                let run_id = synora_core::RunId::new().to_string();
                let _ = self
                    .store
                    .create_run(&run_id, job_name, None, JobStatus::Skipped)
                    .await;
                let _ = self
                    .store
                    .finish_run(
                        &run_id,
                        JobStatus::Skipped,
                        None,
                        None,
                        None,
                        None,
                        Some(&format!("dependency `{dep}` has not succeeded")),
                        0,
                    )
                    .await;
                let _ = self
                    .store
                    .insert_event(
                        Some(job_name),
                        Some(&run_id),
                        "WARN",
                        &format!("skipped: dependency `{dep}` has not succeeded"),
                    )
                    .await;
                return Ok(run_id);
            }
        }
        let lock = self
            .job_locks
            .read()
            .unwrap()
            .get(job_name)
            .cloned()
            .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
        let _guard = lock.lock().await;
        // Manager mode: planner picks (None = stay QUEUED, visible, spec §8).
        // Standalone: everything runs on the local worker.
        let worker: Option<String> = {
            let planner = self.planner.read().unwrap();
            match planner.as_ref() {
                None => Some(LOCAL_WORKER.to_string()),
                Some(p) => p(&job),
            }
        };
        let run_id = synora_core::RunId::new().to_string();
        self.store
            .create_run(&run_id, job_name, worker.as_deref(), JobStatus::Queued)
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
        for job in self.jobs() {
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

    /// Send a webhook notification (spec §90) with alert dedup (spec §91):
    /// consecutive failures alert once after the threshold, then RECOVERED.
    pub async fn notify(&self, event: &str, job: Option<&str>, message: &str) {
        let Some(url) = self.cfg.notifications.webhook_url.clone() else {
            return;
        };
        let dedup_key = job.unwrap_or("");
        let (send, effective_event) = {
            let mut streaks = self.failure_streak.lock().unwrap();
            match event {
                "sync_failed" => {
                    let n = streaks.entry(dedup_key.to_string()).or_insert(0);
                    *n += 1;
                    // dedup: only alert once, at the threshold (spec §91)
                    (*n == self.cfg.notifications.alert_after_failures.max(1), event)
                }
                "sync_success" => {
                    let was_alerted = streaks.remove(dedup_key).unwrap_or(0)
                        >= self.cfg.notifications.alert_after_failures.max(1);
                    (was_alerted, if was_alerted { "sync_recovered" } else { event })
                }
                _ => (true, event),
            }
        };
        if !send {
            return;
        }
        let payload = serde_json::json!({
            "event": effective_event,
            "job": job,
            "message": message,
            "ts": unix_now(),
        });
        let client = match reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("webhook client failed: {e}");
                return;
            }
        };
        match client.post(&url).json(&payload).send().await {
            Ok(_) => {}
            Err(e) => tracing::warn!("webhook to {url} failed: {e}"),
        }
    }

    /// One scheduler tick: retry requeue, due dispatch, QUEUED execution,
    /// control-file processing. Public so tests can drive the loop.
    pub async fn tick(self: &Arc<Self>) {
        let now = unix_now();
        crate::scheduler::retry_tick(self, now).await;
        crate::scheduler::dispatch_due(self, now).await;
        crate::scheduler::execute_queued(self).await;
        self.process_control_dir().await;
    }

    /// True when a planner is installed (manager mode): unassigned runs stay
    /// QUEUED for remote workers instead of being executed locally.
    pub fn has_planner(&self) -> bool {
        self.planner.read().unwrap().is_some()
    }

    /// Install the worker picker (manager mode). Without it, runs stay local.
    pub fn set_planner<F>(&self, f: F)
    where
        F: Fn(&JobSpec) -> Option<String> + Send + Sync + 'static,
    {
        *self.planner.write().unwrap() = Some(Arc::new(f));
    }

    /// Remember where the config came from so `reload()` can re-read it.
    pub fn set_config_source(&self, path: PathBuf, overrides: CliOverrides) {
        *self.config_source.write().unwrap() = Some((path, overrides));
    }

    /// Hot reload (spec §85, Yuki's `yukictl reload` convention): re-read the
    /// config, apply job/schedule changes, reject non-reloadable changes
    /// (db backend, listen address, tls, log dir) as a whole.
    pub async fn reload(&self) -> Result<usize, String> {
        let (path, overrides) = self
            .config_source
            .read()
            .unwrap()
            .clone()
            .ok_or_else(|| "no config source set (started without -c?)".to_string())?;
        let new_cfg = ConfigLoader::load(&path, &overrides).map_err(|e| e.to_string())?;
        // Non-reloadable fields (spec §85).
        let old = &self.cfg;
        let reject = |field: &str, a: &dyn std::fmt::Debug, b: &dyn std::fmt::Debug| {
            Err(format!("reload rejected: `{field}` is not hot-reloadable (old {a:?}, new {b:?})"))
        };
        if old.daemon.db != new_cfg.daemon.db {
            return reject("daemon.db", &old.daemon.db, &new_cfg.daemon.db);
        }
        if old.daemon.log_dir != new_cfg.daemon.log_dir {
            return reject("daemon.log_dir", &old.daemon.log_dir, &new_cfg.daemon.log_dir);
        }
        if old.api.listen != new_cfg.api.listen {
            return reject("api.listen", &old.api.listen, &new_cfg.api.listen);
        }
        if old.api.tls != new_cfg.api.tls {
            return reject("api.tls", &old.api.tls, &new_cfg.api.tls);
        }

        // Apply job changes: upsert changed jobs + schedules, disable removed.
        // (Keep the live-jobs lock across awaits is forbidden — guards are not
        // Send — so: short lock for the diff, awaits, short lock to apply.)
        let now = unix_now();
        let removed: Vec<String> = {
            let jobs = self.live_jobs.read().unwrap();
            jobs.keys()
                .filter(|n| !new_cfg.jobs.iter().any(|j| &j.name == *n))
                .cloned()
                .collect()
        };
        for job in &new_cfg.jobs {
            self.store.sync_job(job).await.map_err(|e| e.to_string())?;
            self.sync_schedule_for(job, now).await?;
        }
        for name in &removed {
            let _ = self
                .store
                .db()
                .execute("UPDATE jobs SET enabled = 0 WHERE name = ?", &[name.clone().into()])
                .await;
        }
        let audit: Vec<(String, Option<String>, Option<String>)> = {
            let mut jobs = self.live_jobs.write().unwrap();
            let mut rows = Vec::new();
            for job in &new_cfg.jobs {
                let before = jobs
                    .get(&job.name)
                    .map(|old| serde_json::to_string(old).unwrap_or_default());
                jobs.insert(job.name.clone(), job.clone());
                if before.is_some() {
                    rows.push((
                        job.name.clone(),
                        Some(before.unwrap_or_default()),
                        serde_json::to_string(job).ok(),
                    ));
                }
            }
            for name in &removed {
                jobs.remove(name);
            }
            rows
        };
        for (name, before, after) in audit {
            let _ = self
                .store
                .db()
                .execute(
                    "INSERT INTO config_history (ts, job_name, before_json, after_json) VALUES (?,?,?,?)",
                    &[now.into(), name.into(), before.into(), after.into()],
                )
                .await;
        }
        // Proxy/egress changes: rebuild the NetRoute (user: TUI-registered
        // proxies apply on reload).
        if old.proxies != new_cfg.proxies
            || old.egresses != new_cfg.egresses
            || old.proxy_groups != new_cfg.proxy_groups
            || old.egress_groups != new_cfg.egress_groups
            || old.daemon.default_proxy != new_cfg.daemon.default_proxy
        {
            *self.netroute.write().unwrap() = netroute::NetRoute::build_optional(
                &new_cfg.proxies,
                &new_cfg.proxy_groups,
                &new_cfg.egresses,
                &new_cfg.egress_groups,
                new_cfg.daemon.default_proxy.as_deref(),
            );
        }
        let changed = new_cfg.jobs.len() + removed.len();
        let _ = self
            .store
            .insert_event(None, None, "INFO", &format!("config reloaded ({changed} job(s) applied)"))
            .await;
        Ok(changed)
    }

    /// Cancel a running run of `job` (spec §5 cancel path). The provider's
    /// cancel token kills the child; the executor records CANCELLED.
    pub async fn stop_job(&self, job_name: &str) -> Result<(), String> {
        let token = self
            .active_runs
            .lock()
            .unwrap()
            .get(job_name)
            .cloned()
            .ok_or_else(|| format!("job `{job_name}` is not running"))?;
        token.cancel();
        tracing::info!("job `{job_name}`: cancel requested");
        Ok(())
    }

    pub(crate) fn register_run(&self, job: &str, token: tokio_util::sync::CancellationToken) {
        self.active_runs
            .lock()
            .unwrap()
            .insert(job.to_string(), token);
    }

    pub(crate) fn remove_run(&self, job: &str) {
        self.active_runs.lock().unwrap().remove(job);
    }

    /// `synora stop <job>` drops a file into <log_dir>/control/stop-<job>;
    /// the tick picks it up and cancels the run.
    async fn process_control_dir(&self) {
        let dir = self.cfg.daemon.log_dir.join("control");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(job) = name.strip_prefix("stop-") else {
                continue;
            };
            if self.stop_job(job).await.is_ok() {
                let _ = std::fs::remove_file(&path);
            }
        }
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
