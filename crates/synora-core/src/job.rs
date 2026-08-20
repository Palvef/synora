//! Job model: the core object of Synora. A job describes one mirror repository.

use crate::schedule::Schedule;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use time::Duration;
use uuid::Uuid;

/// Claimed-run lease lifetime. Heartbeats refresh it; the reaper only
/// marks LOST when the lease is expired *and* the worker is no longer
/// heartbeating. 60s was too short for a swapping/busy worker: production
/// AOSP/GXDE runs were re-dispatched, then `docker rm -f` SIGKILL'd the
/// still-running container (exit 137).
pub const RUN_LEASE_SECS: i64 = 300;

/// A worker with a heartbeat newer than this is still alive for lease
/// expiry purposes, even if a lease row was not refreshed.
pub const WORKER_HEARTBEAT_GRACE_SECS: i64 = 90;

/// Lifecycle of a job / run, per spec §5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Pending,
    Scheduled,
    Queued,
    /// Worker has claimed the run and is syncing (legacy name: starting).
    Syncing,
    Running,
    Success,
    Failed,
    Retrying,
    Cancelling,
    Cancelled,
    /// Lease expired while a worker was supposed to hold it (spec §29).
    Lost,
    /// Dependency failed or didn't run — this run never started (spec §93).
    Skipped,
}

impl JobStatus {
    /// Stable lowercase API / TUI label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Scheduled => "scheduled",
            Self::Queued => "queued",
            Self::Syncing => "syncing",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Retrying => "retrying",
            Self::Cancelling => "cancelling",
            Self::Cancelled => "cancelled",
            Self::Lost => "lost",
            Self::Skipped => "skipped",
        }
    }
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
        /// `--exclude=PATTERN` entries (tunasync `exclude` config).
        #[serde(default)]
        exclude: Vec<String>,
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
        /// docker run --network. None/empty = daemon default (bridge).
        #[serde(default)]
        network: Option<String>,
        /// Command to run inside the container (argv). Empty = the image's
        /// own entrypoint (tunasync-scripts style).
        #[serde(default)]
        command: Vec<String>,
    },
    Git {
        /// Clone only this branch (single-branch checkout); default is a
        /// full `--mirror` clone of all refs.
        #[serde(default)]
        branch: Option<String>,
    },
    TwoStageRsync {
        /// Extra rsync arguments appended to the stage-2 defaults.
        #[serde(default)]
        options: Vec<String>,
        /// `--exclude=PATTERN` entries (applied to both stages).
        #[serde(default)]
        exclude: Vec<String>,
        /// Stage-1 subset profile (tunasync `stage1_profile`): debian |
        /// debian-oldstyle.
        #[serde(default)]
        stage1_profile: String,
    },
    Http {
        /// Directory-listing parser name (spec §14): nginx|apache|caddy|s3|
        /// directory-listing|fallback.
        parser: String,
        /// Delete local files absent from the index (like rsync --delete).
        #[serde(default)]
        delete: bool,
        /// Max concurrent downloads (tunasync `TUNASYNC_TSUMUGU_THREADS`);
        /// `None` = the httpfetch default (8). `0` is clamped to 1.
        #[serde(default)]
        threads: Option<u32>,
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

/// Dangerous-sync thresholds (spec §53). Evaluated after sync runs report
/// deletion counts; the executor blocks runs whose deletions would breach
/// them (needs provider support — rsync --stats reports deletions).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Safety {
    pub max_delete_files: Option<u64>,
    pub max_delete_ratio: Option<f64>,
    pub max_size_drop_ratio: Option<f64>,
}

/// When snapshots are taken around a run (spec §32).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotPolicy {
    Never,
    AfterSuccess,
    BeforeSync,
    BeforeAndAfter,
    Manual,
}

/// Post-sync verification (spec §56): only a verified success can produce an
/// after-success snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VerifyConfig {
    pub enabled: bool,
    /// "path" = storage dir exists, "size" = non-zero size, "command" = run
    /// the configured command.
    pub checks: Vec<String>,
    pub command: Option<String>,
}

/// Snapshot retention buckets (spec §33).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub keep_last: Option<u32>,
    pub keep_daily: Option<u32>,
    pub keep_weekly: Option<u32>,
    pub keep_monthly: Option<u32>,
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
    /// tunasync `mirror_subdir`: store under `<storage>/<sub_dir>` instead of the
    /// storage root (applied at config load; the runtime only ever sees the
    /// final path).
    #[serde(default)]
    pub mirror_subdir: Option<String>,
    /// Which `[storage.<name>]` section this job's storage belongs to.
    /// Workers may define the same name with different local pools/mounts;
    /// the actual path is then `<storage mountpoint>/<job storage>`.
    #[serde(default)]
    pub storage_name: Option<String>,
    /// Proxy / proxy-group name (parsed, inert until Phase 3).
    pub proxy: Option<String>,
    /// Egress / egress-group name — the source address to bind (Phase 3).
    pub egress: Option<String>,
    /// Address family for the sync connection: ipv4 | ipv6 | any.
    /// Mirror sync uses the machine's direct network by default; proxies are
    /// opt-in per job.
    pub family: String,
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
    /// cgroup memory limit, e.g. "4G" (user-requested feature; tunasync has
    /// the same per-mirror memory_limit). Also passed to docker --memory.
    pub memory_limit: Option<u64>,
    /// cgroup CPU limit in cores (docker --cpus).
    pub cpu_limit: Option<f64>,
    /// Jobs that must have succeeded recently for this job to run (spec §93).
    /// A failed/missing dependency marks the run SKIPPED.
    pub depends_on: Vec<String>,
    /// Snapshot timing (spec §32).
    pub snapshot_policy: SnapshotPolicy,
    /// Post-sync verification (spec §56).
    pub verify: VerifyConfig,
}
