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
    pub network: Option<String>,
    pub command: Vec<String>,
}

/// tunasync `command` is a shell string stuffed into a one-element argv
/// (`docker_command = ["timeout 18h python3 …"]`). Docker would otherwise
/// look for a binary whose *name* is the whole string (exit 127).
pub(crate) fn docker_exec_args(command: &[String]) -> Vec<String> {
    match command {
        [] => Vec::new(),
        [single] if needs_shell(single) => {
            vec!["/bin/sh".into(), "-c".into(), single.clone()]
        }
        args => args.to_vec(),
    }
}

fn needs_shell(s: &str) -> bool {
    s.chars().any(|c| {
        c.is_whitespace()
            || matches!(
                c,
                '|' | '&'
                    | ';'
                    | '<'
                    | '>'
                    | '*'
                    | '?'
                    | '['
                    | ']'
                    | '$'
                    | '`'
                    | '\''
                    | '"'
                    | '('
                    | ')'
            )
    })
}

/// Container proxy env, matching tunasync `worker.conf` `[mirrors.env]`:
/// HTTP CONNECT gets ALL_PROXY + HTTP(S)_PROXY so yum/dnf/curl/git work;
/// SOCKS stays on ALL_PROXY only. Never emit empty values (reqwest treats
/// empty HTTPS_PROXY as "no proxy" and then ignores ALL_PROXY).
pub(crate) fn docker_proxy_env(proxy_env: &[(String, String)]) -> Vec<(String, String)> {
    let mut url = None;
    for (k, v) in proxy_env {
        if k.eq_ignore_ascii_case("all_proxy") && !v.trim().is_empty() {
            url = Some(rewrite_loopback_proxy(v));
            break;
        }
    }
    if url.is_none() {
        for (k, v) in proxy_env {
            if (k.eq_ignore_ascii_case("http_proxy") || k.eq_ignore_ascii_case("https_proxy"))
                && !v.trim().is_empty()
            {
                url = Some(rewrite_loopback_proxy(v));
                break;
            }
        }
    }
    let Some(url) = url.filter(|u| !u.trim().is_empty()) else {
        return Vec::new();
    };
    let mut out = vec![
        ("ALL_PROXY".into(), url.clone()),
        ("all_proxy".into(), url.clone()),
    ];
    if !url.to_ascii_lowercase().contains("socks") {
        out.extend([
            ("HTTP_PROXY".into(), url.clone()),
            ("HTTPS_PROXY".into(), url.clone()),
            ("http_proxy".into(), url.clone()),
            ("https_proxy".into(), url.clone()),
        ]);
    }
    out
}

/// Loopback HTTP CONNECT URLs are the manager/worker host, not the
/// container. Rewrite those to the docker bridge gateway. SOCKS URLs are
/// left unchanged: rewriting `socks5h://127.0.0.1:40000` to
/// `172.17.0.1:40000` points the container at docker0, not the manager
/// expose. Production cf-warp is a LAN HTTP CONNECT address and is left
/// unchanged.
fn rewrite_loopback_proxy(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if lower.contains("socks5") {
        return value.to_string();
    }
    const GW: &str = "172.17.0.1";
    value
        .replace("@127.0.0.1", &format!("@{GW}"))
        .replace("://127.0.0.1", &format!("://{GW}"))
        .replace("@localhost", &format!("@{GW}"))
        .replace("://localhost", &format!("://{GW}"))
        .replace("@[::1]", &format!("@{GW}"))
        .replace("://[::1]", &format!("://{GW}"))
}

/// One-shot `docker stats` for a named container: (memory_bytes, cpu_percent).
pub async fn container_stats(name: &str) -> Option<(u64, f64)> {
    let out = tokio::process::Command::new("docker")
        .args([
            "stats",
            "--no-stream",
            "--format",
            "{{.MemUsage}}\t{{.CPUPerc}}",
            name,
        ])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let (mem_s, cpu_s) = line.split_once('\t').or_else(|| line.split_once(' '))?;
    let mem = parse_docker_mem(mem_s.split('/').next().unwrap_or(mem_s).trim())?;
    let cpu = cpu_s.trim().trim_end_matches('%').parse::<f64>().ok()?;
    Some((mem, cpu))
}

fn parse_docker_mem(s: &str) -> Option<u64> {
    let s = s.trim().replace(',', "");
    let (num, unit) = s.split_at(s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len()));
    let n: f64 = num.trim().parse().ok()?;
    let mul = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" | "kib" => 1024.0,
        "mb" | "mib" => 1024.0 * 1024.0,
        "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * mul) as u64)
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
        if let Some(net) = self
            .network
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            cmd.arg("--network").arg(net);
        }
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
        if ctx.job.proxy.is_some() && ctx.proxy_env.is_empty() {
            tracing::warn!(
                "job `{}`: proxy `{}` selected but no proxy env was provided — container will go direct",
                ctx.job_name,
                ctx.job.proxy.as_deref().unwrap_or("")
            );
        }
        // Inject manager-assigned proxy the same way tunasync did:
        // HTTP CONNECT → ALL_PROXY + HTTP(S)_PROXY; SOCKS → ALL_PROXY only.
        for (k, v) in docker_proxy_env(&ctx.proxy_env) {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        cmd.arg("-e").arg("PYTHONUNBUFFERED=1");
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
        // git.sh against an interrupted bare clone (empty HEAD) fails with
        // "not a git repository". Repair before the container starts.
        if self.command.iter().any(|c| c.contains("git.sh")) {
            let _ = crate::git::prepare_existing_repo(host_storage).await;
        }
        cmd.arg(&self.image);
        for arg in docker_exec_args(&self.command) {
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
        let usage = ctx.usage.clone();
        let stats_name = cname.clone();
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut peak_mem: u64 = 0;
        let mut cpu_acc = 0.0f64;
        loop {
            tokio::select! {
                _ = ctx.cancel.cancelled() => {
                    kill_group(&mut child).await;
                    return Err(ProviderError::Cancelled);
                }
                _ = ticker.tick() => {
                    if let Some((mem, pct)) = container_stats(&stats_name).await {
                        peak_mem = peak_mem.max(mem);
                        cpu_acc += (pct / 100.0) * 2.0;
                        if let Some(u) = &usage {
                            u.lock().unwrap().record(peak_mem, cpu_acc, Some(pct));
                        }
                    }
                }
                r = &mut read_fut => {
                    (stdout, stderr) = r;
                    break;
                }
            }
        }
        if let Some(u) = &usage {
            if peak_mem > 0 {
                u.lock().unwrap().record(peak_mem, cpu_acc, None);
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
        let combined = format!(
            "{}
{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        let size_hint = crate::parse_script_size(&combined);
        let result = SyncResult {
            exit_code: Some(code),
            stdout,
            stderr,
            size_hint,
            ..Default::default()
        };
        if let Some(reason) = crate::script_reported_failure(&combined) {
            return Err(ProviderError::Other(format!(
                "script reported failure: {reason}"
            )));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_string_becomes_sh_c() {
        let args = docker_exec_args(&[
            "timeout 18h python3 /home/tunasync-scripts/docker-ce.py --workers 10".into(),
        ]);
        assert_eq!(args[0], "/bin/sh");
        assert_eq!(args[1], "-c");
        assert!(args[2].contains("docker-ce.py"));
    }

    #[test]
    fn argv_passthrough() {
        let args = docker_exec_args(&["/home/tunasync-scripts/aosp.sh".into()]);
        assert_eq!(args, vec!["/home/tunasync-scripts/aosp.sh"]);
    }

    #[test]
    fn parse_docker_mem_units() {
        assert_eq!(parse_docker_mem("16MiB"), Some(16 * 1024 * 1024));
        assert_eq!(parse_docker_mem("1GiB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_docker_mem("512KiB"), Some(512 * 1024));
        assert_eq!(parse_docker_mem("100"), Some(100));
    }

    #[test]
    fn rewrite_auth_and_plain_loopback() {
        assert_eq!(
            rewrite_loopback_proxy("http://127.0.0.1:5354"),
            "http://172.17.0.1:5354"
        );
        assert_eq!(
            rewrite_loopback_proxy("http://127.0.0.1:14000"),
            "http://172.17.0.1:14000"
        );
        assert_eq!(
            rewrite_loopback_proxy("socks5h://synora:pass@127.0.0.1:40000"),
            "socks5h://synora:pass@127.0.0.1:40000"
        );
        assert_eq!(
            rewrite_loopback_proxy("http://172.31.33.205:14000"),
            "http://172.31.33.205:14000"
        );
    }

    #[test]
    fn docker_http_connect_sets_http_and_all_proxy() {
        let env = docker_proxy_env(&[
            ("HTTP_PROXY".into(), "http://172.31.33.205:14000".into()),
            ("HTTPS_PROXY".into(), "http://172.31.33.205:14000".into()),
            ("ALL_PROXY".into(), "http://172.31.33.205:14000".into()),
            ("http_proxy".into(), "http://172.31.33.205:14000".into()),
        ]);
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(
            map.get("ALL_PROXY").map(String::as_str),
            Some("http://172.31.33.205:14000")
        );
        assert_eq!(
            map.get("HTTP_PROXY").map(String::as_str),
            Some("http://172.31.33.205:14000")
        );
        assert_eq!(
            map.get("HTTPS_PROXY").map(String::as_str),
            Some("http://172.31.33.205:14000")
        );
        assert_eq!(
            map.get("http_proxy").map(String::as_str),
            Some("http://172.31.33.205:14000")
        );
    }

    #[test]
    fn tunasync_size_patterns() {
        assert_eq!(
            crate::parse_script_size("Total size is 1.6G\n"),
            Some((1.6f64 * 1024.0 * 1024.0 * 1024.0).round() as u64)
        );
        assert_eq!(
            crate::parse_script_size("size-sum: 12G"),
            Some(12 * 1024u64 * 1024 * 1024)
        );
        assert_eq!(crate::parse_script_size("SYNORA_SIZE=42"), Some(42));
        assert!(crate::script_reported_failure("Failed YUM repos: [('rpm', 'x86_64')]").is_some());
    }

    #[test]
    fn docker_keeps_socks_as_all_proxy_only() {
        let env = docker_proxy_env(&[
            ("HTTP_PROXY".into(), "socks5h://172.31.33.205:14001".into()),
            ("HTTPS_PROXY".into(), "socks5h://172.31.33.205:14001".into()),
            ("ALL_PROXY".into(), "socks5h://172.31.33.205:14001".into()),
        ]);
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert!(!map.contains_key("HTTP_PROXY"));
        assert!(!map.contains_key("HTTPS_PROXY"));
        assert_eq!(
            map.get("ALL_PROXY").map(String::as_str),
            Some("socks5h://172.31.33.205:14001")
        );
        assert_eq!(
            map.get("all_proxy").map(String::as_str),
            Some("socks5h://172.31.33.205:14001")
        );
    }
}
