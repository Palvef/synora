//! `synora-manager` — distributed control plane (spec §9/§46): scheduler +
//! REST API + worker registry + lease reaper. DB: SQLite by default,
//! PostgreSQL via `[daemon.db] kind = "postgres"`.

mod auth;
mod router;

use clap::Parser;
use config::{CliOverrides, ConfigLoader};
use engine::Engine;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "synora-manager",
    version,
    about = "Synora manager (scheduler + API)"
)]
struct Cli {
    /// Main config file
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// DB override (path or postgres:// URL)
    #[arg(long)]
    db: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let cli = Cli::parse();
    let path = find_config(cli.config)?;
    let mut overrides = CliOverrides::default();
    if let Some(s) = &cli.db {
        if s.contains("://") {
            overrides.db_url = Some(s.clone());
        } else {
            overrides.db_path = Some(s.clone());
        }
    }
    let cfg = ConfigLoader::load(&path, &overrides).map_err(|e| e.to_string())?;

    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Serve HTTP CONNECT expose proxies for the manager's own proxy
    // configs. Workers receive these addresses with the assignment and
    // must not talk to manager-local loopback/SOCKS directly. Auth is
    // optional: rsync's RSYNC_PROXY=host:port cannot send Basic credentials.
    for (name, p) in &cfg.proxies {
        let Some(expose) = &p.expose else { continue };
        let config::ProxyKind::Forward { url, .. } = &p.kind else {
            continue;
        };
        let (user, pass) = p
            .expose_auth
            .as_deref()
            .and_then(|auth| auth.split_once(':'))
            .map(|(u, pw)| (u.to_string(), pw.to_string()))
            .unwrap_or_default();
        let e2 = expose.clone();
        let u2 = url.clone();
        tokio::spawn(async move {
            if let Err(e) = netroute::serve_auth_proxy(&e2, &u2, &user, &pass).await {
                tracing::error!("proxy expose {e2} failed: {e}");
            }
        });
        tracing::info!(
            "proxy `{name}`: serving HTTP CONNECT expose {expose} → {url}{}",
            if p.expose_auth.is_some() {
                " (auth)"
            } else {
                ""
            }
        );
    }

    let engine = Engine::new(cfg, &PathBuf::from("migrations"), false).await?;
    engine.set_config_source(path.clone(), overrides);
    engine.sync_config().await?;

    // Worker picker: explicit worker / worker group / labels+capacity (spec §8/§10).
    let picker = router::WorkerPicker::new(engine.clone());
    let picker_clone = picker.clone();
    engine.set_planner(move |job| picker_clone.pick(job));

    // Proxy/egress probing (user: per-proxy latency + egress IP, default CF
    // egress for probe traffic). Reads the engine's NetRoute so reloads
    // (TUI-added proxies) take effect on the next probe tick.
    let probes: std::sync::Arc<
        std::sync::RwLock<std::collections::HashMap<String, netroute::ProxyProbe>>,
    > = std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    {
        let probes = probes.clone();
        let probe_engine = engine.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                let nr = probe_engine.netroute.read().unwrap().clone();
                if let Some(nr) = nr {
                    let results = nr.probe_all().await;
                    *probes.write().unwrap() = results;
                }
            }
        });
    }

    // Periodic refresh: worker snapshot + reaper (lease expiry → LOST,
    // heartbeat timeout → OFFLINE, spec §28–§29).
    let reaper_engine = engine.clone();
    let reaper_picker = picker.clone();
    let reaper_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            tick.tick().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let _ = reaper_engine.store.drop_superseded_runs().await;
            let _ = reaper_engine.store.reconcile_stale_job_status().await;
            if let Ok(expired) = reaper_engine.store.expired_runs(now).await {
                for run in expired {
                    let _ = reaper_engine.store.set_run_lost(&run.id).await;
                    reaper_engine.metrics.inc_counter(
                        "synora_job_lost_total",
                        &[("job", run.job_id.as_str())],
                        1.0,
                    );
                    tracing::warn!("run {} (job {}) lost: lease expired", run.id, run.job_id);
                    let job = reaper_engine.job(&run.job_id);
                    let on_worker_lost_retry = job
                        .as_ref()
                        .map(|j| matches!(j.on_worker_lost, synora_core::OnWorkerLost::Retry))
                        .unwrap_or(false);
                    if on_worker_lost_retry {
                        let worker = job.as_ref().and_then(|j| reaper_picker.pick(j));
                        if let Ok(Some(new_id)) = reaper_engine
                            .store
                            .create_lost_requeue(&run.id, &run.job_id, worker.as_deref())
                            .await
                        {
                            tracing::info!(
                                "job `{}`: re-queued as {new_id} after worker loss",
                                run.job_id
                            );
                        }
                    }
                }
            }
            // Heartbeat timeout → OFFLINE.
            let _ = reaper_engine
                .store
                .db()
                .execute(
                    "UPDATE workers SET status = 'OFFLINE'
                     WHERE (last_heartbeat < ? OR last_heartbeat IS NULL)
                       AND status NOT IN ('DRAINING','MAINTENANCE')",
                    &[(now - synora_core::WORKER_HEARTBEAT_GRACE_SECS).into()],
                )
                .await;
            // QUEUED runs assigned to workers that are no longer ONLINE wait
            // forever otherwise — unassign them for re-dispatch (spec §28).
            let _ = reaper_engine
                .store
                .db()
                .execute(
                    "UPDATE job_runs SET worker_id = NULL
                     WHERE status = 'QUEUED' AND (worker_id IS NULL OR worker_id NOT IN
                           (SELECT id FROM workers WHERE status = 'ONLINE'))",
                    &[],
                )
                .await;
            // Worker lifecycle gauges (spec §36).
            if let Ok(rows) = reaper_engine.store.list_workers().await {
                for row in &rows {
                    let cell = |n: &str| row.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
                    let id = cell("id")
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_default();
                    let status = cell("status")
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_default();
                    let value = match status.as_str() {
                        "ONLINE" => 1.0,
                        "DRAINING" => 2.0,
                        "MAINTENANCE" => 3.0,
                        _ => 0.0,
                    };
                    reaper_engine.metrics.set_gauge(
                        "synora_worker_status",
                        &[("worker", id.as_str())],
                        value,
                    );
                    let running = cell("jobs_running").and_then(|v| v.as_i64()).unwrap_or(0);
                    reaper_engine.metrics.set_gauge(
                        "synora_worker_jobs_running",
                        &[("worker", id.as_str())],
                        running as f64,
                    );
                    let max_c = cell("capabilities")
                        .and_then(|v| v.as_str().map(String::from))
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                        .and_then(|c| c.get("max_concurrency").and_then(|m| m.as_u64()))
                        .unwrap_or(8);
                    reaper_engine.metrics.set_gauge(
                        "synora_worker_max_concurrency",
                        &[("worker", id.as_str())],
                        max_c as f64,
                    );
                }
            }
            if let Ok(queued) = reaper_engine.store.count_waiting_runs().await {
                reaper_engine
                    .metrics
                    .set_gauge("synora_runs_queued", &[], queued as f64);
            }
            // Export every job's latest DB status + known repository size so
            // Grafana is complete even when a job has not reported recently.
            let size_map: std::collections::HashMap<String, i64> = reaper_engine
                .store
                .list_repository_sizes()
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();
            let run_sizes: std::collections::HashMap<String, i64> = reaper_engine
                .store
                .latest_run_sizes()
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();
            let run_stats: std::collections::HashMap<String, db::store::JobRunStats> =
                reaper_engine
                    .store
                    .latest_run_stats()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|s| (s.job_id.clone(), s))
                    .collect();
            if let Ok(statuses) = reaper_engine.store.job_status_list().await {
                for (name, status) in statuses {
                    let job = reaper_engine.job(&name);
                    let worker = match job
                        .as_ref()
                        .and_then(|j| j.worker.clone())
                        .filter(|s| !s.is_empty())
                    {
                        Some(w) => w,
                        None => reaper_engine
                            .store
                            .last_run_worker(&name)
                            .await
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| "unassigned".into()),
                    };
                    let provider = job
                        .as_ref()
                        .map(|j| engine::provider_name(j))
                        .unwrap_or("unknown");
                    reaper_engine.metrics.set_job_gauge(
                        "synora_job_status",
                        &name,
                        &[("job", name.as_str()), ("worker", worker.as_str())],
                        engine::status_value(status),
                    );
                    reaper_engine.metrics.set_job_gauge(
                        "synora_job_info",
                        &name,
                        &[
                            ("job", name.as_str()),
                            ("worker", worker.as_str()),
                            ("provider", provider),
                        ],
                        1.0,
                    );
                    if let Some(j) = job.as_ref() {
                        let raw = j.storage.display().to_string();
                        let resolved = reaper_engine
                            .run_storage
                            .as_ref()
                            .map(|c| c.resolve_storage_path(j).display().to_string())
                            .unwrap_or_else(|| raw.clone());
                        let size = size_map
                            .get(&resolved)
                            .or_else(|| size_map.get(&raw))
                            .or_else(|| size_map.get(&name))
                            .copied()
                            .or_else(|| run_sizes.get(&name).copied())
                            .or_else(|| {
                                size_map.iter().find_map(|(path, sz)| {
                                    let p = path.trim_end_matches('/');
                                    if p.ends_with(&format!("/{name}"))
                                        || p.ends_with(&format!("/{raw}"))
                                        || p == name
                                        || p == raw
                                        || p == resolved
                                    {
                                        Some(*sz)
                                    } else {
                                        None
                                    }
                                })
                            });
                        if let Some(size) = size {
                            reaper_engine.metrics.set_gauge(
                                "synora_repository_size_bytes",
                                &[("job", name.as_str())],
                                size as f64,
                            );
                        }
                    }
                    if let Some(st) = run_stats.get(&name) {
                        if let Some(ts) = st.last_end.filter(|t| *t > 0) {
                            reaper_engine.metrics.set_gauge(
                                "synora_job_last_end_timestamp",
                                &[("job", name.as_str())],
                                ts as f64,
                            );
                        }
                        if let Some(ts) = st.last_start.filter(|t| *t > 0) {
                            reaper_engine.metrics.set_gauge(
                                "synora_job_last_start_timestamp",
                                &[("job", name.as_str())],
                                ts as f64,
                            );
                        }
                        if let Some(ts) = st.last_success.filter(|t| *t > 0) {
                            reaper_engine.metrics.set_gauge(
                                "synora_job_last_success_timestamp",
                                &[("job", name.as_str())],
                                ts as f64,
                            );
                        }
                        if let Some(d) = st.duration_secs {
                            reaper_engine.metrics.set_gauge(
                                "synora_job_duration_seconds",
                                &[("job", name.as_str())],
                                d as f64,
                            );
                        }
                        if let Some(mem) = st.memory_bytes {
                            reaper_engine.metrics.set_gauge(
                                "synora_job_memory_bytes",
                                &[("job", name.as_str()), ("worker", worker.as_str())],
                                mem as f64,
                            );
                        }
                        if let Some(cpu) = st.cpu_seconds {
                            reaper_engine.metrics.set_gauge(
                                "synora_job_cpu_seconds",
                                &[("job", name.as_str()), ("worker", worker.as_str())],
                                cpu,
                            );
                        }
                    }
                }
            }
            reaper_picker.refresh().await;
            // Re-dispatch unassigned QUEUED runs now that workers are online
            // (spec §28): runs queued while no worker was up, or unassigned
            // above, must not wait forever.
            if let Ok(queued) = reaper_engine.store.unassigned_runs().await {
                for run in queued {
                    let Some(job) = reaper_engine.job(&run.job_id) else {
                        continue;
                    };
                    if let Some(worker) = reaper_picker.pick(&job) {
                        if let Ok(true) = reaper_engine
                            .store
                            .assign_queued_run(&run.id, &worker)
                            .await
                        {
                            tracing::info!(
                                "run {} (job `{}`) re-dispatched to worker `{worker}`",
                                run.id,
                                run.job_id
                            );
                        }
                    }
                }
            }
        }
    });

    let (router, state) = router::build(engine.clone(), picker.clone(), probes.clone());
    let listen = engine.cfg.api.listen;
    let tls = engine.cfg.api.tls.clone();
    // Bind BEFORE spawning: a taken port must fail startup, not run silently
    // without an API.
    let plain_listener = if tls.cert.is_none() {
        Some(
            tokio::net::TcpListener::bind(listen)
                .await
                .map_err(|e| format!("cannot bind {listen}: {e}"))?,
        )
    } else {
        None
    };
    let server_task = tokio::spawn(async move {
        let result = match plain_listener {
            Some(listener) => router::serve_plain(router, listener, listen).await,
            None => router::serve(router, listen, &tls, state).await,
        };
        if let Err(e) = result {
            tracing::error!("API server failed: {e}");
        }
    });

    match config::write_pid_file("manager") {
        Ok(path) => tracing::info!("pid file {}", path.display()),
        Err(e) => tracing::warn!("{e}"),
    }

    // SIGHUP is systemd `reload` / `synora reload` fallback. Reload jobs
    // in-process; do not treat it as shutdown.
    #[cfg(unix)]
    {
        let hup_engine = engine.clone();
        tokio::spawn(async move {
            reload_on_sighup(hup_engine).await;
        });
    }

    // Signal: graceful stop of the HTTP server.
    let shutdown_engine = engine.clone();
    let signal_task = tokio::spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = wait_sigterm() => {}
        }
        tracing::info!("shutdown requested");
        shutdown_engine.shutdown();
    });

    // The engine's own loop drives scheduling ticks (dispatch etc.).
    let run_result = engine.clone().run().await;
    server_task.abort();
    reaper_task.abort();
    signal_task.abort();
    config::remove_pid_file("manager");
    run_result
}

fn find_config(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("config file not found: {}", p.display()));
    }
    for candidate in [
        "synora.toml",
        "config/synora.toml",
        "/etc/synora/synora.toml",
    ] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Ok(p);
        }
    }
    Err("no config file found (use -c PATH)".into())
}

#[cfg(unix)]
async fn wait_sigterm() {
    let mut s = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    let _ = s.recv().await;
}

#[cfg(unix)]
async fn reload_on_sighup(engine: Arc<Engine>) {
    let mut s = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .expect("SIGHUP handler");
    while s.recv().await.is_some() {
        match engine.reload().await {
            Ok(n) => tracing::info!("config reloaded: {n} job(s) applied"),
            Err(e) => tracing::warn!("reload rejected: {e}"),
        }
    }
}

#[cfg(not(unix))]
async fn wait_sigterm() {
    // Windows: no SIGTERM; this future simply never resolves.
    std::future::pending::<()>().await;
}
