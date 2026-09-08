//! Bounded control commands. Streaming providers use their existing process-group
//! cancellation and bounded tail collectors; this runner covers CLI probes/storage.
use std::{
    io,
    process::{Output, Stdio},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt};
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub type StorageLocks = std::sync::Arc<Vec<std::fs::File>>;
tokio::task_local! { static STORAGE_LOCK: StorageLocks; }

pub async fn with_storage_lock<F: std::future::Future>(lock: StorageLocks, future: F) -> F::Output {
    STORAGE_LOCK.scope(lock, Box::pin(future)).await
}

fn current_storage_lock() -> Option<StorageLocks> {
    STORAGE_LOCK.try_with(Clone::clone).ok()
}

struct Group(i32);
impl Drop for Group {
    fn drop(&mut self) {
        if self.0 > 0 {
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }
}
async fn drain(mut pipe: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            return Ok(out);
        }
        if out.len().saturating_add(n) > MAX_OUTPUT_BYTES {
            return Err(io::Error::other("command output exceeds 1 MiB"));
        }
        out.extend_from_slice(&buf[..n]);
    }
}
/// Fails on timeout/output overflow instead of returning a truncated parser input.
/// Dropping the future kills the complete process group.
pub async fn run(cmd: &mut tokio::process::Command, timeout: Duration) -> io::Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let lock = current_storage_lock();
    if let Some(files) = &lock {
        use std::os::fd::AsRawFd;
        let fds: Vec<_> = files.iter().map(AsRawFd::as_raw_fd).collect();
        unsafe {
            cmd.pre_exec(move || {
                for fd in &fds {
                    if libc::fcntl(*fd, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn()?;
    let group = Group(child.id().unwrap_or(0) as i32);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr"))?;
    let result = tokio::time::timeout(timeout, async {
        tokio::try_join!(child.wait(), drain(stdout), drain(stderr))
    })
    .await;
    match result {
        Ok(Ok((status, stdout, stderr))) => {
            // Also terminate descendants that closed their inherited pipes.
            drop(group);
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        }
        result => {
            drop(group);
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(match result {
                Ok(Err(e)) => e,
                _ => io::Error::new(io::ErrorKind::TimedOut, "command timed out"),
            })
        }
    }
}
/// Blocking callers (snapshot CLI/size probes) share exactly the same limits.
pub fn run_sync(cmd: &str, args: &[&str]) -> io::Result<Output> {
    let mut command = std::process::Command::new(cmd);
    command.args(args);
    run_sync_command(command)
}

pub fn run_sync_command(command: std::process::Command) -> io::Result<Output> {
    let lock = current_storage_lock();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async move {
            let mut command = tokio::process::Command::from(command);
            match lock {
                Some(lock) => with_storage_lock(lock, run(&mut command, DEFAULT_TIMEOUT)).await,
                None => run(&mut command, DEFAULT_TIMEOUT).await,
            }
        })
    })
    .join()
    .map_err(|_| io::Error::other("command runner panicked"))?
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn bounds_output_and_runtime() {
        let e = run(
            tokio::process::Command::new("sh").args(["-c", "yes output"]),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Other);
        let e = run(
            tokio::process::Command::new("sh").args(["-c", "sleep 30"]),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        let out = run(
            tokio::process::Command::new("sh").args(["-c", "printf ok; printf error >&2; exit 7"]),
            DEFAULT_TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(out.status.code(), Some(7));
        assert_eq!(out.stdout, b"ok");
        assert_eq!(out.stderr, b"error");
    }
}
