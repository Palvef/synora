//! Sync providers (spec §12): the tools that actually move data.
//! Synora only orchestrates — rsync/scripts/docker do the work.
//!
//! A concrete enum rather than `Box<dyn Trait>`: native async fn in traits
//! fights `dyn` compatibility, and there are exactly three providers. The
//! open provider SDK (spec §115) is a Phase 7 concern; when it lands, the
//! enum arm becomes a `Custom(Box<dyn ...>)` or the trait gets boxed futures.

pub mod script;

use std::path::PathBuf;
use synora_core::job::{ErrorKind, JobSpec, RunId};

/// Everything a provider needs for one run.
#[derive(Clone)]
pub struct SyncContext {
    pub run_id: RunId,
    pub job_name: String,
    pub upstream: Option<String>,
    pub storage: PathBuf,
    pub worker: Option<String>,
    /// Proxy / egress names (parsed, inert until Phase 3).
    pub proxy: Option<String>,
    pub egress: Option<String>,
    /// Parsed job (provider-specific config, hooks, safety...).
    pub job: JobSpec,
    /// Cancel signal: providers must kill their child process on cancel
    /// (timeout / `synora stop`).
    pub cancel: tokio_util::sync::CancellationToken,
}

/// Result of one provider run (spec §17: size comes from the provider when it
/// knows it; from the script via SYNORA_SIZE=; else filesystem walk).
#[derive(Debug, Clone, Default)]
pub struct SyncResult {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Provider-reported size in bytes (SYNORA_SIZE=, rsync --stats, ...).
    pub size_hint: Option<u64>,
    /// Provider-reported transferred bytes.
    pub bytes_transferred: Option<u64>,
    /// Human message (SYNORA_MESSAGE= or provider summary).
    pub message: Option<String>,
    /// Machine-readable status from the provider (SYNORA_STATUS=);
    /// "success" counts as success even with a non-zero exit, anything else
    /// forces failure even with exit 0 (spec §16).
    pub status: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("failed to start process: {0}")]
    Spawn(String),
    #[error("provider exited with {0}")]
    Exit(i32),
    #[error("provider timed out")]
    Timeout,
    #[error("invalid provider config: {0}")]
    Config(String),
    #[error("provider failed: {0}")]
    Other(String),
}

impl ProviderError {
    /// Classify into the retry-relevant ErrorKind (spec §54).
    pub fn kind(&self) -> ErrorKind {
        match self {
            ProviderError::Timeout => ErrorKind::Timeout,
            ProviderError::Config(_) | ProviderError::Spawn(_) => ErrorKind::ConfigError,
            _ => ErrorKind::ProviderError,
        }
    }
}

/// One of the concrete providers.
pub enum Provider {
    Script(script::ScriptProvider),
}

impl Provider {
    pub fn name(&self) -> &'static str {
        match self {
            Provider::Script(_) => "script",
        }
    }

    pub async fn sync(&self, ctx: &SyncContext) -> Result<SyncResult, ProviderError> {
        match self {
            Provider::Script(p) => p.sync(ctx).await,
        }
    }
}

/// Build the provider for a job.
pub fn build_provider(job: &JobSpec) -> Result<Provider, ProviderError> {
    match &job.provider {
        synora_core::ProviderConfig::Script { command } => Ok(Provider::Script(
            script::ScriptProvider {
                command: command.clone(),
            },
        )),
        synora_core::ProviderConfig::Rsync { .. } => Err(ProviderError::Config(
            "rsync provider lands in M2".to_string(),
        )),
        synora_core::ProviderConfig::Docker { .. } => Err(ProviderError::Config(
            "docker provider lands in M2".to_string(),
        )),
    }
}
