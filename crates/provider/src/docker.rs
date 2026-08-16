//! Docker provider (spec §18/§75–§77): `docker run` subprocess for now —
//! exit code + stdout/stderr + volumes cover P0/P1. bollard (Docker API) is
//! the later upgrade when lifecycle events/resource limits are needed
//! (spec §75 prefers the API; flagged tradeoff).

use crate::{cancelled_after_wait, kill_group, spawn_group, ProviderError, SyncContext, SyncResult};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub struct DockerProvider {
    pub image: String,
    pub env: Vec<String>,
    pub volumes: Vec<String>,
    pub keep_container: bool,
}

impl DockerProvider {
    pub async fn sync(&self, ctx: &SyncContext) -> Result<SyncResult, ProviderError> {
        let mut cmd = Command::new("docker");
        cmd.arg("run");
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
        // Container convention (spec §77): host storage → /data.
        let host_storage = ctx
            .storage
            .to_str()
            .ok_or_else(|| ProviderError::Config("storage path is not UTF-8".to_string()))?;
        cmd.arg("-v").arg(format!("{host_storage}:/data"));
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
        cmd.arg("-e")
            .arg(format!("SYNORA_STORAGE={host_storage}"))
            .arg("-e")
            .arg(format!("SYNORA_RUN_ID={}", ctx.run_id));
        cmd.arg(&self.image);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = spawn_group(&mut cmd, ctx).map_err(|e| ProviderError::Spawn(format!("docker run: {e}")))?;
        // Read pipes and wait for exit concurrently with cancellation: a
        // long-running child keeps its pipes open, so a plain read_to_end
        // before the select would swallow cancels until the child exits.
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let read_fut = async {
            let mut out = Vec::new();
            let mut err = Vec::new();
            if let Some(mut s) = stdout_pipe {
                let _ = s.read_to_end(&mut out).await;
            }
            if let Some(mut s) = stderr_pipe {
                let _ = s.read_to_end(&mut err).await;
            }
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
            Err(ProviderError::Exit(code))
        }
    }
}
