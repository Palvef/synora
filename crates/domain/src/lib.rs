//! Pure domain types for Synora: jobs, schedules, state machine inputs.
//! No IO, no runtime — everything here is serializable data or pure functions.

pub mod job;
pub mod schedule;

pub use job::{
    ErrorKind, Hooks, JobSpec, JobStatus, MisfirePolicy, OnWorkerLost, ProviderConfig, RunId,
    Safety, StatisticsMode,
};
pub use schedule::{Schedule, ScheduleKind};
