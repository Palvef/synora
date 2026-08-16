//! Schedule model + parsing helpers (spec §6).
//!
//! The no-drift invariant: `next_run` is always computed from the wall clock
//! (`schedule.next_after(now)`), never from "last run end + interval". Actual
//! `next_after` math lands in M1; this module owns types and validation.

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use time::{Duration, Time, Weekday};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub kind: ScheduleKind,
}

/// All schedule kinds of spec §6.1–§6.4. `Manual`/`Startup` have no fixed
/// next time: manual runs via `synora run`, startup fires on daemon boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ScheduleKind {
    /// 5-field cron like "0 */6 * * *" (seconds inserted automatically).
    Cron { expr: String },
    /// Every day at a fixed time.
    Daily { at: Time },
    /// A fixed weekday + time.
    Weekly { weekday: Weekday, at: Time },
    /// Fixed period anchored to a persistent anchor time (no drift).
    Interval { every: Duration },
    Manual,
    Startup,
}

impl Schedule {
    /// Short human-readable form for CLI/TUI display.
    pub fn describe(&self) -> String {
        match &self.kind {
            ScheduleKind::Cron { expr } => format!("cron {expr}"),
            ScheduleKind::Daily { at } => format!("daily {}", fmt_time(*at)),
            ScheduleKind::Weekly { weekday, at } => format!("weekly {weekday} {}", fmt_time(*at)),
            ScheduleKind::Interval { every } => format!("interval {}", fmt_duration(*every)),
            ScheduleKind::Manual => "manual".into(),
            ScheduleKind::Startup => "startup".into(),
        }
    }
}

fn fmt_time(t: Time) -> String {
    format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second())
}

fn fmt_duration(d: Duration) -> String {
    let total = d.whole_seconds();
    match total {
        s if s % 86400 == 0 => format!("{}d", s / 86400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// Parse a human duration like "6h", "5m", "1h30m", "2d" (spec §6.4).
pub fn parse_duration_human(s: &str) -> Result<Duration, String> {
    let mut total = Duration::ZERO;
    let mut num: u64 = 0;
    let mut saw_digit = false;
    let mut saw_unit = false;
    for c in s.chars() {
        match c {
            '0'..='9' => {
                num = num
                    .checked_mul(10)
                    .and_then(|n| n.checked_add((c as u8 - b'0') as u64))
                    .ok_or_else(|| "duration number overflow".to_string())?;
                saw_digit = true;
            }
            's' | 'm' | 'h' | 'd' => {
                if !saw_digit {
                    return Err(format!("invalid duration `{s}`: unit without number"));
                }
                let unit = match c {
                    's' => Duration::seconds(num as i64),
                    'm' => Duration::minutes(num as i64),
                    'h' => Duration::hours(num as i64),
                    'd' => Duration::days(num as i64),
                    _ => unreachable!(),
                };
                total += unit;
                num = 0;
                saw_digit = false;
                saw_unit = true;
            }
            _ => return Err(format!("invalid duration `{s}`: unexpected character `{c}`")),
        }
    }
    if !saw_unit || saw_digit {
        return Err(format!("invalid duration `{s}`: expected like `6h` or `1h30m`"));
    }
    Ok(total)
}

/// Normalize and validate a cron expression. Accepts the 5-field form used
/// throughout the spec ("0 */6 * * *") by prepending seconds, then validates
/// with the `cron` crate. Returns the normalized 7-field expression.
pub fn parse_cron_expr(expr: &str) -> Result<String, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let normalized = match fields.len() {
        5 => format!("0 {expr}"),
        6 | 7 => expr.to_string(),
        n => {
            return Err(format!(
                "invalid cron expression `{expr}`: expected 5 fields, got {n}"
            ))
        }
    };
    cron::Schedule::from_str(&normalized)
        .map_err(|e| format!("invalid cron expression `{expr}`: {e}"))?;
    Ok(normalized)
}

/// Parse a clock time "HH:MM[:SS]" (spec §6.2).
pub fn parse_time_at(s: &str) -> Result<Time, String> {
    let with_secs = if s.split(':').count() == 2 {
        format!("{s}:00")
    } else {
        s.to_string()
    };
    const FORMAT: &[time::format_description::BorrowedFormatItem<'_>] =
        time::macros::format_description!("[hour]:[minute]:[second]");
    Time::parse(&with_secs, FORMAT)
        .map_err(|e| format!("invalid time `{s}`: expected HH:MM[:SS] ({e})"))
}

/// Parse a weekday name, case-insensitive ("sunday".."saturday", spec §6.3).
pub fn parse_weekday(s: &str) -> Result<Weekday, String> {
    match s.to_ascii_lowercase().as_str() {
        "sunday" | "sun" => Ok(Weekday::Sunday),
        "monday" | "mon" => Ok(Weekday::Monday),
        "tuesday" | "tue" => Ok(Weekday::Tuesday),
        "wednesday" | "wed" => Ok(Weekday::Wednesday),
        "thursday" | "thu" => Ok(Weekday::Thursday),
        "friday" | "fri" => Ok(Weekday::Friday),
        "saturday" | "sat" => Ok(Weekday::Saturday),
        other => Err(format!("invalid weekday `{other}`: expected like `Sunday`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_human() {
        assert_eq!(parse_duration_human("6h").unwrap(), Duration::hours(6));
        assert_eq!(parse_duration_human("5m").unwrap(), Duration::minutes(5));
        assert_eq!(parse_duration_human("30s").unwrap(), Duration::seconds(30));
        assert_eq!(parse_duration_human("2d").unwrap(), Duration::days(2));
        assert_eq!(
            parse_duration_human("1h30m").unwrap(),
            Duration::hours(1) + Duration::minutes(30)
        );
        assert!(parse_duration_human("").is_err());
        assert!(parse_duration_human("6").is_err());
        assert!(parse_duration_human("h").is_err());
        assert!(parse_duration_human("6x").is_err());
    }

    #[test]
    fn cron_normalization() {
        assert_eq!(parse_cron_expr("0 */6 * * *").unwrap(), "0 0 */6 * * *");
        assert_eq!(parse_cron_expr("0 0 3 * * *").unwrap(), "0 0 3 * * *");
        assert!(parse_cron_expr("*/5 * * *").is_err());
        assert!(parse_cron_expr("nonsense").is_err());
    }

    #[test]
    fn time_and_weekday() {
        assert_eq!(parse_time_at("03:30:00").unwrap(), Time::from_hms(3, 30, 0).unwrap());
        assert_eq!(parse_time_at("04:00").unwrap(), Time::from_hms(4, 0, 0).unwrap());
        assert!(parse_time_at("25:00:00").is_err());
        assert_eq!(parse_weekday("Sunday").unwrap(), Weekday::Sunday);
        assert_eq!(parse_weekday("mon").unwrap(), Weekday::Monday);
        assert!(parse_weekday("Funday").is_err());
    }
}
