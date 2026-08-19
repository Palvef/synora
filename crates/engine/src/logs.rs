//! Per-run log files (spec §49): `current.log` holds the latest run,
//! `YYYY-MM-DD.log` accumulates. Plain std writes are fine at mirror-sync
//! scale (ponytail: switch to a blocking writer if logs become a bottleneck).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

pub struct RunLogger {
    current: File,
    dated: File,
    /// Per-run file, tunasync naming: `<job>_<YYYY-MM-DD_HH_MM>.log`
    /// inside the job's own directory (`log_dir/<job>/`).
    run: File,
}

impl RunLogger {
    pub fn open(log_dir: &Path, job_name: &str) -> std::io::Result<RunLogger> {
        let dir = log_dir.join(job_name);
        std::fs::create_dir_all(&dir)?;
        let current = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dir.join("current.log"))?;
        let today = time::OffsetDateTime::now_utc().date();
        let dated = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{today}.log")))?;
        // Per-run log (tunasync convention: one file per run, named with
        // the mirror name and start timestamp).
        let now = time::OffsetDateTime::now_utc();
        let run_ts = now
            .format(
                &time::format_description::parse_borrowed::<2>(
                    "[year]-[month]-[day]_[hour]_[minute]",
                )
                .unwrap(),
            )
            .unwrap_or_else(|_| "unknown".into());
        let run = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{job_name}_{run_ts}.log")))?;
        // Keep the latest 20 per-run logs; older ones are removed (user
        // requirement).
        prune_run_logs(&dir, job_name, 20);
        Ok(RunLogger {
            current,
            dated,
            run,
        })
    }

    /// One timestamped line, written to both files.
    pub fn line(&mut self, msg: &str) -> std::io::Result<()> {
        let ts = time::OffsetDateTime::now_utc();
        let fmt = ts
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "?".into());
        let line = format!("{fmt} {msg}\n");
        self.current.write_all(line.as_bytes())?;
        self.dated.write_all(line.as_bytes())?;
        self.run.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Raw provider output (stdout/stderr), written to both files.
    pub fn raw(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.current.write_all(data)?;
        self.dated.write_all(data)?;
        self.run.write_all(data)?;
        if !data.is_empty() && data.last() != Some(&b'\n') {
            self.current.write_all(b"\n")?;
            self.dated.write_all(b"\n")?;
            self.run.write_all(b"\n")?;
        }
        Ok(())
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
    zfs_used_for(root)
}

/// Parse `zfs list -Hp -o used,mountpoint` and pick the longest mountpoint
/// that equals `root` (exact match only — the pool root would otherwise
/// report 70T+ for every job).
pub fn zfs_used_for(root: &Path) -> Option<u64> {
    let out = std::process::Command::new("zfs")
        .args(["list", "-Hp", "-o", "used,mountpoint"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_zfs_used(&String::from_utf8_lossy(&out.stdout), root)
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
