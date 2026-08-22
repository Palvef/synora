//! TOML configuration with `include`, `${VAR}` expansion, layering and
//! `file:line` validation (spec §42–§44).

pub mod error;
mod jobfile;
mod loader;
mod schema;

pub use error::ConfigError;
pub use jobfile::remove_job_block;
pub use loader::{
    ApiConfig, ApiToken, CgroupConfig, CliOverrides, ConfigLoader, DaemonConfig, DbConfig, DbKind,
    EgressConfig, EgressGroupConfig, NotificationConfig, ProxyConfig, ProxyGroupConfig, ProxyKind,
    ResolvedConfig, StorageConfig, StorageKind, TlsConfig,
};

use std::path::PathBuf;

/// Runtime directory for pid files. Logs stay in `[daemon] log_dir`.
pub const DEFAULT_RUNTIME_DIR: &str = "/run/synora";

pub fn runtime_dir() -> PathBuf {
    match std::env::var("SYNORA_RUNTIME_DIR") {
        Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
        _ => PathBuf::from(DEFAULT_RUNTIME_DIR),
    }
}

pub fn pid_path(component: &str) -> PathBuf {
    runtime_dir().join(format!("{component}.pid"))
}

pub fn write_pid_file(component: &str) -> Result<PathBuf, String> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = pid_path(component);
    std::fs::write(&path, format!("{}\n", std::process::id()))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

pub fn remove_pid_file(component: &str) {
    let _ = std::fs::remove_file(pid_path(component));
}

pub fn read_pid_file(component: &str) -> Result<i32, String> {
    let path = pid_path(component);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "cannot read {}: {e} (is the daemon running?)",
            path.display()
        )
    })?;
    text.trim()
        .parse()
        .map_err(|e| format!("bad pid file {}: {e}", path.display()))
}

#[cfg(test)]
mod pid_tests {
    use super::*;

    #[test]
    fn pid_path_is_under_run_synora() {
        assert_eq!(
            PathBuf::from(DEFAULT_RUNTIME_DIR).join("manager.pid"),
            PathBuf::from("/run/synora/manager.pid")
        );
    }
}
