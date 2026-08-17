//! Sync providers (spec §12): the tools that actually move data.
//! Synora only orchestrates — rsync/scripts/docker do the work.
//!
//! A concrete enum rather than `Box<dyn Trait>`: native async fn in traits
//! fights `dyn` compatibility, and there are exactly three providers. The
//! open provider SDK (spec §115) is a Phase 7 concern; when it lands, the
//! enum arm becomes a `Custom(Box<dyn ...>)` or the trait gets boxed futures.

pub mod docker;
pub mod git;
pub mod http;
pub mod rsync;
pub mod script;

use tokio::io::AsyncReadExt;

/// Spawn the child as its own process-group leader so the whole tree
/// (shell + grandchildren) can be killed on cancel (spec §74).
/// Lightweight handle the engine provides; the provider attaches children.
pub trait CgroupScopeRef: Send + Sync {
    fn attach(&self, pid: u32);
}

/// Kills the child's process group if the owning future is dropped
/// (run timeout, task abort) — the explicit cancel paths kill and reap;
/// this guard covers every OTHER way a provider future can end, so no
/// sync process survives its run.
pub(crate) struct KillOnDrop(i32);

impl KillOnDrop {
    pub fn arm(child: &tokio::process::Child) -> Self {
        KillOnDrop(child.id().unwrap_or(0) as i32)
    }
    pub fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.0 > 0 {
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }
}

pub(crate) fn spawn_group(
    cmd: &mut tokio::process::Command,
    ctx: &SyncContext,
) -> Result<(tokio::process::Child, KillOnDrop), ProviderError> {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP = 0x00000200; CREATE_BREAKAWAY_FROM_JOB =
        // 0x01000000. CREATE_BREAKAWAY_FROM_JOB alone is the safe default —
        // a new process group without a console is fine for tree killing.
        cmd.creation_flags(0x01000000);
    }
    let child = cmd
        .spawn()
        .map_err(|e| ProviderError::Spawn(e.to_string()))?;
    if let Some(cg) = &ctx.cgroup {
        cg.attach(child.id().unwrap_or(0));
    }
    let guard = KillOnDrop::arm(&child);
    Ok((child, guard))
}

/// Kill the child's whole process tree and reap it.
/// Read one pipe to EOF, teeing into the run log. Memory keeps only the
/// last `MAX_BUF` bytes (the full stream lives in the log file).
const MAX_BUF: usize = 256 * 1024;

pub(crate) async fn read_pipe_tee(
    mut pipe: Option<tokio::process::ChildStdout>,
    log_file: &Option<std::path::PathBuf>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(mut p) = pipe.take() else { return out };
    let mut buf = [0u8; 8192];
    loop {
        match p.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                tee_log(log_file, &buf[..n]).await;
                out.extend_from_slice(&buf[..n]);
                if out.len() > MAX_BUF {
                    let drop = out.len() - MAX_BUF;
                    out.drain(..drop);
                }
            }
            Err(_) => break,
        }
    }
    out
}

pub(crate) async fn read_pipe_tee_err(
    mut pipe: Option<tokio::process::ChildStderr>,
    log_file: &Option<std::path::PathBuf>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(mut p) = pipe.take() else { return out };
    let mut buf = [0u8; 8192];
    loop {
        match p.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                tee_log(log_file, &buf[..n]).await;
                out.extend_from_slice(&buf[..n]);
                if out.len() > MAX_BUF {
                    let drop = out.len() - MAX_BUF;
                    out.drain(..drop);
                }
            }
            Err(_) => break,
        }
    }
    out
}

/// Append a chunk of child output to the run log (fire-and-forget; the
/// log is best-effort telemetry, not a sync point).
pub(crate) async fn tee_log(path: &Option<std::path::PathBuf>, data: &[u8]) {
    let Some(path) = path else { return };
    if let Ok(mut f) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        use tokio::io::AsyncWriteExt;
        let _ = f.write_all(data).await;
    }
}

pub(crate) async fn kill_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id().unwrap_or(0) as i32;
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        // taskkill /T kills the tree; /F forces. Failures are fine — the
        // child may already be gone.
        let pid = child.id().unwrap_or(0);
        let _ = tokio::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
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
    /// Shared usage sample sink (docker provider fills it with
    /// `docker stats` polls; the executor reads it into the run outcome).
    pub usage: Option<UsageSink>,
    /// Run log file: providers tee their child's output here as it arrives
    /// (the tool's own output belongs in the run log, live).
    pub log_file: Option<std::path::PathBuf>,
}

/// (peak memory bytes, accumulated cpu seconds) shared between a provider
/// and the executor.
pub type UsageSink = std::sync::Arc<std::sync::Mutex<(Option<u64>, Option<f64>)>>;

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
    TwoStageRsync(rsync::TwoStageRsyncProvider),
    Script(script::ScriptProvider),
    Docker(docker::DockerProvider),
    Git(git::GitProvider),
    Http(http::HttpProvider),
}

impl Provider {
    pub fn name(&self) -> &'static str {
        match self {
            Provider::Rsync(_) => "rsync",
            Provider::TwoStageRsync(_) => "two-stage-rsync",
            Provider::Script(_) => "script",
            Provider::Docker(_) => "docker",
            Provider::Git(_) => "git",
            Provider::Http(_) => "http",
        }
    }

    pub async fn sync(&self, ctx: &SyncContext) -> Result<SyncResult, ProviderError> {
        match self {
            Provider::Rsync(p) => p.sync(ctx).await,
            Provider::TwoStageRsync(p) => p.sync(ctx).await,
            Provider::Script(p) => p.sync(ctx).await,
            Provider::Docker(p) => p.sync(ctx).await,
            Provider::Git(p) => p.sync(ctx).await,
            Provider::Http(p) => p.sync(ctx).await,
        }
    }
}

/// Build the provider for a job.
pub fn build_provider(job: &JobSpec) -> Result<Provider, ProviderError> {
    match &job.provider {
        synora_core::ProviderConfig::Rsync { options, exclude } => {
            Ok(Provider::Rsync(rsync::RsyncProvider {
                options: options.clone(),
                exclude: exclude.clone(),
            }))
        }
        synora_core::ProviderConfig::Script { command } => {
            Ok(Provider::Script(script::ScriptProvider {
                command: command.clone(),
            }))
        }
        synora_core::ProviderConfig::Docker {
            image,
            env,
            volumes,
            keep_container,
            command,
        } => Ok(Provider::Docker(docker::DockerProvider {
            image: image.clone(),
            env: env.clone(),
            volumes: volumes.clone(),
            keep_container: *keep_container,
            command: command.clone(),
        })),
        synora_core::ProviderConfig::Git { branch } => Ok(Provider::Git(git::GitProvider {
            branch: branch.clone(),
        })),
        synora_core::ProviderConfig::TwoStageRsync {
            options,
            exclude,
            stage1_profile,
        } => Ok(Provider::TwoStageRsync(rsync::TwoStageRsyncProvider {
            options: options.clone(),
            exclude: exclude.clone(),
            stage1_profile: stage1_profile.clone(),
        })),
        synora_core::ProviderConfig::Http {
            parser,
            delete,
            threads,
        } => Ok(Provider::Http(http::HttpProvider {
            parser: parser.clone(),
            delete: *delete,
            threads: *threads,
        })),
    }
}
