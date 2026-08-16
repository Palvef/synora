//! Run state machine + retry policy (spec §5, §54). Pure functions.

use crate::job::{ErrorKind, JobStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEvent {
    /// Re-queued after a retry delay.
    Queued,
    Starting,
    Running,
    Success,
    Failed,
    Retrying,
    Cancelling,
    Cancelled,
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("illegal state transition: {from:?} -> {event:?}")]
pub struct StateError {
    pub from: JobStatus,
    pub event: RunEvent,
}

/// Legal transitions (spec §5). `LOST` is terminal for the run row; a fresh
/// row is created when the job is re-dispatched (spec §29).
pub fn transition(cur: JobStatus, ev: RunEvent) -> Result<JobStatus, StateError> {
    let next = match (cur, ev) {
        (JobStatus::Queued, RunEvent::Starting) => JobStatus::Starting,
        (JobStatus::Queued, RunEvent::Cancelled) => JobStatus::Cancelled,
        (JobStatus::Starting, RunEvent::Running) => JobStatus::Running,
        (JobStatus::Starting, RunEvent::Cancelling) => JobStatus::Cancelling,
        (JobStatus::Starting, RunEvent::Lost) => JobStatus::Lost,
        (JobStatus::Running, RunEvent::Success) => JobStatus::Success,
        (JobStatus::Running, RunEvent::Failed) => JobStatus::Failed,
        (JobStatus::Running, RunEvent::Cancelling) => JobStatus::Cancelling,
        (JobStatus::Running, RunEvent::Lost) => JobStatus::Lost,
        (JobStatus::Failed, RunEvent::Retrying) => JobStatus::Retrying,
        (JobStatus::Retrying, RunEvent::Queued) => JobStatus::Queued,
        (JobStatus::Cancelling, RunEvent::Cancelled) => JobStatus::Cancelled,
        _ => {
            return Err(StateError {
                from: cur,
                event: ev,
            })
        }
    };
    Ok(next)
}

/// Retry decision after a failure (spec §54): ConfigError never retries,
/// retries exhausted → no retry, otherwise backoff `delay * backoff^attempt`
/// capped at 24h.
pub fn retry_decision(
    kind: ErrorKind,
    attempts: u32,
    retry_max: u32,
    delay_secs: u64,
    backoff: f64,
) -> RetryDecision {
    if kind == ErrorKind::ConfigError || attempts >= retry_max {
        return RetryDecision::NoRetry;
    }
    let cap = 24 * 3600;
    let secs = (delay_secs as f64 * backoff.powi(attempts as i32)) as u64;
    RetryDecision::Retry {
        delay_secs: secs.min(cap),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    NoRetry,
    Retry { delay_secs: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_chain() {
        let mut s = JobStatus::Queued;
        for ev in [
            RunEvent::Starting,
            RunEvent::Running,
            RunEvent::Failed,
            RunEvent::Retrying,
            RunEvent::Queued,
            RunEvent::Starting,
            RunEvent::Running,
            RunEvent::Success,
        ] {
            s = transition(s, ev).unwrap();
        }
        assert_eq!(s, JobStatus::Success);
    }

    #[test]
    fn cancel_paths() {
        assert_eq!(
            transition(JobStatus::Running, RunEvent::Cancelling).unwrap(),
            JobStatus::Cancelling
        );
        assert_eq!(
            transition(JobStatus::Cancelling, RunEvent::Cancelled).unwrap(),
            JobStatus::Cancelled
        );
        assert_eq!(
            transition(JobStatus::Queued, RunEvent::Cancelled).unwrap(),
            JobStatus::Cancelled
        );
    }

    #[test]
    fn lost_paths() {
        assert_eq!(
            transition(JobStatus::Running, RunEvent::Lost).unwrap(),
            JobStatus::Lost
        );
        assert_eq!(
            transition(JobStatus::Starting, RunEvent::Lost).unwrap(),
            JobStatus::Lost
        );
        // Lost is terminal for the run row.
        assert!(transition(JobStatus::Lost, RunEvent::Running).is_err());
    }

    #[test]
    fn illegal_transitions_rejected() {
        assert!(transition(JobStatus::Pending, RunEvent::Running).is_err());
        assert!(transition(JobStatus::Success, RunEvent::Running).is_err());
        assert!(transition(JobStatus::Queued, RunEvent::Success).is_err());
        assert!(transition(JobStatus::Failed, RunEvent::Cancelling).is_err());
    }

    #[test]
    fn retry_backoff() {
        // ConfigError never retries.
        assert_eq!(
            retry_decision(ErrorKind::ConfigError, 0, 3, 300, 2.0),
            RetryDecision::NoRetry
        );
        // Exponential: 5m, 10m, 20m (capped at 24h).
        assert_eq!(
            retry_decision(ErrorKind::NetworkError, 0, 3, 300, 2.0),
            RetryDecision::Retry { delay_secs: 300 }
        );
        assert_eq!(
            retry_decision(ErrorKind::NetworkError, 1, 3, 300, 2.0),
            RetryDecision::Retry { delay_secs: 600 }
        );
        assert_eq!(
            retry_decision(ErrorKind::NetworkError, 2, 3, 300, 2.0),
            RetryDecision::Retry { delay_secs: 1200 }
        );
        // Attempts exhausted.
        assert_eq!(
            retry_decision(ErrorKind::NetworkError, 3, 3, 300, 2.0),
            RetryDecision::NoRetry
        );
        // 24h cap.
        assert_eq!(
            retry_decision(ErrorKind::NetworkError, 10, 20, 300, 2.0),
            RetryDecision::Retry {
                delay_secs: 24 * 3600
            }
        );
    }
}
