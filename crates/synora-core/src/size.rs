//! Human-readable byte sizes.

/// Go `time.Time{}` unix seconds used by tunasync for unset timestamps.
pub const TUNASYNC_ZERO_TS: i64 = -62_135_596_800;

/// tunasync.json timestamps: `2026-08-22 07:13:00 +0800`.
pub fn tunasync_time(ts: i64) -> String {
    const FMT: &[time::format_description::FormatItem<'_>] = time::macros::format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour sign:mandatory padding:zero][offset_minute]"
    );
    if ts <= 0 {
        return "0001-01-01 00:00:00 +0000".into();
    }
    time::OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .map(|t| {
            t.to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC))
        })
        .and_then(|t| t.format(&FMT).ok())
        .unwrap_or_else(|| "0001-01-01 00:00:00 +0000".into())
}

/// tunasync.json numeric timestamp; 0 / missing becomes Go's zero time.
pub fn tunasync_ts(ts: Option<i64>) -> i64 {
    ts.filter(|&t| t > 0).unwrap_or(TUNASYNC_ZERO_TS)
}

/// Binary units with two decimals, e.g. 39_710_000_000 → "36.98 GiB".
/// Used by the TUI, synora.json, and logs.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// tunasync.json / mirror-web size string (`du -h` style): `923G`, `1.8T`.
/// 1024-based, no `iB`, no space. Keep `human_size` for Synora's own UI.
pub fn tunasync_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return bytes.to_string();
    }
    let rounded = (value * 10.0).round() / 10.0;
    if (rounded - rounded.round()).abs() < 1e-9 {
        format!("{:.0}{}", rounded, UNITS[unit])
    } else {
        format!("{rounded:.1}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.00 KiB");
        assert_eq!(human_size(1024 * 1024), "1.00 MiB");
        assert_eq!(human_size(39_710_000_000), "36.98 GiB");
        assert_eq!(human_size(1024 * 1024 * 1024 * 1024), "1.00 TiB");
    }

    #[test]
    fn tunasync_du_style() {
        assert_eq!(tunasync_size(0), "0");
        assert_eq!(tunasync_size(512), "512");
        assert_eq!(tunasync_size(1024), "1K");
        assert_eq!(tunasync_size(1536), "1.5K");
        assert_eq!(tunasync_size(923 * 1024 * 1024 * 1024), "923G");
        assert_eq!(
            tunasync_size((1.8 * 1024.0 * 1024.0 * 1024.0 * 1024.0) as u64),
            "1.8T"
        );
        assert_eq!(tunasync_size(124_156_084_305), "115.6G");
    }

    #[test]
    fn tunasync_time_formats_offset_without_colon() {
        let s = tunasync_time(1_700_000_000);
        // `2023-11-14 22:13:20 +0000` or a local offset, never RFC3339.
        assert!(!s.contains('T'), "{s}");
        assert!(!s.contains("+08:00"), "{s}");
        assert!(s.contains(' '), "{s}");
        assert_eq!(tunasync_time(0), "0001-01-01 00:00:00 +0000");
        assert_eq!(tunasync_ts(None), TUNASYNC_ZERO_TS);
        assert_eq!(tunasync_ts(Some(1_700_000_000)), 1_700_000_000);
    }
}
