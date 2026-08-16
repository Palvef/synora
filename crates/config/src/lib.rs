//! TOML configuration with `include`, `${VAR}` expansion, layering and
//! `file:line` validation (spec §42–§44).

pub mod error;
mod loader;
mod schema;

pub use error::ConfigError;
pub use loader::{
    ApiConfig, ApiToken, CliOverrides, ConfigLoader, DaemonConfig, DbConfig, DbKind,
    ResolvedConfig, TlsConfig,
};
