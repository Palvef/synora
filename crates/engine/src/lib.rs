//! Standalone engine: scheduler loop + run executor + metrics (M1).

pub mod cgroup;
pub mod engine;
pub mod executor;
pub mod logs;
pub mod scheduler;

pub use engine::{Engine, RunStorageCtx, LOCAL_WORKER};
pub use executor::{run_once, status_value, RunOutcome};
