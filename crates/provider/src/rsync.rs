//! Rsync provider (spec §13): spawn rsync with argv (never shell-concat,
//! spec §102), capture exit code + `--stats` output.
//!
//! `success_exit_codes` (23/24 = "done with transfer errors") count as
//! success — tunasync convention, alignment decision.

use crate::{ProviderError, SyncContext, SyncResult};
use std::process::Stdio;
use tokio::process::Command;

use crate::{cancelled_after_wait, kill_group, spawn_group};

pub struct RsyncProvider {
    pub options: Vec<String>,
    pub exclude: Vec<String>,
}

fn rsync_proxy_hostport(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    rest.rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(rest)
        .to_string()
}

/// tunasync: `RSYNC_PROXY = "host:port"` (no scheme). Dispatch already
/// emits it; synthesize from HTTP/ALL_PROXY when missing.
/// Password/exclude files live on the worker. Job specs may still point at
/// a manager-only or legacy tunasync path; rewrite to a local file if needed.
fn rewrite_worker_local_path(opt: &str) -> String {
    const PREFIXES: [&str; 2] = ["--password-file=", "--exclude-from="];
    for prefix in PREFIXES {
        let Some(path) = opt.strip_prefix(prefix) else {
            continue;
        };
        let p = std::path::Path::new(path);
        if p.is_file() {
            return opt.to_string();
        }
        let mut candidates = Vec::new();
        if path.contains("/etc/tunasync/") {
            candidates.push(
                path.replace("/etc/tunasync/syncpassword/", "/etc/synora/syncpassword/")
                    .replace("/etc/tunasync/excludes/", "/etc/synora/excludes/"),
            );
        }
        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if prefix == "--password-file=" {
                candidates.push(format!("/etc/synora/syncpassword/{name}"));
                candidates.push(format!("/etc/tunasync/syncpassword/{name}"));
            } else {
                candidates.push(format!("/etc/synora/excludes/{name}"));
                candidates.push(format!("/etc/tunasync/excludes/{name}"));
            }
        }
        for candidate in candidates {
            if candidate != path && std::path::Path::new(&candidate).is_file() {
                tracing::info!("rsync {prefix}{path} missing; using {candidate}");
                return format!("{prefix}{candidate}");
            }
        }
    }
    opt.to_string()
}

fn apply_rsync_proxy_env(cmd: &mut Command, proxy_env: &[(String, String)]) {
    let mut have_rsync = false;
    let mut url = None;
    for (k, v) in proxy_env {
        cmd.env(k, v);
        if k.eq_ignore_ascii_case("RSYNC_PROXY") {
            have_rsync = true;
        }
        if url.is_none()
            && (k.eq_ignore_ascii_case("all_proxy") || k.eq_ignore_ascii_case("http_proxy"))
        {
            url = Some(v.clone());
        }
    }
    if !have_rsync {
        if let Some(u) = url {
            cmd.env("RSYNC_PROXY", rsync_proxy_hostport(&u));
        }
    }
}

/// Defaults shared with tunasync's rsync providers.  In particular,
/// `.~tmp~` must be both protected from deletion and excluded from transfer:
/// tunasync scripts use it as the staging directory for atomic publication.
fn apply_tunasync_defaults(cmd: &mut Command, delete: bool) {
    cmd.args([
        "-aH",
        "-v",
        "-h",
        "--no-o",
        "--no-g",
        "--filter",
        "risk .~tmp~/",
        "--exclude",
        ".~tmp~/",
    ]);
    if delete {
        cmd.args(["--delete", "--delete-after", "--delay-updates"]);
    }
    cmd.args(["--safe-links", "--timeout=120"]);
}

impl RsyncProvider {
    /// Parse a rsync size figure: "1,645,311,660,221", "1.5G", "37 bytes" —
    /// commas ignored, K/M/G/T suffixes honored.
    fn parse_figure(value: &str) -> Option<u64> {
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

    /// Parse `--stats` output. Returns (transferred bytes, final repository
    /// size) — the size comes from `Total file size: X bytes`, falling back
    /// to the trailing `total size is X` line (which may carry a suffix).
    fn parse_stats(stdout: &[u8]) -> (Option<u64>, Option<u64>) {
        let text = String::from_utf8_lossy(stdout);
        let transferred = text
            .lines()
            .find(|l| l.contains("Total transferred file size"))
            .and_then(|line| line.split(':').nth(1))
            .and_then(|v| Self::parse_figure(v.trim().trim_end_matches("bytes").trim()));
        let total = text
            .lines()
            .find(|l| l.contains("Total file size"))
            .and_then(|line| line.split(':').nth(1))
            .and_then(|v| Self::parse_figure(v.trim().trim_end_matches("bytes").trim()))
            .or_else(|| {
                // Older rsync: "total size is 1.23G  speedup is ..."
                text.lines()
                    .find(|l| l.contains("total size is"))
                    .and_then(|l| l.split("total size is").nth(1))
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(Self::parse_figure)
            });
        (transferred, total)
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
        // tunasync-aligned defaults, then excludes, job options and --stats.
        // -vh --no-o --no-g = tunasync's rsync verbosity: per-file transfer
        // lines (-v, human sizes -h) without owner/group noise. The run log
        // carries the same detail tunasync operators are used to.
        apply_tunasync_defaults(&mut cmd, true);
        // --contimeout is daemon-connection only (rsync errors on local
        // paths); tunasync passes it because its upstreams are rsync://.
        if upstream.starts_with("rsync://") {
            cmd.arg("--contimeout=120");
        }
        for pat in &self.exclude {
            cmd.arg(format!("--exclude={pat}"));
        }
        // Absolute delete cap handed to rsync itself (spec §53): rsync
        // aborts with exit 25 when it would delete more, before touching
        // the mirror. The ratio/size checks still run in the engine.
        if let Some(max) = ctx.job.safety.max_delete_files {
            cmd.arg(format!("--max-delete={max}"));
        }
        apply_rsync_proxy_env(&mut cmd, &ctx.proxy_env);
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
            cmd.arg(rewrite_worker_local_path(opt));
        }
        cmd.arg("--stats");
        cmd.arg(&source).arg(&dest);
        let (code, stdout, stderr) = run_rsync(ctx, &mut cmd).await?;
        let (transferred, total_size) = Self::parse_stats(&stdout);
        let result = SyncResult {
            exit_code: Some(code),
            bytes_transferred: transferred,
            size_hint: total_size,
            stdout,
            stderr,
            ..Default::default()
        };
        if Self::is_success_exit(code, &ctx.job.success_exit_codes) {
            // Exit 23/24 are tunasync-convention successes, but they mean the
            // transfer was PARTIAL — say so in the run message instead of
            // reporting a clean success.
            let mut result = result;
            if code != 0 {
                result.message = Some(format!(
                    "partial transfer (rsync exit {code}, counted as success per success_exit_codes)"
                ));
            }
            Ok(result)
        } else {
            // The tool's own output belongs in the run log — attach it.
            let out = String::from_utf8_lossy(&result.stdout);
            let err = String::from_utf8_lossy(&result.stderr);
            let mut detail = err.trim().to_string();
            if detail.is_empty() {
                detail = out.trim().to_string();
            } else if !out.trim().is_empty() {
                detail.push_str(" | stdout: ");
                detail.push_str(out.trim());
            }
            Err(ProviderError::Other(format!(
                "rsync exited with {code}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {}", detail.chars().take(8000).collect::<String>())
                }
            )))
        }
    }
}

/// Shared tail: spawn rsync, drain pipes concurrently, wait, return
/// (exit code, stdout, stderr). Cancellation kills the whole group.
async fn run_rsync(
    ctx: &SyncContext,
    cmd: &mut Command,
) -> Result<(i32, Vec<u8>, Vec<u8>), ProviderError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, guard) =
        spawn_group(cmd, ctx).map_err(|e| ProviderError::Spawn(format!("rsync: {e}")))?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    // Both pipes are drained CONCURRENTLY (sequential reads deadlock on a
    // full pipe buffer); memory keeps only the tail, the run log has it all.
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
    Ok((status.code().unwrap_or(-1), stdout, stderr))
}

/// tunasync two-stage-rsync stage-1 profiles (subset published fast before
/// the full pass). Ref: tuna/tunasync worker/two_stage_rsync_provider.go.
const STAGE1_PROFILES: &[(&str, &[&str])] = &[
    (
        "debian",
        &[
            "--include=*.diff/",
            "--include=by-hash/",
            "--exclude=*.diff/Index",
            "--exclude=Contents*",
            "--exclude=Packages*",
            "--exclude=Sources*",
            "--exclude=Release*",
            "--exclude=InRelease",
            "--exclude=i18n/*",
            "--exclude=dep11/*",
            "--exclude=installer-*/current",
            "--exclude=ls-lR*",
        ],
    ),
    (
        "debian-oldstyle",
        &[
            "--exclude=Packages*",
            "--exclude=Sources*",
            "--exclude=Release*",
            "--exclude=InRelease",
            "--exclude=i18n/*",
            "--exclude=ls-lR*",
            "--exclude=dep11/*",
        ],
    ),
];

/// Two-stage rsync (tunasync convention): stage 1 syncs a small publishable
/// subset using the profile filters (must exit 0), stage 2 syncs the full
/// mirror (success_exit_codes apply). A stage-1 failure aborts the run.
pub struct TwoStageRsyncProvider {
    pub options: Vec<String>,
    pub exclude: Vec<String>,
    pub stage1_profile: String,
}

impl TwoStageRsyncProvider {
    pub async fn sync(&self, ctx: &SyncContext) -> Result<SyncResult, ProviderError> {
        let upstream = ctx.upstream.as_deref().ok_or_else(|| {
            ProviderError::Config("two-stage-rsync requires `upstream`".to_string())
        })?;
        let profile: &[&str] = STAGE1_PROFILES
            .iter()
            .find(|(name, _)| *name == self.stage1_profile)
            .map(|(_, f)| *f)
            .ok_or_else(|| {
                ProviderError::Config(format!(
                    "unknown stage1_profile `{}`: expected debian|debian-oldstyle",
                    self.stage1_profile
                ))
            })?;
        let mut source = upstream.to_string();
        if !source.ends_with('/') {
            source.push('/');
        }
        let dest = ctx
            .storage
            .to_str()
            .ok_or_else(|| ProviderError::Config("storage path is not UTF-8".to_string()))?
            .to_string()
            + "/";

        // Shared tail: connection timeout (daemon upstreams), excludes,
        // proxy env, egress, family, stats, source and dest. Excludes apply
        // to BOTH stages (excluded data must never enter the mirror); the
        // delete cap stays stage-2-only because stage 1 has no --delete.
        let tail = |cmd: &mut Command, with_delete_cap: bool| {
            if upstream.starts_with("rsync://") {
                cmd.arg("--contimeout=120");
            }
            for pat in &self.exclude {
                cmd.arg(format!("--exclude={pat}"));
            }
            if with_delete_cap {
                if let Some(max) = ctx.job.safety.max_delete_files {
                    cmd.arg(format!("--max-delete={max}"));
                }
            }
            apply_rsync_proxy_env(cmd, &ctx.proxy_env);
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
            cmd.arg("--stats");
            cmd.arg(&source).arg(&dest);
        };

        // Stage 1: the publishable subset (tunasync stage1Options + profile).
        let mut cmd = Command::new("rsync");
        // -vh = per-file transfer lines, same detail as the single provider.
        apply_tunasync_defaults(&mut cmd, false);
        for f in profile {
            cmd.arg(f);
        }
        tail(&mut cmd, false);
        let (code, _out, err) = run_rsync(ctx, &mut cmd).await?;
        if code != 0 {
            let detail = String::from_utf8_lossy(&err).trim().to_string();
            return Err(ProviderError::Other(format!(
                "stage 1 rsync exited with {code}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {}", detail.chars().take(8000).collect::<String>())
                }
            )));
        }

        // Stage 2: the full mirror (tunasync stage2Options + job options).
        let mut cmd = Command::new("rsync");
        apply_tunasync_defaults(&mut cmd, true);
        tail(&mut cmd, true);
        // Options come after excludes: a user --include can still pull a
        // path back in (same order as the single rsync provider).
        for opt in &self.options {
            cmd.arg(rewrite_worker_local_path(opt));
        }
        let (code, stdout, stderr) = run_rsync(ctx, &mut cmd).await?;
        let (transferred, total_size) = RsyncProvider::parse_stats(&stdout);
        let result = SyncResult {
            exit_code: Some(code),
            bytes_transferred: transferred,
            size_hint: total_size,
            stdout,
            stderr,
            ..Default::default()
        };
        if RsyncProvider::is_success_exit(code, &ctx.job.success_exit_codes) {
            let mut result = result;
            if code != 0 {
                result.message = Some(format!(
                    "partial transfer (rsync exit {code}, counted as success per success_exit_codes)"
                ));
            }
            Ok(result)
        } else {
            let out = String::from_utf8_lossy(&result.stdout);
            let err = String::from_utf8_lossy(&result.stderr);
            let mut detail = err.trim().to_string();
            if detail.is_empty() {
                detail = out.trim().to_string();
            } else if !out.trim().is_empty() {
                detail.push_str(" | stdout: ");
                detail.push_str(out.trim());
            }
            Err(ProviderError::Other(format!(
                "rsync exited with {code}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {}", detail.chars().take(8000).collect::<String>())
                }
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_tunasync_defaults, rewrite_worker_local_path, rsync_proxy_hostport};

    #[test]
    fn defaults_match_tunasync_and_protect_staging_dir() {
        let mut cmd = tokio::process::Command::new("rsync");
        apply_tunasync_defaults(&mut cmd, true);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|w| w == ["--filter", "risk .~tmp~/"]));
        assert!(args.windows(2).any(|w| w == ["--exclude", ".~tmp~/"]));
        assert!(args.iter().any(|arg| arg == "--delete-after"));
        assert!(!args.iter().any(|arg| arg == "--delete-delay"));
        assert!(args.iter().any(|arg| arg == "--timeout=120"));
    }

    #[test]
    fn rsync_proxy_hostport_strips_scheme_and_userinfo() {
        assert_eq!(
            rsync_proxy_hostport("http://192.0.2.10:14000"),
            "192.0.2.10:14000"
        );
        assert_eq!(
            rsync_proxy_hostport("http://synora:pass@192.0.2.10:14000"),
            "192.0.2.10:14000"
        );
        assert_eq!(rsync_proxy_hostport("172.17.0.1:5354"), "172.17.0.1:5354");
    }

    #[test]
    fn rewrite_worker_local_path_keeps_existing_file() {
        let dir = std::env::temp_dir().join(format!("synora-rsync-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gxde");
        std::fs::write(&file, b"secret\n").unwrap();
        let opt = format!("--password-file={}", file.display());
        assert_eq!(rewrite_worker_local_path(&opt), opt);
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn rewrite_worker_local_path_passthrough_when_missing() {
        assert_eq!(
            rewrite_worker_local_path("--password-file=/no/such/synora-password"),
            "--password-file=/no/such/synora-password"
        );
        assert_eq!(
            rewrite_worker_local_path("--delete-excluded"),
            "--delete-excluded"
        );
    }
}
