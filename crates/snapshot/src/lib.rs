//! Snapshot providers for ZFS / Btrfs and retention pruning (spec §32–§33).
//!
//! CLI invocations are thin wrappers over pure parsers, so the whole
//! retention logic is testable without the zfs/btrfs binaries (which are
//! not installed on every host). A missing binary surfaces as
//! [`SnapshotError::Unsupported`] before any command runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use config::StorageKind;
use synora_core::RetentionPolicy;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotInfo {
    pub name: String,
    /// Unix seconds; `0` when the provider reports no timestamp (Btrfs).
    pub created_at: i64,
}

pub trait SnapshotProvider: Send + Sync {
    fn create(&self, name: &str) -> Result<SnapshotInfo, SnapshotError>;
    fn delete(&self, name: &str) -> Result<(), SnapshotError>;
    fn list(&self) -> Result<Vec<SnapshotInfo>, SnapshotError>;
}

/// ZFS: `zfs snapshot pool/dataset@name`, `zfs destroy ...@name`,
/// `zfs list -H -t snapshot -o name,creation -s creation pool/dataset`.
#[derive(Debug, Clone)]
pub struct ZfsSnapshotProvider {
    pub pool_dataset: String,
}

/// Btrfs: `btrfs subvolume snapshot -r <subvol> <subvol>-<name>` (read-only),
/// `btrfs subvolume delete <path>`, list via `btrfs subvolume list -o <parent>`.
#[derive(Debug, Clone)]
pub struct BtrfsSnapshotProvider {
    pub subvol: PathBuf,
}

impl SnapshotProvider for ZfsSnapshotProvider {
    fn create(&self, name: &str) -> Result<SnapshotInfo, SnapshotError> {
        let snap = format!("{}@{name}", self.pool_dataset);
        run_cli("zfs", &["snapshot", snap.as_str()])?;
        Ok(SnapshotInfo {
            name: name.to_string(),
            created_at: OffsetDateTime::now_utc().unix_timestamp(),
        })
    }

    fn delete(&self, name: &str) -> Result<(), SnapshotError> {
        let snap = format!("{}@{name}", self.pool_dataset);
        run_cli("zfs", &["destroy", snap.as_str()])?;
        Ok(())
    }

    fn list(&self) -> Result<Vec<SnapshotInfo>, SnapshotError> {
        let out = run_cli(
            "zfs",
            &[
                "list",
                "-H",
                "-t",
                "snapshot",
                "-o",
                "name,creation",
                "-s",
                "creation",
                self.pool_dataset.as_str(),
            ],
        )?;
        Ok(parse_zfs_list(&out, &self.pool_dataset))
    }
}

impl SnapshotProvider for BtrfsSnapshotProvider {
    fn create(&self, name: &str) -> Result<SnapshotInfo, SnapshotError> {
        let sv = self.subvol.to_string_lossy().into_owned();
        let target = snapshot_path(&self.subvol, name).to_string_lossy().into_owned();
        run_cli("btrfs", &["subvolume", "snapshot", "-r", sv.as_str(), target.as_str()])?;
        // Btrfs snapshots carry no creation timestamp: 0 = unknown, and
        // pruning falls back to the timestamp in the name (spec §33).
        Ok(SnapshotInfo {
            name: name.to_string(),
            created_at: 0,
        })
    }

    fn delete(&self, name: &str) -> Result<(), SnapshotError> {
        let target = snapshot_path(&self.subvol, name).to_string_lossy().into_owned();
        run_cli("btrfs", &["subvolume", "delete", target.as_str()])?;
        Ok(())
    }

    fn list(&self) -> Result<Vec<SnapshotInfo>, SnapshotError> {
        let parent = self.subvol.parent().unwrap_or(&self.subvol);
        let p = parent.to_string_lossy().into_owned();
        let out = run_cli("btrfs", &["subvolume", "list", "-o", p.as_str()])?;
        Ok(parse_btrfs_list(&out))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// A shelled-out command failed, or another operational error.
    #[error("{0}")]
    Command(String),
    /// Snapshots cannot be managed here (e.g. binary not installed, or the
    /// storage kind has no snapshot support).
    #[error("{0}")]
    Unsupported(String),
}

/// Build a provider for a storage kind, or Err(Unsupported) for Dir.
pub fn provider_for(
    storage: &StorageKind,
    _mountpoint: &Path,
) -> Result<Box<dyn SnapshotProvider>, SnapshotError> {
    match storage {
        StorageKind::Dir => Err(SnapshotError::Unsupported(
            "dir storage does not support snapshots".into(),
        )),
        StorageKind::Zfs { pool, dataset, .. } => Ok(Box::new(ZfsSnapshotProvider {
            pool_dataset: format!("{pool}/{dataset}"),
        })),
        StorageKind::Btrfs { subvol } => Ok(Box::new(BtrfsSnapshotProvider {
            subvol: PathBuf::from(subvol),
        })),
    }
}

/// Snapshot name convention (spec §32): synora-YYYYMMDD-HHMMSS.
pub fn snapshot_name(now: OffsetDateTime) -> String {
    now.format(&time::macros::format_description!(
        "synora-[year][month][day]-[hour][minute][second]"
    ))
        .unwrap_or_default()
}

/// Retention pruning (spec §33): keep the newest N snapshots in each bucket.
/// Buckets: last (everything), daily (first snapshot per calendar day),
/// weekly (per ISO week), monthly (per calendar month). The kept set is the
/// union; returns the names to DELETE. Snapshots that don't match the
/// `synora-` naming convention are never deleted.
///
/// Snapshots are sorted by (created_at, name). Btrfs reports no timestamp
/// (created_at = 0), so the sort falls back to the name —
/// `synora-YYYYMMDD-HHMMSS` is lexicographically chronological. Calendar
/// buckets are computed in `now`'s offset, using the snapshot's timestamp
/// when known and the one embedded in its name otherwise.
pub fn prune_plan(
    snapshots: &[SnapshotInfo],
    policy: &RetentionPolicy,
    now: OffsetDateTime,
) -> Vec<String> {
    let offset = now.offset();
    let mut sorted: Vec<&SnapshotInfo> = snapshots
        .iter()
        .filter(|s| matches_synora(&s.name))
        .collect();
    sorted.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.name.cmp(&b.name)));

    let mut keep: HashSet<&str> = HashSet::new();
    if let Some(n) = policy.keep_last {
        keep.extend(sorted.iter().rev().take(n as usize).map(|s| s.name.as_str()));
    }
    if let Some(n) = policy.keep_daily {
        keep.extend(periodic_kept(&sorted, n, offset, |s, o| {
            effective_ts(s, o).map(|t| t.date())
        }));
    }
    if let Some(n) = policy.keep_weekly {
        keep.extend(periodic_kept(&sorted, n, offset, |s, o| {
            effective_ts(s, o).map(|t| iso_week_key(t.date()))
        }));
    }
    if let Some(n) = policy.keep_monthly {
        keep.extend(periodic_kept(&sorted, n, offset, |s, o| {
            effective_ts(s, o).map(|t| (t.year(), u8::from(t.month())))
        }));
    }

    sorted
        .iter()
        .filter(|s| !keep.contains(s.name.as_str()))
        .map(|s| s.name.clone())
        .collect()
}

/// Timestamp for calendar bucketing: the provider's when known, else the one
/// parsed from the `synora-YYYYMMDD-HHMMSS` name.
fn effective_ts(s: &SnapshotInfo, offset: UtcOffset) -> Option<OffsetDateTime> {
    if s.created_at > 0 {
        OffsetDateTime::from_unix_timestamp(s.created_at)
            .ok()
            .map(|t| t.to_offset(offset))
    } else {
        parse_snapshot_name(&s.name, offset)
    }
}

/// Oldest snapshot per period ("first snapshot per day"), keeping only the
/// representatives of the newest `n` periods.
fn periodic_kept<'a, K: Eq + std::hash::Hash>(
    sorted: &[&'a SnapshotInfo],
    n: u32,
    offset: UtcOffset,
    key_of: impl Fn(&SnapshotInfo, UtcOffset) -> Option<K>,
) -> HashSet<&'a str> {
    if n == 0 {
        return HashSet::new();
    }
    let mut seen: HashSet<K> = HashSet::new();
    let mut reps: Vec<&str> = Vec::new(); // one per period, first-seen (= oldest) order
    for s in sorted {
        if let Some(k) = key_of(s, offset) {
            if seen.insert(k) {
                reps.push(s.name.as_str());
            }
        }
    }
    reps.into_iter().rev().take(n as usize).collect()
}

fn matches_synora(name: &str) -> bool {
    parse_snapshot_name(name, UtcOffset::UTC).is_some()
}

/// ISO 8601 week key `(iso_year, week)`. The ISO week year is the year of
/// the week's Thursday (weeks can span calendar years).
fn iso_week_key(d: time::Date) -> (i32, u8) {
    let monday = d - time::Duration::days(i64::from(d.weekday().number_from_monday() - 1));
    let thursday = monday + time::Duration::days(3);
    (thursday.year(), d.iso_week())
}

/// Parse `synora-YYYYMMDD-HHMMSS` at `offset` (the name carries no zone).
fn parse_snapshot_name(name: &str, offset: UtcOffset) -> Option<OffsetDateTime> {
    let rest = name.strip_prefix("synora-")?;
    let (date_s, time_s) = rest.split_once('-')?;
    if date_s.len() != 8 || time_s.len() != 6 {
        return None;
    }
    let two = |s: &str| s.parse::<u8>().ok();
    let date = time::Date::from_calendar_date(
        date_s.get(0..4)?.parse::<i32>().ok()?,
        time::Month::try_from(two(date_s.get(4..6)?)?).ok()?,
        two(date_s.get(6..8)?)?,
    )
    .ok()?;
    let tod = time::Time::from_hms(
        two(time_s.get(0..2)?)?,
        two(time_s.get(2..4)?)?,
        two(time_s.get(4..6)?)?,
    )
    .ok()?;
    Some(PrimitiveDateTime::new(date, tod).assume_offset(offset))
}

/// The trailing `synora-YYYYMMDD-HHMMSS` name in a `btrfs subvolume list`
/// path column, if it matches the convention.
fn snapshot_name_from_path(path: &str) -> Option<String> {
    let start = path.rfind("synora-")?;
    let cand = &path[start..];
    matches_synora(cand).then(|| cand.to_string())
}

/// Snapshot path for Btrfs: a read-only sibling `<subvol>-<name>` (spec §32).
fn snapshot_path(subvol: &Path, name: &str) -> PathBuf {
    PathBuf::from(format!("{}-{}", subvol.display(), name))
}

/// `zfs list -H -t snapshot -o name,creation -s creation` lines:
/// `<pool>/<dataset>@<name>\t<epoch-seconds>`.
fn parse_zfs_list(out: &str, pool_dataset: &str) -> Vec<SnapshotInfo> {
    let prefix = format!("{pool_dataset}@");
    out.lines()
        .filter_map(|line| {
            let (full, ts) = line.split_once('\t')?;
            Some(SnapshotInfo {
                name: full.strip_prefix(&prefix)?.to_string(),
                created_at: ts.trim().parse().ok()?,
            })
        })
        .collect()
}

/// `btrfs subvolume list -o <parent>` lines:
/// `ID <id> gen <gen> top level <t> path <name>`.
fn parse_btrfs_list(out: &str) -> Vec<SnapshotInfo> {
    out.lines()
        .filter_map(|line| {
            let path = line.split_whitespace().last()?;
            Some(SnapshotInfo {
                name: snapshot_name_from_path(path)?,
                created_at: 0,
            })
        })
        .collect()
}

/// Run a CLI tool with an argv array. A non-zero exit becomes
/// [`SnapshotError::Command`]; a missing binary (spawn `NotFound`) becomes
/// [`SnapshotError::Unsupported`] with a clear message.
fn run_cli(cmd: &str, args: &[&str]) -> Result<String, SnapshotError> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => SnapshotError::Unsupported(format!(
                "`{cmd}` is not installed; cannot manage snapshots"
            )),
            _ => SnapshotError::Command(format!("failed to run `{cmd}`: {e}")),
        })?;
    if !out.status.success() {
        return Err(SnapshotError::Command(format!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{datetime, format_description};

    fn snap(ts: &str) -> SnapshotInfo {
        let dt = PrimitiveDateTime::parse(
            ts,
            &format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
        )
        .unwrap()
        .assume_utc();
        SnapshotInfo {
            name: snapshot_name(dt),
            created_at: dt.unix_timestamp(),
        }
    }

    /// 10 snapshots across 3 calendar days (4 + 3 + 3), newest last.
    fn ten_across_days() -> Vec<SnapshotInfo> {
        [
            "2026-08-14 00:00:00",
            "2026-08-14 06:00:00",
            "2026-08-14 12:00:00",
            "2026-08-14 18:00:00",
            "2026-08-15 01:00:00",
            "2026-08-15 13:00:00",
            "2026-08-15 23:00:00",
            "2026-08-16 02:00:00",
            "2026-08-16 14:00:00",
            "2026-08-16 22:00:00",
        ]
        .iter()
        .map(|t| snap(t))
        .collect()
    }

    fn have(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn snapshot_name_format() {
        assert_eq!(
            snapshot_name(datetime!(2026-08-16 12:34:56 UTC)),
            "synora-20260816-123456"
        );
        // 24h clock, zero-padded.
        assert_eq!(
            snapshot_name(datetime!(2026-01-02 23:05:09 UTC)),
            "synora-20260102-230509"
        );
    }

    #[test]
    fn prune_keep_last_keeps_only_newest() {
        let snaps = ten_across_days();
        let now = datetime!(2026-08-16 23:59:59 UTC);
        let policy = RetentionPolicy {
            keep_last: Some(3),
            ..Default::default()
        };
        let del = prune_plan(&snaps, &policy, now);
        assert_eq!(
            del,
            [
                "synora-20260814-000000",
                "synora-20260814-060000",
                "synora-20260814-120000",
                "synora-20260814-180000",
                "synora-20260815-010000",
                "synora-20260815-130000",
                "synora-20260815-230000",
            ]
        );
    }

    #[test]
    fn prune_keep_daily_keeps_first_snapshot_of_newest_days() {
        let snaps = ten_across_days();
        let now = datetime!(2026-08-16 23:59:59 UTC);
        let policy = RetentionPolicy {
            keep_daily: Some(2),
            ..Default::default()
        };
        let del = prune_plan(&snaps, &policy, now);
        assert_eq!(del.len(), 8);
        // Daily representatives (oldest per day): 08-14 00:00, 08-15 01:00,
        // 08-16 02:00. Newest 2 days -> keep the 08-15 and 08-16 reps.
        assert!(!del.contains(&"synora-20260815-010000".to_string()));
        assert!(!del.contains(&"synora-20260816-020000".to_string()));
        assert!(del.contains(&"synora-20260814-000000".to_string()));
    }

    #[test]
    fn prune_keep_weekly_keeps_newest_iso_weeks() {
        let snaps = [
            "2026-08-03 00:00:00", // ISO week 32
            "2026-08-03 12:00:00",
            "2026-08-10 00:00:00", // week 33
            "2026-08-10 12:00:00",
            "2026-08-17 00:00:00", // week 34
            "2026-08-17 12:00:00",
        ]
        .iter()
        .map(|t| snap(t))
        .collect::<Vec<_>>();
        let now = datetime!(2026-08-17 23:59:59 UTC);
        let policy = RetentionPolicy {
            keep_weekly: Some(2),
            ..Default::default()
        };
        let del = prune_plan(&snaps, &policy, now);
        assert_eq!(del.len(), 4);
        assert!(!del.contains(&"synora-20260810-000000".to_string())); // week 33 rep
        assert!(!del.contains(&"synora-20260817-000000".to_string())); // week 34 rep
        assert!(del.contains(&"synora-20260803-000000".to_string())); // week 32 rep
        assert!(del.contains(&"synora-20260803-120000".to_string()));
    }

    #[test]
    fn prune_keep_monthly_keeps_newest_months() {
        let snaps = [
            "2026-07-05 00:00:00",
            "2026-07-20 00:00:00",
            "2026-08-10 00:00:00",
            "2026-08-25 00:00:00",
            "2026-09-01 00:00:00",
        ]
        .iter()
        .map(|t| snap(t))
        .collect::<Vec<_>>();
        let now = datetime!(2026-09-01 12:00:00 UTC);
        let policy = RetentionPolicy {
            keep_monthly: Some(2),
            ..Default::default()
        };
        let del = prune_plan(&snaps, &policy, now);
        assert_eq!(del.len(), 3);
        assert!(!del.contains(&"synora-20260810-000000".to_string())); // Aug rep
        assert!(!del.contains(&"synora-20260901-000000".to_string())); // Sep rep
        assert!(del.contains(&"synora-20260705-000000".to_string()));
        assert!(del.contains(&"synora-20260720-000000".to_string()));
        assert!(del.contains(&"synora-20260825-000000".to_string()));
    }

    #[test]
    fn prune_union_of_buckets() {
        let snaps = ten_across_days();
        let now = datetime!(2026-08-16 23:59:59 UTC);
        let policy = RetentionPolicy {
            keep_last: Some(3),   // 08-16's three snapshots
            keep_daily: Some(2),  // reps of 08-15 and 08-16
            ..Default::default()
        };
        let del = prune_plan(&snaps, &policy, now);
        // Kept: 08-16 x3 (last) + 08-15 rep (daily) = 4.
        assert_eq!(del.len(), 6);
        assert!(!del.contains(&"synora-20260815-010000".to_string())); // daily, outside last
        assert!(!del.contains(&"synora-20260816-140000".to_string())); // last, outside daily
        assert!(del.contains(&"synora-20260815-130000".to_string()));
    }

    #[test]
    fn non_synora_names_never_deleted() {
        let mut snaps = ten_across_days();
        snaps.push(SnapshotInfo {
            name: "manual-backup".into(),
            created_at: snaps.last().unwrap().created_at + 1,
        });
        snaps.push(SnapshotInfo {
            name: "synora-20260816".into(), // malformed: no time part
            created_at: snaps.last().unwrap().created_at + 1,
        });
        let now = datetime!(2026-08-16 23:59:59 UTC);
        let policy = RetentionPolicy {
            keep_last: Some(1),
            ..Default::default()
        };
        let del = prune_plan(&snaps, &policy, now);
        assert!(!del.contains(&"manual-backup".to_string()));
        assert!(!del.contains(&"synora-20260816".to_string()));
        assert_eq!(del.len(), 9); // all 9 well-formed synora snaps but the newest
    }

    #[test]
    fn btrfs_zero_timestamps_prune_by_name_order() {
        let mut snaps = ten_across_days();
        for s in &mut snaps {
            s.created_at = 0; // what `btrfs subvolume list` reports
        }
        let now = datetime!(2026-08-16 23:59:59 UTC);
        let policy = RetentionPolicy {
            keep_daily: Some(2),
            ..Default::default()
        };
        let del = prune_plan(&snaps, &policy, now);
        assert_eq!(del.len(), 8); // same semantics as the epoch-based test
        assert!(!del.contains(&"synora-20260816-020000".to_string()));
    }

    #[test]
    fn provider_for_kinds() {
        assert!(matches!(
            provider_for(&StorageKind::Dir, Path::new("/tmp")),
            Err(SnapshotError::Unsupported(_))
        ));
        assert!(provider_for(
            &StorageKind::Zfs {
                pool: "tank".into(),
                dataset: "data".into(),
                options: vec![],
            },
            Path::new("/mnt")
        )
        .is_ok());
        assert!(provider_for(
            &StorageKind::Btrfs {
                subvol: "/srv/mirror".into(),
            },
            Path::new("/mnt")
        )
        .is_ok());
    }

    #[test]
    fn parse_zfs_list_lines() {
        let out = "tank/data@synora-20260814-000000\t1723593600\n\
                   tank/data@manual-snap\t1723600000\n";
        let v = parse_zfs_list(out, "tank/data");
        assert_eq!(
            v,
            [
                SnapshotInfo {
                    name: "synora-20260814-000000".into(),
                    created_at: 1723593600,
                },
                SnapshotInfo {
                    name: "manual-snap".into(),
                    created_at: 1723600000,
                },
            ]
        );
    }

    #[test]
    fn parse_btrfs_list_lines() {
        let out = "ID 256 gen 8 top level 5 path data-synora-20260814-000000\n\
                   ID 257 gen 9 top level 5 path data-synora-20260815-010000\n\
                   ID 258 gen 10 top level 5 path data-other\n";
        let v = parse_btrfs_list(out);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "synora-20260814-000000");
        assert_eq!(v[0].created_at, 0); // btrfs reports no timestamp
    }

    /// zfs/btrfs are not installed on this host; the missing binary must
    /// surface as Unsupported before any shell-out can happen.
    #[test]
    fn zfs_cli_missing_binary_is_unsupported() {
        if have("zfs") {
            eprintln!("zfs present on host; skipping CLI gate test");
            return;
        }
        let p = ZfsSnapshotProvider {
            pool_dataset: "tank/data".into(),
        };
        assert!(matches!(
            p.create("synora-20260816-000000"),
            Err(SnapshotError::Unsupported(_))
        ));
        assert!(matches!(p.list(), Err(SnapshotError::Unsupported(_))));
    }

    #[test]
    fn btrfs_cli_missing_binary_is_unsupported() {
        if have("btrfs") {
            eprintln!("btrfs present on host; skipping CLI gate test");
            return;
        }
        let p = BtrfsSnapshotProvider {
            subvol: PathBuf::from("/srv/mirror"),
        };
        assert!(matches!(
            p.create("synora-20260816-000000"),
            Err(SnapshotError::Unsupported(_))
        ));
        assert!(matches!(p.list(), Err(SnapshotError::Unsupported(_))));
    }
}
