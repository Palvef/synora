//! Scheduler loop ticks (spec §6–§7): boot pass (misfire), retry tick, due
//! dispatch, and QUEUED-run execution. The engine runs all of them every 2s.

use crate::engine::{unix_now, Engine};
use crate::executor;
use std::sync::Arc;
use synora_core::job::JobStatus;

/// First tick after boot: rows whose `next_run` is in the past were missed
/// while offline. `next_run >= boot` means they were synced at this boot —
/// those fire normally. Missed ones follow the job's misfire policy (spec §7).
pub async fn boot_pass(engine: &Arc<Engine>, boot: i64) {
    let now = unix_now();
    let Ok(schedules) = engine.store.all_schedules().await else {
        return;
    };
    for (name, row) in schedules {
        let Some(next_run) = row.next_run else { continue };
        if next_run > now || next_run >= boot {
            continue; // future, or synced-just-now: normal dispatch handles it
        }
        match row.misfire_policy.as_str() {
            "run-immediately" => {
                tracing::info!("job `{name}`: missed run, dispatching immediately (misfire=run-immediately)");
                if let Err(e) = engine.dispatch(&name).await {
                    tracing::warn!("misfire dispatch of `{name}` failed: {e}");
                }
                recompute_next(engine, &name).await;
            }
            other => {
                tracing::info!("job `{name}`: missed run, skipping (misfire={other})");
                recompute_next(engine, &name).await;
            }
        }
    }
}

/// Retries whose wait elapsed: back to the queue.
pub async fn retry_tick(engine: &Arc<Engine>, now: i64) {
    let Ok(due) = engine.store.due_retries(now).await else {
        return;
    };
    for run in due {
        let _ = engine
            .store
            .set_run_status(&run.id, JobStatus::Queued)
            .await;
        tracing::info!("job `{}`: retry re-queued (attempt {})", run.job_id, run.retry_count);
    }
}

/// Dispatch jobs whose schedule is due (strictly future next_run afterwards).
pub async fn dispatch_due(engine: &Arc<Engine>, now: i64) {
    let Ok(schedules) = engine.store.all_schedules().await else {
        return;
    };
    for (name, row) in schedules {
        let Some(next_run) = row.next_run else { continue };
        if next_run > now {
            continue;
        }
        if let Err(e) = engine.dispatch(&name).await {
            tracing::warn!("dispatch of `{name}` failed: {e}");
        }
        recompute_next(engine, &name).await;
    }
}

/// Recompute `next_run` from the wall clock — never from run end (spec §6.5).
async fn recompute_next(engine: &Arc<Engine>, job_name: &str) {
    let Ok(Some(row)) = engine.store.get_schedule(job_name).await else {
        return;
    };
    let Some(job) = engine.job(job_name) else { return };
    let Some(tz) = time_tz::timezones::get_by_name(&row.timezone) else {
        return;
    };
    let now = time::OffsetDateTime::from_unix_timestamp(unix_now())
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let anchor = row
        .anchor_at
        .map(|a| time::OffsetDateTime::from_unix_timestamp(a).unwrap_or(now));
    let next = job.schedule.next_after(now, tz, anchor).map(|t| t.unix_timestamp());
    let _ = engine.store.set_next_run(job_name, next, None).await;
    engine.metrics.set_gauge(
        "synora_job_next_run_timestamp",
        &[("job", job_name)],
        next.unwrap_or(0) as f64,
    );
}

/// Claim QUEUED runs (concurrency gates) and spawn execution tasks.
pub async fn execute_queued(engine: &Arc<Engine>) {
    let Ok(queued) = engine
        .store
        .queued_runs(crate::engine::LOCAL_WORKER)
        .await
    else {
        return;
    };
    for run in queued {
        if engine.is_shutdown() {
            return;
        }
        let Some(job) = engine.job(&run.job_id) else {
            // Job vanished from config: fail the run instead of leaving it stuck.
            let _ = engine
                .store
                .finish_run(&run.id, JobStatus::Failed, None, None, None, None, Some("job removed from config"), 0)
                .await;
            continue;
        };
        // Per-job concurrency gate.
        {
            let active = engine.active.lock().unwrap();
            if let Some(n) = active.get(&run.job_id) {
                if *n >= job.max_concurrency as usize {
                    continue;
                }
            }
        }
        // Global gate: only claim when a permit is available.
        let Ok(permit) = engine.global_sem().clone().try_acquire_owned() else {
            continue;
        };
        let Ok(claimed) = engine
            .store
            .claim_run(&run.id, crate::engine::LOCAL_WORKER)
            .await
        else {
            drop(permit);
            continue;
        };
        if !claimed {
            drop(permit);
            continue;
        }
        engine.active_inc(&run.job_id);
        let engine = engine.clone();
        tokio::spawn(async move {
            executor::execute_run(&engine, run.id, job, permit).await;
        });
    }
}
