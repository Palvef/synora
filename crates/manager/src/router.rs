//! REST API surface (spec §46) + worker picker + TLS serving.

use crate::auth::{require, AuthUser};
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
    pub proxy_probes:
        Arc<RwLock<std::collections::HashMap<String, netroute::ProxyProbe>>>,
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
                let id = cell("id").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
                let labels: Vec<String> = cell("labels")
                    .and_then(|v| v.as_str().map(String::from))
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                let jobs_running = cell("jobs_running").and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let status = cell("status").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
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
                .filter(|(_, w)| job.resources.iter().all(|r| w.labels.iter().any(|l| l == r)))
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
        .route("/workers/{id}", axum::routing::delete(unregister))
        .route("/runs/{id}/claim", post(claim))
        .route("/runs/{id}/complete", post(complete))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{name}/run", post(trigger_run))
        .route("/jobs/{name}/stop", post(stop_run))
        .route("/jobs/{name}/history", get(history))
        .route("/jobs/{name}/logs", get(job_logs))
        .route("/workers", get(list_workers))
        .route("/proxies", get(list_proxies))
        .route("/reload", post(reload))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(state.clone(), crate::auth::require_auth));

    let json_paths = (
        state.engine.cfg.api.synora_json_path.clone(),
        state.engine.cfg.api.tunasync_json_path.clone(),
    );
    let mut router = Router::new()
        .nest(API_V1, authed)
        .route("/metrics", get(metrics))
        .route("/healthz", get(|| async { "ok" }));
    // Status JSON for mirror-web frontends (paths configurable, spec §88–§89).
    if !json_paths.0.is_empty() {
        router = router.route(&json_paths.0, get(synora_json));
    }
    if !json_paths.1.is_empty() {
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
                serde_json::json!({
                    "name": e.get("name"),
                    "status": e.get("status"),
                    "upstream": e.get("upstream"),
                    "size": e.get("size_human"),
                    "last_started_ts": e.get("last_started"),
                    "last_started": e.get("last_started_human"),
                    "last_ended_ts": e.get("last_finished"),
                    "last_ended": e.get("last_finished_human"),
                    "last_update_ts": e.get("last_finished"),
                    "last_update": e.get("last_finished_human"),
                    "next_schedule_ts": e.get("next_run"),
                    "next_schedule": e.get("next_run_human"),
                    "is_master": true,
                })
            })
            .collect(),
    ))
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
            let next_run = schedules.iter().find(|(n, _)| *n == name).and_then(|(_, r)| r.next_run);
            let size = state
                .engine
                .store
                .repository_size(
                    &job.as_ref().map(|j| j.storage.display().to_string()).unwrap_or_default(),
                )
                .await
                .ok()
                .flatten();
            out.push(serde_json::json!({
                "name": name,
                "status": format!("{status:?}").to_lowercase(),
                "worker": last.as_ref().and_then(|r| r.worker_id.clone()),
                "upstream": job.as_ref().and_then(|j| j.upstream.clone()),
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

fn fmt_local(ts: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(ts)
        .map(|t| t.to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC)))
        .ok()
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_else(|| "-".into())
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
    let worker_id = auth.name.clone();
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
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Persist capabilities (incl. max_concurrency).
    let _ = state
        .engine
        .store
        .db()
        .execute(
            "UPDATE workers SET capabilities = ? WHERE id = ?",
            &[
                capabilities.to_string().into(),
                worker_id.clone().into(),
            ],
        )
        .await;
    state.picker.refresh().await;
    tracing::info!("worker `{worker_id}` registered ({})", body.hostname);
    Ok(axum::Json(RegisterResponse {
        worker_id,
        heartbeat_interval_secs: 15,
    }))
}

async fn heartbeat(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(worker_id): Path<String>,
    axum::Json(body): axum::Json<HeartbeatRequest>,
) -> Result<axum::Json<HeartbeatResponse>, StatusCode> {
    require(&auth, "runs.manage")?;
    if auth.name != worker_id {
        return Err(StatusCode::FORBIDDEN);
    }
    state
        .engine
        .store
        .touch_heartbeat(&worker_id, body.jobs_running, &body.status)
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

    let mut response = HeartbeatResponse {
        assignment: None,
        cancel_run: None,
        offline_grace_secs: 45,
    };
    // Offer one queued run assigned to this worker.
    if let Ok(runs) = state.engine.store.assigned_runs(&worker_id).await {
        if let Some(run) = runs.first() {
            if let Some(job) = state.engine.job(&run.job_id) {
                response.assignment = Some(RunAssignment {
                    run_id: run.id.clone(),
                    job,
                });
            }
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
) -> Result<axum::Json<RunAssignment>, StatusCode> {
    require(&auth, "runs.manage")?;
    let claimed = state
        .engine
        .store
        .claim_run(&run_id, &auth.name)
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
    Ok(axum::Json(RunAssignment { run_id, job }))
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
    if run.worker_id.as_deref() != Some(auth.name.as_str()) {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(job) = state.engine.job(&run.job_id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let ended = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let duration = run
        .created_at
        .checked_neg()
        .map(|_| 0)
        .unwrap_or(0)
        .max(ended.saturating_sub(run.created_at));

    match body.status.as_str() {
        "cancelled" => {
            let _ = state
                .engine
                .store
                .finish_run(&run_id, JobStatus::Cancelled, None, None, None, None, body.message.as_deref(), duration)
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
                let _ = state
                    .engine
                    .store
                    .set_repository_size(&job.storage.display().to_string(), size)
                    .await;
                state.engine.metrics.set_gauge(
                    "synora_repository_size_bytes",
                    &[("repository", job.name.as_str())],
                    size as f64,
                );
            }
            if let Some(bytes) = body.bytes_transferred {
                state.engine.metrics.inc_counter(
                    "synora_job_bytes_transferred_total",
                    &[("job", job.name.as_str()), ("worker", auth.name.as_str())],
                    bytes as f64,
                );
            }
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
                    state
                        .engine
                        .metrics
                        .inc_counter("synora_job_retries_total", &[("job", job.name.as_str())], 1.0);
                }
                synora_core::RetryDecision::NoRetry => {
                    let _ = state
                        .engine
                        .store
                        .finish_run(&run_id, JobStatus::Failed, body.exit_code.map(|v| v as i32), None, None, None, body.message.as_deref(), duration)
                        .await;
                    state
                        .engine
                        .metrics
                        .inc_counter("synora_job_failures_total", &[("job", job.name.as_str())], 1.0);
                }
            }
        }
        _ => return Err(StatusCode::BAD_REQUEST),
    }
    state.engine.metrics.set_gauge(
        "synora_job_status",
        &[("job", job.name.as_str()), ("worker", auth.name.as_str())],
        engine::status_value(run.status),
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
    require(&auth, "workers.write")?;
    state
        .engine
        .store
        .set_worker_status(&worker_id, "DRAINING")
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    state.picker.refresh().await;
    tracing::info!("worker `{worker_id}` draining");
    Ok(axum::Json(serde_json::json!({"ok": true})))
}

async fn unregister(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(worker_id): Path<String>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    require(&auth, "workers.write")?;
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
    let statuses = store.job_status_list().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let schedules = store.all_schedules().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
            status: format!("{:?}", status),
            worker: job.as_ref().and_then(|j| j.worker.clone()),
            provider: job
                .as_ref()
                .map(|j| match &j.provider {
                    synora_core::ProviderConfig::Rsync { .. } => "rsync",
                    synora_core::ProviderConfig::Script { .. } => "script",
                    synora_core::ProviderConfig::Docker { .. } => "docker",
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

async fn trigger_run(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthUser>,
    Path(name): Path<String>,
) -> Result<axum::Json<String>, StatusCode> {
    require(&auth, "jobs.write")?;
    state
        .engine
        .dispatch(&name)
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
    if let Ok(runs) = state.engine.store.active_runs_of(engine::LOCAL_WORKER).await {
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
    let tail: usize = params
        .get("tail")
        .and_then(|t| t.parse().ok())
        .unwrap_or(50);
    let path = state
        .engine
        .cfg
        .daemon
        .log_dir
        .join(&name)
        .join("current.log");
    let content =
        std::fs::read_to_string(&path).map_err(|_| StatusCode::NOT_FOUND)?;
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(tail);
    Ok(lines[start..].join("\n"))
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
            id: cell(row, "id").and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
            hostname: cell(row, "hostname").and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
            address: cell(row, "address").and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
            version: cell(row, "version").and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
            labels: cell(row, "labels")
                .and_then(|v| v.as_str().map(String::from))
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
            capabilities: cell(row, "capabilities")
                .and_then(|v| v.as_str().map(String::from))
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Null),
            status: cell(row, "status").and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
            jobs_running: cell(row, "jobs_running").and_then(|v| v.as_i64()).unwrap_or(0) as u32,
            last_heartbeat: cell(row, "last_heartbeat").and_then(|v| v.as_i64()).unwrap_or(0),
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
    let proxies: Vec<serde_json::Value> = state
        .engine
        .cfg
        .proxies
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
    let applied = state.engine.reload().await.map_err(|_| StatusCode::BAD_REQUEST)?;
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
        status: format!("{:?}", run.status),
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
            tracing::info!("serving https://{listen} (mTLS: {})", tls.client_ca.is_some());
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
