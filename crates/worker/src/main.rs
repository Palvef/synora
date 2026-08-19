//! `synora-worker` — the agent that executes runs (spec §9): registers with
//! the manager, heartbeats every 15s, claims assigned runs, executes them
//! with the same provider machinery as the standalone engine, reports back.
//! Pull model: no inbound listener (NAT-friendly).

use api::{Client, CompleteRequest, HeartbeatRequest, RegisterRequest};
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
    /// Hostname reported to the manager (defaults to the system hostname).
    hostname: Option<String>,
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
    job: String,
    usage: provider::UsageSink,
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
        toml::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(t) => {
            serde_json::Value::Object(t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect())
        }
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

    let hostname = worker_cfg
        .hostname
        .clone()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".to_string());
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
    // Authenticated expose proxies (user: registering CF One / WARP with an
    // expose address serves a Basic-auth CONNECT proxy on this machine,
    // forwarding through the local WARP endpoint).
    for (name, p) in &cfg.proxies {
        if let (Some(expose), Some(auth)) = (&p.expose, &p.expose_auth) {
            if let Some((user, pass)) = auth.split_once(':') {
                if let config::ProxyKind::Forward { url, .. } = &p.kind {
                    let e2 = expose.clone();
                    let u2 = url.clone();
                    let user = user.to_string();
                    let pass = pass.to_string();
                    tokio::spawn(async move {
                        let _ = netroute::serve_auth_proxy(&e2, &u2, &user, &pass).await;
                    });
                    tracing::info!("proxy `{name}`: serving authenticated expose {expose} → {url}");
                }
            }
        }
    }

    let worker_id = register.worker_id;
    tracing::info!(
        "registered as `{worker_id}` on {} (labels: {:?}, max_concurrency: {})",
        worker_cfg.manager,
        worker_cfg.labels,
        worker_cfg.max_concurrency
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let running: Arc<tokio::sync::Mutex<HashMap<String, Running>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    // Worker exit: stop accepting work, cancel runs, then the main loop
    // drains, removes leftover job containers, and unregisters.
    async fn request_stop(
        running: Arc<tokio::sync::Mutex<HashMap<String, Running>>>,
        shutdown: Arc<AtomicBool>,
    ) {
        shutdown.store(true, Ordering::SeqCst);
        for r in running.lock().await.values() {
            r.cancel.cancel();
        }
    }

    async fn cleanup_job_containers() {
        let out = tokio::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", "name=synora-job-"])
            .output()
            .await;
        let Ok(out) = out else {
            return;
        };
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        if ids.is_empty() {
            return;
        }
        let mut cmd = tokio::process::Command::new("docker");
        cmd.arg("rm").arg("-f");
        for id in &ids {
            cmd.arg(id);
        }
        let _ = cmd.status().await;
    }

    // SIGTERM/SIGINT: drain — finish current runs, unregister, exit (spec §11).
    {
        let running2 = running.clone();
        let shutdown2 = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = wait_sigterm() => {}
            }
            tracing::info!("stopping all runs and removing job containers");
            request_stop(running2, shutdown2).await;
        });
        // SIGHUP (systemd reload): exit cleanly — Restart=always brings the
        // worker back with the updated config and proxy listeners.
        {
            let running2 = running.clone();
            let shutdown2 = shutdown.clone();
            tokio::spawn(async move {
                wait_sighup().await;
                tracing::info!("SIGHUP received — stopping runs for restart");
                request_stop(running2, shutdown2).await;
            });
        }
    }

    loop {
        // Poll fast while under the concurrency cap so a worker can fill
        // all slots (production: 30) instead of claiming one every 15s.
        let jobs_running = running.lock().await.len() as u32;
        if shutdown.load(Ordering::SeqCst) {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            while tokio::time::Instant::now() < deadline && !running.lock().await.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            cleanup_job_containers().await;
            if let Err(e) = client.unregister(&worker_id).await {
                tracing::warn!("unregister failed: {e}");
            } else {
                tracing::info!("unregistered cleanly");
            }
            break;
        }
        let delay = if jobs_running < worker_cfg.max_concurrency {
            2
        } else {
            15
        };
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        if shutdown.load(Ordering::SeqCst) {
            continue;
        }
        let (resources, active_jobs) = {
            let guard = running.lock().await;
            let active_jobs: Vec<String> = guard.values().map(|r| r.job.clone()).collect();
            let mut out = Vec::new();
            for r in guard.values() {
                let sample = *r.usage.lock().unwrap();
                if sample.memory_bytes.is_none()
                    && sample.cpu_seconds.is_none()
                    && sample.cpu_percent.is_none()
                {
                    continue;
                }
                out.push(api::JobResourceSample {
                    job: r.job.clone(),
                    memory_bytes: sample.memory_bytes,
                    cpu_seconds: sample.cpu_seconds,
                    cpu_percent: sample.cpu_percent,
                });
            }
            (out, active_jobs)
        };
        let heartbeat = client
            .heartbeat(
                &worker_id,
                &HeartbeatRequest {
                    status: if jobs_running > 0 {
                        "running".into()
                    } else {
                        "idle".into()
                    },
                    jobs_running,
                    resources,
                    repository_sizes: collect_repo_sizes(),
                    active_jobs,
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
            let cancel = running
                .lock()
                .await
                .get(&cancel_id)
                .map(|r| r.cancel.clone());
            if let Some(token) = cancel {
                tracing::info!("run {cancel_id}: cancel requested by manager");
                token.cancel();
            }
        }

        // New assignment: claim it (capacity-gated).
        let has_capacity = (jobs_running as u32) < worker_cfg.max_concurrency;
        if has_capacity && !shutdown.load(Ordering::SeqCst) {
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
                        let usage = std::sync::Arc::new(std::sync::Mutex::new(
                            provider::ResourceUsage::default(),
                        ));
                        running.lock().await.insert(
                            a.run_id.clone(),
                            Running {
                                cancel: cancel.clone(),
                                job: a.job.name.clone(),
                                usage: usage.clone(),
                            },
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
                                Some(a.proxy_env),
                                Some(usage),
                            )
                            .await;
                            let req = outcome_to_complete(
                                &worker_id,
                                &job,
                                &outcome,
                                &log_dir,
                                run_storage.as_ref(),
                            );
                            let mut reported = false;
                            for attempt in 1..=8u32 {
                                match client.complete_run(&a.run_id, &req).await {
                                    Ok(()) => {
                                        reported = true;
                                        break;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "complete_run {} attempt {attempt} failed: {e}",
                                            a.run_id
                                        );
                                        tokio::time::sleep(std::time::Duration::from_secs(
                                            2 * u64::from(attempt),
                                        ))
                                        .await;
                                    }
                                }
                            }
                            if !reported {
                                tracing::error!(
                                    "complete_run {} dropped after retries ({})",
                                    a.run_id,
                                    req.status
                                );
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
    }
    Ok(())
}

/// RunOutcome → manager report (size detection priority: provider hint →
/// ZFS used on the resolved worker path).
fn outcome_to_complete(
    worker_id: &str,
    job: &JobSpec,
    outcome: &engine::RunOutcome,
    log_dir: &std::path::Path,
    storage_ctx: Option<&engine::RunStorageCtx>,
) -> CompleteRequest {
    let status = match &outcome.result {
        Ok(_) => "success",
        Err(provider::ProviderError::Cancelled) => "cancelled",
        Err(_) => "failed",
    };
    let ok = outcome.result.as_ref().ok();
    let dest = storage_ctx
        .map(|ctx| ctx.resolve_storage_path(job))
        .unwrap_or_else(|| job.storage.clone());
    let size_after = ok
        .and_then(|r| r.size_hint)
        .or_else(|| engine::logs::measure_repo_size(&dest))
        .or_else(|| match job.statistics {
            synora_core::StatisticsMode::Filesystem => Some(engine::logs::walk_size(&dest)),
            synora_core::StatisticsMode::Provider => None,
        })
        .map(|v| v as i64);
    // Report the run log so the manager can serve job_logs for
    // distributed runs (the log lives on this worker host).
    let log = std::fs::read_to_string(log_dir.join(&job.name).join("current.log"))
        .ok()
        .map(|c| {
            c.lines()
                .rev()
                .take(500)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        });
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
        log,
        memory_bytes: outcome.mem_peak,
        cpu_seconds: outcome.cpu_seconds,
    }
}

fn collect_repo_sizes() -> Vec<api::RepoSizeSample> {
    let out = std::process::Command::new("zfs")
        .args(["list", "-Hp", "-o", "used,mountpoint"])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let bytes = parts.next()?.parse::<u64>().ok()?;
            let path = parts.next()?.trim();
            if path.is_empty() || path == "/" {
                return None;
            }
            Some(api::RepoSizeSample {
                path: path.to_string(),
                bytes,
            })
        })
        .collect()
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

#[cfg(not(unix))]
async fn wait_sigterm() {
    // Windows: no SIGTERM; this future simply never resolves.
    std::future::pending::<()>().await;
}

#[cfg(unix)]
async fn wait_sighup() {
    let mut s = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .expect("SIGHUP handler");
    let _ = s.recv().await;
}

#[cfg(not(unix))]
async fn wait_sighup() {
    std::future::pending::<()>().await;
}
