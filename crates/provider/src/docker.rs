//! Docker provider (spec §18/§75–§77): `docker run` subprocess for now —
//! exit code + stdout/stderr + volumes cover P0/P1. bollard (Docker API) is
//! the later upgrade when lifecycle events/resource limits are needed
//! (spec §75 prefers the API; flagged tradeoff).

use crate::{
    cancelled_after_wait, kill_group, spawn_group, ProviderError, SyncContext, SyncResult,
};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

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
pub fn docker_exec_args(command: &[String]) -> Vec<String> {
    match command {
        [] => Vec::new(),
        [single] if needs_shell(single) => {
            vec!["/bin/sh".into(), "-c".into(), single.clone()]
        }
        args => args.to_vec(),
    }
}

/// `docker run` suffix: docker-init as PID 1 (reaps git/repo zombies) and
/// `--entrypoint` so a leftover image ENTRYPOINT cannot wrap the job.
pub fn image_command_args(image: &str, command: &[String]) -> Vec<String> {
    let mut out = vec!["--init".to_string()];
    match command.split_first() {
        Some((entry, rest)) => {
            out.push("--entrypoint".into());
            out.push(entry.clone());
            out.push(image.to_string());
            out.extend(rest.iter().cloned());
        }
        None => out.push(image.to_string()),
    }
    out
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

/// Worker `[worker] scripts_image`. Empty/None means run git/script natively.
pub fn scripts_image_name(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// Shared `docker run` description for the docker provider and for git/script
/// jobs that execute inside `synora-scripts`.
pub struct DockerRunSpec {
    pub image: String,
    pub command: Vec<String>,
    pub extra_env: Vec<String>,
    pub extra_volumes: Vec<String>,
    pub keep_container: bool,
    pub network: Option<String>,
}

/// Shared `docker stats` sample for `synora-job-*` containers.
///
/// Bound the CLI wait and kill the child on timeout/cancel. A hung
/// `docker stats` used to stall the job task, stop draining pipes, and
/// freeze the container on a full stdout pipe.
#[derive(Debug, Clone, Copy)]
pub struct ContainerStats {
    pub memory_bytes: u64,
    pub cpu_percent: f64,
    /// Cumulative rx+tx from `NetIO`, when Docker reports it.
    pub net_bytes: Option<u64>,
    /// Cumulative read+write from `BlockIO`, when Docker reports it.
    pub block_bytes: Option<u64>,
}

struct StatsCache {
    at: Instant,
    by_name: HashMap<String, ContainerStats>,
}

static STATS_CACHE: OnceLock<Mutex<StatsCache>> = OnceLock::new();

fn stats_cache() -> &'static Mutex<StatsCache> {
    STATS_CACHE.get_or_init(|| {
        Mutex::new(StatsCache {
            at: Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(Instant::now),
            by_name: HashMap::new(),
        })
    })
}

/// One shared `docker stats --no-stream` for every `synora-job-*` container.
/// Per-job CLI processes were the worker CPU spike and the docker zombies.
pub async fn container_stats(name: &str) -> Option<ContainerStats> {
    let mut cache = stats_cache().lock().await;
    if cache.at.elapsed() >= Duration::from_secs(2) {
        cache.by_name = collect_container_stats().await;
        cache.at = Instant::now();
    }
    cache.by_name.get(name).copied()
}

async fn collect_container_stats() -> HashMap<String, ContainerStats> {
    let mut cmd = Command::new("docker");
    cmd.args([
        "stats",
        "--no-stream",
        "--format",
        "{{.Name}}\t{{.MemUsage}}\t{{.CPUPerc}}\t{{.NetIO}}\t{{.BlockIO}}",
    ])
    .kill_on_drop(true)
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    let mut stdout = match child.stdout.take() {
        Some(pipe) => pipe,
        None => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return HashMap::new();
        }
    };
    let mut buf = Vec::new();
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(6)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            HashMap::new()
        }
        n = stdout.read_to_end(&mut buf) => {
            let _ = n;
            let _ = child.wait().await;
            parse_stats_table(&buf)
        }
    }
}

fn parse_stats_table(bytes: &[u8]) -> HashMap<String, ContainerStats> {
    let mut out = HashMap::new();
    for line in String::from_utf8_lossy(bytes).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, stats)) = parse_stats_line(line) else {
            continue;
        };
        if name.starts_with("synora-job-") {
            out.insert(name, stats);
        }
    }
    out
}

fn parse_stats_line(line: &str) -> Option<(String, ContainerStats)> {
    let mut parts = line.split('\t');
    let name = parts.next()?.trim();
    if name.is_empty() {
        return None;
    }
    let mem_s = parts.next().unwrap_or("");
    let cpu_s = parts.next().unwrap_or("");
    let net_s = parts.next();
    let block_s = parts.next();
    let mem = parse_docker_mem(mem_s.split('/').next().unwrap_or(mem_s).trim())?;
    let cpu = cpu_s.trim().trim_end_matches('%').parse::<f64>().ok()?;
    Some((
        name.to_string(),
        ContainerStats {
            memory_bytes: mem,
            cpu_percent: cpu,
            net_bytes: net_s.and_then(parse_docker_netio),
            block_bytes: block_s.and_then(parse_docker_netio),
        },
    ))
}

fn parse_docker_netio(s: &str) -> Option<u64> {
    // Docker prints `rx / tx`, e.g. `1.2GB / 3.4MB`.
    let (rx, tx) = s.split_once('/')?;
    let rx = parse_docker_mem(rx.trim())?;
    let tx = parse_docker_mem(tx.trim()).unwrap_or(0);
    Some(rx.saturating_add(tx))
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
        run_named_container(
            DockerRunSpec {
                image: self.image.clone(),
                command: docker_exec_args(&self.command),
                extra_env: self.env.clone(),
                extra_volumes: self.volumes.clone(),
                keep_container: self.keep_container,
                network: self.network.clone(),
            },
            ctx,
        )
        .await
    }
}

fn should_bind_host_storage_path(host_storage: &str) -> bool {
    let path = std::path::Path::new(host_storage);
    host_storage != "/data" && !path.starts_with("/data")
}

pub async fn run_named_container(
    spec: DockerRunSpec,
    ctx: &SyncContext,
) -> Result<SyncResult, ProviderError> {
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
    if let Some(net) = spec
        .network
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        cmd.arg("--network").arg(net);
    }
    if !spec.keep_container {
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
    let mounts_data = spec.extra_volumes.iter().any(|v| {
        v.split(':')
            .nth(1)
            .map(|dst| dst == "/data")
            .unwrap_or(false)
    });
    if !mounts_data {
        cmd.arg("-v").arg(format!("{host_storage}:/data"));
    }
    // Scripts that still refer to the host path expect a bind at that
    // path. Skip when it sits under /data: `/data/kali:/data` plus
    // `/data/kali:/data/kali` nests the whole mirror at ./kali and
    // rsync --delete tries to remove it.
    if should_bind_host_storage_path(host_storage) {
        cmd.arg("-v").arg(format!("{host_storage}:{host_storage}"));
    }
    for v in &spec.extra_volumes {
        cmd.arg("-v").arg(v);
    }
    for e in &spec.extra_env {
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
    cmd.arg("-e").arg(format!("SYNORA_JOB={}", ctx.job_name));
    if let Some(up) = &ctx.upstream {
        cmd.arg("-e").arg(format!("SYNORA_UPSTREAM={up}"));
        if let Some(rest) = up.strip_prefix("rsync://") {
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            cmd.arg("-e").arg(format!("RSYNC_HOST={host}"));
            cmd.arg("-e").arg(format!("RSYNC_PATH=/{path}"));
        }
    }
    let storage_in_container = if mounts_data { host_storage } else { "/data" };
    let workdir = if should_bind_host_storage_path(host_storage) {
        host_storage.to_string()
    } else {
        storage_in_container.to_string()
    };
    cmd.arg("-w").arg(&workdir);
    cmd.arg("-e")
        .arg(format!("SYNORA_STORAGE={storage_in_container}"));
    cmd.arg("-e")
        .arg(format!("SYNORA_LOG_DIR={storage_in_container}/.synora-log"));
    cmd.arg("-e").arg(format!("SYNORA_RUN_ID={}", ctx.run_id));
    if let Some(w) = &ctx.worker {
        cmd.arg("-e").arg(format!("SYNORA_WORKER={w}"));
    }
    if let Some(pr) = &ctx.proxy {
        cmd.arg("-e").arg(format!("SYNORA_PROXY={pr}"));
    }
    if let Some(e) = &ctx.egress {
        cmd.arg("-e").arg(format!("SYNORA_EGRESS={e}"));
    }
    if let Some(addr) = &ctx.egress_address {
        cmd.arg("-e").arg(format!("SYNORA_EGRESS_ADDRESS={addr}"));
    }
    cmd.arg("-e").arg(format!("SYNORA_FAMILY={}", ctx.family));
    if let Some(api) = ctx.manager_url.as_deref().filter(|s| !s.trim().is_empty()) {
        cmd.arg("-e")
            .arg(format!("SYNORA_API={}", rewrite_loopback_proxy(api)));
    }
    // git.sh against an interrupted bare clone (empty HEAD) fails with
    // "not a git repository". Repair before the container starts.
    if spec.command.iter().any(|c| c.contains("git.sh")) {
        let _ = crate::git::prepare_existing_repo(host_storage).await;
    }
    for arg in image_command_args(&spec.image, &spec.command) {
        cmd.arg(arg);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let (mut child, _guard) =
        spawn_group(&mut cmd, ctx).map_err(|e| ProviderError::Spawn(format!("docker run: {e}")))?;
    async fn stop_container(name: &str) {
        let mut rm = Command::new("docker");
        rm.args(["rm", "-f", name])
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = rm.spawn() {
            let _ = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
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
    // Resource samples live in the executor background task. Do not
    // await `docker stats` on this task: a hung CLI call stops pipe
    // draining and the container blocks on a full stdout pipe.
    let (stdout, stderr) = tokio::select! {
        _ = ctx.cancel.cancelled() => {
            kill_group(&mut child).await;
            stop_container(&cname).await;
            return Err(ProviderError::Cancelled);
        }
        r = &mut read_fut => r,
    };
    let status = tokio::select! {
        _ = ctx.cancel.cancelled() => {
            kill_group(&mut child).await;
            stop_container(&cname).await;
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
    let status = crate::parse_script_status(&combined);
    let result = SyncResult {
        exit_code: Some(code),
        stdout,
        stderr,
        size_hint,
        status: status.clone(),
        ..Default::default()
    };
    if let Some(reason) = crate::script_reported_failure(&combined) {
        return Err(ProviderError::Other(format!(
            "script reported failure: {reason}"
        )));
    }
    if let Some(status) = status.as_deref() {
        if status == "success" {
            return Ok(result);
        }
        let mut detail = String::from_utf8_lossy(&result.stderr).trim().to_string();
        if detail.is_empty() {
            detail = String::from_utf8_lossy(&result.stdout).trim().to_string();
        }
        return Err(ProviderError::Other(format!(
            "script reported status {status}: {}",
            detail.chars().take(800).collect::<String>()
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
    fn image_command_overrides_entrypoint() {
        assert_eq!(
            image_command_args(
                "synora-scripts:latest",
                &["/usr/lib/synora/scripts/homebrew.sh".into()]
            ),
            vec![
                "--init",
                "--entrypoint",
                "/usr/lib/synora/scripts/homebrew.sh",
                "synora-scripts:latest",
            ]
        );
        assert_eq!(
            image_command_args(
                "synora-scripts:latest",
                &["/bin/sh".into(), "-c".into(), "git fetch".into()]
            ),
            vec![
                "--init",
                "--entrypoint",
                "/bin/sh",
                "synora-scripts:latest",
                "-c",
                "git fetch",
            ]
        );
        assert_eq!(
            image_command_args("synora-scripts:latest", &[]),
            vec!["--init", "synora-scripts:latest"]
        );
    }

    #[test]
    fn parse_named_docker_stats_line() {
        let (name, stats) = parse_stats_line(
            "synora-job-homebrew\t488.6MiB / 2GiB\t31.1%\t1.0GiB / 512MiB\t0B / 0B",
        )
        .unwrap();
        assert_eq!(name, "synora-job-homebrew");
        assert_eq!(stats.memory_bytes, (488.6 * 1024.0 * 1024.0) as u64);
        assert!((stats.cpu_percent - 31.1).abs() < f64::EPSILON);
        assert_eq!(
            stats.net_bytes,
            Some(1024 * 1024 * 1024 + 512 * 1024 * 1024)
        );
    }

    #[test]
    fn parse_docker_mem_units() {
        assert_eq!(parse_docker_mem("16MiB"), Some(16 * 1024 * 1024));
        assert_eq!(parse_docker_mem("1GiB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_docker_mem("512KiB"), Some(512 * 1024));
        assert_eq!(parse_docker_mem("100"), Some(100));
        assert_eq!(
            parse_docker_netio("1.0GiB / 512MiB"),
            Some(1024 * 1024 * 1024 + 512 * 1024 * 1024)
        );
        assert_eq!(parse_docker_netio("100kB / 0B"), Some(100 * 1024));
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
            rewrite_loopback_proxy("http://192.0.2.10:14000"),
            "http://192.0.2.10:14000"
        );
        assert_eq!(
            rewrite_loopback_proxy("http://127.0.0.1:9290"),
            "http://172.17.0.1:9290"
        );
        assert_eq!(
            rewrite_loopback_proxy("http://192.0.2.10:9290"),
            "http://192.0.2.10:9290"
        );
        assert_eq!(
            rewrite_loopback_proxy("http://localhost:9290"),
            "http://172.17.0.1:9290"
        );
    }

    #[test]
    fn docker_http_connect_sets_http_and_all_proxy() {
        let env = docker_proxy_env(&[
            ("HTTP_PROXY".into(), "http://192.0.2.10:14000".into()),
            ("HTTPS_PROXY".into(), "http://192.0.2.10:14000".into()),
            ("ALL_PROXY".into(), "http://192.0.2.10:14000".into()),
            ("http_proxy".into(), "http://192.0.2.10:14000".into()),
        ]);
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(
            map.get("ALL_PROXY").map(String::as_str),
            Some("http://192.0.2.10:14000")
        );
        assert_eq!(
            map.get("HTTP_PROXY").map(String::as_str),
            Some("http://192.0.2.10:14000")
        );
        assert_eq!(
            map.get("HTTPS_PROXY").map(String::as_str),
            Some("http://192.0.2.10:14000")
        );
        assert_eq!(
            map.get("http_proxy").map(String::as_str),
            Some("http://192.0.2.10:14000")
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
        assert!(
            crate::script_reported_failure("Failed APT repos of http://example: [('a', 'b')]")
                .is_some()
        );
        assert_eq!(
            crate::parse_script_status("SYNORA_STATUS=success"),
            Some("success".into())
        );
    }

    #[test]
    fn host_storage_under_data_is_not_rebound() {
        assert!(!should_bind_host_storage_path("/data"));
        assert!(!should_bind_host_storage_path("/data/kali"));
        assert!(should_bind_host_storage_path("/datas/virtualbox"));
        assert!(should_bind_host_storage_path("/srv/mirror/debian"));
    }

    #[test]
    fn scripts_image_name_skips_blank() {
        assert!(scripts_image_name(None).is_none());
        assert!(scripts_image_name(Some("")).is_none());
        assert!(scripts_image_name(Some("   ")).is_none());
        assert_eq!(
            scripts_image_name(Some(" synora-scripts:latest ")),
            Some("synora-scripts:latest")
        );
    }

    #[test]
    fn docker_keeps_socks_as_all_proxy_only() {
        let env = docker_proxy_env(&[
            ("HTTP_PROXY".into(), "socks5h://192.0.2.10:14001".into()),
            ("HTTPS_PROXY".into(), "socks5h://192.0.2.10:14001".into()),
            ("ALL_PROXY".into(), "socks5h://192.0.2.10:14001".into()),
        ]);
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert!(!map.contains_key("HTTP_PROXY"));
        assert!(!map.contains_key("HTTPS_PROXY"));
        assert_eq!(
            map.get("ALL_PROXY").map(String::as_str),
            Some("socks5h://192.0.2.10:14001")
        );
        assert_eq!(
            map.get("all_proxy").map(String::as_str),
            Some("socks5h://192.0.2.10:14001")
        );
    }
}
