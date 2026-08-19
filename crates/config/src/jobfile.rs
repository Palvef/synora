//! Rewrite `[[jobs]]` blocks on disk (TUI / CLI / manager delete).

use std::path::{Path, PathBuf};

/// Remove every `[[jobs]]` block named `job_name` under the config tree.
/// Empty job-only files (not the main config) are deleted.
pub fn remove_job_block(config_path: &Path, job_name: &str) -> Result<Vec<PathBuf>, String> {
    let dir = config_path.parent().unwrap_or(Path::new("."));
    let needle = format!("name = \"{job_name}\"");
    let mut changed = Vec::new();
    for entry in glob::glob(&format!("{}/**/*.toml", dir.display()))
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let text = std::fs::read_to_string(&entry).map_err(|e| e.to_string())?;
        if !text.contains(&needle) {
            continue;
        }
        let Some(rebuilt) = strip_job_blocks(&text, job_name) else {
            continue;
        };
        let is_main = entry == config_path
            || entry.file_name().and_then(|n| n.to_str()) == Some("synora.toml");
        if !is_main && !rebuilt.contains("[[jobs]]") && only_blank_or_comments(&rebuilt) {
            std::fs::remove_file(&entry).map_err(|e| e.to_string())?;
        } else {
            std::fs::write(&entry, rebuilt).map_err(|e| e.to_string())?;
        }
        changed.push(entry);
    }
    if changed.is_empty() {
        Err(format!("job `{job_name}` not found in any config file"))
    } else {
        Ok(changed)
    }
}

fn strip_job_blocks(text: &str, job_name: &str) -> Option<String> {
    let needle = format!("name = \"{job_name}\"");
    let parts: Vec<&str> = text.split("[[jobs]]").collect();
    if parts.len() == 1 {
        return None;
    }
    let mut rebuilt = String::new();
    rebuilt.push_str(parts[0]);
    let mut removed = false;
    for part in &parts[1..] {
        if part.contains(&needle) {
            if let Some(rest) = trailing_non_job(part) {
                rebuilt.push_str(rest);
            }
            removed = true;
            continue;
        }
        rebuilt.push_str("[[jobs]]");
        rebuilt.push_str(part);
    }
    if !removed {
        return None;
    }
    Some(collapse_blank_lines(&rebuilt))
}

/// After a deleted job block, keep a following top-level section (`[storage]` etc.).
fn trailing_non_job(part: &str) -> Option<&str> {
    let mut offset = 0;
    for (i, line) in part.lines().enumerate() {
        let t = line.trim();
        if i > 0 && t.starts_with('[') && !t.starts_with("[jobs.") && !t.starts_with("[[jobs]]") {
            return Some(&part[offset..]);
        }
        offset += line.len();
        if offset < part.len() {
            offset += 1; // newline
        }
    }
    None
}

fn only_blank_or_comments(text: &str) -> bool {
    text.lines().all(|l| {
        let t = l.trim();
        t.is_empty() || t.starts_with('#')
    })
}

fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::new();
    let mut blanks = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_one_job_and_keeps_neighbor() {
        let text = "[[jobs]]\nname = \"a\"\nprovider = \"rsync\"\n\n[[jobs]]\nname = \"b\"\nprovider = \"git\"\n";
        let out = strip_job_blocks(text, "a").unwrap();
        assert!(!out.contains("name = \"a\""), "{out}");
        assert!(out.contains("name = \"b\""), "{out}");
        assert_eq!(out.matches("[[jobs]]").count(), 1);
    }

    #[test]
    fn does_not_leave_empty_jobs_header() {
        let text = "[[jobs]]\nname = \"only\"\nprovider = \"rsync\"\n";
        let out = strip_job_blocks(text, "only").unwrap();
        assert!(!out.contains("[[jobs]]"), "{out}");
        assert!(!out.contains("name = \"only\""), "{out}");
    }
}
