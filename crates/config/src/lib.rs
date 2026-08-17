//! TOML configuration with `include`, `${VAR}` expansion, layering and
//! `file:line` validation (spec §42–§44).

pub mod error;
mod loader;
mod schema;

pub use error::ConfigError;
pub use loader::{
    ApiConfig, ApiToken, CgroupConfig, CliOverrides, ConfigLoader, DaemonConfig, DbConfig, DbKind,
    EgressConfig, EgressGroupConfig, NotificationConfig, ProxyConfig, ProxyGroupConfig, ProxyKind,
    ResolvedConfig, StorageConfig, StorageKind, TlsConfig,
};
