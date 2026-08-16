//! Job model: the core object of Synora. A job describes one mirror repository.

use crate::schedule::Schedule;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use time::Duration;
use uuid::Uuid;

/// Lifecycle of a job / run, per spec §5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Pending,
    Scheduled,
    Queued,
    Starting,
    Running,
    Success,
    Failed,
    Retrying,
    Cancelling,
    Cancelled,
    /// Lease expired while a worker was supposed to hold it (spec §29).
    Lost,
}

/// Classified failure cause — decides whether a retry makes sense (spec §54).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    NetworkError,
    Timeout,
    ProxyError,
    ProviderError,
    StorageError,
    /// Never retried.
    ConfigError,
}

/// Unique id of one execution of a job (spec §48).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(pub Uuid);

impl RunId {
    pub fn new() -> Self {
        RunId(Uuid::new_v4())
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

/// Which sync tool actually moves the data (spec §12). Synora only orchestrates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProviderConfig {
    Rsync {
        /// Extra rsync arguments appended after the defaults (spec §13).
        #[serde(default)]
        options: Vec<String>,
    },
    Script {
        /// Path of the script/command to run (spec §16).
        command: String,
    },
    Docker {
        /// Image to run (spec §18).
        image: String,
        /// "KEY=VALUE" environment lines.
        #[serde(default)]
        env: Vec<String>,
        /// "host:container" volume mappings.
        #[serde(default)]
        volumes: Vec<String>,
        /// Keep the container after exit for debugging (spec §18).
        #[serde(default)]
        keep_container: bool,
    },
}

/// What to do when a scheduled time was missed because the machine was offline (spec §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MisfirePolicy {
    Skip,
    RunImmediately,
    RunNext,
}

/// What to do when a worker holding a run goes away (spec §29).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnWorkerLost {
    Retry,
    Fail,
}

/// Where repository size numbers come from (spec §58).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatisticsMode {
    Provider,
    Filesystem,
}

/// Shell hooks around a run (spec §50). Commands run via the same executor as scripts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hooks {
    #[serde(default)]
    pub before_sync: Vec<String>,
    #[serde(default)]
    pub after_sync: Vec<String>,
    #[serde(default)]
    pub on_success: Vec<String>,
    #[serde(default)]
    pub on_failure: Vec<String>,
}

/// Dangerous-sync thresholds (spec §53). Parsed now, enforced in a later phase.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Safety {
    pub max_delete_files: Option<u64>,
    pub max_delete_ratio: Option<f64>,
    pub max_size_drop_ratio: Option<f64>,
}

/// Fully resolved job definition. `proxy`/`egress` are parsed but inert in P0/P1
/// (network egress selection arrives in Phase 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    pub name: String,
    pub enabled: bool,
    /// Explicit worker id / worker group name; `None` = scheduler picks ("auto").
    pub worker: Option<String>,
    pub provider: ProviderConfig,
    /// Upstream URL. Required for rsync; optional for script/docker.
    pub upstream: Option<String>,
    /// Local repository path. Canonicalized and `..`-checked at config load,
    /// re-checked at exec time.
    pub storage: PathBuf,
    /// Proxy / proxy-group name (parsed, inert until Phase 3).
    pub proxy: Option<String>,
    /// Egress / egress-group name (parsed, inert until Phase 3).
    pub egress: Option<String>,
    /// Hard wall-clock limit for one run.
    pub timeout: Duration,
    pub retry: u32,
    pub retry_delay: Duration,
    /// Multiplier: delay * backoff^attempt, capped at 24h.
    pub retry_backoff: f64,
    /// Exit codes treated as success (rsync 23/24 etc., tunasync convention).
    pub success_exit_codes: Vec<i32>,
    /// Regex on provider output: a match means FAILED even with exit 0 (tunasync convention).
    pub fail_on_match: Option<String>,
    /// Max concurrent runs of this job.
    pub max_concurrency: u32,
    pub misfire_policy: MisfirePolicy,
    pub on_worker_lost: OnWorkerLost,
    /// IANA timezone name, e.g. "Asia/Shanghai". Internal times stay UTC.
    pub timezone: String,
    pub statistics: StatisticsMode,
    /// Resource tags the worker must have in its labels (spec §8).
    pub resources: Vec<String>,
    /// Higher runs first (spec §92).
    pub priority: i32,
    pub schedule: Schedule,
    pub hooks: Hooks,
    pub safety: Safety,
}
