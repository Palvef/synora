//! REST API surface (spec §46) + worker picker + TLS serving.

use crate::auth::{has_perm, require, AuthUser};
use api::{
    CompleteRequest, HeartbeatRequest, HeartbeatResponse, JobDTO, RegisterRequest,
    RegisterResponse, ReloadResponse, RunAssignment, RunDTO, WorkerDTO, API_V1,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;
use config::TlsConfig;
use engine::Engine;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use synora_core::job::{JobSpec, JobStatus};

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub picker: WorkerPicker,
    pub proxy_probes: Arc<RwLock<std::collections::HashMap<String, netroute::ProxyProbe>>>,
}

/// Snapshot of workers for sync dispatch decisions, refreshed by the reaper
/// loop and on register/heartbeat events.
#[derive(Clone)]
pub struct WorkerPicker {
    engine: Arc<Engine>,
    snapshot: Arc<RwLock<HashMap<String, WorkerInfo>>>,
}

#[derive(Clone, Debug)]
pub struct WorkerInfo {
    pub labels: Vec<String>,
    pub jobs_running: u32,
    pub max_concurrency: u32,
    pub status: String,
}

impl WorkerPicker {
    pub fn new(engine: Arc<Engine>) -> Self {
        WorkerPicker {
            engine,
            snapshot: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn refresh(&self) {
        let mut map = HashMap::new();
        if let Ok(rows) = self.engine.store.list_workers().await {
            for row in &rows {
                let cell = |n: &str| row.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
                let id = cell("id")
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                let labels: Vec<String> = cell("labels")
                    .and_then(|v| v.as_str().map(String::from))
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                let jobs_running =
                    cell("jobs_running").and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let status = cell("status")
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                let max_concurrency = cell("capabilities")
                    .and_then(|v| v.as_str().map(String::from))
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|c| c.get("max_concurrency").and_then(|m| m.as_u64()))
                    .unwrap_or(8) as u32;
                map.insert(
                    id,
                    WorkerInfo {
                        labels,
                        jobs_running,
                        max_concurrency,
                        status,
                    },
                );
            }
        }
        *self.snapshot.write().unwrap() = map;
    }

    /// Pick a worker for a job (spec §8/§10): explicit worker id, else a
    /// worker-group (label) member, else any worker whose labels cover the
    /// job's resource tags. Least-loaded first; workers at their cap are
    /// skipped. `None` = stay QUEUED (visible, no crash loop).
    pub fn pick(&self, job: &JobSpec) -> Option<String> {
        let snap = self.snapshot.read().unwrap();
        let online: Vec<(&String, &WorkerInfo)> = snap
            .iter()
            .filter(|(_, w)| w.status == "ONLINE" && w.jobs_running < w.max_concurrency)
            .collect();
        if online.is_empty() {
            return None;
        }
        match &job.worker {
            Some(name) => {
                // explicit worker id wins; otherwise treat as a group label.
                if online.iter().any(|(id, _)| *id == name) {
                    return Some(name.clone());
                }
                online
                    .iter()
                    .filter(|(_, w)| w.labels.iter().any(|l| l == name))
                    .min_by_key(|(_, w)| w.jobs_running)
                    .map(|(id, _)| (*id).clone())
            }
            None => online
                .iter()
                .filter(|(_, w)| {
                    job.resources
                        .iter()
                        .all(|r| w.labels.iter().any(|l| l == r))
                })
                .min_by_key(|(_, w)| w.jobs_running)
                .map(|(id, _)| (*id).clone()),
        }
    }
}

pub fn build(
    engine: Arc<Engine>,
    picker: WorkerPicker,
    proxy_probes: Arc<RwLock<std::collections::HashMap<String, netroute::ProxyProbe>>>,
) -> (Router, AppState) {
    let state = AppState {
        engine,
        picker: picker.clone(),
        proxy_probes,
    };
    let authed = Router::new()
        .route("/workers/register", post(register))
        .route("/workers/{id}/heartbeat", post(heartbeat))
        .route("/workers/{id}/drain", post(drain))
        .route("/workers/{id}/retire", post(drain))
        .route("/workers/{id}", axum::routing::delete(unregister))
        .route("/runs/{id}/claim", post(claim))
        .route("/runs/{id}/complete", post(complete))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{name}/run", post(trigger_run))
        .route("/jobs/{name}/stop", post(stop_run))
        .route("/jobs/{name}/history", get(history))
        .route("/jobs/{name}/spec", get(job_spec))
        .route("/jobs/{name}/logs", get(job_logs))
        .route("/jobs/{name}", axum::routing::delete(delete_job))
        .route("/workers", get(list_workers))
        .route("/proxies", get(list_proxies))
        .route("/reload", post(reload))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_auth,
        ));

    let json_paths = (
        state.engine.cfg.api.synora_json_path.clone(),
        state.engine.cfg.api.tunasync_json_path.clone(),
    );
    let mut router = Router::new()
        .nest(API_V1, authed)
        .route("/metrics", get(metrics))
        .route("/healthz", get(|| async { "ok" }));
    // Status JSON for mirror-web frontends (paths configurable, spec
    // §88–§89). `status_format` picks which shape(s) to expose:
    // "synora", "tunasync", or "both" (default).
    let format = state.engine.cfg.api.status_format.as_str();
    if !json_paths.0.is_empty() && matches!(format, "synora" | "both") {
        router = router.route(&json_paths.0, get(synora_json));
    }
    if !json_paths.1.is_empty() && matches!(format, "tunasync" | "both") {
        router = router.route(&json_paths.1, get(tunasync_json));
    }
    (router.with_state(state.clone()), state)
}

/// Native status JSON (spec §89).
async fn synora_json(State(state): State<AppState>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "jobs": status_entries(&state).await,
    }))
}

/// tunasync-compatible JSON for mirror-web drop-in (user requirement).
/// Format mirrors TUNA's /static/tunasync.json (a bare array of mirrors).
async fn tunasync_json(State(state): State<AppState>) -> axum::Json<serde_json::Value> {
    let entries = status_entries(&state).await;
    axum::Json(serde_json::Value::Array(
        entries
            .into_iter()
            .map(|e| {
                let size = e
                    .get("size_bytes")
                    .and_then(json_u64)
                    .map(synora_core::tunasync_size)
                    .unwrap_or_default();
                serde_json::json!({
                    "name": e.get("name"),
                    "is_master": true,
                    "status": e.get("status"),
                    "last_update": e.get("last_finished").and_then(json_i64).map(fmt_tunasync),
                    "last_update_ts": e.get("last_finished"),
                    "last_started": e.get("last_started").and_then(json_i64).map(fmt_tunasync),
                    "last_started_ts": e.get("last_started"),
                    "last_ended": e.get("last_finished").and_then(json_i64).map(fmt_tunasync),
                    "last_ended_ts": e.get("last_finished"),
                    "next_schedule": e.get("next_run").and_then(json_i64).map(fmt_tunasync),
                    "next_schedule_ts": e.get("next_run"),
                    "upstream": e.get("upstream"),
                    "size": size,
                })
            })
            .collect(),
    ))
}

fn json_i64(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_u64().and_then(|n| i64::try_from(n).ok()))
}

fn json_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
}

/// Strip userinfo from a URL (rsync/git upstreams embed credentials).
fn strip_userinfo(u: &str) -> String {
    match u.split_once("://") {
        Some((scheme, rest)) => match rest.find('@') {
            Some(at) if rest[..at].contains(':') => format!("{scheme}://{}", &rest[at + 1..]),
            _ => u.to_string(),
        },
        None => u.to_string(),
    }
}

/// Shared status collection for both JSON shapes.
async fn status_entries(state: &AppState) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if let (Ok(statuses), Ok(schedules)) = (
        state.engine.store.job_status_list().await,
        state.engine.store.all_schedules().await,
    ) {
        for (name, status) in statuses {
            let job = state.engine.job(&name);
            let last = state
                .engine
                .store
                .run_history(&name, 1)
                .await
                .ok()
                .and_then(|mut v| v.pop());
            let next_run = schedules
                .iter()
                .find(|(n, _)| *n == name)
                .and_then(|(_, r)| r.next_run);
            let size = state
                .engine
                .store
                .repository_size(
                    &job.as_ref()
                        .map(|j| j.storage.display().to_string())
                        .unwrap_or_default(),
                )
                .await
                .ok()
                .flatten();
            out.push(serde_json::json!({
                "name": name,
                "status": status.as_str().to_string(),
                "worker": last.as_ref().and_then(|r| r.worker_id.clone()),
                "upstream": job
                    .as_ref()
                    .and_then(|j| j.upstream.clone())
                    .map(|u| strip_userinfo(&u)),
                "size_bytes": size,
                "size_human": size.map(|s| synora_core::human_size(s as u64)),
                "last_started": last.as_ref().and_then(|r| r.started_at),
                "last_finished": last.as_ref().and_then(|r| r.finished_at),
                "last_started_human": last.as_ref().and_then(|r| r.started_at).map(fmt_local),
                "last_finished_human": last.as_ref().and_then(|r| r.finished_at).map(fmt_local),
                "next_run": next_run,
                "next_run_human": next_run.map(fmt_local),
            }));
        }
    }
    out
}

fn local_datetime(ts: i64) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::from_unix_timestamp(ts).ok().map(|t| {
        t.to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC))
    })
}

fn fmt_local(ts: i64) -> String {
    local_datetime(ts)
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| "-".into())
}

/// tunasync.json timestamps: `2026-08-22 07:13:00 +0800`.
fn fmt_tunasync(ts: i64) -> String {
    const FMT: &[time::format_description::FormatItem<'_>] = time::macros::format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour sign:mandatory padding:zero][offset_minute]"
    );
    local_datetime(ts)
        .and_then(|t| t.format(&FMT).ok())
        .unwrap_or_else(|| "-".into())
}

fn proxy_env_for(state: &AppState, job: &synora_core::job::JobSpec) -> Vec<(String, String)> {
    state
        .engine
        .netroute
        .read()
        .ok()
        .and_then(|g| {
            g.as_ref().map(|nr| {
                let sel = nr.select_proxy(job.proxy.as_deref());
                let cfg = match &sel {
                    netroute::Selection::Forward { name, .. } => nr.proxy_configs().get(name),
                    _ => None,
                };
                netroute::dispatch_proxy_env(cfg, &sel)
            })
        })
        .unwrap_or_default()
}

fn run_assignment(
    state: &AppState,
    run_id: String,
    job: synora_core::job::JobSpec,
) -> RunAssignment {
    let proxy_env = proxy_env_for(state, &job);
    RunAssignment {
        run_id,
        job,
        proxy_env,
    }
}

// ---------------------------------------------------------------------------
// Worker-facing handlers (token name = worker id)
// ---------------------------------------------------------------------------

async fn register(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    axum::Json(body): axum::Json<RegisterRequest>,
) -> Result<axum::Json<RegisterResponse>, StatusCode> {
    require(&auth, "runs.manage")?;
    // Worker id: the requested name, or the token name (spec §9).
    let worker_id = body
        .name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| auth.name.clone());
    if let Ok(Some(owner)) = state.engine.store.worker_token(&worker_id).await {
        if !owner.is_empty() && owner != auth.name {
            return Err(StatusCode::CONFLICT);
        }
    }
    let mut capabilities = body.capabilities.clone();
    if capabilities.get("max_concurrency").is_none() {
        capabilities["max_concurrency"] = serde_json::json!(8);
    }
    state
        .engine
        .store
        .upsert_worker(
            &worker_id,
            &body.hostname,
            &body.address,
            &body.version,
            &body.labels,
            &auth.name,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // The worker process restarted: claimed runs cannot be resumed.
    // Mark them LOST and honor on_worker_lost=retry here — the reaper
    // only re-queues runs that *it* expires, so a register-path LOST
    // would otherwise sit until the next interval.
    let lost = state
        .engine
        .store
        .mark_worker_runs_lost(&worker_id)
        .await
        .unwrap_or_default();
    for run in lost {
        tracing::warn!(
            "run {} (job {}) lost: worker `{worker_id}` re-registered",
            run.id,
            run.job_id
        );
        let job = state.engine.job(&run.job_id);
        let retry = job
            .as_ref()
            .map(|j| matches!(j.on_worker_lost, synora_core::OnWorkerLost::Retry))
            .unwrap_or(false);
        if !retry {
            continue;
        }
        let worker = job.as_ref().and_then(|j| state.picker.pick(j));
        match state
            .engine
            .store
            .create_lost_requeue(&run.id, &run.job_id, worker.as_deref())
            .await
        {
            Ok(Some(new_id)) => tracing::info!(
                "job `{}`: re-queued as {new_id} after worker re-register",
                run.job_id
            ),
            Ok(None) => {}
            Err(e) => tracing::warn!("job `{}`: lost re-queue failed: {e}", run.job_id),
        }
    }
    // Persist capabilities (incl. max_concurrency).
    let _ = state
        .engine
        .store
        .db()
        .execute(
            "UPDATE workers SET capabilities = ? WHERE id = ?",
            &[capabilities.to_string().into(), worker_id.clone().into()],
        )
        .await;
    state.picker.refresh().await;
    tracing::info!("worker `{worker_id}` registered ({})", body.hostname);
    Ok(axum::Json(RegisterResponse {
        worker_id,
        heartbeat_interval_secs: 15,
    }))
}

/// Identity gate: the calling token must have registered this worker.
/// A worker token whose name equals the worker id is always accepted
/// (self-identity), so a renamed/reissued token can still heartbeat.
async fn token_owns_worker(state: &AppState, auth: &AuthUser, worker_id: &str) -> bool {
    if auth.name == worker_id {
        return true;
    }
    match state.engine.store.worker_token(worker_id).await {
        Ok(Some(t)) => t.is_empty() || t == auth.name,
        Ok(None) => false,
        Err(_) => false,
    }
}

fn size_keys_for(state: &AppState, job: &synora_core::job::JobSpec) -> Vec<String> {
    let raw = job.storage.display().to_string();
    let resolved = state
        .engine
        .run_storage
        .as_ref()
        .map(|c| c.resolve_storage_path(job).display().to_string())
        .unwrap_or_else(|| raw.clone());
    let mut keys = vec![job.name.clone(), raw.clone(), resolved.clone()];
    if let Some(file) = std::path::Path::new(&resolved)
        .file_name()
        .and_then(|s| s.to_str())
    {
        keys.push(file.to_string());
    }
    keys.sort();
    keys.dedup();
    keys
}

async fn persist_repository_size(state: &AppState, job: &synora_core::job::JobSpec, size: i64) {
    for key in size_keys_for(state, job) {
        let _ = state.engine.store.set_repository_size(&key, size).await;
    }
    state.engine.metrics.set_gauge(
        "synora_repository_size_bytes",
        &[("job", job.name.as_str())],
        size as f64,
    );
}

async fn apply_repository_sizes(state: &AppState, samples: &[api::RepoSizeSample]) {
    if samples.is_empty() {
        return;
    }
    let jobs = state.engine.jobs();
    for job in &jobs {
        let raw = job.storage.display().to_string();
        let resolved = state
            .engine
            .run_storage
            .as_ref()
            .map(|c| c.resolve_storage_path(job).display().to_string())
            .unwrap_or_else(|| raw.clone());
        let wants = [
            resolved.trim_end_matches('/'),
            raw.trim_end_matches('/'),
            job.name.as_str(),
        ];
        // Exact mountpoint / storage path only. A prefix match would assign
        // the whole pool (70T+) to every job.
        let hit = samples.iter().find(|s| {
            let p = s.path.trim_end_matches('/');
            wants.iter().any(|want| !want.is_empty() && p == *want)
        });
        if let Some(sample) = hit {
            persist_repository_size(state, job, sample.bytes as i64).await;
        }
    }
}

async fn heartbeat(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(worker_id): Path<String>,
    axum::Json(body): axum::Json<HeartbeatRequest>,
) -> Result<axum::Json<HeartbeatResponse>, StatusCode> {
    require(&auth, "runs.manage")?;
    if !token_owns_worker(&state, &auth, &worker_id).await {
        return Err(StatusCode::FORBIDDEN);
    }
    state
        .engine
        .store
        .touch_heartbeat(
            &worker_id,
            body.jobs_running,
            &body.status,
            &body.active_jobs,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state.engine.metrics.set_gauge(
        "synora_worker_jobs_running",
        &[("worker", worker_id.as_str())],
        body.jobs_running as f64,
    );
    // Lifecycle gauge (spec §36): 1=ONLINE, 0=OFFLINE, 2=DRAINING, 3=MAINTENANCE.
    // Busy/idle is a separate dimension: synora_worker_jobs_running.
    state.engine.metrics.set_gauge(
        "synora_worker_status",
        &[("worker", worker_id.as_str())],
        1.0,
    );
    for sample in &body.resources {
        if let Some(mem) = sample.memory_bytes {
            state.engine.metrics.set_gauge(
                "synora_job_memory_bytes",
                &[("job", sample.job.as_str()), ("worker", worker_id.as_str())],
                mem as f64,
            );
        }
        if let Some(cpu) = sample.cpu_seconds {
            state.engine.metrics.set_gauge(
                "synora_job_cpu_seconds",
                &[("job", sample.job.as_str()), ("worker", worker_id.as_str())],
                cpu,
            );
        }
        if let Some(pct) = sample.cpu_percent {
            state.engine.metrics.set_gauge(
                "synora_job_cpu_percent",
                &[("job", sample.job.as_str()), ("worker", worker_id.as_str())],
                pct,
            );
        }
        if let Some(bps) = sample.bandwidth_bytes {
            if bps.is_finite() && (0.0..=20_000_000_000.0).contains(&bps) {
                state.engine.metrics.set_job_gauge(
                    "synora_job_bandwidth_bytes",
                    &sample.job,
                    &[("job", sample.job.as_str()), ("worker", worker_id.as_str())],
                    bps,
                );
            }
        }
    }
    apply_repository_sizes(&state, &body.repository_sizes).await;
    if !body.active_jobs.is_empty() {
        let _ = state
            .engine
            .store
            .mark_jobs_running(&worker_id, &body.active_jobs)
            .await;
        // Do not overwrite job_status from the heartbeat. A worker can
        // still list a job for one tick after complete_run marked it
        // RETRYING/FAILED; the reaper and complete/claim handlers own
        // the gauge.
    }

    let mut response = HeartbeatResponse {
        assignment: None,
        assignments: Vec::new(),
        cancel_run: None,
        offline_grace_secs: 45,
    };
    // Offer enough queued runs to fill free slots in one beat (old workers
    // still claim `assignment` only).
    if let Ok(runs) = state.engine.store.assigned_runs(&worker_id).await {
        for run in runs.into_iter().take(32) {
            let Some(job) = state.engine.job(&run.job_id) else {
                continue;
            };
            let offered = run_assignment(&state, run.id.clone(), job);
            if response.assignment.is_none() {
                response.assignment = Some(offered.clone());
            }
            response.assignments.push(offered);
        }
    }
    // Ask the worker to cancel any CANCELLING runs it holds.
    if let Ok(runs) = state.engine.store.cancelling_runs_of(&worker_id).await {
        if let Some(run) = runs.first() {
            response.cancel_run = Some(run.id.clone());
        }
    }
    Ok(axum::Json(response))
}

async fn claim(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(run_id): Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<axum::Json<RunAssignment>, StatusCode> {
    require(&auth, "runs.manage")?;
    // Per-job concurrency gate: never claim a run while another run of the
    // same job is active (prevents two workers — or two claims — racing on
    // one mirror's directory/container name).
    if let Ok(Some(row)) = state.engine.store.get_run(&run_id).await {
        if let Ok(active) = state.engine.store.active_runs_of_job(&row.job_id).await {
            if !active.is_empty() {
                return Err(StatusCode::CONFLICT);
            }
        }
    }
    // Worker id: registered name via ?worker= (must be owned by this
    // token), else the token's own name.
    let worker = params
        .get("worker")
        .cloned()
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| auth.name.clone());
    if !token_owns_worker(&state, &auth, &worker).await {
        return Err(StatusCode::FORBIDDEN);
    }
    let claimed = state
        .engine
        .store
        .claim_run(&run_id, &worker)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !claimed {
        return Err(StatusCode::CONFLICT);
    }
    let run = state
        .engine
        .store
        .get_run(&run_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let job = state.engine.job(&run.job_id).ok_or(StatusCode::NOT_FOUND)?;
    let proxy_env = proxy_env_for(&state, &job);
    state
        .engine
        .metrics
        .inc_counter("synora_job_runs_total", &[("job", job.name.as_str())], 1.0);
    state.engine.metrics.set_gauge(
        "synora_job_last_start_timestamp",
        &[("job", job.name.as_str())],
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as f64)
            .unwrap_or(0.0),
    );
    state.engine.metrics.set_job_gauge(
        "synora_job_status",
        &job.name,
        &[("job", job.name.as_str()), ("worker", worker.as_str())],
        engine::status_value(JobStatus::Syncing),
    );
    Ok(axum::Json(RunAssignment {
        run_id,
        job,
        proxy_env,
    }))
}

async fn complete(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(run_id): Path<String>,
    axum::Json(body): axum::Json<CompleteRequest>,
) -> Result<StatusCode, StatusCode> {
    require(&auth, "runs.manage")?;
    let run = state
        .engine
        .store
        .get_run(&run_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    // Identity: the calling token must own the worker this run is
    // assigned to (register-time binding, verified against the DB).
    let run_worker = run.worker_id.clone().unwrap_or_default();
    if !token_owns_worker(&state, &auth, &run_worker).await {
        return Err(StatusCode::FORBIDDEN);
    }
    let _ = &body.worker_id;
    let Some(job) = state.engine.job(&run.job_id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let ended = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let duration = ended.saturating_sub(run.started_at.unwrap_or(run.created_at));
    // Metric labels use the run's worker (named workers register as their
    // own id; auth.name is only the token name).
    let worker_label = run.worker_id.clone().unwrap_or_else(|| auth.name.clone());
    let mut new_status = JobStatus::Success;
    // Store the worker-reported log (distributed runs: the log lives on the
    // worker host; the manager keeps the text for job_logs).
    if let Some(log) = body.log.as_deref().filter(|l| !l.is_empty()) {
        let _ = state
            .engine
            .store
            .insert_log_with(
                &run_id,
                &job.name,
                &format!("/var/log/synora/{}/current.log", job.name),
                log,
            )
            .await;
    }

    match body.status.as_str() {
        "cancelled" => {
            new_status = JobStatus::Cancelled;
            let _ = state
                .engine
                .store
                .finish_run(
                    &run_id,
                    JobStatus::Cancelled,
                    None,
                    None,
                    None,
                    None,
                    body.message.as_deref(),
                    duration,
                )
                .await;
        }
        "success" => {
            let _ = state
                .engine
                .store
                .finish_run(
                    &run_id,
                    JobStatus::Success,
                    body.exit_code.map(|v| v as i32),
                    body.size_before,
                    body.size_after,
                    body.bytes_transferred,
                    body.message.as_deref(),
                    duration,
                )
                .await;
            if let Some(size) = body.size_after {
                persist_repository_size(&state, &job, size).await;
            }
            if let Some(bytes) = body.bytes_transferred {
                state.engine.metrics.inc_counter(
                    "synora_job_bytes_transferred_total",
                    &[
                        ("job", job.name.as_str()),
                        ("worker", worker_label.as_str()),
                    ],
                    bytes as f64,
                );
            }
            state.engine.metrics.set_gauge(
                "synora_job_last_success_timestamp",
                &[("job", job.name.as_str())],
                ended as f64,
            );
            state.engine.metrics.inc_counter(
                "synora_job_success_total",
                &[("job", job.name.as_str())],
                1.0,
            );
        }
        "failed" => {
            let kind = synora_core::ErrorKind::ProviderError; // worker-reported failure
            let decision = synora_core::state::retry_decision(
                kind,
                run.retry_count,
                job.retry,
                job.retry_delay.whole_seconds().max(1) as u64,
                job.retry_backoff,
            );
            match decision {
                synora_core::RetryDecision::Retry { delay_secs } => {
                    new_status = JobStatus::Retrying;
                    let _ = state
                        .engine
                        .store
                        .set_retry(&run_id, ended + delay_secs as i64, run.retry_count + 1)
                        .await;
                    let _ = state
                        .engine
                        .store
                        .set_run_status(&run_id, JobStatus::Retrying)
                        .await;
                    state.engine.metrics.inc_counter(
                        "synora_job_retries_total",
                        &[("job", job.name.as_str())],
                        1.0,
                    );
                }
                synora_core::RetryDecision::NoRetry => {
                    new_status = JobStatus::Failed;
                    let _ = state
                        .engine
                        .store
                        .finish_run(
                            &run_id,
                            JobStatus::Failed,
                            body.exit_code.map(|v| v as i32),
                            None,
                            None,
                            None,
                            body.message.as_deref(),
                            duration,
                        )
                        .await;
                    state.engine.metrics.inc_counter(
                        "synora_job_failures_total",
                        &[("job", job.name.as_str())],
                        1.0,
                    );
                }
            }
        }
        _ => return Err(StatusCode::BAD_REQUEST),
    }
    if body.memory_bytes.is_some() || body.cpu_seconds.is_some() {
        let _ = state
            .engine
            .store
            .set_run_resources(&run_id, body.memory_bytes, body.cpu_seconds)
            .await;
    }
    if let Some(mem) = body.memory_bytes {
        state.engine.metrics.set_gauge(
            "synora_job_memory_bytes",
            &[
                ("job", job.name.as_str()),
                ("worker", worker_label.as_str()),
            ],
            mem as f64,
        );
    }
    if let Some(cpu) = body.cpu_seconds {
        state.engine.metrics.inc_counter(
            "synora_job_cpu_usage_seconds_total",
            &[
                ("job", job.name.as_str()),
                ("worker", worker_label.as_str()),
            ],
            cpu,
        );
        state.engine.metrics.set_gauge(
            "synora_job_cpu_seconds",
            &[
                ("job", job.name.as_str()),
                ("worker", worker_label.as_str()),
            ],
            cpu,
        );
    }
    state.engine.metrics.set_gauge(
        "synora_job_bandwidth_bytes",
        &[
            ("job", job.name.as_str()),
            ("worker", worker_label.as_str()),
        ],
        0.0,
    );
    state.engine.metrics.set_job_gauge(
        "synora_job_status",
        &job.name,
        &[
            ("job", job.name.as_str()),
            ("worker", worker_label.as_str()),
        ],
        engine::status_value(new_status),
    );
    state.engine.metrics.set_gauge(
        "synora_job_duration_seconds",
        &[("job", job.name.as_str())],
        duration as f64,
    );
    state.engine.metrics.set_gauge(
        "synora_job_last_end_timestamp",
        &[("job", job.name.as_str())],
        ended as f64,
    );
    state.picker.refresh().await;
    Ok(StatusCode::OK)
}

async fn drain(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(worker_id): Path<String>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    if !has_perm(&auth, "workers.write") && !token_owns_worker(&state, &auth, &worker_id).await {
        return Err(StatusCode::FORBIDDEN);
    }
    state
        .engine
        .store
        .set_worker_status(&worker_id, "DRAINING")
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    state.picker.refresh().await;
    tracing::info!("worker `{worker_id}` stopped accepting new runs");
    Ok(axum::Json(serde_json::json!({"ok": true})))
}

async fn unregister(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(worker_id): Path<String>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    // Operators use workers.write; a worker token (runs.manage) may
    // unregister itself on shutdown.
    if !has_perm(&auth, "workers.write") {
        require(&auth, "runs.manage")?;
        if !token_owns_worker(&state, &auth, &worker_id).await {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    // Only unregister when nothing is running on it (spec §11).
    if let Ok(runs) = state.engine.store.active_runs_of(&worker_id).await {
        if !runs.is_empty() {
            return Err(StatusCode::CONFLICT);
        }
    }
    state
        .engine
        .store
        .delete_worker(&worker_id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    state.picker.refresh().await;
    tracing::info!("worker `{worker_id}` unregistered");
    Ok(axum::Json(serde_json::json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Operator-facing handlers
// ---------------------------------------------------------------------------

async fn list_jobs(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
) -> Result<axum::Json<Vec<JobDTO>>, StatusCode> {
    require(&auth, "jobs.read")?;
    let store = &state.engine.store;
    let statuses = store
        .job_status_list()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let schedules = store
        .all_schedules()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut out = Vec::new();
    for (name, status) in statuses {
        let job = state.engine.job(&name);
        let next_run = schedules
            .iter()
            .find(|(n, _)| *n == name)
            .and_then(|(_, r)| r.next_run);
        let last_run = store
            .run_history(&name, 1)
            .await
            .ok()
            .and_then(|mut v| v.pop())
            .map(run_dto);
        let size_bytes = store
            .repository_size(
                &job.as_ref()
                    .map(|j| j.storage.display().to_string())
                    .unwrap_or_default(),
            )
            .await
            .ok()
            .flatten();
        out.push(JobDTO {
            name: name.clone(),
            enabled: job.as_ref().map(|j| j.enabled).unwrap_or(false),
            status: status.as_str().to_string(),
            worker: job.as_ref().and_then(|j| j.worker.clone()),
            provider: job
                .as_ref()
                .map(|j| match &j.provider {
                    synora_core::ProviderConfig::Rsync { .. } => "rsync",
                    synora_core::ProviderConfig::TwoStageRsync { .. } => "two-stage-rsync",
                    synora_core::ProviderConfig::Script { .. } => "script",
                    synora_core::ProviderConfig::Docker { .. } => "docker",
                    synora_core::ProviderConfig::Git { .. } => "git",
                    synora_core::ProviderConfig::Http { .. } => "http",
                })
                .unwrap_or("")
                .to_string(),
            upstream: job.as_ref().and_then(|j| j.upstream.clone()),
            storage_path: job
                .as_ref()
                .map(|j| j.storage.display().to_string())
                .unwrap_or_default(),
            schedule: job
                .as_ref()
                .map(|j| j.schedule.describe())
                .unwrap_or_default(),
            next_run,
            last_run,
            size_bytes,
        });
    }
    Ok(axum::Json(out))
}

async fn delete_job(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(name): Path<String>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    require(&auth, "jobs.write")?;
    if state.engine.job(&name).is_none()
        && state
            .engine
            .store
            .job_status_list()
            .await
            .map(|rows| !rows.iter().any(|(n, _)| n == &name))
            .unwrap_or(true)
    {
        return Err(StatusCode::NOT_FOUND);
    }
    if let Some(path) = state.engine.config_path() {
        match config::remove_job_block(&path, &name) {
            Ok(_) => {}
            Err(e) if e.contains("not found") => {}
            Err(e) => {
                tracing::error!("delete `{name}` config: {e}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }
    state.engine.forget_job(&name).await.map_err(|e| {
        tracing::error!("purge `{name}`: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(axum::Json(serde_json::json!({"ok": true, "deleted": name})))
}

async fn trigger_run(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(name): Path<String>,
) -> Result<axum::Json<String>, StatusCode> {
    require(&auth, "jobs.write")?;
    state
        .engine
        .dispatch(&name, true)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)
        .map(axum::Json)
}

async fn stop_run(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(name): Path<String>,
) -> Result<StatusCode, StatusCode> {
    require(&auth, "jobs.write")?;
    // Local runs (standalone-mode manager) cancel via the engine; remote runs
    // get marked CANCELLING and the worker cancels on its next heartbeat.
    if let Ok(runs) = state
        .engine
        .store
        .active_runs_of(engine::LOCAL_WORKER)
        .await
    {
        if runs.iter().any(|r| r.job_id == name) {
            let _ = state.engine.stop_job(&name).await;
            return Ok(StatusCode::OK);
        }
    }
    state
        .engine
        .store
        .set_cancelling_by_job(&name)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

/// Full job definition (for the TUI's structured editor).
async fn job_spec(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(name): Path<String>,
) -> Result<axum::Json<JobSpec>, StatusCode> {
    require(&auth, "jobs.read")?;
    state
        .engine
        .job(&name)
        .map(axum::Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn history(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(name): Path<String>,
) -> Result<axum::Json<Vec<RunDTO>>, StatusCode> {
    require(&auth, "jobs.read")?;
    let runs = state
        .engine
        .store
        .run_history(&name, 20)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(axum::Json(runs.into_iter().map(run_dto).collect()))
}

async fn job_logs(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<String, StatusCode> {
    require(&auth, "logs.read")?;
    // The name must be a configured job — a raw path segment must never
    // reach log_dir.join (path traversal).
    if state.engine.job(&name).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    // Cap the tail at 10_000 lines: the caller wants the tail, not the
    // whole file replayed.
    let tail: usize = params
        .get("tail")
        .and_then(|t| t.parse().ok())
        .unwrap_or(50)
        .min(10_000);
    let path = state
        .engine
        .cfg
        .daemon
        .log_dir
        .join(&name)
        .join("current.log");
    let mut content = tail_of_file(&path, tail);
    if content.as_deref().unwrap_or("").trim().is_empty() {
        // Distributed runs: worker logs arrive with `complete` and are
        // stored in job_logs.content.
        match state.engine.store.latest_log_content(&name).await {
            Ok(Some(c)) if !c.is_empty() => content = Some(tail_of_str(&c, tail)),
            _ => return Err(StatusCode::NOT_FOUND),
        }
    }
    match content {
        Some(c) => Ok(c),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Read the last `n` lines of a file without loading the whole thing:
/// seek to the end, walk back over at most the final 4 MiB.
fn tail_of_file(path: &std::path::Path, n: usize) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let size = f.metadata().ok()?.len();
    let want = size.min(4 * 1024 * 1024);
    f.seek(SeekFrom::End(-(want as i64))).ok()?;
    let mut buf = Vec::with_capacity(want as usize);
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    // The window starts mid-line; drop the first partial line.
    let skip = usize::from(size > want);
    Some(tail_of_str_skip(&text, n, skip))
}

fn tail_of_str(s: &str, n: usize) -> String {
    tail_of_str_skip(s, n, 0)
}

fn tail_of_str_skip(s: &str, n: usize, skip_first: usize) -> String {
    let mut lines: Vec<&str> = s.lines().collect();
    if skip_first > 0 && !lines.is_empty() {
        let drop_n = skip_first.min(lines.len());
        lines.drain(0..drop_n);
    }
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

async fn list_workers(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
) -> Result<axum::Json<Vec<WorkerDTO>>, StatusCode> {
    require(&auth, "workers.read")?;
    let rows = state
        .engine
        .store
        .list_workers()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let cell = |row: &Vec<(String, db::DbValue)>, n: &str| {
        row.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone())
    };
    let mut out = Vec::new();
    for row in &rows {
        out.push(WorkerDTO {
            id: cell(row, "id")
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default(),
            hostname: cell(row, "hostname")
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default(),
            address: cell(row, "address")
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default(),
            version: cell(row, "version")
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default(),
            labels: cell(row, "labels")
                .and_then(|v| v.as_str().map(String::from))
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
            capabilities: cell(row, "capabilities")
                .and_then(|v| v.as_str().map(String::from))
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Null),
            status: cell(row, "status")
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default(),
            jobs_running: cell(row, "jobs_running")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as u32,
            last_heartbeat: cell(row, "last_heartbeat")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        });
    }
    Ok(axum::Json(out))
}

/// Proxy list with probe results (latency + detected egress IP), for the TUI
/// and operator tooling.
async fn list_proxies(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    require(&auth, "workers.read")?;
    // Prefer the reload-updated NetRoute snapshot; fall back to the startup
    // config (identical when no proxy/egress config exists at all).
    let proxy_cfgs: Vec<(String, config::ProxyConfig)> = match state.engine.netroute.read() {
        Ok(guard) => match guard.as_ref() {
            Some(nr) => nr
                .proxy_configs()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            None => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    let proxy_cfgs = if proxy_cfgs.is_empty() && state.engine.cfg.proxies.is_empty() {
        proxy_cfgs
    } else if proxy_cfgs.is_empty() {
        state
            .engine
            .cfg
            .proxies
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    } else {
        proxy_cfgs
    };
    let proxies: Vec<serde_json::Value> = proxy_cfgs
        .iter()
        .map(|(name, p)| {
            let probe = state.proxy_probes.read().unwrap().get(name).cloned().unwrap_or_default();
            serde_json::json!({
                "name": name,
                "type": match &p.kind {
                    config::ProxyKind::Direct => "direct",
                    config::ProxyKind::Forward { url, .. } if url.starts_with("socks5h://") => "socks5h",
                    config::ProxyKind::Forward { .. } => "http",
                    config::ProxyKind::Command { .. } => "command",
                },
                "expose": p.expose,
                "healthcheck": p.healthcheck,
                "latency_ms": probe.latency_ms,
                "egress_ip": probe.egress_ip,
                "healthy": probe.healthy,
                "last_probe_at": probe.last_probe_at,
            })
        })
        .collect();
    Ok(axum::Json(serde_json::json!({ "proxies": proxies })))
}

async fn reload(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
) -> Result<axum::Json<ReloadResponse>, StatusCode> {
    require(&auth, "jobs.write")?;
    let applied = state
        .engine
        .reload()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(axum::Json(ReloadResponse { applied }))
}

async fn metrics(State(state): State<AppState>) -> String {
    state.engine.metrics().render()
}

fn run_dto(run: db::store::RunRow) -> RunDTO {
    RunDTO {
        id: run.id,
        job_id: run.job_id,
        worker_id: run.worker_id,
        status: run.status.as_str().to_string(),
        retry_count: run.retry_count,
        started_at: run.started_at,
        finished_at: run.finished_at,
        duration_secs: run.duration_secs,
        exit_code: run.exit_code.map(|v| v as i64),
        size_before: None,
        size_after: None,
        bytes_transferred: None,
        message: run.message,
        created_at: run.created_at,
    }
}

// ---------------------------------------------------------------------------
// Serving (plain HTTP or TLS / mTLS, tunasync-style, spec §64)
// ---------------------------------------------------------------------------

pub async fn serve(
    router: Router,
    listen: std::net::SocketAddr,
    tls: &TlsConfig,
    _state: AppState,
) -> Result<(), String> {
    match (&tls.cert, &tls.key) {
        (Some(cert), Some(key)) => {
            let config = if let Some(ca) = &tls.client_ca {
                // mTLS: build the server config manually so the client
                // certificate verifier can be attached.
                let cert_pem = std::fs::read(cert).map_err(|e| e.to_string())?;
                let key_pem = std::fs::read(key).map_err(|e| e.to_string())?;
                let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
                    rustls_pemfile::certs(&mut cert_pem.as_slice())
                        .collect::<Result<_, _>>()
                        .map_err(|e| e.to_string())?;
                let key_der = rustls_pemfile::private_key(&mut key_pem.as_slice())
                    .map_err(|e| e.to_string())?
                    .ok_or("no private key found in key file")?;
                let ca_pem = std::fs::read(ca).map_err(|e| e.to_string())?;
                let mut roots = rustls::RootCertStore::empty();
                for cert in rustls_pemfile::certs(&mut ca_pem.as_slice()) {
                    let cert = cert.map_err(|e| e.to_string())?;
                    roots.add(cert).map_err(|e| e.to_string())?;
                }
                let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                    .build()
                    .map_err(|e| e.to_string())?;
                let inner = rustls::ServerConfig::builder()
                    .with_client_cert_verifier(verifier)
                    .with_single_cert(certs, key_der)
                    .map_err(|e| e.to_string())?;
                axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(inner))
            } else {
                axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                    .await
                    .map_err(|e| e.to_string())?
            };
            tracing::info!(
                "serving https://{listen} (mTLS: {})",
                tls.client_ca.is_some()
            );
            axum_server::bind_rustls(listen, config)
                .serve(router.into_make_service())
                .await
                .map_err(|e| e.to_string())
        }
        // Plain-HTTP path: bound by the caller so bind failures fail
        // startup loudly.
        _ => Err("plain-HTTP path needs a pre-bound listener".to_string()),
    }
}

/// Plain-HTTP serving on a listener the caller already bound.
pub async fn serve_plain(
    router: Router,
    listener: tokio::net::TcpListener,
    listen: std::net::SocketAddr,
) -> Result<(), String> {
    tracing::info!("serving http://{listen}");
    axum::serve(listener, router)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{tail_of_file, tail_of_str_skip};

    #[test]
    fn tail_of_str_skip_drops_partial_first_line() {
        let text = "l1\nl2\nl3\nl4\nl5\n";
        assert_eq!(tail_of_str_skip(text, 2, 0), "l4\nl5");
        assert_eq!(tail_of_str_skip(text, 2, 1), "l4\nl5");
        // tail larger than the file: everything
        assert_eq!(tail_of_str_skip(text, 99, 0), "l1\nl2\nl3\nl4\nl5");
    }

    #[test]
    fn tail_of_file_reads_last_lines_only() {
        let dir = std::env::temp_dir().join("synora-tail-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.txt");
        let big = (0..100_000)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, big).unwrap();
        let tail = tail_of_file(&path, 3).unwrap();
        assert_eq!(tail, "line-99997\nline-99998\nline-99999");
        std::fs::remove_dir_all(&dir).ok();
    }
}
