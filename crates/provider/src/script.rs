//! Script provider (spec §16): run a configured command with the SYNORA_*
//! environment, and parse machine-readable result lines.
//!
//! On workers this runs inside `synora-scripts`. Standalone/tests without
//! `scripts_image` still exec the command on the host.

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
                    command: crate::docker::docker_exec_args(std::slice::from_ref(&self.command)),
                    extra_options: Vec::new(),
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
        // Keep tunasync's command-provider contract so migrated scripts and
        // images do not need an all-at-once environment-variable rewrite.
        cmd.env("TUNASYNC_MIRROR_NAME", &ctx.job_name);
        cmd.env("TUNASYNC_WORKING_DIR", ctx.storage.display().to_string());
        if let Some(up) = &ctx.upstream {
            cmd.env("SYNORA_UPSTREAM", up);
            cmd.env("TUNASYNC_UPSTREAM_URL", up);
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
        if let Some(api) = ctx.manager_url.as_deref().filter(|s| !s.trim().is_empty()) {
            cmd.env("SYNORA_API", api);
        }
        cmd.env("PYTHONUNBUFFERED", "1");
        cmd.env(
            "SYNORA_LOG_DIR",
            format!("{}/.synora-log", ctx.storage.display()),
        );
        let compat_log_dir = format!("{}/.synora-log", ctx.storage.display());
        cmd.env("TUNASYNC_LOG_DIR", &compat_log_dir);
        cmd.env(
            "TUNASYNC_LOG_FILE",
            ctx.log_file
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new("/dev/null")),
        );
        for e in &self.env {
            if let Some((k, v)) = e.split_once('=') {
                cmd.env(k, v);
            }
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let (mut child, guard) = spawn_group(&mut cmd, ctx)
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
        guard.disarm();

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

        // Exit 0 = SUCCESS, non-zero/no-code = FAILED (spec §16).
        // SYNORA_STATUS can turn exit 0 into failure, but success must never
        // hide a real process failure.
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
        if crate::process_result_is_success(result.exit_code, result.status.as_deref(), &[]) {
            return Ok(result);
        }
        match (result.exit_code, result.status.as_deref()) {
            (None, _) => Err(ProviderError::Other(
                "script terminated without an exit code".to_string(),
            )),
            (Some(0), Some(reported)) => Err(ProviderError::Other(format!(
                "script reported status {reported}"
            ))),
            // The success predicate above accepts exit 0 without a status.
            (Some(0), None) => unreachable!(),
            (Some(code), _) => Err(fail(code)),
        }
    }
}
