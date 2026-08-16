//! Sync providers (spec §12): the tools that actually move data.
//! Synora only orchestrates — rsync/scripts/docker do the work.
//!
//! A concrete enum rather than `Box<dyn Trait>`: native async fn in traits
//! fights `dyn` compatibility, and there are exactly three providers. The
//! open provider SDK (spec §115) is a Phase 7 concern; when it lands, the
//! enum arm becomes a `Custom(Box<dyn ...>)` or the trait gets boxed futures.

pub mod docker;
pub mod http;
pub mod rsync;
pub mod script;


/// Spawn the child as its own process-group leader so the whole tree
/// (shell + grandchildren) can be killed on cancel (spec §74).
/// Lightweight handle the engine provides; the provider attaches children.
pub trait CgroupScopeRef: Send + Sync {
    fn attach(&self, pid: u32);
}

pub(crate) fn spawn_group(
    cmd: &mut tokio::process::Command,
    ctx: &SyncContext,
) -> Result<tokio::process::Child, ProviderError> {
    // process_group comes from CommandExt on unix; the import is needed inside
    // the function body.
    #[allow(unused_imports)]
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let child = cmd.spawn().map_err(|e| ProviderError::Spawn(e.to_string()))?;
    if let Some(cg) = &ctx.cgroup {
        cg.attach(child.id().unwrap_or(0));
    }
    Ok(child)
}

/// Kill the child's whole process group and reap it.
pub(crate) async fn kill_group(child: &mut tokio::process::Child) {
    let pid = child.id().unwrap_or(0) as i32;
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.wait().await;
}

/// After `wait()` resolved: if a cancel raced us, the run is cancelled —
/// never treat a killed process as a normal exit.
pub(crate) async fn cancelled_after_wait(
    ctx: &SyncContext,
    child: &mut tokio::process::Child,
) -> Result<(), ProviderError> {
    if ctx.cancel.is_cancelled() {
        kill_group(child).await;
        return Err(ProviderError::Cancelled);
    }
    Ok(())
}

use std::path::PathBuf;
use synora_core::job::{ErrorKind, JobSpec};

/// Everything a provider needs for one run.
#[derive(Clone)]
pub struct SyncContext {
    pub run_id: String,
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
    /// Optional cgroup scope (user-requested resource limits): providers
    /// attach their child right after spawn.
    pub cgroup: Option<std::sync::Arc<dyn crate::CgroupScopeRef>>,
    /// Resolved proxy environment (empty = direct — mirror sync defaults to
    /// the machine's own network).
    pub proxy_env: Vec<(String, String)>,
    /// Resolved egress source bind address (rsync --address etc.).
    pub egress_address: Option<String>,
    /// Resolved address family for the connection: ipv4 | ipv6 | any.
    pub family: String,
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
    #[error("cancelled by operator")]
    Cancelled,
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
    Rsync(rsync::RsyncProvider),
    Script(script::ScriptProvider),
    Docker(docker::DockerProvider),
    Http(http::HttpProvider),
}

impl Provider {
    pub fn name(&self) -> &'static str {
        match self {
            Provider::Rsync(_) => "rsync",
            Provider::Script(_) => "script",
            Provider::Docker(_) => "docker",
            Provider::Http(_) => "http",
        }
    }

    pub async fn sync(&self, ctx: &SyncContext) -> Result<SyncResult, ProviderError> {
        match self {
            Provider::Rsync(p) => p.sync(ctx).await,
            Provider::Script(p) => p.sync(ctx).await,
            Provider::Docker(p) => p.sync(ctx).await,
            Provider::Http(p) => p.sync(ctx).await,
        }
    }
}

/// Build the provider for a job.
pub fn build_provider(job: &JobSpec) -> Result<Provider, ProviderError> {
    match &job.provider {
        synora_core::ProviderConfig::Rsync { options } => {
            Ok(Provider::Rsync(rsync::RsyncProvider {
                options: options.clone(),
            }))
        }
        synora_core::ProviderConfig::Script { command } => Ok(Provider::Script(
            script::ScriptProvider {
                command: command.clone(),
            },
        )),
        synora_core::ProviderConfig::Docker {
            image,
            env,
            volumes,
            keep_container,
        } => Ok(Provider::Docker(docker::DockerProvider {
            image: image.clone(),
            env: env.clone(),
            volumes: volumes.clone(),
            keep_container: *keep_container,
        })),
        synora_core::ProviderConfig::Http { parser, delete } => {
            Ok(Provider::Http(http::HttpProvider {
                parser: parser.clone(),
                delete: *delete,
            }))
        }
    }
}
