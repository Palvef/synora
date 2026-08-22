//! Repository backends (spec §30–§31, §34, §104).
//!
//! [`StorageManager`] ensures a configured repository backend exists — a
//! plain directory, a ZFS dataset or a Btrfs subvolume — optionally
//! auto-creating it, returns its mountpoint path, and provides free-space
//! checks for the job-run gate (spec §51).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use config::{StorageConfig, StorageKind};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// A shelled-out command failed, or another operational error.
    #[error("{0}")]
    Command(String),
    /// Storage name is not configured, or the backend does not exist.
    #[error("storage backend not found")]
    NotFound,
    /// The target directory is non-empty while `require_empty` is set.
    #[error("storage target is not empty (require_empty)")]
    NotEmpty,
    /// The backend kind cannot be used here (e.g. binary not installed).
    #[error("{0}")]
    Unsupported(String),
}

/// Manage repository backends (spec §30–§31, §34, §104).
pub struct StorageManager {
    storages: HashMap<String, StorageConfig>,
}

impl StorageManager {
    /// All configured storages by name (config crate's HashMap).
    pub fn new(storages: &HashMap<String, StorageConfig>) -> Self {
        Self {
            storages: storages.clone(),
        }
    }

    /// Ensure the repository backend exists and return its mountpoint path.
    ///
    /// - Dir: `mkdir -p`.
    /// - Zfs: `zfs list <pool>/<dataset>`; if missing and `auto_create`:
    ///   `zfs create [-o k=v ...] pool/dataset`, then
    ///   `zfs set mountpoint=<path> pool/dataset`, then `mkdir -p` the
    ///   mountpoint (default `/pool/dataset` when none is configured).
    /// - Btrfs: `btrfs subvolume list <mount>`; if missing and `auto_create`:
    ///   `btrfs subvolume create <path>` (the subvol string is the path).
    ///
    /// `require_empty` errors when the target directory is non-empty.
    /// Existing datasets/subvolumes are never destroyed (spec §104).
    pub async fn ensure(&self, name: &str) -> Result<PathBuf, StorageError> {
        let cfg = self.storages.get(name).ok_or(StorageError::NotFound)?;
        match &cfg.kind {
            StorageKind::Dir => {
                let mount = cfg.mountpoint.as_ref().ok_or_else(|| {
                    StorageError::Unsupported("dir storage requires a mountpoint".into())
                })?;
                tokio::fs::create_dir_all(mount).await.map_err(|e| {
                    StorageError::Command(format!("mkdir -p {}: {e}", mount.display()))
                })?;
                self.check_empty(mount, cfg.require_empty).await?;
                Ok(mount.clone())
            }
            StorageKind::Zfs {
                pool,
                dataset,
                options,
            } => {
                let ds = zfs_dataset_id(pool, dataset);
                let mount = cfg
                    .mountpoint
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(format!("/{ds}")));
                match run_cli("zfs", &["list", ds.as_str()]).await {
                    Ok(_) => {}
                    Err(e @ StorageError::Unsupported(_)) => return Err(e),
                    Err(_) if cfg.auto_create => {
                        let mut args: Vec<String> = vec!["create".into()];
                        for (k, v) in options {
                            args.push("-o".into());
                            args.push(format!("{k}={v}"));
                        }
                        args.push(ds.clone());
                        let args: Vec<&str> = args.iter().map(String::as_str).collect();
                        run_cli("zfs", &args).await?;
                        let mountpoint = format!("mountpoint={}", to_arg(&mount));
                        run_cli("zfs", &["set", mountpoint.as_str(), ds.as_str()]).await?;
                    }
                    Err(_) => {
                        return Err(StorageError::Command(format!(
                            "zfs dataset `{ds}` not found"
                        )));
                    }
                }
                tokio::fs::create_dir_all(&mount).await.map_err(|e| {
                    StorageError::Command(format!("mkdir -p {}: {e}", mount.display()))
                })?;
                self.check_empty(&mount, cfg.require_empty).await?;
                Ok(mount)
            }
            StorageKind::Btrfs { subvol } => {
                let subvol_path = PathBuf::from(subvol);
                let list_target = cfg
                    .mountpoint
                    .clone()
                    .unwrap_or_else(|| subvol_path.clone());
                let target = to_arg(&list_target);
                match run_cli("btrfs", &["subvolume", "list", target.as_str()]).await {
                    Ok(_) => {}
                    Err(e @ StorageError::Unsupported(_)) => return Err(e),
                    Err(_) if cfg.auto_create => {
                        run_cli("btrfs", &["subvolume", "create", subvol.as_str()]).await?;
                    }
                    Err(_) => return Err(StorageError::NotFound),
                }
                self.check_empty(&subvol_path, cfg.require_empty).await?;
                Ok(subvol_path)
            }
        }
    }

    /// Free bytes on the filesystem containing `path` (statvfs via libc).
    pub async fn free_bytes(&self, path: &Path) -> Result<u64, StorageError> {
        use std::os::unix::ffi::OsStrExt;
        let cpath = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| StorageError::Command(format!("path contains NUL: {}", path.display())))?;
        let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut vfs) };
        if rc != 0 {
            return Err(StorageError::Command(format!(
                "statvfs({}): {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        // f_frsize / f_bavail are u64 on Linux.
        Ok(vfs.f_frsize * vfs.f_bavail)
    }

    /// Check the configured min-free threshold (job-run gate, spec §51).
    pub async fn check_min_free(
        &self,
        path: &Path,
        min_free_bytes: Option<u64>,
    ) -> Result<(), StorageError> {
        let Some(min) = min_free_bytes else {
            return Ok(());
        };
        let free = self.free_bytes(path).await?;
        if free < min {
            return Err(StorageError::Command(format!(
                "insufficient free space on {}: {} bytes free, {} required",
                path.display(),
                free,
                min
            )));
        }
        Ok(())
    }

    async fn check_empty(&self, dir: &Path, require: bool) -> Result<(), StorageError> {
        if !require {
            return Ok(());
        }
        let mut rd = match tokio::fs::read_dir(dir).await {
            Ok(rd) => rd,
            // A missing directory is vacuously empty.
            Err(_) => return Ok(()),
        };
        if rd
            .next_entry()
            .await
            .map_err(|e| StorageError::Command(format!("read_dir({}): {e}", dir.display())))?
            .is_some()
        {
            return Err(StorageError::NotEmpty);
        }
        Ok(())
    }
}

/// Run a CLI tool with an argv array. A non-zero exit becomes
/// [`StorageError::Command`]; a missing binary (spawn `NotFound`) becomes
/// [`StorageError::Unsupported`] with a clear message.
async fn run_cli(cmd: &str, args: &[&str]) -> Result<String, StorageError> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StorageError::Unsupported(format!(
                "`{cmd}` is not installed; cannot manage this storage backend"
            )),
            _ => StorageError::Command(format!("failed to run `{cmd}`: {e}")),
        })?;
    if !out.status.success() {
        return Err(StorageError::Command(format!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn zfs_dataset_id(pool: &str, dataset: &str) -> String {
    let dataset = dataset.trim();
    if dataset.is_empty() {
        pool.to_string()
    } else if dataset.contains('/') {
        dataset.to_string()
    } else {
        format!("{pool}/{dataset}")
    }
}

fn to_arg(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(
        kind: StorageKind,
        mountpoint: Option<&str>,
        auto_create: bool,
        require_empty: bool,
    ) -> StorageConfig {
        StorageConfig {
            kind,
            mountpoint: mountpoint.map(PathBuf::from),
            auto_create,
            require_empty,
        }
    }

    fn manager(pairs: Vec<(&str, StorageConfig)>) -> StorageManager {
        StorageManager::new(
            &pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<HashMap<_, _>>(),
        )
    }

    /// Fresh temp dir for one test (removed up front, never across tests).
    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("synora-storage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn have(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn ensure_dir_creates_and_returns_path() {
        let dir = tmp("dir");
        let m = manager(vec![(
            "repo",
            cfg(StorageKind::Dir, Some(dir.to_str().unwrap()), true, false),
        )]);
        let got = m.ensure("repo").await.unwrap();
        assert_eq!(got, dir);
        assert!(got.is_dir());
    }

    #[tokio::test]
    async fn ensure_unknown_name_is_not_found() {
        let m = manager(vec![]);
        assert!(matches!(
            m.ensure("nope").await,
            Err(StorageError::NotFound)
        ));
    }

    #[tokio::test]
    async fn require_empty_rejects_nonempty_target() {
        let dir = tmp("nonempty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file"), b"x").unwrap();
        let m = manager(vec![(
            "repo",
            cfg(StorageKind::Dir, Some(dir.to_str().unwrap()), true, true),
        )]);
        assert!(matches!(
            m.ensure("repo").await,
            Err(StorageError::NotEmpty)
        ));

        // Empty target passes.
        let empty = tmp("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let m = manager(vec![(
            "repo",
            cfg(StorageKind::Dir, Some(empty.to_str().unwrap()), true, true),
        )]);
        assert!(m.ensure("repo").await.is_ok());
    }

    #[tokio::test]
    async fn free_bytes_and_min_free_gate() {
        let dir = tmp("free");
        std::fs::create_dir_all(&dir).unwrap();
        let m = manager(vec![]);
        let free = m.free_bytes(&dir).await.unwrap();
        assert!(free > 0);
        m.check_min_free(&dir, Some(0)).await.unwrap();
        m.check_min_free(&dir, None).await.unwrap();
        assert!(m.check_min_free(&dir, Some(u64::MAX)).await.is_err());
    }

    /// zfs/btrfs are not installed on this host; the missing binary must
    /// surface as Unsupported before any shell-out can happen.
    #[tokio::test]
    async fn zfs_ensure_missing_binary_is_unsupported() {
        if have("zfs") {
            eprintln!("zfs present on host; skipping gate test");
            return;
        }
        let m = manager(vec![(
            "z",
            cfg(
                StorageKind::Zfs {
                    pool: "tank".into(),
                    dataset: "data".into(),
                    options: vec![("compression".into(), "lz4".into())],
                },
                Some("/tmp/synora-zfs-mount"),
                true,
                false,
            ),
        )]);
        assert!(matches!(
            m.ensure("z").await,
            Err(StorageError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn btrfs_ensure_missing_binary_is_unsupported() {
        if have("btrfs") {
            eprintln!("btrfs present on host; skipping gate test");
            return;
        }
        let m = manager(vec![(
            "b",
            cfg(
                StorageKind::Btrfs {
                    subvol: "/srv/mirror".into(),
                },
                None,
                true,
                false,
            ),
        )]);
        assert!(matches!(
            m.ensure("b").await,
            Err(StorageError::Unsupported(_))
        ));
    }

    #[test]
    fn zfs_dataset_id_pool_root_when_empty() {
        assert_eq!(zfs_dataset_id("datas", ""), "datas");
        assert_eq!(zfs_dataset_id("datas", "  "), "datas");
        assert_eq!(zfs_dataset_id("datas", "mirror"), "datas/mirror");
        assert_eq!(zfs_dataset_id("datas", "datas/GXDE"), "datas/GXDE");
    }
}
