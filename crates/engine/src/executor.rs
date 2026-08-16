//! Run executor: one run row → provider execution → state/retry/metrics.

use crate::engine::{unix_now, Engine, LOCAL_WORKER};
use crate::logs::{walk_size, RunLogger};
use provider::{build_provider, SyncContext};
use std::sync::Arc;
use synora_core::job::{JobSpec, JobStatus};
use synora_core::state::retry_decision;
use synora_core::RunId;

/// Execute one claimed run. Called from a spawned task; drops the global
/// semaphore permit when done.
pub async fn execute_run(
    engine: &Arc<Engine>,
    run_id: String,
    job: JobSpec,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    let started = unix_now();
    let log_path = {
        let p = engine
            .cfg
            .daemon
            .log_dir
            .join(&job.name)
            .join("current.log");
        p.display().to_string()
    };
    let _ = engine
        .store
        .insert_log(&run_id, &job.name, &log_path)
        .await;

    let mut logger = RunLogger::open(&engine.cfg.daemon.log_dir, &job.name).ok();
    if let Some(l) = logger.as_mut() {
        let _ = l.line(&format!("run {run_id} started ({} provider)", provider_name(&job)));
    }

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

    // Storage dir must exist before providers run.
    if let Err(e) = std::fs::create_dir_all(&job.storage) {
        finish_run(
            engine,
            &run_id,
            &job,
            Err(provider::ProviderError::Config(format!("cannot create storage dir: {e}"))),
            None,
            started,
        )
        .await;
        engine.active_dec(&job.name);
        return;
    }

    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = SyncContext {
        run_id: RunId::new(),
        job_name: job.name.clone(),
        upstream: job.upstream.clone(),
        storage: job.storage.clone(),
        worker: Some(LOCAL_WORKER.to_string()),
        proxy: job.proxy.clone(),
        egress: job.egress.clone(),
        job: job.clone(),
        cancel: cancel.clone(),
    };

    let provider = match build_provider(&job) {
        Ok(p) => p,
        Err(e) => {
            finish_run(engine, &run_id, &job, Err(e), None, started).await;
            engine.active_dec(&job.name);
            return;
        }
    };

    // Timeout wraps the provider; cancel kills the child process.
    let outcome = tokio::select! {
        r = tokio::time::timeout(
            std::time::Duration::from_secs(job.timeout.whole_seconds().max(1) as u64),
            provider.sync(&ctx),
        ) => {
            match r {
                Err(_) => {
                    cancel.cancel();
                    Err(provider::ProviderError::Timeout)
                }
                Ok(r) => r,
            }
        }
    };

    let result = match outcome {
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
                    Err(provider::ProviderError::Other(format!(
                        "output matched fail_on_match `{re}`"
                    )))
                } else {
                    Ok(result)
                }
            } else {
                Ok(result)
            }
        }
        Err(e) => Err(e),
    };

    finish_run(engine, &run_id, &job, result, logger.as_mut(), started).await;
    engine.active_dec(&job.name);
}

fn provider_name(job: &JobSpec) -> &'static str {
    match &job.provider {
        synora_core::ProviderConfig::Rsync { .. } => "rsync",
        synora_core::ProviderConfig::Script { .. } => "script",
        synora_core::ProviderConfig::Docker { .. } => "docker",
    }
}

/// Numeric mapping for `synora_job_status` gauge (spec §37). Distinct values
/// let dashboards color-code states.
fn status_value(s: JobStatus) -> f64 {
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
    }
}

/// Common tail: success / failure / retry, size update, metrics, logs.
#[allow(clippy::too_many_arguments)]
async fn finish_run(
    engine: &Arc<Engine>,
    run_id: &str,
    job: &JobSpec,
    outcome: Result<provider::SyncResult, provider::ProviderError>,
    logger: Option<&mut RunLogger>,
    started: i64,
) {
    let ended = unix_now();
    let duration = ended - started;

    // First read retry_count for the retry decision.
    let retry_count = engine
        .store
        .get_run(run_id)
        .await
        .ok()
        .flatten()
        .map(|r| r.retry_count)
        .unwrap_or(0);

    let success = match &outcome {
        Ok(r) => r.status.as_deref().map(|s| s == "success").unwrap_or(true),
        Err(_) => false,
    };
    let final_status = if success {
        JobStatus::Success
    } else {
        JobStatus::Failed
    };

    if success {
        let _ = engine
            .store
            .finish_run(
                run_id,
                JobStatus::Success,
                outcome.as_ref().ok().and_then(|r| r.exit_code),
                None,
                size_after(job, outcome.as_ref().ok()),
                outcome.as_ref().ok().and_then(|r| r.bytes_transferred).map(|v| v as i64),
                outcome.as_ref().ok().and_then(|r| r.message.as_deref()),
                duration,
            )
            .await;
        // Repository size (spec §17): provider hint → script → filesystem.
        if let Some(size) = size_after(job, outcome.as_ref().ok()) {
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
        if let Some(l) = logger {
            let _ = l.line(&format!("run {run_id} succeeded in {duration}s"));
        }
        let _ = engine
            .store
            .insert_event(Some(&job.name), Some(run_id), "INFO", "run succeeded")
            .await;
    } else {
        // Failure: classify, maybe retry (spec §54).
        let kind = outcome
            .as_ref()
            .err()
            .map(|e| e.kind())
            .unwrap_or(synora_core::ErrorKind::ProviderError);
        let message = match &outcome {
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
                if let Some(l) = logger {
                    let _ = l.line(&format!(
                        "run {run_id} failed ({message}); retry {}/{} in {delay_secs}s",
                        retry_count + 1,
                        job.retry
                    ));
                }
                let _ = engine
                    .store
                    .insert_event(Some(&job.name), Some(run_id), "WARN", &format!("retry scheduled: {message}"))
                    .await;
                return;
            }
            synora_core::RetryDecision::NoRetry => {
                let _ = engine
                    .store
                    .finish_run(
                        run_id,
                        JobStatus::Failed,
                        outcome.as_ref().ok().and_then(|r| r.exit_code),
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
                if let Some(l) = logger {
                    let _ = l.line(&format!("run {run_id} failed: {message}"));
                }
                let _ = engine
                    .store
                    .insert_event(Some(&job.name), Some(run_id), "ERROR", &format!("run failed: {message}"))
                    .await;
                // on_failure hooks land in M2 (same executor machinery).
            }
        }
    }

    // Metrics tail.
    engine.metrics.set_gauge(
        "synora_job_status",
        &[("job", job.name.as_str()), ("worker", LOCAL_WORKER)],
        status_value(final_status),
    );
    engine.metrics.set_gauge(
        "synora_job_last_end_timestamp",
        &[("job", job.name.as_str())],
        ended as f64,
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

/// Size detection priority (spec §17): provider hint → script output
/// (both via SyncResult.size_hint) → filesystem walk when configured.
fn size_after(job: &JobSpec, result: Option<&provider::SyncResult>) -> Option<i64> {
    if let Some(hint) = result.and_then(|r| r.size_hint) {
        return Some(hint as i64);
    }
    match job.statistics {
        synora_core::StatisticsMode::Filesystem => Some(walk_size(&job.storage) as i64),
        synora_core::StatisticsMode::Provider => None,
    }
}
