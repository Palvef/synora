//! Git mirror provider (spec §59): keep a local clone of a remote repository
//! in sync. Default is a full `--mirror` clone (all refs, no checkout) —
//! the right shape for serving mirrors; `branch` opts into a single-branch
//! checkout instead. Updates are `remote update --prune` (mirror) or
//! `fetch + reset --hard` (branch). Cancellation/timeouts/cgroups come from
//! the shared spawn helpers.

use crate::{
    cancelled_after_wait, kill_group, spawn_group, ProviderError, SyncContext, SyncResult,
};
use std::process::Stdio;
use tokio::process::Command;

pub struct GitProvider {
    /// Clone only this branch (checkout mode); None = full mirror.
    pub branch: Option<String>,
}

impl GitProvider {
    pub async fn sync(&self, ctx: &SyncContext) -> Result<SyncResult, ProviderError> {
        let url = ctx.upstream.as_deref().ok_or_else(|| {
            ProviderError::Config("git requires `upstream` (repository URL)".into())
        })?;
        let dest = ctx
            .storage
            .to_str()
            .ok_or_else(|| ProviderError::Config("storage path is not UTF-8".into()))?;
        // A directory that is already a VALID git repo gets updated;
        // anything else (including interrupted clone leftovers like an empty
        // HEAD file) gets a fresh clone.
        let is_repo = tokio::process::Command::new("git")
            .args(["-C", dest, "rev-parse", "--git-dir"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);

        let mut cmd = Command::new("git");
        match (&self.branch, is_repo) {
            (Some(branch), false) => {
                cmd.args(["clone", "--single-branch", "--branch", branch, url, dest]);
            }
            (None, false) => {
                cmd.args(["clone", "--mirror", url, dest]);
            }
            (Some(branch), true) => {
                // Update the checked-out branch: fetch then reset to origin.
                cmd.arg("-C").arg(dest);
                cmd.args(["fetch", "origin", branch]);
            }
            (None, true) => {
                cmd.arg("-C").arg(dest);
                cmd.args(["remote", "update", "--prune"]);
            }
        }
        for (k, v) in &ctx.proxy_env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child =
            spawn_group(&mut cmd, ctx).map_err(|e| ProviderError::Spawn(format!("git: {e}")))?;
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
        if code != 0 {
            let err = String::from_utf8_lossy(&stderr);
            return Err(ProviderError::Other(format!(
                "git exited with {code}{}",
                if err.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", err.trim().chars().take(8000).collect::<String>())
                }
            )));
        }
        // Branch mode: after the fetch, move the checkout to origin's head.
        if let (Some(branch), true) = (&self.branch, is_repo) {
            let mut reset = Command::new("git");
            reset.arg("-C").arg(dest);
            reset.args(["reset", "--hard", &format!("origin/{branch}")]);
            reset
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = spawn_group(&mut reset, ctx)?;
            let status = tokio::select! {
                _ = ctx.cancel.cancelled() => {
                    kill_group(&mut child).await;
                    return Err(ProviderError::Cancelled);
                }
                r = child.wait() => r.map_err(|e| ProviderError::Other(e.to_string()))?,
            };
            if !status.success() {
                return Err(ProviderError::Other(format!(
                    "git reset exited with {}",
                    status.code().unwrap_or(-1)
                )));
            }
        }
        Ok(SyncResult {
            exit_code: Some(0),
            stdout,
            stderr,
            message: Some(if is_repo {
                "repository updated".into()
            } else {
                "repository cloned".into()
            }),
            ..Default::default()
        })
    }
}
