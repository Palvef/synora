//! Script provider (spec §16): run a configured command with the SYNORA_*
//! environment, and parse machine-readable result lines.
//!
//! Also injects TUNASYNC_MIRROR_NAME / TUNASYNC_UPSTREAM_URL /
//! TUNASYNC_WORKING_DIR so existing tunasync-scripts work unchanged
//! (alignment decision, see plan).

use crate::{ProviderError, SyncContext, SyncResult};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub struct ScriptProvider {
    pub command: String,
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
        // Command as argv list, never through a shell (spec §102).
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
        cmd.env("SYNORA_RUN_ID", ctx.run_id.to_string());
        // tunasync-scripts compatibility (alignment decision).
        cmd.env("TUNASYNC_MIRROR_NAME", &ctx.job_name);
        if let Some(up) = &ctx.upstream {
            cmd.env("TUNASYNC_UPSTREAM_URL", up);
        }
        cmd.env("TUNASYNC_WORKING_DIR", ctx.storage.display().to_string());
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| ProviderError::Spawn(format!("`{}`: {e}", self.command)))?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(mut s) = child.stdout.take() {
            let _ = s.read_to_end(&mut stdout).await;
        }
        if let Some(mut s) = child.stderr.take() {
            let _ = s.read_to_end(&mut stderr).await;
        }
        // Kill the child on cancel (timeout / synora stop) — a leaked child
        // would keep writing to a closed pipe and hang forever.
        let status = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(ProviderError::Timeout);
            }
            r = child.wait() => r.map_err(|e| ProviderError::Other(e.to_string()))?,
        };

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
        match (&result.status, status.code()) {
            (Some(s), _) if s == "success" => Ok(result),
            (Some(_), _) => Err(ProviderError::Exit(status.code().unwrap_or(1))),
            (None, Some(0)) | (None, None) => Ok(result),
            (None, Some(code)) => Err(ProviderError::Exit(code)),
        }
    }
}
