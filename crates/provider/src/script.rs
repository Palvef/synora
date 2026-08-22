//! Script provider (spec §16): run a configured command with the SYNORA_*
//! environment, and parse machine-readable result lines.
//!
//! Also injects TUNASYNC_MIRROR_NAME / TUNASYNC_UPSTREAM_URL /
//! TUNASYNC_WORKING_DIR so existing tunasync-scripts work unchanged
//! (alignment decision, see plan).

use crate::{ProviderError, SyncContext, SyncResult};
use std::process::Stdio;
use tokio::process::Command;

use crate::{cancelled_after_wait, kill_group, spawn_group};

pub struct ScriptProvider {
    pub command: String,
    pub env: Vec<String>,
}

/// Machine-readable lines parsed from provider output (spec §16).
#[derive(Debug, Default)]
struct ParsedOutput {
    size_hint: Option<u64>,
    status: Option<String>,
    message: Option<String>,
}

/// Parse provider output lines: `SYNORA_SIZE=123`, `SYNORA_STATUS=success`,
/// `SYNORA_MESSAGE=...` (spec §16).
fn parse_output(stdout: &[u8]) -> ParsedOutput {
    let mut out = ParsedOutput::default();
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("SYNORA_SIZE=") {
            if let Ok(v) = rest.trim().parse::<u64>() {
                out.size_hint = Some(v);
            }
        } else if let Some(rest) = line.strip_prefix("SYNORA_STATUS=") {
            out.status = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("SYNORA_MESSAGE=") {
            out.message = Some(rest.trim().to_string());
        }
    }
    out
}

impl ScriptProvider {
    pub async fn sync(&self, ctx: &SyncContext) -> Result<SyncResult, ProviderError> {
        if let Some(image) = crate::docker::scripts_image_name(ctx.scripts_image.as_deref()) {
            return crate::docker::run_named_container(
                crate::docker::DockerRunSpec {
                    image: image.to_string(),
                    command: crate::docker::docker_exec_args(&[self.command.clone()]),
                    extra_env: self.env.clone(),
                    extra_volumes: Vec::new(),
                    keep_container: false,
                    network: None,
                },
                ctx,
            )
            .await;
        }
        // Scripts run through a shell by design (tunasync compatibility:
        // tunasync-scripts are shell scripts). The command string comes from
        // trusted local config only — never from API input. Other providers
        // execute argv arrays without a shell.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(&self.command);
        cmd.current_dir(&ctx.storage);
        cmd.env("SYNORA_JOB", &ctx.job_name);
        if let Some(up) = &ctx.upstream {
            cmd.env("SYNORA_UPSTREAM", up);
        }
        cmd.env("SYNORA_STORAGE", ctx.storage.display().to_string());
        if let Some(w) = &ctx.worker {
            cmd.env("SYNORA_WORKER", w);
        }
        if let Some(p) = &ctx.proxy {
            cmd.env("SYNORA_PROXY", p);
        }
        if let Some(e) = &ctx.egress {
            cmd.env("SYNORA_EGRESS", e);
        }
        cmd.env("SYNORA_RUN_ID", &ctx.run_id);
        for (k, v) in &ctx.proxy_env {
            cmd.env(k, v);
        }
        if let Some(addr) = &ctx.egress_address {
            cmd.env("SYNORA_EGRESS_ADDRESS", addr);
        }
        cmd.env("SYNORA_FAMILY", &ctx.family);
        // tunasync-scripts compatibility (alignment decision).
        cmd.env("PYTHONUNBUFFERED", "1");
        cmd.env("TUNASYNC_MIRROR_NAME", &ctx.job_name);
        if let Some(up) = &ctx.upstream {
            cmd.env("TUNASYNC_UPSTREAM_URL", up);
        }
        cmd.env("TUNASYNC_WORKING_DIR", ctx.storage.display().to_string());
        cmd.env(
            "TUNASYNC_LOG_DIR",
            format!("{}/.synora-log", ctx.storage.display()),
        );
        for e in &self.env {
            if let Some((k, v)) = e.split_once('=') {
                cmd.env(k, v);
            }
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let (mut child, _guard) = spawn_group(&mut cmd, ctx)
            .map_err(|e| ProviderError::Spawn(format!("`{}`: {e}", self.command)))?;
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

        let parsed = parse_output(&stdout);
        let result = SyncResult {
            exit_code: status.code(),
            stdout,
            stderr,
            size_hint: parsed.size_hint,
            message: parsed.message,
            status: parsed.status,
            ..Default::default()
        };

        // Exit 0 = SUCCESS, non-zero = FAILED (spec §16) — but SYNORA_STATUS=
        // can override both directions.
        // Failures carry the script's output so it lands in the run log.
        let fail = |code: i32| -> ProviderError {
            let out = String::from_utf8_lossy(&result.stdout);
            let err = String::from_utf8_lossy(&result.stderr);
            let mut detail = err.trim().to_string();
            if detail.is_empty() {
                detail = out.trim().to_string();
            } else if !out.trim().is_empty() {
                detail.push_str(" | stdout: ");
                detail.push_str(out.trim());
            }
            ProviderError::Other(format!(
                "script exited with {code}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {}", detail.chars().take(8000).collect::<String>())
                }
            ))
        };
        match (&result.status, status.code()) {
            (Some(s), _) if s == "success" => Ok(result),
            (Some(_), _) => Err(fail(status.code().unwrap_or(1))),
            (None, Some(0)) | (None, None) => Ok(result),
            (None, Some(code)) => Err(fail(code)),
        }
    }
}
