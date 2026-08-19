//! Pure domain types for Synora: jobs, schedules, state machine inputs.
//! No IO, no runtime — everything here is serializable data or pure functions.

pub mod job;
pub mod metrics;
pub mod schedule;
pub mod size;
pub mod state;

pub use job::{
    ErrorKind, Hooks, JobSpec, JobStatus, MisfirePolicy, OnWorkerLost, ProviderConfig,
    RetentionPolicy, RunId, Safety, SnapshotPolicy, StatisticsMode, VerifyConfig, RUN_LEASE_SECS,
    WORKER_HEARTBEAT_GRACE_SECS,
};
pub use metrics::Metrics;
pub use schedule::{parse_cron_expr, parse_duration_human, Schedule, ScheduleKind};
pub use size::human_size;
pub use state::{retry_decision, transition, RetryDecision, RunEvent, StateError};
