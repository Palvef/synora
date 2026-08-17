//! Docker provider (spec §18/§75–§77): `docker run` subprocess for now —
//! exit code + stdout/stderr + volumes cover P0/P1. bollard (Docker API) is
//! the later upgrade when lifecycle events/resource limits are needed
//! (spec §75 prefers the API; flagged tradeoff).

use crate::{
    cancelled_after_wait, kill_group, spawn_group, ProviderError, SyncContext, SyncResult,
};
use std::process::Stdio;
use tokio::process::Command;

pub struct DockerProvider {
    pub image: String,
    pub env: Vec<String>,
    pub volumes: Vec<String>,
    pub keep_container: bool,
    pub command: Vec<String>,
}

impl DockerProvider {
    pub async fn sync(&self, ctx: &SyncContext) -> Result<SyncResult, ProviderError> {
        let mut cmd = Command::new("docker");
        cmd.arg("run");
        // Container name follows the `synora-job-<job>` convention
        // (tunasync uses tunasync-job-<mirror>; operator requirement).
        // A leftover container from a killed worker would collide with
        // --name — remove it first (docker run --rm cleans up after itself,
        // this only handles crash leftovers).
        let cname = format!("synora-job-{}", ctx.job_name);
        let _ = tokio::process::Command::new("docker")
            .args(["rm", "-f", &cname])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        cmd.arg("--name").arg(&cname);
        if !self.keep_container {
            cmd.arg("--rm");
        }
        // Resource limits via docker's own cgroup integration.
        if let Some(mem) = ctx.job.memory_limit {
            cmd.arg("--memory").arg(mem.to_string());
        }
        if let Some(cpu) = ctx.job.cpu_limit {
            cmd.arg("--cpus").arg(format!("{cpu}"));
        }
        // Container convention (spec §77): host storage → /data. A user
        // volume that already mounts /data wins (no duplicate mount point).
        let host_storage = ctx
            .storage
            .to_str()
            .ok_or_else(|| ProviderError::Config("storage path is not UTF-8".to_string()))?;
        let mounts_data = self.volumes.iter().any(|v| {
            v.split(':')
                .nth(1)
                .map(|dst| dst == "/data")
                .unwrap_or(false)
        });
        if !mounts_data {
            cmd.arg("-v").arg(format!("{host_storage}:/data"));
        }
        // tunasync-scripts images expect the mirror at its HOST path inside
        // the container (TUNASYNC_WORKING_DIR=/datas/...): bind it under its
        // own path too, so both conventions work.
        cmd.arg("-v").arg(format!("{host_storage}:{host_storage}"));
        for v in &self.volumes {
            cmd.arg("-v").arg(v);
        }
        for e in &self.env {
            cmd.arg("-e").arg(e);
        }
        for (k, v) in &ctx.proxy_env {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        // SYNORA_* env for scripts inside the container.
        cmd.arg("-e").arg(format!("SYNORA_JOB={}", ctx.job_name));
        if let Some(up) = &ctx.upstream {
            cmd.arg("-e").arg(format!("SYNORA_UPSTREAM={up}"));
        }
        // TUNASYNC_* compatibility (tunasync-scripts images read these).
        // tunasync convention: the working dir is the per-mirror directory;
        // synora's resolved storage path IS that directory (the relative
        // path / mirror_subdir composition already happened), so no name
        // is appended here.
        let tunasync_workdir = host_storage.to_string();
        cmd.arg("-w").arg(&tunasync_workdir);
        cmd.arg("-e")
            .arg(format!("TUNASYNC_MIRROR_NAME={}", ctx.job_name));
        if let Some(up) = &ctx.upstream {
            cmd.arg("-e").arg(format!("TUNASYNC_UPSTREAM_URL={up}"));
            // ustcmirror/rsync-style sync.sh scripts read RSYNC_HOST /
            // RSYNC_PATH instead of the tunasync variables.
            if let Some(rest) = up.strip_prefix("rsync://") {
                let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
                cmd.arg("-e").arg(format!("RSYNC_HOST={host}"));
                cmd.arg("-e").arg(format!("RSYNC_PATH=/{path}"));
            }
        }
        cmd.arg("-e")
            .arg(format!("TUNASYNC_WORKING_DIR={tunasync_workdir}"));
        cmd.arg("-e")
            .arg(format!("TUNASYNC_LOG_DIR={tunasync_workdir}/.synora-log"));
        // SYNORA_STORAGE is the in-container path (spec §77: /data by
        // convention; when a user volume takes /data the host value is
        // their problem, pass it through).
        cmd.arg("-e")
            .arg(format!(
                "SYNORA_STORAGE={}",
                if mounts_data { host_storage } else { "/data" }
            ))
            .arg("-e")
            .arg(format!("SYNORA_RUN_ID={}", ctx.run_id));
        cmd.arg(&self.image);
        for arg in &self.command {
            cmd.arg(arg);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let (mut child, _guard) = spawn_group(&mut cmd, ctx)
            .map_err(|e| ProviderError::Spawn(format!("docker run: {e}")))?;
        // Read pipes and wait for exit concurrently with cancellation: a
        // long-running child keeps its pipes open, so a plain read_to_end
        // before the select would swallow cancels until the child exits.
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        // Both pipes are drained CONCURRENTLY: reading stdout to EOF before
        // touching stderr deadlocks when the child fills the stderr buffer.
        // Memory keeps only the tail (the full stream is in the run log).
        let log_file = ctx.log_file.clone();
        let read_fut = async {
            let (out, err) = tokio::join!(
                crate::read_pipe_tee(stdout_pipe, &log_file),
                crate::read_pipe_tee_err(stderr_pipe, &log_file),
            );
            (out, err)
        };
        tokio::pin!(read_fut);
        let stdout: Vec<u8>;
        let stderr: Vec<u8>;
        tokio::select! {
            _ = ctx.cancel.cancelled() => {
                kill_group(&mut child).await;
                return Err(ProviderError::Cancelled);
            }
            r = &mut read_fut => {
                (stdout, stderr) = r;
            }
        }
        let status = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                kill_group(&mut child).await;
                return Err(ProviderError::Cancelled);
            }
            r = child.wait() => r.map_err(|e| ProviderError::Other(e.to_string()))?,
        };
        cancelled_after_wait(ctx, &mut child).await?;
        let code = status.code().unwrap_or(-1);
        let result = SyncResult {
            exit_code: Some(code),
            stdout,
            stderr,
            ..Default::default()
        };
        if code == 0 {
            Ok(result)
        } else {
            // Carry both streams: tunasync scripts often print the reason
            // on stdout, not stderr.
            let mut detail = String::from_utf8_lossy(&result.stderr).trim().to_string();
            if detail.is_empty() {
                detail = String::from_utf8_lossy(&result.stdout).trim().to_string();
            } else {
                let out = String::from_utf8_lossy(&result.stdout);
                if !out.trim().is_empty() {
                    detail.push_str(" | stdout: ");
                    detail.push_str(out.trim());
                }
            }
            detail = detail.chars().take(800).collect();
            Err(ProviderError::Other(format!(
                "docker exited with {code}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            )))
        }
    }
}
