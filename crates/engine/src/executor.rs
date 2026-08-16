//! Run executor, split in two (spec §74):
//! - `run_once`: the worker-side part — provider + hooks + local logs. Used
//!   by the standalone engine AND by remote workers (which then report the
//!   result to the manager over the API).
//! - `execute_run`: the engine-side wrapper — DB state, retry, metrics.

use crate::engine::{unix_now, Engine, LOCAL_WORKER};
use crate::logs::{walk_size, RunLogger};
use provider::{build_provider, ProviderError, SyncContext, SyncResult};
use std::path::Path;
use std::sync::Arc;
use synora_core::job::{JobSpec, JobStatus};
use synora_core::state::retry_decision;
use tokio_util::sync::CancellationToken;

/// What one provider execution produced.
pub struct RunOutcome {
    pub result: Result<SyncResult, ProviderError>,
    pub duration_secs: i64,
    /// cgroup-sampled peak memory (bytes) and accumulated CPU seconds.
    pub mem_peak: Option<u64>,
    pub cpu_seconds: Option<f64>,
}

/// Execute one claimed run. Called from a spawned task; drops the global
/// semaphore permit when done.
pub async fn execute_run(
    engine: &Arc<Engine>,
    run_id: String,
    job: JobSpec,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    let started = unix_now();
    let log_path = engine
        .cfg
        .daemon
        .log_dir
        .join(&job.name)
        .join("current.log")
        .display()
        .to_string();
    let _ = engine
        .store
        .insert_log(&run_id, &job.name, &log_path)
        .await;

    let _ = engine
        .store
        .set_run_status(&run_id, JobStatus::Running)
        .await;
    engine.metrics.set_gauge(
        "synora_job_status",
        &[("job", job.name.as_str()), ("worker", LOCAL_WORKER)],
        status_value(JobStatus::Running),
    );
    engine.metrics.set_gauge(
        "synora_job_last_start_timestamp",
        &[("job", job.name.as_str())],
        started as f64,
    );
    engine
        .metrics
        .inc_counter("synora_job_runs_total", &[("job", job.name.as_str())], 1.0);

    let cancel = CancellationToken::new();
    engine.register_run(&job.name, cancel.clone());
    let netroute = engine.netroute.read().unwrap().clone();
    let outcome = run_once(
        &job,
        &run_id,
        LOCAL_WORKER,
        cancel,
        &engine.cfg.daemon.log_dir,
        engine.run_storage.as_ref(),
        netroute.as_deref(),
    )
    .await;

    finish_run(engine, &run_id, &job, outcome).await;
    engine.remove_run(&job.name);
    engine.active_dec(&job.name);
}

/// Provider execution + hooks + local logs. No DB access — remote workers
/// reuse this and report the outcome themselves.
pub async fn run_once(
    job: &JobSpec,
    run_id: &str,
    worker: &str,
    cancel: CancellationToken,
    log_dir: &Path,
    storage_ctx: Option<&crate::engine::RunStorageCtx>,
    netroute: Option<&netroute::NetRoute>,
) -> RunOutcome {
    let started = unix_now();
    let mut logger = RunLogger::open(log_dir, &job.name).ok();
    if let Some(l) = logger.as_mut() {
        let _ = l.line(&format!("run {run_id} started ({} provider)", provider_name(job)));
    }

    // Storage backend (dir / zfs / btrfs) — spec §30–§31/§51.
    let storage_name = storage_ctx.and_then(|c| c.storage_for(job).map(|(n, _)| n.clone()));
    if let (Some(ctx), Some(name)) = (storage_ctx, storage_name.as_deref()) {
        if let Some(manager) = &ctx.manager {
            match manager.ensure(name).await {
                Ok(path) => {
                    // min free space gate (spec §51) — block before syncing.
                    if let Err(e) = manager.check_min_free(&path, ctx.min_free_bytes).await {
                        if let Some(l) = logger.as_mut() {
                            let _ = l.line(&format!("run {run_id} BLOCKED_STORAGE: {e}"));
                        }
                        return RunOutcome {
                            result: Err(ProviderError::Other(format!("BLOCKED_STORAGE: {e}"))),
                            duration_secs: unix_now() - started,
                            mem_peak: None,
                            cpu_seconds: None,
                        };
                    }
                }
                Err(e) => {
                    if let Some(l) = logger.as_mut() {
                        let _ = l.line(&format!("run {run_id} failed: storage: {e}"));
                    }
                    return RunOutcome {
                        result: Err(ProviderError::Other(format!("storage: {e}"))),
                        duration_secs: unix_now() - started,
                        mem_peak: None,
                        cpu_seconds: None,
                    };
                }
            }
        }
    }
    // Plain storage dir must exist before providers run.
    if let Err(e) = std::fs::create_dir_all(&job.storage) {
        if let Some(l) = logger.as_mut() {
            let _ = l.line(&format!("run {run_id} failed: cannot create storage dir: {e}"));
        }
        return RunOutcome {
            result: Err(ProviderError::Config(format!(
                "cannot create storage dir: {e}"
            ))),
            duration_secs: unix_now() - started,
            mem_peak: None,
            cpu_seconds: None,
        };
    }

    // Snapshots (spec §32–§33): before-sync / before-and-after.
    let snapshot_provider = storage_ctx.and_then(|ctx| {
        let (_, sc) = ctx.storage_for(job)?;
        snapshot::provider_for(&sc.kind, &job.storage).ok()
    });
    let wants_before = matches!(
        job.snapshot_policy,
        synora_core::SnapshotPolicy::BeforeSync | synora_core::SnapshotPolicy::BeforeAndAfter
    );
    if wants_before {
        if let Some(p) = snapshot_provider.as_ref() {
            let name = snapshot::snapshot_name(time::OffsetDateTime::now_utc());
            match p.create(&name) {
                Ok(info) => {
                    if let Some(l) = logger.as_mut() {
                        let _ = l.line(&format!("run {run_id}: snapshot {name} created"));
                    }
                    let _ = info;
                }
                Err(e) => {
                    if let Some(l) = logger.as_mut() {
                        let _ = l.line(&format!("run {run_id}: snapshot failed: {e}"));
                    }
                }
            }
        }
    }

    // cgroup scope for resource limits (user-requested; tunasync parity).
    let cgroup_scope = crate::cgroup::CgroupScope::create(
        std::path::Path::new("/sys/fs/cgroup/synora"),
        &job.name,
        run_id,
        job.memory_limit,
        job.cpu_limit,
    );
    if cgroup_scope.is_none() && (job.memory_limit.is_some() || job.cpu_limit.is_some()) {
        tracing::warn!(
            "job `{}`: limits configured but cgroup v2 unavailable — running unconstrained",
            job.name
        );
    }
    let cgroup_ref: Option<std::sync::Arc<dyn provider::CgroupScopeRef>> = cgroup_scope
        .as_ref()
        .map(|c| std::sync::Arc::new(CgroupHandle(c.path().to_path_buf())) as std::sync::Arc<_>);

    let ctx = SyncContext {
        run_id: run_id.to_string(),
        job_name: job.name.clone(),
        upstream: job.upstream.clone(),
        storage: job.storage.clone(),
        worker: Some(worker.to_string()),
        proxy: job.proxy.clone(),
        egress: job.egress.clone(),
        job: job.clone(),
        cancel: cancel.clone(),
        cgroup: cgroup_ref,
        proxy_env: netroute
            .map(|nr| match nr.select_proxy(job.proxy.as_deref()) {
                netroute::Selection::Forward { env, .. } => env,
                netroute::Selection::Direct => Vec::new(),
            })
            .unwrap_or_default(),
        egress_address: netroute
            .and_then(|nr| nr.select_egress(job.egress.as_deref()))
            .map(|a| a.to_string()),
        family: netroute
            .map(|nr| {
                let egress = nr.select_egress(job.egress.as_deref());
                match nr.select_family(&job.family, egress) {
                    netroute::Family::Any => "any",
                    netroute::Family::V4 => "ipv4",
                    netroute::Family::V6 => "ipv6",
                }
                .to_string()
            })
            .unwrap_or_else(|| job.family.clone()),
    };

    let provider = match build_provider(job) {
        Ok(p) => p,
        Err(e) => {
            if let Some(l) = logger.as_mut() {
                let _ = l.line(&format!("run {run_id} failed: {e}"));
            }
            return RunOutcome {
                result: Err(e),
                duration_secs: unix_now() - started,
                mem_peak: None,
                cpu_seconds: None,
            };
        }
    };

    run_hooks(&job.hooks.before_sync, &ctx, logger.as_mut()).await;

    // Timeout wraps the provider; cancel kills the child process group.
    // Unlimited unless the job sets a real timeout (user requirement).
    let enforce_timeout = job.timeout.whole_seconds() < i64::MAX / 8;
    let outcome = if enforce_timeout {
        tokio::select! {
            r = tokio::time::timeout(
                std::time::Duration::from_secs(job.timeout.whole_seconds().max(1) as u64),
                provider.sync(&ctx),
            ) => {
                match r {
                    Err(_) => {
                        cancel.cancel();
                        Err(ProviderError::Timeout)
                    }
                    Ok(r) => r,
                }
            }
        }
    } else {
        provider.sync(&ctx).await
    };

    run_hooks(&job.hooks.after_sync, &ctx, logger.as_mut()).await;

    let result = match outcome {
        Err(e) => Err(e),
        Ok(result) => {
            if let Some(l) = logger.as_mut() {
                let _ = l.raw(&result.stdout);
                let _ = l.raw(&result.stderr);
            }
            // fail_on_match: output regex forces failure even with exit 0
            // (tunasync convention, alignment decision).
            if let Some(re) = &job.fail_on_match {
                let hay = String::from_utf8_lossy(&result.stdout);
                let hay = format!("{hay}\n{}", String::from_utf8_lossy(&result.stderr));
                if regex::Regex::new(re)
                    .ok()
                    .map(|rx| rx.is_match(&hay))
                    .unwrap_or(false)
                {
                    Err(ProviderError::Other(format!(
                        "output matched fail_on_match `{re}`"
                    )))
                } else {
                    Ok(result)
                }
            } else {
                Ok(result)
            }
        }
    };

    // Post-sync verification (spec §56): only a verified success produces
    // after-success snapshots.
    let result = match &result {
        Ok(r) => match run_verify(job, r) {
            Ok(()) => result,
            Err(msg) => {
                if let Some(l) = logger.as_mut() {
                    let _ = l.line(&format!("run {run_id} verify failed: {msg}"));
                }
                Err(ProviderError::Other(format!("verify failed: {msg}")))
            }
        },
        Err(_) => result,
    };

    // after-success snapshot (only when verify passed — result is Ok here).
    let wants_after = matches!(
        job.snapshot_policy,
        synora_core::SnapshotPolicy::AfterSuccess | synora_core::SnapshotPolicy::BeforeAndAfter
    );
    if wants_after && result.is_ok() {
        if let Some(p) = snapshot_provider.as_ref() {
            let name = snapshot::snapshot_name(time::OffsetDateTime::now_utc());
            match p.create(&name) {
                Ok(_) => {
                    if let Some(l) = logger.as_mut() {
                        let _ = l.line(&format!("run {run_id}: snapshot {name} created"));
                    }
                    // Retention prune (spec §33).
                    if let (Some(ctx), Ok(list)) =
                        (storage_ctx, p.list())
                    {
                        for to_delete in snapshot::prune_plan(
                            &list,
                            &ctx.retention,
                            time::OffsetDateTime::now_utc(),
                        ) {
                            if let Err(e) = p.delete(&to_delete) {
                                if let Some(l) = logger.as_mut() {
                                    let _ = l.line(&format!("run {run_id}: snapshot prune {to_delete} failed: {e}"));
                                }
                            } else if let Some(l) = logger.as_mut() {
                                let _ = l.line(&format!("run {run_id}: pruned snapshot {to_delete}"));
                            }
                        }
                    }
                }
                Err(e) => {
                    if let Some(l) = logger.as_mut() {
                        let _ = l.line(&format!("run {run_id}: snapshot failed: {e}"));
                    }
                }
            }
        }
    }

    let duration = unix_now() - started;
    if let Some(l) = logger.as_mut() {
        match &result {
            Ok(_) => {
                let _ = l.line(&format!("run {run_id} succeeded in {duration}s"));
            }
            Err(ProviderError::Cancelled) => {
                let _ = l.line(&format!("run {run_id} cancelled"));
            }
            Err(e) => {
                let _ = l.line(&format!("run {run_id} failed: {e}"));
            }
        }
    }
    // Sample cgroup usage before cleanup (peak-ish, end-of-run).
    let (mem_peak, cpu_seconds) = match cgroup_scope.as_ref().and_then(|cg| cg.usage()) {
        Some((mem, cpu)) => {
            if let Some(l) = logger.as_mut() {
                let _ = l.line(&format!(
                    "run {run_id} resources: {} memory, {cpu:.2}s cpu",
                    synora_core::human_size(mem)
                ));
            }
            (Some(mem), Some(cpu))
        }
        None => (None, None),
    };
    if let Some(cg) = cgroup_scope.as_ref() {
        cg.cleanup();
    }
    RunOutcome {
        result,
        duration_secs: duration,
        mem_peak,
        cpu_seconds,
    }
}

/// cgroup scope handle implementing the provider's attach trait.
struct CgroupHandle(std::path::PathBuf);
impl provider::CgroupScopeRef for CgroupHandle {
    fn attach(&self, pid: u32) {
        let _ = std::fs::write(self.0.join("cgroup.procs"), pid.to_string());
    }
}

fn provider_name(job: &JobSpec) -> &'static str {
    match &job.provider {
        synora_core::ProviderConfig::Rsync { .. } => "rsync",
        synora_core::ProviderConfig::Script { .. } => "script",
        synora_core::ProviderConfig::Docker { .. } => "docker",
        synora_core::ProviderConfig::Http { .. } => "http",
    }
}

/// Numeric mapping for `synora_job_status` gauge (spec §37). Distinct values
/// let dashboards color-code states.
pub fn status_value(s: JobStatus) -> f64 {
    match s {
        JobStatus::Pending => 0.0,
        JobStatus::Scheduled => 1.0,
        JobStatus::Queued => 2.0,
        JobStatus::Starting => 3.0,
        JobStatus::Running => 4.0,
        JobStatus::Success => 5.0,
        JobStatus::Failed => 6.0,
        JobStatus::Retrying => 7.0,
        JobStatus::Cancelling => 8.0,
        JobStatus::Cancelled => 9.0,
        JobStatus::Lost => 10.0,
        JobStatus::Skipped => 11.0,
    }
}

/// Engine-side tail: apply the outcome to the DB (state/retry/metrics).
async fn finish_run(engine: &Arc<Engine>, run_id: &str, job: &JobSpec, outcome: RunOutcome) {
    let ended = unix_now();
    let duration = outcome.duration_secs;
    let started = ended - duration;

    let retry_count = engine
        .store
        .get_run(run_id)
        .await
        .ok()
        .flatten()
        .map(|r| r.retry_count)
        .unwrap_or(0);

    // Cancelled: terminal, no retry (spec §5).
    if matches!(outcome.result, Err(ProviderError::Cancelled)) {
        let _ = engine
            .store
            .finish_run(
                run_id,
                JobStatus::Cancelled,
                None,
                None,
                None,
                None,
                Some("cancelled by operator"),
                duration,
            )
            .await;
        let _ = engine
            .store
            .insert_event(Some(&job.name), Some(run_id), "WARN", "run cancelled")
            .await;
        run_hooks(&job.hooks.on_failure, &hook_ctx(job, run_id), None).await;
        metrics_tail(engine, job, JobStatus::Cancelled, started, ended, duration, false);
        return;
    }

    let success = match &outcome.result {
        Ok(r) => r.status.as_deref().map(|s| s == "success").unwrap_or(true),
        Err(_) => false,
    };
    let final_status = if success {
        JobStatus::Success
    } else {
        JobStatus::Failed
    };

    if success {
        let result = outcome.result.as_ref().ok();
        let _ = engine
            .store
            .finish_run(
                run_id,
                JobStatus::Success,
                result.and_then(|r| r.exit_code),
                None,
                size_after(job, result),
                result.and_then(|r| r.bytes_transferred).map(|v| v as i64),
                result.and_then(|r| r.message.as_deref()),
                duration,
            )
            .await;
        if let Some(size) = size_after(job, result) {
            let _ = engine
                .store
                .set_repository_size(&job.storage.display().to_string(), size)
                .await;
            engine.metrics.set_gauge(
                "synora_repository_size_bytes",
                &[("repository", job.name.as_str())],
                size as f64,
            );
        }
        if let Some(bytes) = result.and_then(|r| r.bytes_transferred) {
            engine.metrics.inc_counter(
                "synora_job_bytes_transferred_total",
                &[("job", job.name.as_str()), ("worker", LOCAL_WORKER)],
                bytes as f64,
            );
        }
        if let Some(mem) = outcome.mem_peak {
            engine.metrics.set_gauge(
                "synora_job_memory_bytes",
                &[("job", job.name.as_str()), ("worker", LOCAL_WORKER)],
                mem as f64,
            );
        }
        if let Some(cpu) = outcome.cpu_seconds {
            engine.metrics.inc_counter(
                "synora_job_cpu_usage_seconds_total",
                &[("job", job.name.as_str()), ("worker", LOCAL_WORKER)],
                cpu,
            );
        }
        let _ = engine
            .store
            .insert_event(Some(&job.name), Some(run_id), "INFO", "run succeeded")
            .await;
        engine
            .notify("sync_success", Some(&job.name), "run succeeded")
            .await;
        run_hooks(&job.hooks.on_success, &hook_ctx(job, run_id), None).await;
    } else {
        let kind = outcome
            .result
            .as_ref()
            .err()
            .map(|e| e.kind())
            .unwrap_or(synora_core::ErrorKind::ProviderError);
        let message = match &outcome.result {
            Err(e) => e.to_string(),
            Ok(r) => r
                .status
                .clone()
                .unwrap_or_else(|| "status marked failure".to_string()),
        };
        let decision = retry_decision(
            kind,
            retry_count,
            job.retry,
            job.retry_delay.whole_seconds().max(1) as u64,
            job.retry_backoff,
        );
        match decision {
            synora_core::RetryDecision::Retry { delay_secs } => {
                let next = ended + delay_secs as i64;
                let _ = engine
                    .store
                    .set_retry(run_id, next, retry_count + 1)
                    .await;
                let _ = engine
                    .store
                    .set_run_status(run_id, JobStatus::Retrying)
                    .await;
                engine
                    .metrics
                    .inc_counter("synora_job_retries_total", &[("job", job.name.as_str())], 1.0);
                let _ = engine
                    .store
                    .insert_event(
                        Some(&job.name),
                        Some(run_id),
                        "WARN",
                        &format!("retry scheduled: {message}"),
                    )
                    .await;
                return;
            }
            synora_core::RetryDecision::NoRetry => {
                let _ = engine
                    .store
                    .finish_run(
                        run_id,
                        JobStatus::Failed,
                        outcome.result.as_ref().ok().and_then(|r| r.exit_code),
                        None,
                        None,
                        None,
                        Some(&message),
                        duration,
                    )
                    .await;
                engine
                    .metrics
                    .inc_counter("synora_job_failures_total", &[("job", job.name.as_str())], 1.0);
                let _ = engine
                    .store
                    .insert_event(
                        Some(&job.name),
                        Some(run_id),
                        "ERROR",
                        &format!("run failed: {message}"),
                    )
                    .await;
                engine
                    .notify("sync_failed", Some(&job.name), &message)
                    .await;
                run_hooks(&job.hooks.on_failure, &hook_ctx(job, run_id), None).await;
            }
        }
    }

    metrics_tail(engine, job, final_status, started, ended, duration, success);
}

#[allow(clippy::too_many_arguments)]
fn metrics_tail(
    engine: &Arc<Engine>,
    job: &JobSpec,
    status: JobStatus,
    started: i64,
    ended: i64,
    duration: i64,
    success: bool,
) {
    engine.metrics.set_gauge(
        "synora_job_status",
        &[("job", job.name.as_str()), ("worker", LOCAL_WORKER)],
        status_value(status),
    );
    engine.metrics.set_gauge(
        "synora_job_last_end_timestamp",
        &[("job", job.name.as_str())],
        ended as f64,
    );
    engine.metrics.set_gauge(
        "synora_job_last_start_timestamp",
        &[("job", job.name.as_str())],
        started as f64,
    );
    engine.metrics.set_gauge(
        "synora_job_duration_seconds",
        &[("job", job.name.as_str())],
        duration as f64,
    );
    if success {
        engine.metrics.set_gauge(
            "synora_job_last_success_timestamp",
            &[("job", job.name.as_str())],
            ended as f64,
        );
    }
}

/// Run a hook list via the same process machinery as the script provider.
/// Hook failures are warnings — they never change the run verdict (spec §50).
async fn run_hooks(hooks: &[String], ctx: &SyncContext, mut logger: Option<&mut RunLogger>) {
    for hook in hooks {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(hook);
        cmd.current_dir(&ctx.storage);
        cmd.env("SYNORA_JOB", &ctx.job_name);
        if let Some(up) = &ctx.upstream {
            cmd.env("SYNORA_UPSTREAM", up);
        }
        cmd.env("SYNORA_STORAGE", ctx.storage.display().to_string());
        cmd.env("SYNORA_RUN_ID", &ctx.run_id);
        let out = cmd.output().await;
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                tracing::warn!("hook `{hook}` exited with {:?}", o.status.code());
                if let Some(l) = logger.as_mut() {
                    let _ = l.line(&format!("hook `{hook}` exited with {:?}", o.status.code()));
                    let _ = l.raw(&o.stderr);
                }
            }
            Err(e) => {
                tracing::warn!("hook `{hook}` failed to run: {e}");
                if let Some(l) = logger.as_mut() {
                    let _ = l.line(&format!("hook `{hook}` failed to run: {e}"));
                }
            }
        }
    }
}

/// Minimal context for hooks called after the run (no provider cancel).
fn hook_ctx(job: &JobSpec, run_id: &str) -> SyncContext {
    SyncContext {
        run_id: run_id.to_string(),
        job_name: job.name.clone(),
        upstream: job.upstream.clone(),
        storage: job.storage.clone(),
        worker: Some(LOCAL_WORKER.to_string()),
        proxy: job.proxy.clone(),
        egress: job.egress.clone(),
        job: job.clone(),
        cancel: CancellationToken::new(),
        cgroup: None,
        proxy_env: Vec::new(),
        egress_address: None,
        family: job.family.clone(),
    }
}

/// Size detection priority (spec §17): provider hint → script output
/// (both via SyncResult.size_hint) → filesystem walk when configured.
fn size_after(job: &JobSpec, result: Option<&SyncResult>) -> Option<i64> {
    if let Some(hint) = result.and_then(|r| r.size_hint) {
        return Some(hint as i64);
    }
    match job.statistics {
        synora_core::StatisticsMode::Filesystem => Some(walk_size(&job.storage) as i64),
        synora_core::StatisticsMode::Provider => None,
    }
}

/// Post-sync verification checks (spec §56): "path" (storage exists),
/// "size" (non-zero size), "command" (run the configured command; exit 0).
fn run_verify(job: &JobSpec, result: &SyncResult) -> Result<(), String> {
    if !job.verify.enabled {
        return Ok(());
    }
    for check in &job.verify.checks {
        match check.as_str() {
            "path" => {
                if !job.storage.exists() {
                    return Err(format!("storage path {} missing", job.storage.display()));
                }
            }
            "size" => {
                let size = result
                    .size_hint
                    .or_else(|| (job.statistics == synora_core::StatisticsMode::Filesystem).then(|| walk_size(&job.storage)));
                if size.unwrap_or(0) == 0 {
                    return Err("repository size is zero".to_string());
                }
            }
            "command" => {
                let cmd = job
                    .verify
                    .command
                    .as_deref()
                    .ok_or("verify `command` check configured without a command")?;
                let out = std::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(cmd)
                    .current_dir(&job.storage)
                    .env("SYNORA_JOB", &job.name)
                    .env("SYNORA_STORAGE", job.storage.display().to_string())
                    .output()
                    .map_err(|e| e.to_string())?;
                if !out.status.success() {
                    return Err(format!(
                        "verify command `{cmd}` exited with {:?}",
                        out.status.code()
                    ));
                }
            }
            other => return Err(format!("unknown verify check `{other}`")),
        }
    }
    Ok(())
}
