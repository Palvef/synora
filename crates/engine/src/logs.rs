//! Unique per-run logs with an atomic current.log symlink, bounded output,
//! and retention. Legacy daily logs expire after 30 days.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

pub struct RunLogger {
    run: File,
}
const MAX_RUN_LOG_BYTES: u64 = 16 * 1024 * 1024;
impl RunLogger {
    pub fn open(log_dir: &Path, job_name: &str) -> std::io::Result<RunLogger> {
        if job_name.is_empty()
            || job_name == "."
            || job_name.contains("..")
            || job_name.contains('/')
            || job_name.contains('\\')
            || job_name.contains('\0')
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid job name",
            ));
        }
        let dir = log_dir.join(job_name);
        std::fs::create_dir_all(&dir)?;
        let now = time::OffsetDateTime::now_utc();
        let filename = format!(
            "{job_name}_{}-{}.log",
            now.unix_timestamp(),
            synora_core::RunId::new()
        );
        let run = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(dir.join(&filename))?;
        let link = dir.join(format!(".current-{}", synora_core::RunId::new()));
        std::os::unix::fs::symlink(&filename, &link)?;
        std::fs::rename(&link, dir.join("current.log"))?;
        prune_run_logs(&dir, job_name, 20);
        prune_daily_logs(&dir);
        Ok(Self { run })
    }
    pub fn line(&mut self, msg: &str) -> std::io::Result<()> {
        let ts = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        self.raw(format!("{ts} {msg}\n").as_bytes())
    }
    pub fn raw(&mut self, data: &[u8]) -> std::io::Result<()> {
        let remaining = MAX_RUN_LOG_BYTES.saturating_sub(self.run.metadata()?.len()) as usize;
        self.run.write_all(&data[..data.len().min(remaining)])
    }
}
fn prune_daily_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = time::OffsetDateTime::now_utc().date() - time::Duration::days(30);
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let Some(date) = name.to_str().and_then(|s| s.strip_suffix(".log")) else {
            continue;
        };
        if time::Date::parse(
            date,
            &time::macros::format_description!("[year]-[month]-[day]"),
        )
        .is_ok_and(|d| d < cutoff)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Recursive file-size walk (statistics = "filesystem", spec §58).
/// Does not follow symlinks.
pub fn walk_size(root: &Path) -> u64 {
    walk(root).1
}

/// Best-effort repository size: prefer ZFS `used` (instant, matches the
/// on-disk mirror) and never fall back to a full tree walk — alpine/AOSP
/// are multi-terabyte.
pub fn measure_repo_size(root: &Path) -> Option<u64> {
    zfs_used_for(root).or_else(|| btrfs_used_for(root))
}

/// Parse `zfs list -Hp -o used,mountpoint` and pick the longest mountpoint
/// that equals `root` (exact match only — the pool root would otherwise
/// report 70T+ for every job).
pub fn zfs_used_for(root: &Path) -> Option<u64> {
    let out = command_runner::run_sync("zfs", &["list", "-Hp", "-o", "used,mountpoint"]).ok()?;
    if !out.status.success() {
        return None;
    }
    parse_zfs_used(&String::from_utf8_lossy(&out.stdout), root)
}

/// qgroup referenced bytes for this exact subvolume; unavailable quotas return
/// unknown, never the whole filesystem's usage for one repository.
fn btrfs_used_for(root: &Path) -> Option<u64> {
    let path = root.to_str()?;
    let id = command_runner::run_sync("btrfs", &["inspect-internal", "rootid", path]).ok()?;
    if !id.status.success() {
        return None;
    }
    let id: u64 = String::from_utf8_lossy(&id.stdout).trim().parse().ok()?;
    let out = command_runner::run_sync("btrfs", &["qgroup", "show", "--raw", path]).ok()?;
    if !out.status.success() {
        return None;
    }
    let key = format!("0/{id}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next()? != key {
                return None;
            }
            fields.next()?.parse().ok()
        })
}

pub fn parse_zfs_used(text: &str, root: &Path) -> Option<u64> {
    let want = root.to_string_lossy().trim_end_matches('/').to_string();
    let mut best: Option<(usize, u64)> = None;
    for line in text.lines() {
        let mut parts = line.split('\t');
        let (Some(used_s), Some(mp)) = (parts.next(), parts.next()) else {
            continue;
        };
        let mp = mp.trim().trim_end_matches('/');
        if mp.is_empty() {
            continue;
        }
        if want == mp {
            if let Ok(used) = used_s.parse::<u64>() {
                let score = mp.len();
                if best.map(|(s, _)| score >= s).unwrap_or(true) {
                    best = Some((score, used));
                }
            }
        }
    }
    best.map(|(_, used)| used)
}

/// Remove per-run log files beyond the newest `keep` (named
/// `<job>_<timestamp>.log`).
fn prune_run_logs(dir: &Path, job_name: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&format!("{job_name}_")) && n.ends_with(".log"))
                .unwrap_or(false)
        })
        .filter_map(|p| {
            std::fs::metadata(&p)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| (t, p))
        })
        .collect();
    files.sort_by_key(|(t, _)| *t);
    let excess = files.len().saturating_sub(keep);
    for p in files.iter().take(excess).map(|(_, p)| p) {
        let _ = std::fs::remove_file(p);
    }
}

/// (file count, total bytes) of a repository tree. Missing roots measure
/// empty (a repository that doesn't exist yet has nothing to protect).
pub fn walk(root: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.file_type().is_dir() {
                stack.push(entry.path());
            } else if meta.file_type().is_file() {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    (files, bytes)
}

#[cfg(test)]
mod tests {
    use super::parse_zfs_used;
    use std::path::Path;

    #[test]
    fn zfs_used_exact_mountpoint_only() {
        let text = "\
83902267392\t/datas/adobe-fonts
1530082091008\t/datas/docker-ce
1975684956160\t/datas/git/AOSP
85899345920\t/datas
";
        assert_eq!(
            parse_zfs_used(text, Path::new("/datas/git/AOSP")),
            Some(1975684956160)
        );
        assert_eq!(
            parse_zfs_used(text, Path::new("/datas/adobe-fonts")),
            Some(83902267392)
        );
        assert_eq!(parse_zfs_used(text, Path::new("/datas/missing")), None);
        // Pool root must not be used as a fallback for a child dataset.
        assert_eq!(parse_zfs_used(text, Path::new("/datas/rubygems")), None);
    }
}

#[cfg(test)]
mod path_safety_tests {
    use super::*;

    #[test]
    fn reject_log_paths_before_creating_files() {
        let root =
            std::env::temp_dir().join(format!("synora-log-path-{}", synora_core::RunId::new()));
        for name in [
            "",
            ".",
            "..",
            "../outside",
            "/tmp/outside",
            "a/b",
            "a\\b",
            "a\0b",
        ] {
            let err = RunLogger::open(&root, name)
                .err()
                .expect("unsafe name accepted");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
        assert!(!root.exists());
        let mut logger = RunLogger::open(&root, "ubuntu-24.04").unwrap();
        logger.line("safe log").unwrap();
        assert!(
            std::fs::read_to_string(root.join("ubuntu-24.04/current.log"))
                .unwrap()
                .contains("safe log")
        );
        drop(logger);
        std::fs::remove_dir_all(root).unwrap();
    }
}
