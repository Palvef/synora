//! Per-run log files (spec §49): `current.log` holds the latest run,
//! `YYYY-MM-DD.log` accumulates. Plain std writes are fine at mirror-sync
//! scale (ponytail: switch to a blocking writer if logs become a bottleneck).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

pub struct RunLogger {
    current: File,
    dated: File,
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
        Ok(RunLogger { current, dated })
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
        Ok(())
    }

    /// Raw provider output (stdout/stderr), written to both files.
    pub fn raw(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.current.write_all(data)?;
        self.dated.write_all(data)?;
        if !data.is_empty() && data.last() != Some(&b'\n') {
            self.current.write_all(b"\n")?;
            self.dated.write_all(b"\n")?;
        }
        Ok(())
    }
}

/// Recursive file-size walk (statistics = "filesystem", spec §58).
/// Does not follow symlinks.
pub fn walk_size(root: &Path) -> u64 {
    let mut total = 0u64;
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
                total += meta.len();
            }
        }
    }
    total
}
