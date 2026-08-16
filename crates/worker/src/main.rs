//! `synora-worker` — the agent that executes runs (spec §9): registers with
//! the manager, heartbeats every 15s, claims assigned runs, executes them
//! with the same provider machinery as the standalone engine, reports back.
//! Pull model: no inbound listener (NAT-friendly).

use api::{
    Client, CompleteRequest, HeartbeatRequest, RegisterRequest,
};
use clap::Parser;
use config::{CliOverrides, ConfigLoader};
use engine::run_once;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use synora_core::job::JobSpec;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(name = "synora-worker", version, about = "Synora worker agent")]
struct Cli {
    /// Main config file (needs a [worker] section)
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct WorkerConfig {
    /// Friendly worker name; registers as this id instead of the token name.
    name: Option<String>,
    manager: String,
    token: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default = "default_max_concurrency")]
    max_concurrency: u32,
    #[serde(default = "default_log_dir")]
    log_dir: String,
    ca_cert: Option<String>,
}

fn default_max_concurrency() -> u32 {
    8
}
fn default_log_dir() -> String {
    "/var/log/synora".into()
}

struct Running {
    cancel: CancellationToken,
}

/// toml::Value → serde_json::Value (our inert sections are TOML).
fn toml_to_json(v: toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::Value::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Array(a) => serde_json::Value::Array(a.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => serde_json::Value::Object(
            t.into_iter()
                .map(|(k, v)| (k, toml_to_json(v)))
                .collect(),
        ),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let cli = Cli::parse();
    let path = find_config(cli.config)?;
    let cfg = ConfigLoader::load(&path, &CliOverrides::default()).map_err(|e| e.to_string())?;

    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let run_storage = engine::RunStorageCtx::from_config(&cfg);
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
    let worker_cfg: WorkerConfig = cfg
        .extras
        .get("worker")
        .and_then(|v| toml::Value::try_from(v.clone()).ok())
        .and_then(|t| serde_json::from_value(toml_to_json(t)).ok())
        .ok_or("config needs a [worker] section with manager + token")?;
    if worker_cfg.manager.is_empty() || worker_cfg.token.is_empty() {
        return Err("[worker] requires `manager` (URL) and `token`".to_string());
    }


    let client = match &worker_cfg.ca_cert {
        Some(ca) => {
            let pem = std::fs::read(ca).map_err(|e| format!("cannot read ca_cert: {e}"))?;
            Client::new_with_ca(&worker_cfg.manager, &worker_cfg.token, &pem)
                .map_err(|e| e.to_string())?
        }
        None => Client::new(&worker_cfg.manager, &worker_cfg.token).map_err(|e| e.to_string())?,
    };

    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
    let register_req = RegisterRequest {
        name: worker_cfg.name.clone(),
        hostname: hostname.clone(),
        address: "".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        labels: worker_cfg.labels.clone(),
        capabilities: serde_json::json!({"max_concurrency": worker_cfg.max_concurrency}),
    };
    // Retry registration until the manager is reachable (manager may start
    // after the worker, or restart) — don't die on a boot race.
    let register = loop {
        match client.register_worker(&register_req).await {
            Ok(r) => break r,
            Err(e) => {
                tracing::warn!("register failed ({e}), retrying in 10s");
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        }
    };
    let worker_id = register.worker_id;
    tracing::info!(
        "registered as `{worker_id}` on {} (labels: {:?}, max_concurrency: {})",
        worker_cfg.manager,
        worker_cfg.labels,
        worker_cfg.max_concurrency
    );

    let draining = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::new(AtomicBool::new(false));

    // SIGTERM/SIGINT: drain — finish current runs, unregister, exit (spec §11).
    {
        let draining = draining.clone();
        tokio::spawn(async move {
            let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
            tracing::info!("draining (finishing current runs, no new claims)");
            draining.store(true, Ordering::SeqCst);
        });
    }

    let running: Arc<tokio::sync::Mutex<HashMap<String, Running>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let jobs_running = running.lock().await.len() as u32;
        let heartbeat = client
            .heartbeat(
                &worker_id,
                &HeartbeatRequest {
                    status: if jobs_running > 0 { "running".into() } else { "idle".into() },
                    jobs_running,
                },
            )
            .await;
        let heartbeat = match heartbeat {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("heartbeat failed: {e}");
                continue;
            }
        };

        // Cancel requests from the operator (stop).
        if let Some(cancel_id) = heartbeat.cancel_run {
            let cancel = running.lock().await.get(&cancel_id).map(|r| r.cancel.clone());
            if let Some(token) = cancel {
                tracing::info!("run {cancel_id}: cancel requested by manager");
                token.cancel();
            }
        }

        // New assignment: claim it (capacity-gated).
        let has_capacity = (jobs_running as u32) < worker_cfg.max_concurrency;
        if has_capacity && !draining.load(Ordering::SeqCst) {
            if let Some(assignment) = heartbeat.assignment {
                match client.claim_run(&assignment.run_id, &worker_id).await {
                    Ok(Some(a)) => {
                        let client = client.clone();
                        let running = running.clone();
                        let log_dir = PathBuf::from(&worker_cfg.log_dir);
                        let worker_id = worker_id.clone();
                        let cancel = CancellationToken::new();
                        let run_storage = run_storage.clone();
                        let netroute = netroute.clone();
                        running.lock().await.insert(
                            a.run_id.clone(),
                            Running { cancel: cancel.clone() },
                        );
                        tokio::spawn(async move {
                            let job = a.job;
                            let name = job.name.clone();
                            let outcome = run_once(
                                &job,
                                &a.run_id,
                                &worker_id,
                                cancel,
                                &log_dir,
                                run_storage.as_ref(),
                                netroute.as_deref(),
                            )
                            .await;
                            let req = outcome_to_complete(&worker_id, &job, &outcome);
                            if let Err(e) = client.complete_run(&a.run_id, &req).await {
                                tracing::warn!("complete_run {} failed: {e}", a.run_id);
                            }
                            running.lock().await.remove(&a.run_id);
                            tracing::info!("job `{name}` finished ({})", req.status);
                        });
                    }
                    Ok(None) => {
                        // claimed by someone else — fine, next heartbeat offers again.
                    }
                    Err(e) => tracing::warn!("claim failed: {e}"),
                }
            }
        }

        // Draining and idle → unregister and exit.
        if draining.load(Ordering::SeqCst) && running.lock().await.is_empty() {
            if let Err(e) = client.unregister(&worker_id).await {
                tracing::warn!("unregister failed: {e}");
            } else {
                tracing::info!("unregistered cleanly");
            }
            shutdown.store(true, Ordering::SeqCst);
        }
    }
    Ok(())
}

/// RunOutcome → manager report (size detection priority: provider hint →
/// filesystem walk, spec §17/§58).
fn outcome_to_complete(
    worker_id: &str,
    job: &JobSpec,
    outcome: &engine::RunOutcome,
) -> CompleteRequest {
    let status = match &outcome.result {
        Ok(_) => "success",
        Err(provider::ProviderError::Cancelled) => "cancelled",
        Err(_) => "failed",
    };
    let ok = outcome.result.as_ref().ok();
    let size_after = match job.statistics {
        synora_core::StatisticsMode::Provider => ok.and_then(|r| r.size_hint).map(|v| v as i64),
        synora_core::StatisticsMode::Filesystem => {
            Some(engine::logs::walk_size(&job.storage) as i64)
        }
    };
    CompleteRequest {
        worker_id: worker_id.to_string(),
        status: status.to_string(),
        exit_code: ok.and_then(|r| r.exit_code).map(|v| v as i64),
        size_before: None,
        size_after,
        bytes_transferred: ok.and_then(|r| r.bytes_transferred).map(|v| v as i64),
        message: match &outcome.result {
            Ok(r) => r.message.clone(),
            Err(e) => Some(e.to_string()),
        },
    }
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
