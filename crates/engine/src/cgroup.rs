//! cgroup v2 scopes for run resource limits (user-requested feature;
//! tunasync has the same per-mirror memory_limit). Each run gets a scope
//! `<base>/synora/<job>-<run8>` with memory.max / cpu.max; the spawned child
//! is attached via cgroup.procs (children inherit on fork). Scope is removed
//! after the run. cgroup v1 or missing mounts degrade to "no limits" with a
//! warning — never fail the run because of it.

use std::path::{Path, PathBuf};

pub struct CgroupScope {
    path: PathBuf,
}

impl CgroupScope {
    /// Create the scope. Returns None when cgroups are unavailable/not
    /// writable — limits are then simply not enforced (warned by the caller).
    pub fn create(
        base: &Path,
        job: &str,
        run_id: &str,
        memory_limit: Option<u64>,
        cpu_limit: Option<f64>,
    ) -> Option<CgroupScope> {
        if !is_cgroup_v2() {
            return None;
        }
        let short = run_id.get(..8).unwrap_or(run_id);
        let path = base.join(format!("{job}-{short}"));
        std::fs::create_dir_all(&path).ok()?;
        if let Some(mem) = memory_limit {
            let _ = std::fs::write(path.join("memory.max"), mem.to_string());
        }
        if let Some(cpu) = cpu_limit {
            // cpu.max: "QUOTA PERIOD" in microseconds; period fixed at 100ms.
            let quota = (cpu * 100_000.0) as u64;
            let _ = std::fs::write(path.join("cpu.max"), format!("{quota} 100000"));
        }
        Some(CgroupScope { path })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Move the child (and, via inheritance, everything it forks) into the
    /// scope. Must run right after spawn, before the child forks workers.
    pub fn attach(&self, pid: u32) -> std::io::Result<()> {
        std::fs::write(self.path.join("cgroup.procs"), pid.to_string())?;
        Ok(())
    }

    /// Current memory usage (bytes) + accumulated CPU seconds, for metrics.
    pub fn usage(&self) -> Option<(u64, f64)> {
        let mem = std::fs::read_to_string(self.path.join("memory.current"))
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()?;
        let cpu_usec = std::fs::read_to_string(self.path.join("cpu.stat"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("usage_usec "))
                    .and_then(|v| v.trim().parse::<u64>().ok())
            })
            .unwrap_or(0);
        Some((mem, cpu_usec as f64 / 1_000_000.0))
    }

    /// Remove the scope. Children must have exited; rmdir fails if busy —
    /// that is fine (a leftover scope is harmless; the next run reuses the
    /// name and overwrites limits).
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

fn is_cgroup_v2() -> bool {
    // cgroup v2 unified hierarchy: the controllers file exists at the root.
    Path::new("/sys/fs/cgroup/cgroup.controllers").exists()
}
