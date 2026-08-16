//! `synora-manager` — distributed control plane (spec §9/§46): scheduler +
//! REST API + worker registry + lease reaper. DB: SQLite by default,
//! PostgreSQL via `[daemon.db] kind = "postgres"`.

mod auth;
mod router;

use clap::Parser;
use config::{CliOverrides, ConfigLoader};
use engine::Engine;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "synora-manager", version, about = "Synora manager (scheduler + API)")]
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

    // Proxy/egress probing (user: per-proxy latency + egress IP, default CF
    // egress for probe traffic). Built before cfg moves into the engine.
    let netroute = if cfg.proxies.is_empty() && cfg.egresses.is_empty() {
        None
    } else {
        Some(std::sync::Arc::new(netroute::NetRoute::new(
            &cfg.proxies,
            &cfg.proxy_groups,
            &cfg.egresses,
            &cfg.egress_groups,
            cfg.daemon.default_proxy.as_deref(),
        )))
    };
    let engine = Engine::new(cfg, &PathBuf::from("migrations")).await?;
    engine.set_config_source(path.clone(), overrides);
    engine.sync_config().await?;

    // Worker picker: explicit worker / worker group / labels+capacity (spec §8/§10).
    let picker = router::WorkerPicker::new(engine.clone());
    let picker_clone = picker.clone();
    engine.set_planner(move |job| picker_clone.pick(job));

    // (netroute is built above, before cfg moves into the engine)
    let _ = netroute;
    let probes: std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, netroute::ProxyProbe>>> =
        std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    if let Some(nr) = netroute.clone() {
        let probes = probes.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                let results = nr.probe_all().await;
                *probes.write().unwrap() = results;
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
            if let Ok(expired) = reaper_engine.store.expired_runs(now).await {
                for run in expired {
                    let _ = reaper_engine.store.set_run_lost(&run.id).await;
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
                            tracing::info!("job `{}`: re-queued as {new_id} after worker loss", run.job_id);
                        }
                    }
                }
            }
            // Heartbeat timeout → OFFLINE (45s, spec §29).
            let _ = reaper_engine
                .store
                .db()
                .execute(
                    "UPDATE workers SET status = 'OFFLINE'
                     WHERE last_heartbeat < ? AND status NOT IN ('DRAINING','MAINTENANCE')",
                    &[(now - 45).into()],
                )
                .await;
            // QUEUED runs assigned to workers that are no longer ONLINE wait
            // forever otherwise — unassign them for re-dispatch (spec §28).
            let _ = reaper_engine
                .store
                .db()
                .execute(
                    "UPDATE job_runs SET worker_id = NULL
                     WHERE status = 'QUEUED' AND worker_id IN
                           (SELECT id FROM workers WHERE status != 'ONLINE')",
                    &[],
                )
                .await;
            // Worker lifecycle gauges (spec §36).
            if let Ok(rows) = reaper_engine.store.list_workers().await {
                for row in &rows {
                    let cell = |n: &str| row.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
                    let id = cell("id").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
                    let status = cell("status").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
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
                }
            }
            reaper_picker.refresh().await;
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

    // Signal: graceful stop of the HTTP server.
    let shutdown_engine = engine.clone();
    let signal_task = tokio::spawn(async move {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        tracing::info!("shutdown requested");
        shutdown_engine.shutdown();
    });

    // The engine's own loop drives scheduling ticks (dispatch etc.).
    let run_result = engine.clone().run().await;
    server_task.abort();
    reaper_task.abort();
    signal_task.abort();
    run_result
}

fn find_config(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("config file not found: {}", p.display()));
    }
    for candidate in ["synora.toml", "config/synora.toml"] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Ok(p);
        }
    }
    Err("no config file found (use -c PATH)".into())
}
