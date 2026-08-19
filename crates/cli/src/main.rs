//! `synora` — unified CLI (spec §45).

use clap::{Parser, Subcommand};
use config::{CliOverrides, ConfigLoader, DbKind};
use engine::Engine;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use synora_core::job::JobStatus;

#[derive(Parser)]
#[command(
    name = "synora",
    version,
    about = "Synora — mirror synchronization engine",
    arg_required_else_help = true
)]
struct Cli {
    /// Main config file (default: synora.toml or config/synora.toml)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
    // Hidden --style aliases for the most-used subcommands (user request:
    // `synora --check` must behave exactly like `synora check`).
    #[arg(long, hide = true)]
    check: bool,
    #[arg(long, hide = true)]
    start: bool,
    #[arg(long, hide = true)]
    status: bool,
    #[arg(long, hide = true)]
    reload: bool,
    #[arg(long, hide = true, value_name = "JOB")]
    run: Option<String>,
    #[arg(long, hide = true, value_name = "GROUP")]
    run_group: Option<String>,
    #[arg(long, hide = true, value_name = "JOB")]
    stop: Option<String>,
    #[arg(long, hide = true, value_name = "JOB")]
    logs: Option<String>,
    #[arg(long, hide = true, default_value_t = 50)]
    lines: usize,
    #[arg(long, hide = true)]
    db: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Validate configuration; errors report file:line (spec §44)
    Check {},
    /// Configuration subcommands
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Run the standalone daemon: scheduler + executor + metrics endpoint
    Start {
        /// DB override (path or postgres:// URL)
        #[arg(long)]
        db: Option<String>,
    },
    /// Trigger one job now and wait for it to finish
    Run { job: String },
    /// Trigger every job in a config `[groups.<name>]` group (spec §94)
    RunGroup { group: String },
    /// Show job statuses and next run times
    Status {},
    /// Job subcommands
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Tail a job's latest run log
    Logs {
        job: String,
        /// Lines to show (default 50)
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Cancel a running job (asks the daemon via a control file)
    Stop { job: String },
    /// Hot-reload configuration (SIGHUP to the daemon; job/schedule changes
    /// apply, non-reloadable changes are rejected)
    Reload {},
    /// Remove a proxy from the config (writes the change, then reloads)
    ProxyDelete { name: String },
    /// Terminal console (jobs/workers/proxies/logs/config editor)
    Tui {
        #[arg(long)]
        manager: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Snapshot management for a job's storage (spec §35)
    Snapshot {
        #[command(subcommand)]
        cmd: SnapshotCmd,
    },
    /// Worker management (talks to the manager API)
    Worker {
        #[command(subcommand)]
        cmd: WorkerCmd,
    },
}

#[derive(Subcommand)]
enum WorkerCmd {
    /// List registered workers
    List {
        /// Manager base URL override (default: config api.listen)
        #[arg(long)]
        manager: Option<String>,
        /// API token override (default: first configured token)
        #[arg(long)]
        token: Option<String>,
    },
    /// Stop a worker accepting new runs; `drain` is a compatibility alias
    #[command(visible_alias = "drain")]
    Retire {
        id: String,
        #[arg(long)]
        manager: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand)]
enum JobCmd {
    /// List jobs with their current status
    List {},
    /// Trigger one job now and wait for it to finish
    Run { job: String },
    /// Start a job (alias of run; `-f` cancels any running instance first)
    Start {
        job: String,
        /// Force: stop the running instance, then start
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Cancel a running job
    Stop { job: String },
    /// Stop (if running) then start
    Restart { job: String },
    /// Tail a job's latest run log
    Logs {
        job: String,
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Remove a job from the config (writes the change, then reloads)
    Delete { job: String },
}

#[derive(Subcommand)]
enum SnapshotCmd {
    /// List snapshots of a job's storage
    List { job: String },
    /// Restore a job's storage to a snapshot
    Rollback { job: String, snapshot: String },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Same as `check`
    Validate {},
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // The TUI owns its own runtime (blocking terminal loop) — it must run
    // outside this binary's async runtime, or nested runtimes panic.
    if let Some(Command::Tui { manager, token }) = &cli.command {
        return match tui::run(cli.config.clone(), manager.clone(), token.clone()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let config = cli.config.clone();
    // Hidden --style invocations behave exactly like their subcommands.
    if cli.check {
        let (cfg, path) = load_config(config, None)?;
        print_summary(&cfg, &path);
        return Ok(());
    }
    if cli.start {
        return cmd_start(config, cli.db).await;
    }
    if cli.status {
        return cmd_status(config).await;
    }
    if cli.reload {
        return cmd_reload(config);
    }
    if let Some(job) = cli.run {
        return cmd_run(job, config).await;
    }
    if let Some(group) = cli.run_group {
        return cmd_run_group(group, config).await;
    }
    if let Some(job) = cli.stop {
        return cmd_stop(job, config);
    }
    if let Some(job) = cli.logs {
        return cmd_logs(job, cli.lines, config);
    }
    let command = cli.command.expect("clap requires a subcommand");
    match command {
        Command::Check {} => {
            let (cfg, path) = load_config(config, None)?;
            print_summary(&cfg, &path);
        }
        Command::Config {
            cmd: ConfigCmd::Validate {},
        } => {
            let (cfg, path) = load_config(config, None)?;
            print_summary(&cfg, &path);
        }
        Command::Start { db } => cmd_start(config, db).await?,
        Command::Run { job } => cmd_run(job, config).await?,
        Command::RunGroup { group } => cmd_run_group(group, config).await?,
        Command::Status {} => cmd_status(config).await?,
        Command::Job { cmd } => match cmd {
            JobCmd::List {} => cmd_status(config).await?,
            JobCmd::Run { job } => cmd_run(job, config).await?,
            JobCmd::Start { job, force } => {
                if force {
                    let _ = cmd_stop(job.clone(), config.clone());
                }
                cmd_run(job, config).await?
            }
            JobCmd::Restart { job } => {
                let _ = cmd_stop(job.clone(), config.clone());
                cmd_run(job, config).await?
            }
            JobCmd::Stop { job } => cmd_stop(job, config)?,
            JobCmd::Logs { job, lines } => cmd_logs(job, lines, config)?,
            JobCmd::Delete { job } => cmd_delete_job(job, config).await?,
        },
        Command::Logs { job, lines } => cmd_logs(job, lines, config)?,
        Command::Stop { job } => cmd_stop(job, config)?,
        Command::Reload {} => cmd_reload(config)?,
        Command::ProxyDelete { name } => cmd_delete_proxy(name, config)?,
        Command::Tui { manager, token } => tui::run(config, manager, token)?,
        Command::Snapshot { cmd } => match cmd {
            SnapshotCmd::List { job } => cmd_snapshot_list(job, config)?,
            SnapshotCmd::Rollback { job, snapshot } => {
                cmd_snapshot_rollback(job, snapshot, config)?
            }
        },
        Command::Worker { cmd } => match cmd {
            WorkerCmd::List { manager, token } => cmd_worker_list(config, manager, token).await?,
            WorkerCmd::Retire { id, manager, token } => {
                cmd_worker_drain(id, config, manager, token).await?
            }
        },
    }
    Ok(())
}

fn load_config(
    config: Option<PathBuf>,
    db_override: Option<&str>,
) -> Result<(config::ResolvedConfig, PathBuf), String> {
    let path = find_config(config)?;
    let overrides = CliOverrides {
        db_kind: None,
        db_path: None,
        db_url: match db_override {
            Some(s) if s.contains("://") => Some(s.to_string()),
            _ => None,
        },
        api_listen: None,
    };
    let mut overrides = overrides;
    if let Some(s) = db_override {
        if !s.contains("://") {
            overrides.db_path = Some(s.to_string());
        }
    }
    let cfg = ConfigLoader::load(&path, &overrides).map_err(|e| e.to_string())?;
    Ok((cfg, path))
}

fn print_summary(cfg: &config::ResolvedConfig, path: &std::path::Path) {
    println!("config OK ({}): {} job(s)", path.display(), cfg.jobs.len());
    for j in &cfg.jobs {
        let state = if j.enabled { "enabled " } else { "disabled" };
        let provider = match &j.provider {
            synora_core::ProviderConfig::Rsync { .. } => "rsync",
            synora_core::ProviderConfig::TwoStageRsync { .. } => "two-stage-rsync",
            synora_core::ProviderConfig::Script { .. } => "script",
            synora_core::ProviderConfig::Docker { .. } => "docker",
            synora_core::ProviderConfig::Git { .. } => "git",
            synora_core::ProviderConfig::Http { .. } => "http",
        };
        println!(
            "  {:<20} {:<24} {:>8} {:>9} {}",
            j.name,
            j.schedule.describe(),
            provider,
            state,
            j.storage.display()
        );
    }
}

async fn cmd_start(config: Option<PathBuf>, db: Option<String>) -> Result<(), String> {
    let (cfg, config_path) = load_config(config, db.as_deref())?;
    let migrations = PathBuf::from("migrations");
    let engine = Engine::new(cfg, &migrations, true).await?;
    engine.set_config_source(config_path.clone(), cli_overrides(db.as_deref()));
    engine.sync_config().await?;

    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Pid file: `synora reload` / `synora stop` talk to the daemon.
    let pid_dir = engine.cfg.daemon.log_dir.clone();
    std::fs::create_dir_all(&pid_dir).map_err(|e| e.to_string())?;
    std::fs::write(pid_dir.join("synora.pid"), std::process::id().to_string())
        .map_err(|e| e.to_string())?;

    // Metrics endpoint (spec §36).
    let metrics_engine = engine.clone();
    let listen = metrics_engine.cfg.api.listen;
    let metrics_task = tokio::spawn(async move {
        serve_metrics(metrics_engine, listen).await;
    });

    // Signals: SIGINT/SIGTERM stop the loop gracefully; SIGHUP hot-reloads.
    let engine_sig = engine.clone();
    let engine_hup = engine.clone();
    let signal_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = wait_sigterm() => break,
                _ = wait_sighup() => {
                    match engine_hup.reload().await {
                        Ok(n) => tracing::info!("config reloaded: {n} job(s) applied"),
                        Err(e) => tracing::warn!("reload rejected: {e}"),
                    }
                }
            }
        }
        tracing::info!("shutdown requested");
        engine_sig.shutdown();
    });

    let result = engine.clone().run().await;
    metrics_task.abort();
    signal_task.abort();
    let _ = std::fs::remove_file(pid_dir.join("synora.pid"));
    result
}

fn cli_overrides(db: Option<&str>) -> CliOverrides {
    let mut o = CliOverrides::default();
    if let Some(s) = db {
        if s.contains("://") {
            o.db_url = Some(s.to_string());
        } else {
            o.db_path = Some(s.to_string());
        }
    }
    o
}

async fn serve_metrics(engine: Arc<Engine>, listen: std::net::SocketAddr) {
    use axum::routing::get;
    let app = axum::Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(engine);
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("cannot bind metrics endpoint on {listen}: {e}");
            return;
        }
    };
    tracing::info!("metrics endpoint on http://{listen}/metrics");
    let _ = axum::serve(listener, app).await;
}

async fn metrics_handler(
    axum::extract::State(engine): axum::extract::State<Arc<Engine>>,
) -> String {
    engine.metrics().render()
}

async fn cmd_run(job: String, config: Option<PathBuf>) -> Result<(), String> {
    let (cfg, _) = load_config(config, None)?;
    let engine = Engine::new(cfg, &PathBuf::from("migrations"), true).await?;
    let status = engine.clone().run_once(&job).await?;
    match status {
        JobStatus::Success => println!("{job}: SUCCESS"),
        other => {
            println!("{job}: {other:?}");
            return Err(format!("job `{job}` finished with status {other:?}"));
        }
    }
    Ok(())
}

/// Run every job in a config group, in order (spec §94).
async fn cmd_run_group(group: String, config: Option<PathBuf>) -> Result<(), String> {
    let (cfg, _) = load_config(config, None)?;
    let jobs = cfg
        .groups
        .get(&group)
        .cloned()
        .ok_or_else(|| format!("no group `{group}` in config (define [groups.{group}])"))?;
    if jobs.is_empty() {
        return Err(format!("group `{group}` has no jobs"));
    }
    println!("group `{group}`: {} job(s)", jobs.len());
    let engine = Engine::new(cfg, &PathBuf::from("migrations"), true).await?;
    let mut failed = 0;
    for job in &jobs {
        let status = engine.clone().run_once(job).await?;
        println!("  {job}: {status:?}");
        if status != JobStatus::Success {
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(format!(
            "{failed}/{n} job(s) in group `{group}` failed",
            n = jobs.len()
        ));
    }
    Ok(())
}

async fn cmd_status(config: Option<PathBuf>) -> Result<(), String> {
    let (cfg, _) = load_config(config, None)?;
    if cfg.daemon.db.kind != DbKind::Sqlite {
        return Err("status requires a sqlite database".to_string());
    }
    let db = db::Db::Sqlite(std::sync::Arc::new(
        db::SqliteDb::open(&PathBuf::from(&cfg.daemon.db.path)).map_err(|e| e.to_string())?,
    ));
    db::migrator::Migrator::new(&PathBuf::from("migrations"))
        .run(&db)
        .await
        .map_err(|e| e.to_string())?;
    let store = db::store::Store::new(db);
    let statuses = store.job_status_list().await.map_err(|e| e.to_string())?;
    let schedules = store.all_schedules().await.map_err(|e| e.to_string())?;
    println!(
        "{:<20} {:<12} {:<14} {:<24} LAST RUN",
        "JOB", "STATUS", "SIZE", "NEXT RUN"
    );
    for (name, status) in &statuses {
        let next = schedules
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, r)| r.next_run)
            .map(format_ts)
            .unwrap_or_else(|| "-".to_string());
        // Repository size: from the repositories table (path = job storage).
        let size_str = match cfg.jobs.iter().find(|j| &j.name == name) {
            Some(j) => match store
                .repository_size(&j.storage.to_string_lossy())
                .await
                .ok()
                .flatten()
            {
                Some(s) if s > 0 => synora_core::human_size(s as u64),
                _ => "-".to_string(),
            },
            None => "-".to_string(),
        };
        let last = store
            .run_history(name, 1)
            .await
            .ok()
            .and_then(|mut v| v.pop())
            .map(|r| format!("{} {:?}", format_ts(r.created_at), r.status))
            .unwrap_or_else(|| "-".to_string());
        println!("{name:<20} {status:<12?} {size_str:<14} {next:<24} {last}");
    }
    Ok(())
}

fn cmd_logs(job: String, lines: usize, config: Option<PathBuf>) -> Result<(), String> {
    let (cfg, _) = load_config(config, None)?;
    let path = cfg.daemon.log_dir.join(&job).join("current.log");
    let content = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(lines);
    for l in &all[start..] {
        println!("{l}");
    }
    Ok(())
}

/// Remove a `[[jobs]]` entry from every file in the include tree, then
/// ask the manager to purge every DB row for that job.
async fn cmd_delete_job(job: String, config: Option<PathBuf>) -> Result<(), String> {
    if !valid_job_name(&job) {
        return Err(format!("invalid job name `{job}`"));
    }
    let path = find_config(config.clone())?;
    match config::remove_job_block(&path, &job) {
        Ok(files) => {
            for f in files {
                println!("removed job `{job}` from {}", f.display());
            }
        }
        Err(e) => println!("config: {e}"),
    }
    let (url, token) = manager_creds(config, None, None)?;
    let client = api::Client::new(&url, &token).map_err(|e| e.to_string())?;
    match client.delete_job(&job).await {
        Ok(()) => println!("purged job `{job}` from manager database"),
        Err(e) => {
            return Err(format!(
                "config updated but manager purge failed: {e} (is synora-manager running?)"
            ));
        }
    }
    Ok(())
}

/// Remove a `[proxy.<name>]` section from the main config, then reload.
fn cmd_delete_proxy(name: String, config: Option<PathBuf>) -> Result<(), String> {
    let path = find_config(config)?;
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let marker = format!("[proxy.{name}]");
    let Some(start) = text.find(&marker) else {
        return Err(format!("proxy `{name}` not found in {}", path.display()));
    };
    let end = text[start..]
        .find("\n[")
        .map(|i| start + i)
        .unwrap_or(text.len());
    let new_text = format!("{}{}", &text[..start], &text[end..]);
    std::fs::write(&path, new_text).map_err(|e| e.to_string())?;
    println!(
        "removed proxy `{name}` from {} (reload to apply)",
        path.display()
    );
    Ok(())
}

/// Job names become file names (control files, log dirs) — refuse path
/// separators and traversal segments up front.
fn valid_job_name(job: &str) -> bool {
    !job.is_empty()
        && !job.starts_with('.')
        && !job
            .split('/')
            .any(|c| c.is_empty() || c == ".." || c == ".")
}

/// `synora stop`: drop a control file the daemon's tick picks up.
fn cmd_stop(job: String, config: Option<PathBuf>) -> Result<(), String> {
    let (cfg, _) = load_config(config, None)?;
    if !valid_job_name(&job) {
        return Err(format!("invalid job name `{job}`"));
    }
    let control = cfg.daemon.log_dir.join("control");
    std::fs::create_dir_all(&control).map_err(|e| e.to_string())?;
    std::fs::write(control.join(format!("stop-{job}")), b"").map_err(|e| e.to_string())?;
    println!("cancel requested for `{job}` (the daemon will pick it up within a tick)");
    Ok(())
}

/// Snapshot provider for a job's storage (spec §35).
fn snapshot_provider_for_job(
    cfg: &config::ResolvedConfig,
    job_name: &str,
) -> Result<(Box<dyn snapshot::SnapshotProvider>, synora_core::JobSpec), String> {
    let job = cfg
        .jobs
        .iter()
        .find(|j| j.name == job_name)
        .cloned()
        .ok_or_else(|| format!("no job `{job_name}` in config"))?;
    let sc = cfg
        .storages
        .values()
        .find(|sc| sc.mountpoint.as_deref() == Some(std::path::Path::new(&job.storage)))
        .ok_or_else(|| format!("job `{job_name}` has no snapshot-capable storage section"))?;
    let provider = snapshot::provider_for(&sc.kind, &job.storage).map_err(|e| e.to_string())?;
    Ok((provider, job))
}

fn cmd_snapshot_list(job: String, config: Option<PathBuf>) -> Result<(), String> {
    let (cfg, _) = load_config(config, None)?;
    let (provider, _) = snapshot_provider_for_job(&cfg, &job)?;
    let snaps = provider.list().map_err(|e| e.to_string())?;
    println!("{:<32} CREATED", "SNAPSHOT");
    for s in snaps {
        println!(
            "{:<32} {}",
            s.name,
            if s.created_at > 0 {
                format_ts(s.created_at)
            } else {
                "-".into()
            }
        );
    }
    Ok(())
}

fn cmd_snapshot_rollback(
    job: String,
    snapshot: String,
    config: Option<PathBuf>,
) -> Result<(), String> {
    let (cfg, _) = load_config(config, None)?;
    let (provider, _) = snapshot_provider_for_job(&cfg, &job)?;
    let exists = provider
        .list()
        .map_err(|e| e.to_string())?
        .iter()
        .any(|s| s.name == snapshot);
    if !exists {
        return Err(format!("no snapshot `{snapshot}` for job `{job}`"));
    }
    provider.rollback(&snapshot).map_err(|e| e.to_string())?;
    println!("job `{job}` restored to snapshot `{snapshot}`");
    Ok(())
}

/// Resolve manager URL + token from flags or the config's api section.
fn manager_creds(
    config: Option<PathBuf>,
    manager: Option<String>,
    token: Option<String>,
) -> Result<(String, String), String> {
    let (url, token) = match (manager, token) {
        (Some(u), Some(t)) => (u, t), // fully explicit: no config file needed
        (manager, token) => {
            let (cfg, _) = load_config(config, None)?;
            let url = match manager {
                Some(u) => u,
                None => format!("http://{}", cfg.api.listen),
            };
            let token =
                match token {
                    Some(t) => t,
                    None => cfg.api.tokens.first().map(|t| t.token.clone()).ok_or(
                        "no api token configured (set one in [api.tokens] or pass --token)",
                    )?,
                };
            (url, token)
        }
    };
    Ok((url, token))
}

async fn cmd_worker_list(
    config: Option<PathBuf>,
    manager: Option<String>,
    token: Option<String>,
) -> Result<(), String> {
    let (url, token) = manager_creds(config, manager, token)?;
    let client = api::Client::new(&url, &token).map_err(|e| e.to_string())?;
    let workers = client.list_workers().await.map_err(|e| e.to_string())?;
    println!(
        "{:<16} {:<20} {:<10} {:<8} LABELS",
        "ID", "HOSTNAME", "STATUS", "RUNNING"
    );
    for w in workers {
        println!(
            "{:<16} {:<20} {:<10} {:<8} {}",
            w.id,
            w.hostname,
            w.status,
            w.jobs_running,
            w.labels.join(",")
        );
    }
    Ok(())
}

async fn cmd_worker_drain(
    id: String,
    config: Option<PathBuf>,
    manager: Option<String>,
    token: Option<String>,
) -> Result<(), String> {
    let (url, token) = manager_creds(config, manager, token)?;
    let client = api::Client::new(&url, &token).map_err(|e| e.to_string())?;
    client.drain_worker(&id).await.map_err(|e| e.to_string())?;
    println!("worker `{id}` stopped accepting new runs");
    Ok(())
}

/// `synora reload`: SIGHUP the daemon from its pid file.
fn cmd_reload(config: Option<PathBuf>) -> Result<(), String> {
    let (cfg, _) = load_config(config, None)?;
    let pid_path = cfg.daemon.log_dir.join("synora.pid");
    let pid: i32 = std::fs::read_to_string(&pid_path)
        .map_err(|e| {
            format!(
                "cannot read {}: {e} (is the daemon running?)",
                pid_path.display()
            )
        })?
        .trim()
        .parse()
        .map_err(|e| format!("bad pid file {}: {e}", pid_path.display()))?;
    #[cfg(unix)]
    unsafe {
        libc::kill(pid, libc::SIGHUP);
    }
    #[cfg(not(unix))]
    {
        // Windows: no SIGHUP; reload through the API instead.
        let (url, token) = manager_creds(None, None, None)?;
        let client = api::Client::new(&url, &token).map_err(|e| e.to_string())?;
        tokio::runtime::Handle::current()
            .block_on(client.reload())
            .map_err(|e| e.to_string())?;
    }
    println!("reload requested for pid {pid}; it will be validated and applied");
    Ok(())
}

fn format_ts(ts: i64) -> String {
    if ts <= 0 {
        return "-".into();
    }
    match time::OffsetDateTime::from_unix_timestamp(ts) {
        Ok(t) => t
            .to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "?".into()),
        Err(_) => "-".into(),
    }
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
    Err("no config file found (looked for synora.toml, config/synora.toml; use -c PATH)".into())
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
