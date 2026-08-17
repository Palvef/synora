//! Schedule model + no-drift next-run computation (spec §6).
//!
//! The no-drift invariant: `next_run` is always computed from the wall clock
//! (`schedule.next_after(now)`), never from "last run end + interval".
//!
//! Cron handling is hand-rolled: the `cron` crate forces chrono (+ chrono-tz
//! for DST-correct evaluation — a huge embedded tzdb). A ~150-line matcher
//! instead evaluates the cron in the job's own wall-clock time via the system
//! tzdata (`time-tz`), which is DST-correct by construction: each candidate
//! wall time is converted through `Tz::from_local_datetime`, so spring-forward
//! gaps are skipped and fall-back repeats take the earliest occurrence.
//! Granularity is one minute (seconds must be `0` or `*`).

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, PrimitiveDateTime, Time, Weekday};
use time_tz::{Offset, OffsetResult, TimeZone};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub kind: ScheduleKind,
}

/// All schedule kinds of spec §6.1–§6.4. `Manual`/`Startup` have no fixed
/// next time: manual runs via `synora run`, startup fires on daemon boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ScheduleKind {
    /// 6-field cron "sec min hour dom mon dow"; a 5-field expression gets
    /// seconds prepended at parse time.
    Cron {
        expr: String,
    },
    /// Every day at a fixed time.
    Daily {
        at: Time,
    },
    /// A fixed weekday + time.
    Weekly {
        weekday: Weekday,
        at: Time,
    },
    /// Fixed period anchored to a persistent anchor time (no drift).
    Interval {
        every: Duration,
    },
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

    /// Next run strictly after `now` (UTC), evaluated in `tz` (the job's
    /// timezone). `None` for manual/startup.
    /// `anchor` is required for `Interval` — the persistent alignment point.
    pub fn next_after(
        &self,
        now: OffsetDateTime,
        tz: &time_tz::Tz,
        anchor: Option<OffsetDateTime>,
    ) -> Option<OffsetDateTime> {
        match &self.kind {
            ScheduleKind::Cron { expr } => cron_next(expr, now, tz),
            ScheduleKind::Daily { at } => daily_next(*at, now, tz),
            ScheduleKind::Weekly { weekday, at } => weekly_next(*weekday, *at, now, tz),
            ScheduleKind::Interval { every } => {
                Some(interval_next(anchor.unwrap_or(now), *every, now))
            }
            ScheduleKind::Manual | ScheduleKind::Startup => None,
        }
    }
}

/// Wall-clock fields of `now` in `tz` (time-tz exposes offsets, not time 0.3's
/// full TimeZone trait, so conversions go through its offset lookups).
fn wall(now: OffsetDateTime, tz: &time_tz::Tz) -> OffsetDateTime {
    now.to_offset(tz.get_offset_utc(&now).to_utc())
}

/// Local wall time → UTC instant. `None` when the wall time does not exist
/// (spring-forward gap); ambiguity (fall-back repeat) resolves to the earliest.
/// Result is always a UTC-offset timestamp: `OffsetDateTime` equality in
/// `time` compares the offset too, so callers must get a normalized value.
fn to_utc(tz: &time_tz::Tz, local: PrimitiveDateTime) -> Option<OffsetDateTime> {
    let local_as_utc = local.assume_utc();
    let off_secs = match tz.get_offset_local(&local_as_utc) {
        OffsetResult::Some(off) => off.to_utc().whole_seconds() as i64,
        OffsetResult::Ambiguous(a, _b) => a.to_utc().whole_seconds() as i64,
        OffsetResult::None => return None,
    };
    // wall - offset = UTC instant
    OffsetDateTime::from_unix_timestamp(local_as_utc.unix_timestamp() - off_secs).ok()
}

fn daily_next(at: Time, now: OffsetDateTime, tz: &time_tz::Tz) -> Option<OffsetDateTime> {
    let now_local = wall(now, tz);
    let mut date = now_local.date();
    // at most a couple of iterations; loops keep DST edge cases safe.
    for _ in 0..3 {
        let local = date.with_time(at);
        if let Some(cand) = to_utc(tz, local) {
            if cand > now {
                return Some(cand);
            }
        }
        date = date.next_day()?;
    }
    None
}

fn weekly_next(
    weekday: Weekday,
    at: Time,
    now: OffsetDateTime,
    tz: &time_tz::Tz,
) -> Option<OffsetDateTime> {
    let now_local = wall(now, tz);
    let mut date = now_local.date();
    for _ in 0..9 {
        if date.weekday() == weekday {
            let local = date.with_time(at);
            if let Some(cand) = to_utc(tz, local) {
                if cand > now {
                    return Some(cand);
                }
            }
        }
        date = date.next_day()?;
    }
    None
}

/// Next slot on the anchor grid strictly after `now`: anchor is the alignment
/// point; run times are anchor + k*every. Immune to run duration and restarts
/// (spec §6.5).
pub fn interval_next(
    anchor: OffsetDateTime,
    every: Duration,
    now: OffsetDateTime,
) -> OffsetDateTime {
    let every_secs = every.whole_seconds().max(1) as u64;
    let span_secs = (now - anchor).whole_seconds().max(0) as u64;
    let k = span_secs / every_secs;
    let mut next = anchor + Duration::seconds(((k + 1) * every_secs) as i64);
    while next <= now {
        next += every;
    }
    next
}

fn cron_next(expr: &str, now: OffsetDateTime, tz: &time_tz::Tz) -> Option<OffsetDateTime> {
    let cron = CronExpr::parse(expr).ok()?;
    let now_local = wall(now, tz);
    // Minute granularity: start at the next minute boundary.
    let t0 = now_local
        .time()
        .replace_nanosecond(0)
        .ok()?
        .replace_second(0)
        .ok()?;
    let mut candidate = now_local.date().with_time(t0);
    candidate += Duration::minutes(1);
    // Worst case (a cron that fires once a year) scans ~1M minutes — still
    // microseconds of field math. 2 years bounds impossible crons.
    const MAX_MINUTES: i64 = 2 * 366 * 24 * 60;
    for i in 0..MAX_MINUTES {
        let t = candidate + Duration::minutes(i);
        if cron.matches(t) {
            if let Some(utc) = to_utc(tz, t) {
                if utc > now {
                    return Some(utc);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Cron expression parsing and matching
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct CronField {
    parts: Vec<FieldPart>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FieldPart {
    min: u32,
    max: u32,
    step: u32,
}

impl FieldPart {
    fn matches(&self, v: u32) -> bool {
        v >= self.min && v <= self.max && (v - self.min).is_multiple_of(self.step)
    }
}

impl CronField {
    /// Full range `*` for the field (used to detect dom/dow restriction).
    fn is_full_range(&self, lo: u32, hi: u32) -> bool {
        self.parts.len() == 1
            && self.parts[0].min == lo
            && self.parts[0].max == hi
            && self.parts[0].step == 1
    }

    fn matches(&self, v: u32) -> bool {
        self.parts.iter().any(|p| p.matches(v))
    }
}

#[derive(Debug, Clone)]
struct CronExpr {
    minute: CronField,
    hour: CronField,
    dom: CronField,
    month: CronField,
    dow: CronField,
}

impl CronExpr {
    fn parse(expr: &str) -> Result<CronExpr, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 6 {
            return Err(format!(
                "expected 6 fields `sec min hour dom mon dow`, got {}",
                fields.len()
            ));
        }
        let (sec_s, min_s, hour_s, dom_s, mon_s, dow_s) = (
            fields[0], fields[1], fields[2], fields[3], fields[4], fields[5],
        );
        // Minute granularity: seconds must be 0 or *.
        if sec_s != "0" && sec_s != "*" {
            return Err(format!(
                "second-level cron `{sec_s}` is not supported (scheduling is minute-granular)"
            ));
        }
        Ok(CronExpr {
            minute: parse_field(min_s, 0, 59, &[])?,
            hour: parse_field(hour_s, 0, 23, &[])?,
            dom: parse_field(dom_s, 1, 31, &[])?,
            month: parse_field(mon_s, 1, 12, &MONTHS)?,
            dow: parse_field(dow_s, 0, 7, &WEEKDAYS)?,
        })
    }

    fn matches(&self, t: PrimitiveDateTime) -> bool {
        let month_ok = self.month.matches(t.month() as u32);
        if !month_ok {
            return false;
        }
        // dom/dow OR-semantics: when both are restricted, either may match.
        let dom_restricted = !self.dom.is_full_range(1, 31);
        let dow_restricted = !self.dow.is_full_range(0, 7);
        let day_ok = match (dom_restricted, dow_restricted) {
            (true, true) => {
                self.dom.matches(t.day() as u32) || self.dow.matches(dow_num(t.weekday()))
            }
            (true, false) => self.dom.matches(t.day() as u32),
            (false, true) => self.dow.matches(dow_num(t.weekday())),
            (false, false) => true,
        };
        day_ok && self.hour.matches(t.hour() as u32) && self.minute.matches(t.minute() as u32)
    }
}

const MONTHS: [(&str, u32); 12] = [
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

const WEEKDAYS: [(&str, u32); 7] = [
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

/// time::Weekday (Mon=1..Sun=7) → cron dow number (Sun=0..Sat=6, 7 accepted
/// as Sunday in input).
fn dow_num(wd: Weekday) -> u32 {
    match wd {
        Weekday::Sunday => 0,
        Weekday::Monday => 1,
        Weekday::Tuesday => 2,
        Weekday::Wednesday => 3,
        Weekday::Thursday => 4,
        Weekday::Friday => 5,
        Weekday::Saturday => 6,
    }
}

/// Parse one cron field: comma-separated parts of `a`, `a-b`, `*`, `a/step`,
/// `a-b/step`, or a name (mon/dow). 7 in dow is Sunday.
fn parse_field(s: &str, lo: u32, hi: u32, names: &[(&str, u32)]) -> Result<CronField, String> {
    let mut parts = Vec::new();
    for chunk in s.split(',') {
        if chunk.is_empty() {
            return Err(format!("invalid cron field `{s}`: empty list element"));
        }
        let (range, step) = match chunk.split_once('/') {
            Some((r, st)) => {
                let step: u32 = st
                    .parse()
                    .map_err(|_| format!("invalid cron step `{st}` in `{s}`"))?;
                if step == 0 {
                    return Err(format!("invalid cron step 0 in `{s}`"));
                }
                (r, step)
            }
            None => (chunk, 1),
        };
        let (a, b) = match range.split_once('-') {
            Some((a, b)) => (a, b),
            None => (range, range),
        };
        let is_dow = names.first().map(|n| n.0) == Some("sun");
        let resolve = |v: &str| -> Result<u32, String> {
            if let Ok(n) = v.parse::<u32>() {
                // dow: user-written 7 means Sunday (0); `*` expansion keeps hi=7
                // so a full-range part covers the alias too.
                return Ok(if is_dow && n == 7 { 0 } else { n });
            }
            let lower = v.to_ascii_lowercase();
            for (name, num) in names {
                // full name or common 3-letter prefix, e.g. "sun"/"sunday"
                if lower.len() >= 3 && (name.starts_with(&lower) || lower.starts_with(name)) {
                    return Ok(*num);
                }
            }
            Err(format!("invalid cron value `{v}` in `{s}`"))
        };
        // `*` covers the field's full range.
        let a = if a == "*" { lo } else { resolve(a)? };
        let b = if b == "*" { hi } else { resolve(b)? };
        if a > b {
            return Err(format!(
                "invalid cron range `{range}` in `{s}`: start > end"
            ));
        }
        if a < lo || b > hi {
            return Err(format!(
                "invalid cron value in `{s}`: {range} outside {lo}-{hi}"
            ));
        }
        parts.push(FieldPart {
            min: a,
            max: b,
            step,
        });
    }
    Ok(CronField { parts })
}

/// Normalize and validate a cron expression. Accepts the 5-field form used
/// throughout the spec ("0 */6 * * *") by prepending seconds; 7-field input
/// is accepted only if the year is `*` (then dropped). Returns the normalized
/// 6-field expression `sec min hour dom mon dow`.
pub fn parse_cron_expr(expr: &str) -> Result<String, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let normalized = match fields.len() {
        5 => format!("0 {expr}"),
        6 => expr.to_string(),
        7 if fields[6] == "*" => fields[..6].join(" "),
        7 => return Err(format!("year-restricted cron `{expr}` is not supported")),
        n => {
            return Err(format!(
                "invalid cron expression `{expr}`: expected 5 fields, got {n}"
            ))
        }
    };
    CronExpr::parse(&normalized).map_err(|e| format!("invalid cron expression `{expr}`: {e}"))?;
    Ok(normalized)
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
            _ => {
                return Err(format!(
                    "invalid duration `{s}`: unexpected character `{c}`"
                ))
            }
        }
    }
    if !saw_unit || saw_digit {
        return Err(format!(
            "invalid duration `{s}`: expected like `6h` or `1h30m`"
        ));
    }
    Ok(total)
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

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::time;
    use time::{Date, Month};
    use time_tz::timezones;

    fn utc(y: i32, mo: u8, d: u8, h: u8, mi: u8) -> OffsetDateTime {
        Date::from_calendar_date(y, Month::try_from(mo).unwrap(), d)
            .unwrap()
            .with_time(Time::from_hms(h, mi, 0).unwrap())
            .assume_utc()
    }

    fn shanghai() -> &'static time_tz::Tz {
        timezones::get_by_name("Asia/Shanghai").unwrap()
    }

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
        assert_eq!(parse_cron_expr("0 0 3 * * * *").unwrap(), "0 0 3 * * *");
        assert!(parse_cron_expr("*/5 * * *").is_err());
        assert!(parse_cron_expr("nonsense").is_err());
        assert!(parse_cron_expr("0 0 61 * * *").is_err()); // minute 61
        assert!(parse_cron_expr("0 0 0 0 * *").is_err()); // dom 0
        assert!(parse_cron_expr("30 0 3 * * *").is_err()); // seconds unsupported
    }

    #[test]
    fn time_and_weekday() {
        assert_eq!(
            parse_time_at("03:30:00").unwrap(),
            Time::from_hms(3, 30, 0).unwrap()
        );
        assert_eq!(
            parse_time_at("04:00").unwrap(),
            Time::from_hms(4, 0, 0).unwrap()
        );
        assert!(parse_time_at("25:00:00").is_err());
        assert_eq!(parse_weekday("Sunday").unwrap(), Weekday::Sunday);
        assert_eq!(parse_weekday("mon").unwrap(), Weekday::Monday);
        assert!(parse_weekday("Funday").is_err());
    }

    #[test]
    fn cron_next_basic() {
        let s = Schedule {
            kind: ScheduleKind::Cron {
                expr: "0 0 */4 * * *".into(),
            },
        };
        // 2026-08-16T01:30 UTC → next at 04:00 UTC.
        let next = s
            .next_after(utc(2026, 8, 16, 1, 30), timezones::db::UTC, None)
            .unwrap();
        assert_eq!(next, utc(2026, 8, 16, 4, 0));
        // exactly on a slot → strictly after.
        let next = s
            .next_after(utc(2026, 8, 16, 4, 0), timezones::db::UTC, None)
            .unwrap();
        assert_eq!(next, utc(2026, 8, 16, 8, 0));
    }

    #[test]
    fn cron_next_with_list_and_range() {
        let s = Schedule {
            kind: ScheduleKind::Cron {
                expr: "0 30 2 * * 1-5".into(), // weekdays 02:30
            },
        };
        // 2026-08-14 is a Friday; 02:30 Saturday 2026-08-15 is excluded.
        let next = s
            .next_after(utc(2026, 8, 14, 10, 0), timezones::db::UTC, None)
            .unwrap();
        assert_eq!(next, utc(2026, 8, 17, 2, 30)); // Monday
    }

    #[test]
    fn cron_next_in_timezone() {
        let s = Schedule {
            kind: ScheduleKind::Cron {
                expr: "0 30 3 * * *".into(),
            },
        };
        // 01:00 UTC = 09:00 Shanghai (UTC+8): 03:30 local already passed,
        // next occurrence is tomorrow 03:30 +08 = 19:30 UTC.
        let next = s
            .next_after(utc(2026, 8, 16, 1, 0), shanghai(), None)
            .unwrap();
        assert_eq!(next, utc(2026, 8, 16, 19, 30));
    }

    #[test]
    fn daily_next() {
        let s = Schedule {
            kind: ScheduleKind::Daily {
                at: time!(03:30:00),
            },
        };
        let next = s
            .next_after(utc(2026, 8, 16, 1, 0), timezones::db::UTC, None)
            .unwrap();
        assert_eq!(next, utc(2026, 8, 16, 3, 30));
        let next = s
            .next_after(utc(2026, 8, 16, 3, 30), timezones::db::UTC, None)
            .unwrap();
        assert_eq!(next, utc(2026, 8, 17, 3, 30));
        // 01:00 UTC = 09:00 Shanghai: next 03:30 local is tomorrow = 19:30 UTC.
        let next = s
            .next_after(utc(2026, 8, 16, 1, 0), shanghai(), None)
            .unwrap();
        assert_eq!(next, utc(2026, 8, 16, 19, 30));
    }

    #[test]
    fn weekly_next() {
        let s = Schedule {
            kind: ScheduleKind::Weekly {
                weekday: Weekday::Sunday,
                at: time!(04:00:00),
            },
        };
        // 2026-08-16 is a Sunday; from Monday 08-17 the next is Sunday 08-23.
        let next = s
            .next_after(utc(2026, 8, 17, 0, 0), timezones::db::UTC, None)
            .unwrap();
        assert_eq!(next, utc(2026, 8, 23, 4, 0));
        // same Sunday before 04:00 → today.
        let next = s
            .next_after(utc(2026, 8, 16, 3, 0), timezones::db::UTC, None)
            .unwrap();
        assert_eq!(next, utc(2026, 8, 16, 4, 0));
        // same Sunday after 04:00 → next week.
        let next = s
            .next_after(utc(2026, 8, 16, 5, 0), timezones::db::UTC, None)
            .unwrap();
        assert_eq!(next, utc(2026, 8, 23, 4, 0));
    }

    #[test]
    fn interval_next_no_drift() {
        // Anchor 06:00; every 6h → slots 00:00, 06:00, 12:00, 18:00 UTC.
        let anchor = utc(2026, 8, 16, 6, 0);
        let every = Duration::hours(6);
        assert_eq!(
            interval_next(anchor, every, utc(2026, 8, 16, 7, 20)),
            utc(2026, 8, 16, 12, 0)
        );
        // Long-running jobs do not shift the grid.
        assert_eq!(
            interval_next(anchor, every, utc(2026, 8, 16, 13, 40)),
            utc(2026, 8, 16, 18, 0)
        );
        // Restart after downtime: still anchored.
        assert_eq!(
            interval_next(anchor, every, utc(2026, 8, 17, 2, 0)),
            utc(2026, 8, 17, 6, 0)
        );
        // Exactly on a slot → next slot.
        assert_eq!(
            interval_next(anchor, every, utc(2026, 8, 16, 12, 0)),
            utc(2026, 8, 16, 18, 0)
        );
    }

    #[test]
    fn no_drift_property() {
        // Whatever the "last run end" was, next_after only depends on now.
        let s = Schedule {
            kind: ScheduleKind::Interval {
                every: Duration::hours(6),
            },
        };
        let anchor = utc(2026, 8, 16, 6, 0);
        let now = utc(2026, 8, 16, 7, 20);
        let a = s.next_after(now, timezones::db::UTC, Some(anchor));
        let b = s.next_after(now, timezones::db::UTC, Some(anchor));
        assert_eq!(a, b);
        assert!(a.unwrap() > now);
    }

    #[test]
    fn manual_and_startup_never_schedule() {
        let m = Schedule {
            kind: ScheduleKind::Manual,
        };
        let st = Schedule {
            kind: ScheduleKind::Startup,
        };
        assert_eq!(
            m.next_after(utc(2026, 8, 16, 0, 0), timezones::db::UTC, None),
            None
        );
        assert_eq!(
            st.next_after(utc(2026, 8, 16, 0, 0), timezones::db::UTC, None),
            None
        );
    }
}
