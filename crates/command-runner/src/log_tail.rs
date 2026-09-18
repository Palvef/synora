//! Bounded rolling logs: retain recent output instead of silently freezing at the cap.
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;

pub const LIMIT: usize = 16 * 1024 * 1024;
const MARKER: &[u8] = b"[synora: older output discarded; retaining recent log tail]\n";

pub fn append(file: &mut File, data: &[u8]) -> std::io::Result<()> {
    append_with_limit(file, data, LIMIT)
}

fn append_with_limit(file: &mut File, data: &[u8], limit: usize) -> std::io::Result<()> {
    struct Unlock(i32);
    impl Drop for Unlock {
        fn drop(&mut self) {
            unsafe {
                libc::flock(self.0, libc::LOCK_UN);
            }
        }
    }
    let fd = file.as_raw_fd();
    if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let _unlock = Unlock(fd);
    let length = file.metadata()?.len();
    let data = &data[data.len().saturating_sub(limit / 2)..];
    if length.saturating_add(data.len() as u64) > limit as u64 {
        let retained = (limit / 2)
            .saturating_sub(MARKER.len())
            .min(length as usize);
        file.seek(SeekFrom::End(-(retained as i64)))?;
        let mut tail = vec![0; retained];
        file.read_exact(&mut tail)?;
        let first_line = tail.iter().position(|b| *b == b'\n').map_or(0, |p| p + 1);
        file.set_len(0)?;
        file.write_all(MARKER)?;
        file.write_all(&tail[first_line..])?;
    }
    file.write_all(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    #[test]
    fn output_continues_after_cap_and_remains_bounded() {
        let path = std::env::temp_dir().join(format!(
            "synora-log-tail-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        for n in 0..100 {
            append_with_limit(&mut file, format!("progress {n:03}\n").as_bytes(), 256).unwrap();
            assert!(file.metadata().unwrap().len() <= 256);
        }
        let output = std::fs::read_to_string(&path).unwrap();
        assert!(output.contains("older output discarded"));
        assert!(output.ends_with("progress 099\n"));
        assert!(!output.contains("progress 000\n"));
        std::fs::remove_file(path).unwrap();
    }
}
