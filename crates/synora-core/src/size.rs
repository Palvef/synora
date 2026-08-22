//! Human-readable byte sizes.

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
}
