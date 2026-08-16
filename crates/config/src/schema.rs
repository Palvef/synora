//! TOML document shapes. `[[jobs]]` entries are extracted per file (to keep
//! their file:line), so `RootDoc` covers everything else: daemon, api, and
//! the sections that are parsed but inert in P0/P1 (proxy/egress/storage/worker).

use serde::Deserialize;
use std::collections::HashMap;

/// Integer seconds (spec §5: `timeout = 7200`) or human duration (spec §78: `"2h"`).
#[derive(Debug, Clone)]
pub enum TomlDuration {
    Seconds(u64),
    Human(String),
}

impl<'de> Deserialize<'de> for TomlDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = TomlDuration;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("integer seconds or duration string like \"2h\"")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<TomlDuration, E> {
                Ok(TomlDuration::Seconds(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<TomlDuration, E> {
                if v < 0 {
                    return Err(E::custom("duration cannot be negative"));
                }
                Ok(TomlDuration::Seconds(v as u64))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<TomlDuration, E> {
                Ok(TomlDuration::Human(v.to_string()))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RootDoc {
    #[serde(default)]
    pub version: Option<u64>,
    #[serde(default)]
    pub daemon: DaemonDoc,
    #[serde(default)]
    pub api: ApiDoc,
    /// Parsed-but-inert sections (proxy/proxy_groups/egress/storage/worker).
    #[serde(flatten)]
    pub extras: HashMap<String, toml::Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonDoc {
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
    #[serde(default)]
    pub db: DbDoc,
    #[serde(default = "default_log_dir")]
    pub log_dir: String,
    /// Default proxy for probe/tooling traffic (user: default Cloudflare
    /// egress). Mirror sync itself stays direct unless a job opts in.
    #[serde(default)]
    pub default_proxy: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbDoc {
    #[serde(default = "default_db_kind")]
    pub kind: String,
    #[serde(default = "default_db_path")]
    pub path: String,
    pub url: Option<String>,
}

// Default must match the serde defaults above (used when `[daemon.db]` is absent).
impl Default for DbDoc {
    fn default() -> Self {
        Self {
            kind: default_db_kind(),
            path: default_db_path(),
            url: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiDoc {
    #[serde(default = "default_api_listen")]
    pub listen: String,
    #[serde(default)]
    pub tls: TlsDoc,
    #[serde(default)]
    pub tokens: Vec<TokenDoc>,
    /// Path serving the native status JSON (mirror-web consumption).
    #[serde(default = "default_synora_json")]
    pub synora_json_path: String,
    /// Path serving tunasync-compatible JSON (mirror-web drop-in).
    #[serde(default = "default_tunasync_json")]
    pub tunasync_json_path: String,
}

impl Default for ApiDoc {
    fn default() -> Self {
        Self {
            listen: default_api_listen(),
            tls: TlsDoc::default(),
            tokens: Vec::new(),
            synora_json_path: default_synora_json(),
            tunasync_json_path: default_tunasync_json(),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsDoc {
    pub cert: Option<String>,
    pub key: Option<String>,
    pub client_ca: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenDoc {
    pub name: String,
    pub token: String,
    #[serde(default = "default_role")]
    pub role: String,
    #[serde(default)]
    pub permissions: Vec<String>,
}

/// TOML shape of one job table (spec §5/§78). Unknown fields are rejected.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobDoc {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub worker: Option<String>,
    pub provider: Option<String>,
    // rsync
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    // script
    pub command: Option<String>,
    // docker
    pub image: Option<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default = "no")]
    pub keep_container: bool,
    // http (Phase 5)
    pub parser: Option<String>,
    #[serde(default = "no")]
    pub delete: bool,
    // common
    pub upstream: Option<String>,
    pub storage: Option<String>,
    pub proxy: Option<String>,
    pub egress: Option<String>,
    /// ipv4 | ipv6 | any — which address family the sync uses (user: mirror
    /// sync goes direct; family/bind are the knobs).
    #[serde(default = "default_family")]
    pub family: String,
    /// None = no limit (user requirement: runs are unlimited unless a
    /// timeout is explicitly configured). Accepts seconds or "1m"/"1h"/"1d".
    pub timeout: Option<TomlDuration>,
    #[serde(default = "default_retry")]
    pub retry: u32,
    #[serde(default = "default_retry_delay")]
    pub retry_delay: String,
    #[serde(default = "default_backoff")]
    pub retry_backoff: f64,
    #[serde(default)]
    pub success_exit_codes: Vec<i32>,
    pub fail_on_match: Option<String>,
    #[serde(default = "default_one")]
    pub max_concurrency: u32,
    #[serde(default = "default_misfire")]
    pub misfire_policy: String,
    #[serde(default = "default_worker_lost")]
    pub on_worker_lost: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default = "default_statistics")]
    pub statistics: String,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default = "default_priority")]
    pub priority: i32,
    // schedule
    pub schedule: Option<String>,
    pub cron: Option<String>,
    pub at: Option<String>,
    pub weekday: Option<String>,
    pub every: Option<String>,
    // nested sections
    #[serde(default)]
    pub hooks: HooksDoc,
    #[serde(default)]
    pub safety: SafetyDoc,
    #[serde(default)]
    pub snapshot: SnapshotJobDoc,
    #[serde(default)]
    pub verify: VerifyDoc,
    // P2+: cgroup limits (user-requested; tunasync parity)
    pub memory_limit: Option<String>,
    pub cpu_limit: Option<f64>,
    /// Dependencies: jobs that must have succeeded recently (spec §93).
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksDoc {
    #[serde(default)]
    pub before_sync: Vec<String>,
    #[serde(default)]
    pub after_sync: Vec<String>,
    #[serde(default)]
    pub on_success: Vec<String>,
    #[serde(default)]
    pub on_failure: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafetyDoc {
    pub max_delete_files: Option<u64>,
    pub max_delete_ratio: Option<f64>,
    pub max_size_drop_ratio: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotJobDoc {
    /// after-success | before-sync | before-and-after | manual | never
    #[serde(default = "default_snapshot_policy")]
    pub policy: String,
}

impl Default for SnapshotJobDoc {
    fn default() -> Self {
        Self {
            policy: default_snapshot_policy(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyDoc {
    #[serde(default = "no")]
    pub enabled: bool,
    #[serde(default)]
    pub checks: Vec<String>,
    pub command: Option<String>,
}

fn yes() -> bool {
    true
}
fn no() -> bool {
    false
}
fn default_max_concurrency() -> u32 {
    16
}
fn default_log_dir() -> String {
    "/var/log/synora".into()
}
fn default_db_kind() -> String {
    "sqlite".into()
}
fn default_db_path() -> String {
    "data/synora.db".into()
}
fn default_api_listen() -> String {
    "127.0.0.1:8100".into()
}
fn default_synora_json() -> String {
    "/synora.json".into()
}
fn default_tunasync_json() -> String {
    "/tunasync.json".into()
}
fn default_role() -> String {
    "admin".into()
}
fn default_retry() -> u32 {
    3
}
fn default_retry_delay() -> String {
    "5m".into()
}
fn default_backoff() -> f64 {
    2.0
}
fn default_one() -> u32 {
    1
}
fn default_misfire() -> String {
    "skip".into()
}
fn default_worker_lost() -> String {
    "retry".into()
}
fn default_timezone() -> String {
    "UTC".into()
}
fn default_statistics() -> String {
    "provider".into()
}
fn default_priority() -> i32 {
    50
}
fn default_snapshot_policy() -> String {
    "never".into()
}
fn default_family() -> String {
    "any".into()
}
