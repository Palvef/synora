//! Rsync provider (spec §13): spawn rsync with argv (never shell-concat,
//! spec §102), capture exit code + `--stats` output.
//!
//! `success_exit_codes` (23/24 = "done with transfer errors") count as
//! success — tunasync convention, alignment decision.

use crate::{ProviderError, SyncContext, SyncResult};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::{cancelled_after_wait, kill_group, spawn_group};

pub struct RsyncProvider {
    pub options: Vec<String>,
    pub exclude: Vec<String>,
}

impl RsyncProvider {
    /// Parse `--stats` output: `Total transferred file size: 123,456 bytes`.
    /// Returns bytes transferred (nil when rsync didn't print stats).
    fn parse_stats(stdout: &[u8]) -> Option<u64> {
        let text = String::from_utf8_lossy(stdout);
        let line = text
            .lines()
            .find(|l| l.contains("Total transferred file size"))?;
        let value = line.split(':').nth(1)?.trim();
        let value = value.trim_end_matches("bytes").trim();
        let mut num = String::new();
        let mut suffix = None;
        for c in value.chars() {
            match c {
                '0'..='9' | '.' => num.push(c),
                ',' => {} // thousands separator
                'K' | 'M' | 'G' | 'T' => suffix = Some(c),
                _ => break,
            }
        }
        let v: f64 = num.parse().ok()?;
        let mult = match suffix {
            Some('K') => 1024f64,
            Some('M') => 1024f64 * 1024.0,
            Some('G') => 1024f64 * 1024.0 * 1024.0,
            Some('T') => 1024f64 * 1024.0 * 1024.0 * 1024.0,
            _ => 1.0,
        };
        Some((v * mult).round() as u64)
    }

    fn is_success_exit(code: i32, whitelist: &[i32]) -> bool {
        code == 0 || whitelist.contains(&code)
    }

    pub async fn sync(&self, ctx: &SyncContext) -> Result<SyncResult, ProviderError> {
        let upstream = ctx
            .upstream
            .as_deref()
            .ok_or_else(|| ProviderError::Config("rsync requires `upstream`".to_string()))?;
        let dest = ctx
            .storage
            .to_str()
            .ok_or_else(|| ProviderError::Config("storage path is not UTF-8".to_string()))?;
        // Trailing slashes: sync the upstream dir *into* the storage dir.
        let mut source = upstream.to_string();
        if !source.ends_with('/') {
            source.push('/');
        }
        let mut dest = dest.to_string();
        if !dest.ends_with('/') {
            dest.push('/');
        }

        let mut cmd = Command::new("rsync");
        // tunasync-aligned defaults (same argv as tunasync's rsync provider:
        // `-aH --delete --delete-delay --delay-updates --safe-links
        //  --timeout=120 --contimeout=120`), then exclude, then job options,
        // then --stats.
        cmd.args([
            "-aH",
            "--delete",
            "--delete-delay",
            "--delay-updates",
            "--safe-links",
            "--timeout=120",
        ]);
        // --contimeout is daemon-connection only (rsync errors on local
        // paths); tunasync passes it because its upstreams are rsync://.
        if upstream.starts_with("rsync://") {
            cmd.arg("--contimeout=120");
        }
        for pat in &self.exclude {
            cmd.arg(format!("--exclude={pat}"));
        }
        for (k, v) in &ctx.proxy_env {
            cmd.env(k, v);
        }
        if let Some(addr) = &ctx.egress_address {
            cmd.arg("--address").arg(addr);
        }
        match ctx.family.as_str() {
            "ipv4" => {
                cmd.arg("--ipv4");
            }
            "ipv6" => {
                cmd.arg("--ipv6");
            }
            _ => {}
        }
        for opt in &self.options {
            cmd.arg(opt);
        }
        cmd.arg("--stats");
        cmd.arg(&source).arg(&dest);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = spawn_group(&mut cmd, ctx).map_err(|e| ProviderError::Spawn(format!("rsync: {e}")))?;
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
            bytes_transferred: Self::parse_stats(&stdout),
            stdout,
            stderr,
            ..Default::default()
        };
        if Self::is_success_exit(code, &ctx.job.success_exit_codes) {
            Ok(result)
        } else {
            Err(ProviderError::Exit(code))
        }
    }
}
