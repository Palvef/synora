//! Network routing layer: proxy selection, proxy groups, health + egress-IP
//! probing, and egress (source bind address / address family) resolution.
//!
//! Config types come from the `config` crate; jobs reference proxies and
//! egresses by name ([`synora_core::job::JobSpec::proxy`] /
//! [`synora_core::job::JobSpec::egress`]). Probes are recorded into shared
//! state so the sync selection methods (`select_proxy` / `select_egress`)
//! can prefer healthy routes without blocking on network I/O. Anything that
//! can fail is folded into a [`ProxyProbe`] — never a panic.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use config::{EgressConfig, EgressGroupConfig, ProxyConfig, ProxyGroupConfig, ProxyKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::process::Command;
use tokio::sync::Semaphore;

/// Default probe URL used for egress-IP detection; override via the
/// `SYNORA_IP_PROBE_URL` environment variable.
const DEFAULT_IP_PROBE_URL: &str = "https://api.ipify.org";
/// Max concurrent proxy probes in [`NetRoute::probe_all`].
const PROBE_CONCURRENCY: usize = 8;
/// Body cap when reading a plain-text probe response.
const MAX_BODY: usize = 4096;
/// Default timeout for egress TCP probes.
const EGRESS_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors from a single probe attempt. Never surfaced to callers — every
/// failure is folded into a [`ProxyProbe`] — but kept typed internally.
#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected proxy response: {0}")]
    BadResponse(String),
    #[error("check command exited with {0}")]
    NonZeroExit(i32),
    #[error("reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),
}

/// Health + probe state of one proxy.
#[derive(Debug, Clone, Default)]
pub struct ProxyProbe {
    /// last probe latency in ms (None = unknown/down)
    pub latency_ms: Option<u64>,
    /// detected egress IP seen by the probe target
    pub egress_ip: Option<String>,
    pub healthy: bool,
    pub last_probe_at: Option<i64>,
}

/// Proxy selection: Direct, or a forward proxy with env vars to inject.
#[derive(Debug, Clone, PartialEq)]
pub enum Selection {
    Direct,
    /// (proxy name, url scheme preserved — http:// or socks5h://, env pairs)
    Forward {
        name: String,
        url: String,
        env: Vec<(String, String)>,
    },
}

/// Resolved address family. `Any` = the child process may use either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Any,
    V4,
    V6,
}

/// Network routing state: configured proxies/egresses plus the last probe
/// results, so sync selection can prefer healthy routes.
pub struct NetRoute {
    proxies: HashMap<String, ProxyConfig>,
    proxy_groups: HashMap<String, ProxyGroupConfig>,
    egresses: HashMap<String, EgressConfig>,
    egress_groups: HashMap<String, EgressGroupConfig>,
    default_proxy: Option<String>,
    /// Shared probe results, written by `probe`/`probe_all`, read by
    /// `select_proxy`. Unprobed proxies are treated as healthy (fail-open).
    probe_states: Mutex<HashMap<String, ProxyProbe>>,
    /// Shared egress health, written by `probe_egress`, read by
    /// `select_egress`. Unprobed egresses are treated as healthy.
    egress_health: Mutex<HashMap<String, bool>>,
    /// Round-robin cursor across both proxy and egress groups.
    rr: AtomicU64,
}

impl NetRoute {
    /// Build from the config crate's typed sections.
    /// Build the router, or None when no proxy/egress config exists
    /// (pure direct mode).
    pub fn build_optional(
        proxies: &std::collections::HashMap<String, config::ProxyConfig>,
        proxy_groups: &std::collections::HashMap<String, config::ProxyGroupConfig>,
        egresses: &[config::EgressConfig],
        egress_groups: &std::collections::HashMap<String, config::EgressGroupConfig>,
        default_proxy: Option<&str>,
    ) -> Option<std::sync::Arc<NetRoute>> {
        if proxies.is_empty() && egresses.is_empty() {
            return None;
        }
        Some(std::sync::Arc::new(NetRoute::new(
            proxies,
            proxy_groups,
            egresses,
            egress_groups,
            default_proxy,
        )))
    }

    /// Configured proxies (reload-updated; the /proxies endpoint reads this
    /// so TUI-added proxies show up after a reload).
    pub fn proxy_configs(&self) -> &HashMap<String, ProxyConfig> {
        &self.proxies
    }

    pub fn new(
        proxies: &HashMap<String, config::ProxyConfig>,
        proxy_groups: &HashMap<String, config::ProxyGroupConfig>,
        egresses: &[config::EgressConfig],
        egress_groups: &HashMap<String, config::EgressGroupConfig>,
        default_proxy: Option<&str>,
    ) -> Self {
        NetRoute {
            proxies: proxies.clone(),
            proxy_groups: proxy_groups.clone(),
            egresses: egresses
                .iter()
                .map(|e| (e.name.clone(), e.clone()))
                .collect(),
            egress_groups: egress_groups.clone(),
            default_proxy: default_proxy.map(str::to_string),
            probe_states: Mutex::new(HashMap::new()),
            egress_health: Mutex::new(HashMap::new()),
            rr: AtomicU64::new(0),
        }
    }

    /// Resolve a job's `proxy` (job-level name, or None → default_proxy).
    /// Group strategies: fixed (first), failover (first healthy), round-robin,
    /// random. Unhealthy proxies are skipped (except `fixed`, which returns
    /// it anyway — operator intent). Unprobed proxies count as healthy;
    /// command proxies can't route traffic and are skipped in groups.
    pub fn select_proxy(&self, job_proxy: Option<&str>) -> Selection {
        let name = job_proxy.or(self.default_proxy.as_deref());
        let Some(name) = name else {
            return Selection::Direct;
        };
        if let Some(group) = self.proxy_groups.get(name) {
            return match self.pick_from_group(group) {
                Some(picked) => match self.proxies.get(&picked) {
                    Some(p) => self.selection_for_name(&picked, p),
                    None => Selection::Direct,
                },
                None => Selection::Direct,
            };
        }
        match self.proxies.get(name) {
            Some(p) => self.selection_for_name(name, p),
            None => {
                tracing::warn!(
                    proxy = name,
                    "select_proxy: unknown proxy or group, using direct"
                );
                Selection::Direct
            }
        }
    }

    /// Resolve a job's `egress` (name or group) → source bind address.
    pub fn select_egress(&self, job_egress: Option<&str>) -> Option<IpAddr> {
        let name = job_egress?;
        if let Some(group) = self.egress_groups.get(name) {
            return self.pick_egress_from_group(group);
        }
        self.egresses.get(name).map(|e| e.address)
    }

    /// Resolve a job's family (ipv4|ipv6|any) → concrete family, narrowed by
    /// the selected egress address when both are set. When family and egress
    /// conflict (e.g. family=ipv4 with an IPv6 egress) the egress wins — the
    /// bind address is the hard constraint that can't be relaxed.
    pub fn select_family(&self, family: &str, egress: Option<IpAddr>) -> Family {
        let requested = match family {
            "ipv4" => Family::V4,
            "ipv6" => Family::V6,
            _ => Family::Any,
        };
        match (requested, egress) {
            (Family::Any, Some(ip)) if ip.is_ipv4() => Family::V4,
            (Family::Any, Some(_)) => Family::V6,
            (Family::V4, Some(ip)) if ip.is_ipv6() => Family::V6,
            (Family::V6, Some(ip)) if ip.is_ipv4() => Family::V4,
            _ => requested,
        }
    }

    /// Probe ONE proxy: run its healthcheck (GET through the proxy for
    /// Forward proxies; run the check command for Command proxies; Direct is
    /// always healthy) and detect the egress IP. Detection: GET a probe URL
    /// (https://api.ipify.org by default, override via env SYNORA_IP_PROBE_URL)
    /// through the proxy and parse the plain-text IP. Record latency.
    pub async fn probe(&self, name: &str) -> ProxyProbe {
        let result = match self.proxies.get(name) {
            Some(p) => self.probe_one(name, p).await,
            None => {
                tracing::warn!(proxy = name, "probe: unknown proxy");
                ProxyProbe::default()
            }
        };
        if let Ok(mut states) = self.probe_states.lock() {
            states.insert(name.to_string(), result.clone());
        }
        result
    }

    /// Probe ALL proxies concurrently (spawn per proxy, bounded). Returns
    /// name → probe. Use the proxy's configured timeout.
    pub async fn probe_all(&self) -> HashMap<String, ProxyProbe> {
        let names: Vec<String> = self.proxies.keys().cloned().collect();
        let sem = Arc::new(Semaphore::new(PROBE_CONCURRENCY));
        let me = Arc::new(self.clone());
        let mut set = tokio::task::JoinSet::new();
        for name in names {
            let sem = Arc::clone(&sem);
            let me = Arc::clone(&me);
            set.spawn(async move {
                let _permit = sem.acquire().await.ok();
                let name = name.clone();
                (name.clone(), me.probe(&name).await)
            });
        }
        let mut out = HashMap::new();
        while let Some(joined) = set.join_next().await {
            if let Ok((name, probe)) = joined {
                out.insert(name, probe);
            }
        }
        out
    }

    /// Egress health: TCP-connect to the egress's probe target ("host:port")
    /// bound to its source address; record healthy/unhealthy. An egress
    /// without a probe target can't be verified and counts as healthy.
    pub async fn probe_egress(&self, name: &str) -> bool {
        let Some(eg) = self.egresses.get(name) else {
            return false;
        };
        let healthy = match eg.probe.as_deref() {
            None => {
                tracing::debug!(
                    egress = name,
                    "probe_egress: no probe target, assuming healthy"
                );
                true
            }
            Some(target) => {
                match tokio::time::timeout(EGRESS_PROBE_TIMEOUT, connect_bound(eg.address, target))
                    .await
                {
                    Ok(Ok(())) => true,
                    Ok(Err(e)) => {
                        tracing::warn!(egress = name, error = %e, "probe_egress: connect failed");
                        false
                    }
                    Err(_) => {
                        tracing::warn!(egress = name, "probe_egress: connect timed out");
                        false
                    }
                }
            }
        };
        if let Ok(mut health) = self.egress_health.lock() {
            health.insert(name.to_string(), healthy);
        }
        healthy
    }

    // --- internals --------------------------------------------------------

    async fn probe_one(&self, name: &str, p: &ProxyConfig) -> ProxyProbe {
        match &p.kind {
            ProxyKind::Direct => ProxyProbe {
                healthy: true,
                last_probe_at: now_ts(),
                ..Default::default()
            },
            ProxyKind::Command { check, env } => probe_command(name, check, env, p.timeout).await,
            ProxyKind::Forward { url, .. } => {
                probe_forward(name, url, p.healthcheck.as_deref(), p.timeout).await
            }
        }
    }

    fn selection_for_name(&self, name: &str, p: &ProxyConfig) -> Selection {
        match &p.kind {
            ProxyKind::Direct => Selection::Direct,
            ProxyKind::Forward { url, env } => Selection::Forward {
                name: name.to_string(),
                url: url.clone(),
                env: env.clone(),
            },
            ProxyKind::Command { .. } => {
                tracing::warn!(
                    proxy = name,
                    "select_proxy: command proxy cannot route traffic, using direct"
                );
                Selection::Direct
            }
        }
    }

    /// Pick one group member per the strategy; command proxies are skipped
    /// (they can't route traffic). `fixed` returns the first member even if
    /// unhealthy — operator intent.
    fn pick_from_group(&self, group: &ProxyGroupConfig) -> Option<String> {
        let members: Vec<String> = group
            .proxies
            .iter()
            .filter(|n| {
                matches!(
                    self.proxies.get(*n).map(|p| &p.kind),
                    Some(ProxyKind::Forward { .. }) | Some(ProxyKind::Direct)
                )
            })
            .cloned()
            .collect();
        if members.is_empty() {
            return None;
        }
        let healthy = |n: &str| {
            self.probe_states
                .lock()
                .map(|s| s.get(n).map(|p| p.healthy).unwrap_or(true))
                .unwrap_or(true)
        };
        match group.strategy.as_str() {
            "fixed" => Some(members[0].clone()),
            "failover" => members.iter().find(|n| healthy(n)).cloned(),
            "round-robin" => {
                let start = (self.rr.fetch_add(1, Ordering::Relaxed) as usize) % members.len();
                (0..members.len())
                    .map(|i| (start + i) % members.len())
                    .find(|&i| healthy(&members[i]))
                    .map(|i| members[i].clone())
            }
            "random" => {
                let start = (lcg() as usize) % members.len();
                (0..members.len())
                    .map(|i| (start + i) % members.len())
                    .find(|&i| healthy(&members[i]))
                    .map(|i| members[i].clone())
            }
            other => {
                tracing::warn!(
                    strategy = other,
                    "select_proxy: unknown group strategy, treating as fixed"
                );
                Some(members[0].clone())
            }
        }
    }

    fn pick_egress_from_group(&self, group: &EgressGroupConfig) -> Option<IpAddr> {
        let members: Vec<&str> = group
            .addresses
            .iter()
            .map(String::as_str)
            .filter(|n| self.egresses.contains_key(*n))
            .collect();
        if members.is_empty() {
            return None;
        }
        let healthy = |n: &str| {
            self.egress_health
                .lock()
                .map(|s| s.get(n).copied().unwrap_or(true))
                .unwrap_or(true)
        };
        let pick = |i: usize| self.egresses.get(members[i]).map(|e| e.address);
        match group.strategy.as_str() {
            "fixed" => pick(0),
            "failover" => members.iter().position(|n| healthy(n)).and_then(pick),
            "round-robin" => {
                let start = (self.rr.fetch_add(1, Ordering::Relaxed) as usize) % members.len();
                (0..members.len())
                    .map(|i| members[(start + i) % members.len()])
                    .position(&healthy)
                    .map(|i| (start + i) % members.len())
                    .and_then(pick)
            }
            "random" => {
                let start = (lcg() as usize) % members.len();
                (0..members.len())
                    .map(|i| members[(start + i) % members.len()])
                    .position(healthy)
                    .map(|i| (start + i) % members.len())
                    .and_then(pick)
            }
            other => {
                tracing::warn!(
                    strategy = other,
                    "select_egress: unknown group strategy, treating as fixed"
                );
                pick(0)
            }
        }
    }
}

// `probe_all` clones the route so spawned tasks can own it. Lock-poisoned
// maps degrade to empty, which selection already treats as "unknown =
// healthy".
impl Clone for NetRoute {
    fn clone(&self) -> Self {
        NetRoute {
            proxies: self.proxies.clone(),
            proxy_groups: self.proxy_groups.clone(),
            egresses: self.egresses.clone(),
            egress_groups: self.egress_groups.clone(),
            default_proxy: self.default_proxy.clone(),
            probe_states: Mutex::new(
                self.probe_states
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or_default(),
            ),
            egress_health: Mutex::new(
                self.egress_health
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or_default(),
            ),
            rr: AtomicU64::new(self.rr.load(Ordering::Relaxed)),
        }
    }
}

/// Build the proxy env for a child process: HTTP_PROXY/HTTPS_PROXY/ALL_PROXY
/// (+ NO_PROXY passthrough) from a Selection. `Direct` yields no entries.
pub fn proxy_env(selection: &Selection) -> Vec<(String, String)> {
    let Selection::Forward { url, env, .. } = selection else {
        return Vec::new();
    };
    let mut out = vec![
        ("HTTP_PROXY".to_string(), url.clone()),
        ("HTTPS_PROXY".to_string(), url.clone()),
        ("ALL_PROXY".to_string(), url.clone()),
    ];
    for key in ["NO_PROXY", "no_proxy"] {
        if let Ok(v) = std::env::var(key) {
            out.push((key.to_string(), v));
        }
    }
    out.extend(env.iter().cloned());
    out
}

/// Probe a Forward proxy: GET the probe URL through it. http:// goes via
/// reqwest's proxy support; socks5h:// needs a manual SOCKS5 CONNECT + plain
/// HTTP GET (reqwest's socks feature is not enabled). Returns the response
/// body on success.
async fn probe_forward(
    name: &str,
    url: &str,
    healthcheck: Option<&str>,
    timeout_secs: u64,
) -> ProxyProbe {
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let probe_url = healthcheck.map(str::to_string).unwrap_or_else(ip_probe_url);
    let started = Instant::now();
    let body = if url.starts_with("socks5h://") {
        match tokio::time::timeout(timeout, manual_socks5_get(url, &probe_url)).await {
            Ok(Ok(b)) => Some(b),
            Ok(Err(e)) => {
                tracing::warn!(proxy = name, error = %e, "probe: socks5 probe failed");
                None
            }
            Err(_) => {
                tracing::warn!(proxy = name, "probe: socks5 probe timed out");
                None
            }
        }
    } else {
        match tokio::time::timeout(timeout, reqwest_get(url, &probe_url)).await {
            Ok(Ok(b)) => Some(b),
            Ok(Err(e)) => {
                tracing::warn!(proxy = name, error = %e, "probe: http probe failed");
                None
            }
            Err(_) => {
                tracing::warn!(proxy = name, "probe: http probe timed out");
                None
            }
        }
    };
    let Some(body) = body else {
        return ProxyProbe::default();
    };
    ProxyProbe {
        latency_ms: Some(started.elapsed().as_millis() as u64),
        egress_ip: parse_ip_text(&body),
        healthy: true,
        last_probe_at: now_ts(),
    }
}

/// GET through an http(s) proxy via reqwest.
async fn reqwest_get(proxy_url: &str, probe_url: &str) -> Result<String, RouteError> {
    let proxy = reqwest::Proxy::all(proxy_url)
        .map_err(|e| RouteError::BadResponse(format!("invalid proxy url: {e}")))?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .build()
        .map_err(|e| RouteError::BadResponse(format!("client build: {e}")))?;
    let resp = client.get(probe_url).send().await?;
    if !resp.status().is_success() {
        return Err(RouteError::BadResponse(format!("http {}", resp.status())));
    }
    Ok(resp.text().await?)
}

/// Manual SOCKS5 (no-auth) CONNECT + plain HTTP GET. The probe URL is
/// downgraded to plain http: this path can't speak TLS, and the default
/// probe URL serves plaintext on http as well.
async fn manual_socks5_get(proxy_url: &str, probe_url: &str) -> Result<String, RouteError> {
    let (proxy_host, proxy_port) = url_host_port(proxy_url);
    let (target_host, target_port) = url_host_port(probe_url);
    let mut stream = TcpStream::connect((proxy_host.as_str(), proxy_port)).await?;
    stream.set_nodelay(true).ok();

    // greeting: SOCKS5, one method: no-auth
    stream.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut method = [0u8; 2];
    stream.read_exact(&mut method).await?;
    if method[0] != 0x05 || method[1] != 0x00 {
        return Err(RouteError::BadResponse(format!(
            "SOCKS5 auth method {:#04x}",
            method[1]
        )));
    }
    // CONNECT request, domain (ATYP 0x03)
    if target_host.len() > 255 {
        return Err(RouteError::BadResponse("SOCKS5 host too long".to_string()));
    }
    let mut req = Vec::with_capacity(7 + target_host.len());
    req.extend_from_slice(&[0x05, 0x01, 0x00, 0x03, target_host.len() as u8]);
    req.extend_from_slice(target_host.as_bytes());
    req.extend_from_slice(&target_port.to_be_bytes());
    stream.write_all(&req).await?;
    // reply: ver, rep, rsv, atyp, then address+port
    let mut head = [0u8; 10];
    stream.read_exact(&mut head).await?;
    if head[0] != 0x05 || head[1] != 0x00 {
        return Err(RouteError::BadResponse(format!(
            "SOCKS5 CONNECT failed: {:#04x}",
            head[1]
        )));
    }
    let total = match head[3] {
        0x01 => 10, // IPv4
        0x04 => 22, // IPv6
        0x03 => {
            // domain
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            4 + 1 + len[0] as usize + 2
        }
        atyp => return Err(RouteError::BadResponse(format!("SOCKS5 reply ATYP {atyp}"))),
    };
    if total > 10 {
        let mut rest = vec![0u8; total - 10];
        stream.read_exact(&mut rest).await?;
    }
    // plain HTTP GET. Read headers, then the body — bounded by
    // Content-Length when the server sends one, otherwise read to EOF (the
    // probe timeout is the backstop). Relying on EOF alone deadlocks against
    // proxies that don't propagate the close, so never wait for it.
    let head =
        format!("GET {probe_url} HTTP/1.0\r\nHost: {target_host}\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    let mut resp = Vec::new();
    let mut buf = [0u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break resp.len();
        }
        resp.extend_from_slice(&buf[..n]);
        if resp.len() > MAX_BODY {
            break resp.len();
        }
        if let Some(pos) = resp.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let text = String::from_utf8_lossy(&resp[..header_end.min(resp.len())]);
    let status = text.split("\r\n").next().unwrap_or("");
    if !status.contains(" 200 ") {
        return Err(RouteError::BadResponse(format!("HTTP {status}")));
    }
    let content_length = text
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse::<usize>().ok());
    let mut body = resp.split_off(header_end.min(resp.len()));
    match content_length {
        Some(len) => {
            while body.len() < len.min(MAX_BODY) {
                let n = stream.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&buf[..n]);
            }
            body.truncate(len.min(MAX_BODY));
        }
        None => loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 || body.len() > MAX_BODY {
                break;
            }
            body.extend_from_slice(&buf[..n]);
        },
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Run a Command proxy's `check` via `sh -c` with the configured env;
/// exit 0 = healthy. Latency and egress IP can't be measured for commands.
async fn probe_command(
    name: &str,
    check: &str,
    env: &[(String, String)],
    timeout_secs: u64,
) -> ProxyProbe {
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(check).kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let healthy = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(o)) if o.status.success() => true,
        Ok(Ok(o)) => {
            tracing::warn!(proxy = name, status = ?o.status, "probe: check command failed");
            false
        }
        Ok(Err(e)) => {
            tracing::warn!(proxy = name, error = %e, "probe: check command errored");
            false
        }
        Err(_) => {
            tracing::warn!(proxy = name, "probe: check command timed out");
            false
        }
    };
    ProxyProbe {
        healthy,
        last_probe_at: now_ts(),
        ..Default::default()
    }
}

/// TCP-connect to `target` ("host:port") bound to the egress source address.
async fn connect_bound(src: IpAddr, target: &str) -> Result<(), RouteError> {
    let sock = match src {
        IpAddr::V4(_) => TcpSocket::new_v4()?,
        IpAddr::V6(_) => TcpSocket::new_v6()?,
    };
    sock.bind(SocketAddr::new(src, 0))?;
    let addr: SocketAddr = target
        .parse()
        .map_err(|_| RouteError::BadResponse(format!("invalid probe target {target}")))?;
    let stream = sock.connect(addr).await?;
    drop(stream); // connect success = healthy
    Ok(())
}

/// Split a plain URL into (host, port). The manual SOCKS5 path can't speak
/// TLS, so https:// probe URLs are downgraded to http (default port 80).
fn url_host_port(url: &str) -> (String, u16) {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let rest = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if let Some(inner) = rest.strip_prefix('[') {
        // [::1]:8080
        if let Some((host, port)) = inner.split_once("]:") {
            let port = port.split('/').next().unwrap_or(port).parse().unwrap_or(80);
            return (host.to_string(), port);
        }
        return (inner.trim_end_matches(']').to_string(), 80);
    }
    match rest.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => {
            (host.to_string(), port.parse().unwrap_or(80))
        }
        _ => (rest.to_string(), 80),
    }
}

/// The probe URL: env override or default.
fn ip_probe_url() -> String {
    std::env::var("SYNORA_IP_PROBE_URL").unwrap_or_else(|_| DEFAULT_IP_PROBE_URL.to_string())
}

/// Parse a plain-text response body as an IP address.
fn parse_ip_text(body: &str) -> Option<String> {
    let t = body.trim();
    if t.parse::<IpAddr>().is_ok() {
        Some(t.to_string())
    } else {
        None
    }
}

fn now_ts() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

/// Small LCG (no rand dep), seeded lazily from process id + wall clock.
fn lcg() -> u64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut s = STATE.load(Ordering::Relaxed);
    if s == 0 {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        s = (std::process::id() as u64) ^ t.rotate_left(17);
        if s == 0 {
            s = 0x9E37_79B9_7F4A_7C15;
        }
        STATE.store(s, Ordering::Relaxed);
    }
    let next = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    STATE.store(next, Ordering::Relaxed);
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;

    const PROBE_BODY: &str = "203.0.113.7";

    fn proxy(name: &str, kind: ProxyKind) -> (String, ProxyConfig) {
        (
            name.to_string(),
            ProxyConfig {
                kind,
                healthcheck: None,
                timeout: 2,
                expose: None,
            },
        )
    }

    fn egress(name: &str, address: IpAddr, probe: Option<&str>) -> EgressConfig {
        EgressConfig {
            name: name.to_string(),
            address,
            probe: probe.map(str::to_string),
        }
    }

    fn route_with(
        proxies: Vec<(String, ProxyConfig)>,
        groups: Vec<(String, ProxyGroupConfig)>,
        egresses: Vec<EgressConfig>,
        egress_groups: Vec<(String, EgressGroupConfig)>,
    ) -> NetRoute {
        NetRoute::new(
            &proxies.into_iter().collect(),
            &groups.into_iter().collect(),
            &egresses,
            &egress_groups.into_iter().collect(),
            None,
        )
    }

    fn fwd(url: &str) -> ProxyKind {
        ProxyKind::Forward {
            url: url.to_string(),
            env: Vec::new(),
        }
    }

    /// HTTP server returning PROBE_BODY; used as the probe target.
    async fn spawn_http_target() -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    continue;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{}",
                        PROBE_BODY.len(),
                        PROBE_BODY
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        (addr, handle)
    }

    /// Fake SOCKS5 no-auth proxy: accepts, then CONNECTs to the requested
    /// host:port on loopback and pipes bytes both ways.
    async fn spawn_socks5_proxy() -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    continue;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 3];
                    if client.read_exact(&mut buf).await.is_err() {
                        return;
                    }
                    if client.write_all(&[0x05, 0x00]).await.is_err() {
                        return;
                    }
                    let mut head = [0u8; 4];
                    if client.read_exact(&mut head).await.is_err() {
                        return;
                    }
                    match head[3] {
                        0x01 => {
                            let mut a = [0u8; 4];
                            if client.read_exact(&mut a).await.is_err() {
                                return;
                            }
                        }
                        0x03 => {
                            let mut len = [0u8; 1];
                            if client.read_exact(&mut len).await.is_err() {
                                return;
                            }
                            let mut d = vec![0u8; len[0] as usize];
                            if client.read_exact(&mut d).await.is_err() {
                                return;
                            }
                        }
                        _ => return,
                    }
                    let mut port = [0u8; 2];
                    if client.read_exact(&mut port).await.is_err() {
                        return;
                    }
                    let Ok(mut upstream) =
                        TcpStream::connect(("127.0.0.1", u16::from_be_bytes(port))).await
                    else {
                        return;
                    };
                    let _ = client
                        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                        .await;
                    let (mut cr, mut cw) = client.split();
                    let (mut ur, mut uw) = upstream.split();
                    let _ = tokio::join!(
                        tokio::io::copy(&mut cr, &mut uw),
                        tokio::io::copy(&mut ur, &mut cw)
                    );
                });
            }
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn selection_fixed_returns_first_even_if_unhealthy() {
        let route = route_with(
            vec![
                proxy("bad", fwd("socks5h://127.0.0.1:1")),
                proxy("good", fwd("http://127.0.0.1:2")),
            ],
            vec![(
                "g".into(),
                ProxyGroupConfig {
                    proxies: vec!["bad".into(), "good".into()],
                    strategy: "fixed".into(),
                },
            )],
            vec![],
            vec![],
        );
        let probe = route.probe("bad").await;
        assert!(!probe.healthy);
        assert!(probe.latency_ms.is_none());
        let sel = route.select_proxy(Some("g"));
        assert_eq!(
            sel,
            Selection::Forward {
                name: "bad".into(),
                url: "socks5h://127.0.0.1:1".into(),
                env: vec![]
            }
        );
    }

    #[tokio::test]
    async fn selection_failover_skips_unhealthy() {
        let route = route_with(
            vec![
                proxy("bad", fwd("socks5h://127.0.0.1:1")),
                proxy("good", fwd("http://127.0.0.1:2")),
            ],
            vec![(
                "g".into(),
                ProxyGroupConfig {
                    proxies: vec!["bad".into(), "good".into()],
                    strategy: "failover".into(),
                },
            )],
            vec![],
            vec![],
        );
        let probe = route.probe("bad").await;
        assert!(!probe.healthy);
        let sel = route.select_proxy(Some("g"));
        assert_eq!(
            sel,
            Selection::Forward {
                name: "good".into(),
                url: "http://127.0.0.1:2".into(),
                env: vec![]
            }
        );
    }

    #[tokio::test]
    async fn selection_round_robin_cycles() {
        let route = route_with(
            vec![proxy("a", fwd("http://a")), proxy("b", fwd("http://b"))],
            vec![(
                "g".into(),
                ProxyGroupConfig {
                    proxies: vec!["a".into(), "b".into()],
                    strategy: "round-robin".into(),
                },
            )],
            vec![],
            vec![],
        );
        let picks: Vec<String> = (0..4)
            .map(|_| match route.select_proxy(Some("g")) {
                Selection::Forward { name, .. } => name,
                Selection::Direct => "direct".into(),
            })
            .collect();
        assert_eq!(picks, ["a", "b", "a", "b"]);
    }

    #[tokio::test]
    async fn selection_random_stays_in_set() {
        let route = route_with(
            vec![
                proxy("a", fwd("http://a")),
                proxy("b", fwd("http://b")),
                proxy("c", fwd("http://c")),
            ],
            vec![(
                "g".into(),
                ProxyGroupConfig {
                    proxies: vec!["a".into(), "b".into(), "c".into()],
                    strategy: "random".into(),
                },
            )],
            vec![],
            vec![],
        );
        for _ in 0..20 {
            let sel = route.select_proxy(Some("g"));
            let name = match sel {
                Selection::Forward { name, .. } => name,
                Selection::Direct => panic!("random group returned direct"),
            };
            assert!(
                ["a", "b", "c"].contains(&name.as_str()),
                "pick {name} outside set"
            );
        }
    }

    #[tokio::test]
    async fn selection_direct_and_default_proxy() {
        let route = NetRoute::new(
            &vec![proxy("p", fwd("http://p"))].into_iter().collect(),
            &HashMap::new(),
            &[],
            &HashMap::new(),
            Some("p"),
        );
        // job-level wins over default
        assert_eq!(
            route.select_proxy(None),
            Selection::Forward {
                name: "p".into(),
                url: "http://p".into(),
                env: vec![]
            }
        );
        // no default, no job proxy → direct
        let route2 = route_with(vec![], vec![], vec![], vec![]);
        assert_eq!(route2.select_proxy(None), Selection::Direct);
        // unknown name → direct (never panics)
        assert_eq!(route2.select_proxy(Some("nope")), Selection::Direct);
        // direct proxy kind → direct
        let route3 = route_with(vec![proxy("d", ProxyKind::Direct)], vec![], vec![], vec![]);
        assert_eq!(route3.select_proxy(Some("d")), Selection::Direct);
    }

    #[tokio::test]
    async fn egress_resolution() {
        let route = route_with(
            vec![],
            vec![],
            vec![egress("e1", IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), None)],
            vec![],
        );
        assert_eq!(
            route.select_egress(Some("e1")),
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))
        );
        assert_eq!(route.select_egress(None), None);
        assert_eq!(route.select_egress(Some("nope")), None);
    }

    #[tokio::test]
    async fn egress_group_failover() {
        let route = route_with(
            vec![],
            vec![],
            vec![
                egress(
                    "dead",
                    IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                    Some("127.0.0.1:1"),
                ),
                egress("alive", IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), None),
            ],
            vec![(
                "g".into(),
                EgressGroupConfig {
                    addresses: vec!["dead".into(), "alive".into()],
                    strategy: "failover".into(),
                },
            )],
        );
        assert!(
            !route.probe_egress("dead").await,
            "closed port should be unhealthy"
        );
        assert!(
            route.probe_egress("alive").await,
            "no probe target = healthy"
        );
        assert_eq!(
            route.select_egress(Some("g")),
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)))
        );
    }

    #[tokio::test]
    async fn egress_group_fixed_and_rr() {
        let v4 = |n: u8| IpAddr::V4(Ipv4Addr::new(10, 0, 0, n));
        let route = route_with(
            vec![],
            vec![],
            vec![egress("a", v4(1), None), egress("b", v4(2), None)],
            vec![
                (
                    "fixed".into(),
                    EgressGroupConfig {
                        addresses: vec!["a".into(), "b".into()],
                        strategy: "fixed".into(),
                    },
                ),
                (
                    "rr".into(),
                    EgressGroupConfig {
                        addresses: vec!["a".into(), "b".into()],
                        strategy: "round-robin".into(),
                    },
                ),
            ],
        );
        assert_eq!(route.select_egress(Some("fixed")), Some(v4(1)));
        assert_eq!(route.select_egress(Some("rr")), Some(v4(1)));
        assert_eq!(route.select_egress(Some("rr")), Some(v4(2)));
    }

    #[tokio::test]
    async fn family_narrowing() {
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let route = route_with(vec![], vec![], vec![], vec![]);
        assert_eq!(route.select_family("any", None), Family::Any);
        assert_eq!(route.select_family("ipv4", None), Family::V4);
        assert_eq!(route.select_family("ipv6", None), Family::V6);
        assert_eq!(route.select_family("bogus", None), Family::Any);
        // egress narrows any
        assert_eq!(route.select_family("any", Some(v4)), Family::V4);
        assert_eq!(route.select_family("any", Some(v6)), Family::V6);
        // conflict: egress wins
        assert_eq!(route.select_family("ipv4", Some(v6)), Family::V6);
        assert_eq!(route.select_family("ipv6", Some(v4)), Family::V4);
        assert_eq!(route.select_family("ipv4", Some(v4)), Family::V4);
        assert_eq!(route.select_family("ipv6", Some(v6)), Family::V6);
    }

    #[tokio::test]
    async fn proxy_env_forward() {
        let sel = Selection::Forward {
            name: "p".into(),
            url: "http://127.0.0.1:3128".into(),
            env: vec![("CUSTOM".into(), "1".into())],
        };
        std::env::set_var("NO_PROXY", "example.com");
        let env = proxy_env(&sel);
        std::env::remove_var("NO_PROXY");
        let map: HashMap<&str, &str> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(map.get("HTTP_PROXY"), Some(&"http://127.0.0.1:3128"));
        assert_eq!(map.get("HTTPS_PROXY"), Some(&"http://127.0.0.1:3128"));
        assert_eq!(map.get("ALL_PROXY"), Some(&"http://127.0.0.1:3128"));
        assert_eq!(map.get("NO_PROXY"), Some(&"example.com"));
        assert_eq!(map.get("CUSTOM"), Some(&"1"));
        assert!(proxy_env(&Selection::Direct).is_empty());
    }

    #[tokio::test]
    async fn socks5_probe_through_fake_proxy() {
        let (target, th) = spawn_http_target().await;
        let (proxy, ph) = spawn_socks5_proxy().await;
        let route = route_with(
            vec![(
                "s5".into(),
                ProxyConfig {
                    kind: fwd(&format!("socks5h://127.0.0.1:{}", proxy.port())),
                    healthcheck: Some(format!("http://127.0.0.1:{}", target.port())),
                    timeout: 5,
                    expose: None,
                },
            )],
            vec![],
            vec![],
            vec![],
        );
        let probe = route.probe("s5").await;
        assert!(
            probe.healthy,
            "probe through fake socks5 proxy should succeed: {probe:?}"
        );
        assert_eq!(probe.egress_ip.as_deref(), Some(PROBE_BODY));
        assert!(probe.latency_ms.is_some());
        let sel = route.select_proxy(Some("s5"));
        assert_eq!(
            sel,
            Selection::Forward {
                name: "s5".into(),
                url: format!("socks5h://127.0.0.1:{}", proxy.port()),
                env: vec![]
            }
        );
        th.abort();
        ph.abort();
    }

    #[tokio::test]
    async fn http_probe_through_fake_proxy() {
        let (target, th) = spawn_http_target().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        // trivial relay: pipe every connection to the target
        let ph = tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    continue;
                };
                tokio::spawn(async move {
                    let Ok(mut upstream) = TcpStream::connect(("127.0.0.1", target.port())).await
                    else {
                        return;
                    };
                    let (mut cr, mut cw) = client.split();
                    let (mut ur, mut uw) = upstream.split();
                    let _ = tokio::join!(
                        tokio::io::copy(&mut cr, &mut uw),
                        tokio::io::copy(&mut ur, &mut cw)
                    );
                });
            }
        });
        let route = route_with(
            vec![(
                "hp".into(),
                ProxyConfig {
                    kind: fwd(&format!("http://127.0.0.1:{}", proxy_addr.port())),
                    healthcheck: Some(format!("http://127.0.0.1:{}", target.port())),
                    timeout: 5,
                    expose: None,
                },
            )],
            vec![],
            vec![],
            vec![],
        );
        let probe = route.probe("hp").await;
        assert!(
            probe.healthy,
            "probe through fake http proxy should succeed: {probe:?}"
        );
        assert_eq!(probe.egress_ip.as_deref(), Some(PROBE_BODY));
        th.abort();
        ph.abort();
    }

    #[tokio::test]
    async fn probe_unreachable_proxy_is_down() {
        let route = route_with(
            vec![proxy("gone", fwd("socks5h://127.0.0.1:1"))],
            vec![],
            vec![],
            vec![],
        );
        let probe = route.probe("gone").await;
        assert!(!probe.healthy);
        assert!(probe.latency_ms.is_none());
        assert!(probe.egress_ip.is_none());
    }

    #[tokio::test]
    async fn probe_unknown_and_direct() {
        let route = route_with(vec![], vec![], vec![], vec![]);
        let probe = route.probe("nope").await;
        assert!(!probe.healthy);
        let route2 = route_with(vec![proxy("d", ProxyKind::Direct)], vec![], vec![], vec![]);
        let probe = route2.probe("d").await;
        assert!(probe.healthy);
    }

    #[tokio::test]
    async fn command_probe_exit_code() {
        let route = route_with(
            vec![
                proxy(
                    "ok",
                    ProxyKind::Command {
                        check: "exit 0".into(),
                        env: vec![],
                    },
                ),
                proxy(
                    "ko",
                    ProxyKind::Command {
                        check: "exit 7".into(),
                        env: vec![],
                    },
                ),
            ],
            vec![],
            vec![],
            vec![],
        );
        assert!(route.probe("ok").await.healthy);
        assert!(!route.probe("ko").await.healthy);
        // command proxy can't be selected
        assert_eq!(route.select_proxy(Some("ok")), Selection::Direct);
    }

    #[tokio::test]
    async fn probe_all_collects_every_proxy() {
        let route = route_with(
            vec![
                proxy("a", ProxyKind::Direct),
                proxy("b", fwd("socks5h://127.0.0.1:1")),
            ],
            vec![],
            vec![],
            vec![],
        );
        let probes = route.probe_all().await;
        assert_eq!(probes.len(), 2);
        assert!(probes["a"].healthy);
        assert!(!probes["b"].healthy);
    }

    #[tokio::test]
    async fn probe_egress_connect_bound_and_unprobed() {
        let (target, th) = spawn_http_target().await;
        let route = route_with(
            vec![],
            vec![],
            vec![egress(
                "ok",
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                Some(&format!("127.0.0.1:{}", target.port())),
            )],
            vec![],
        );
        assert!(route.probe_egress("ok").await);
        assert!(!route.probe_egress("unknown").await);
        th.abort();
    }

    #[tokio::test]
    async fn ip_probe_url_env_override() {
        std::env::set_var("SYNORA_IP_PROBE_URL", "http://127.0.0.1:9999");
        let url = ip_probe_url();
        std::env::remove_var("SYNORA_IP_PROBE_URL");
        assert_eq!(url, "http://127.0.0.1:9999");
        assert_eq!(ip_probe_url(), DEFAULT_IP_PROBE_URL);
    }

    #[tokio::test]
    async fn url_host_port_parsing() {
        assert_eq!(
            url_host_port("http://api.ipify.org"),
            ("api.ipify.org".into(), 80)
        );
        assert_eq!(
            url_host_port("http://127.0.0.1:8080/path"),
            ("127.0.0.1".into(), 8080)
        );
        assert_eq!(url_host_port("https://[::1]:8443"), ("::1".into(), 8443));
        assert_eq!(url_host_port("[::1]"), ("::1".into(), 80));
        assert_eq!(
            url_host_port("socks5h://proxy.example:1080"),
            ("proxy.example".into(), 1080)
        );
    }

    #[test]
    fn parse_ip_text_variants() {
        assert_eq!(
            parse_ip_text("203.0.113.7\n"),
            Some("203.0.113.7".to_string())
        );
        assert_eq!(
            parse_ip_text("  2001:db8::1  "),
            Some("2001:db8::1".to_string())
        );
        assert_eq!(parse_ip_text("not-an-ip"), None);
    }
}
