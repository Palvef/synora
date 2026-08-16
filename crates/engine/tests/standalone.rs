//! Standalone engine end-to-end: script jobs through the full loop
//! (dispatch → execute → size → retry → metrics) against a temp dir.

use config::{CliOverrides, ConfigLoader};
use engine::Engine;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "synora-engine-test-{}-{tag}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

async fn engine_for(dir: &Path) -> Arc<Engine> {
    let cfg = ConfigLoader::load(&dir.join("synora.toml"), &CliOverrides::default()).unwrap();
    Engine::new(cfg, &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations"))
        .await
        .unwrap()
}

fn config_text(dir: &Path) -> String {
    format!(
        r#"
include = ["jobs/*.toml"]

[daemon]
log_dir = "{dir}/logs"

[daemon.db]
kind = "sqlite"
path = "{dir}/data/synora.db"

[api]
listen = "127.0.0.1:0"
"#,
        dir = dir.display()
    )
}

/// Drive ticks until the run reaches a terminal state (bounded).
async fn wait_terminal(engine: &Arc<Engine>, run_id: &str, timeout_secs: u64) -> synora_core::job::JobStatus {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        engine.tick().await;
        if let Some(run) = engine.store.get_run(run_id).await.unwrap() {
            if matches!(
                run.status,
                synora_core::JobStatus::Success
                    | synora_core::JobStatus::Failed
                    | synora_core::JobStatus::Cancelled
                    | synora_core::JobStatus::Lost
            ) {
                return run.status;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run {run_id} never reached a terminal state"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn startup_script_job_runs_success_and_records_size() {
    let dir = temp_dir("smoke");
    let repo = dir.join("repo/smoke");
    write(
        &dir,
        "jobs/smoke.toml",
        &format!(
            r#"[[jobs]]
name = "smoke"
schedule = "startup"
provider = "script"
command = "mkdir -p sub && echo data > sub/f.txt && echo SYNORA_SIZE=999"
storage = "{}"
"#,
            repo.display()
        ),
    );
    write(&dir, "synora.toml", &config_text(&dir));
    let engine = engine_for(&dir).await;
    engine.sync_config().await.unwrap();
    let run_id = engine.dispatch("smoke").await.unwrap();
    let status = wait_terminal(&engine, &run_id, 15).await;
    assert_eq!(status, synora_core::JobStatus::Success);
    // SYNORA_SIZE= recorded into repositories + metrics (spec §17/§38).
    let size = engine
        .store
        .repository_size(&repo.display().to_string())
        .await
        .unwrap();
    assert_eq!(size, Some(999));
    assert!(repo.join("sub/f.txt").exists());
    let metrics = engine.metrics().render();
    assert!(
        metrics.contains("synora_job_status{job=\"smoke\",worker=\"local\"} 5"),
        "{metrics}"
    );
    assert!(
        metrics.contains("synora_repository_size_bytes{repository=\"smoke\"} 999"),
        "{metrics}"
    );
    // Log file exists with the run header (spec §49).
    let log = dir.join("logs/smoke/current.log");
    assert!(log.exists());
    let content = std::fs::read_to_string(&log).unwrap();
    assert!(content.contains("started (script provider)"), "{content}");
    assert!(content.contains("succeeded"), "{content}");
}

#[tokio::test]
async fn failing_script_retries_then_fails() {
    let dir = temp_dir("retry");
    write(
        &dir,
        "jobs/flaky.toml",
        r#"[[jobs]]
name = "flaky"
schedule = "startup"
provider = "script"
command = "exit 1"
storage = "/tmp/synora-engine-flaky-repo"
retry = 2
retry_delay = "1s"
"#,
    );
    write(&dir, "synora.toml", &config_text(&dir));
    let engine = engine_for(&dir).await;
    engine.sync_config().await.unwrap();
    let run_id = engine.dispatch("flaky").await.unwrap();
    let status = wait_terminal(&engine, &run_id, 30).await;
    assert_eq!(status, synora_core::JobStatus::Failed);
    // retry = 2 → two scheduled retries, then terminal failure (spec §54).
    let metrics = engine.metrics().render();
    assert!(
        metrics.contains("synora_job_retries_total{job=\"flaky\"} 2"),
        "{metrics}"
    );
    assert!(
        metrics.contains("synora_job_failures_total{job=\"flaky\"} 1"),
        "{metrics}"
    );
}

#[tokio::test]
async fn fail_on_match_forces_failure_despite_exit_zero() {
    let dir = temp_dir("failonmatch");
    write(
        &dir,
        "jobs/m.toml",
        r#"[[jobs]]
name = "m"
schedule = "startup"
provider = "script"
command = "echo 'FATAL: disk exploded' && exit 0"
storage = "/tmp/synora-engine-fom-repo"
retry = 0
fail_on_match = "FATAL"
"#,
    );
    write(&dir, "synora.toml", &config_text(&dir));
    let engine = engine_for(&dir).await;
    engine.sync_config().await.unwrap();
    let run_id = engine.dispatch("m").await.unwrap();
    let status = wait_terminal(&engine, &run_id, 15).await;
    assert_eq!(status, synora_core::JobStatus::Failed, "fail_on_match must force failure");
}
