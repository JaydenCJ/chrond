//! Cron expression parsing and next-occurrence computation.
//!
//! Supports the classic five-field vixie-cron syntax (`minute hour
//! day-of-month month day-of-week`) including lists, ranges, steps, month
//! and weekday names, `7` as Sunday, the vixie day-of-month/day-of-week OR
//! rule, and the `@hourly` / `@daily` / `@weekly` / `@monthly` / `@yearly` /
//! `@reboot` aliases.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};
use std::fmt;

/// A parsed cron schedule. Field sets are stored as bitmasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    /// Minutes 0-59.
    minutes: u64,
    /// Hours 0-23.
    hours: u32,
    /// Days of month 1-31 (bit 1 = day 1).
    dom: u32,
    /// Months 1-12 (bit 1 = January).
    months: u16,
    /// Days of week 0-6 (0 = Sunday).
    dow: u8,
    /// True when the day-of-month field was `*` (affects the vixie OR rule).
    dom_star: bool,
    /// True when the day-of-week field was `*`.
    dow_star: bool,
    /// True for `@reboot` schedules (run once at daemon startup).
    pub reboot: bool,
}

/// Error produced when a cron expression cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronParseError(pub String);

impl fmt::Display for CronParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid cron expression: {}", self.0)
    }
}

impl std::error::Error for CronParseError {}

struct FieldSpec {
    min: u32,
    max: u32,
    names: &'static [&'static str],
    /// Offset applied when a name matches (names[i] maps to min_for_names + i).
    name_base: u32,
}

const MONTH_NAMES: &[&str] = &[
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DOW_NAMES: &[&str] = &["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn parse_value(token: &str, spec: &FieldSpec) -> Result<u32, CronParseError> {
    if let Ok(n) = token.parse::<u32>() {
        return Ok(n);
    }
    let lower = token.to_ascii_lowercase();
    if let Some(idx) = spec.names.iter().position(|n| *n == lower) {
        return Ok(spec.name_base + idx as u32);
    }
    Err(CronParseError(format!("unrecognized value '{token}'")))
}

/// Parse one cron field into a bitmask. Returns (mask, was_star).
fn parse_field(field: &str, spec: &FieldSpec) -> Result<(u64, bool), CronParseError> {
    let mut mask: u64 = 0;
    let mut was_star = true;
    for part in field.split(',') {
        if part.is_empty() {
            return Err(CronParseError(format!("empty list item in '{field}'")));
        }
        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| CronParseError(format!("invalid step '{s}'")))?;
                if step == 0 {
                    return Err(CronParseError("step must be >= 1".into()));
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range_part == "*" {
            (spec.min, spec.max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            was_star = false;
            (parse_value(a, spec)?, parse_value(b, spec)?)
        } else {
            was_star = false;
            let v = parse_value(range_part, spec)?;
            // A bare value with a step (e.g. "5/15") behaves like "5-max/15",
            // matching vixie-cron.
            if part.contains('/') {
                (v, spec.max)
            } else {
                (v, v)
            }
        };
        if range_part == "*" && part.contains('/') {
            was_star = false;
        }
        let (mut lo, mut hi) = (lo, hi);
        // Normalize day-of-week 7 to 0 (Sunday).
        if spec.max == 6 {
            if lo == 7 && hi == 7 {
                // Bare "7" (or "7-7").
                lo = 0;
                hi = 0;
            } else if hi == 7 {
                // Ranges ending at 7 (e.g. "5-7" = Fri..Sun): include Sunday
                // when reachable by the step, then clamp the range to 6.
                if lo <= 6 && (7 - lo) % step == 0 {
                    mask |= 1u64 << 0;
                }
                hi = 6;
            } else if lo == 7 {
                // "7-x" wraps through Sunday; treat 7 as 0.
                lo = 0;
            }
        }
        if lo < spec.min || hi > spec.max || lo > hi {
            return Err(CronParseError(format!(
                "value out of range in '{part}' (allowed {}-{})",
                spec.min, spec.max
            )));
        }
        let mut v = lo;
        while v <= hi {
            if (v - lo) % step == 0 {
                mask |= 1u64 << v;
            }
            v += 1;
        }
    }
    Ok((mask, was_star))
}

impl CronExpr {
    /// Parse a five-field cron expression or an `@alias`.
    pub fn parse(expr: &str) -> Result<Self, CronParseError> {
        let expr = expr.trim();
        if let Some(alias) = expr.strip_prefix('@') {
            return Self::parse_alias(alias);
        }
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronParseError(format!(
                "expected 5 fields, got {} in '{expr}'",
                fields.len()
            )));
        }
        let minute_spec = FieldSpec {
            min: 0,
            max: 59,
            names: &[],
            name_base: 0,
        };
        let hour_spec = FieldSpec {
            min: 0,
            max: 23,
            names: &[],
            name_base: 0,
        };
        let dom_spec = FieldSpec {
            min: 1,
            max: 31,
            names: &[],
            name_base: 0,
        };
        let month_spec = FieldSpec {
            min: 1,
            max: 12,
            names: MONTH_NAMES,
            name_base: 1,
        };
        let dow_spec = FieldSpec {
            min: 0,
            max: 6,
            names: DOW_NAMES,
            name_base: 0,
        };

        let (minutes, _) = parse_field(fields[0], &minute_spec)?;
        let (hours, _) = parse_field(fields[1], &hour_spec)?;
        let (dom, dom_star) = parse_field(fields[2], &dom_spec)?;
        let (months, _) = parse_field(fields[3], &month_spec)?;
        let (dow, dow_star) = parse_field(fields[4], &dow_spec)?;

        Ok(CronExpr {
            minutes,
            hours: hours as u32,
            dom: dom as u32,
            months: months as u16,
            dow: dow as u8,
            dom_star,
            dow_star,
            reboot: false,
        })
    }

    fn parse_alias(alias: &str) -> Result<Self, CronParseError> {
        let expr = match alias.to_ascii_lowercase().as_str() {
            "reboot" => {
                let mut c = Self::parse("* * * * *")?;
                c.reboot = true;
                return Ok(c);
            }
            "hourly" => "0 * * * *",
            "daily" | "midnight" => "0 0 * * *",
            "weekly" => "0 0 * * 0",
            "monthly" => "0 0 1 * *",
            "yearly" | "annually" => "0 0 1 1 *",
            other => {
                return Err(CronParseError(format!("unknown alias '@{other}'")));
            }
        };
        Self::parse(expr)
    }

    fn day_matches(&self, t: &NaiveDateTime) -> bool {
        let dom_ok = self.dom & (1 << t.day()) != 0;
        let dow_ok = self.dow & (1 << t.weekday().num_days_from_sunday()) != 0;
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (true, false) => dow_ok,
            (false, true) => dom_ok,
            // vixie rule: when both fields are restricted, either may match.
            (false, false) => dom_ok || dow_ok,
        }
    }

    /// Whether the given minute matches this schedule.
    pub fn matches(&self, t: &NaiveDateTime) -> bool {
        self.months & (1 << t.month()) != 0
            && self.day_matches(t)
            && self.hours & (1 << t.hour()) != 0
            && self.minutes & (1u64 << t.minute()) != 0
    }

    /// The first matching minute strictly after `after`. Returns `None`
    /// if no occurrence exists within roughly five years (impossible
    /// schedules such as `0 0 31 2 *`).
    pub fn next_after(&self, after: NaiveDateTime) -> Option<NaiveDateTime> {
        let mut t = after
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(after)
            + Duration::minutes(1);
        let limit_year = after.year() + 5;
        loop {
            if t.year() > limit_year {
                return None;
            }
            if self.months & (1 << t.month()) == 0 {
                // Jump to the first minute of the next month.
                let (y, m) = if t.month() == 12 {
                    (t.year() + 1, 1)
                } else {
                    (t.year(), t.month() + 1)
                };
                t = NaiveDate::from_ymd_opt(y, m, 1)?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.day_matches(&t) {
                let d = t.date() + Duration::days(1);
                t = d.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if self.hours & (1 << t.hour()) == 0 {
                t = t.with_minute(0)? + Duration::hours(1);
                continue;
            }
            if self.minutes & (1u64 << t.minute()) == 0 {
                t += Duration::minutes(1);
                continue;
            }
            return Some(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    #[test]
    fn parses_wildcards() {
        let c = CronExpr::parse("* * * * *").unwrap();
        assert!(c.matches(&dt(2026, 7, 8, 5, 44)));
    }

    #[test]
    fn parses_fixed_time() {
        let c = CronExpr::parse("15 2 * * *").unwrap();
        assert!(c.matches(&dt(2026, 7, 8, 2, 15)));
        assert!(!c.matches(&dt(2026, 7, 8, 2, 16)));
        assert!(!c.matches(&dt(2026, 7, 8, 3, 15)));
    }

    #[test]
    fn parses_steps_and_ranges() {
        let c = CronExpr::parse("*/15 9-17 * * *").unwrap();
        assert!(c.matches(&dt(2026, 1, 1, 9, 0)));
        assert!(c.matches(&dt(2026, 1, 1, 17, 45)));
        assert!(!c.matches(&dt(2026, 1, 1, 8, 45)));
        assert!(!c.matches(&dt(2026, 1, 1, 9, 10)));
    }

    #[test]
    fn parses_lists() {
        let c = CronExpr::parse("0 0 1,15 * *").unwrap();
        assert!(c.matches(&dt(2026, 3, 1, 0, 0)));
        assert!(c.matches(&dt(2026, 3, 15, 0, 0)));
        assert!(!c.matches(&dt(2026, 3, 2, 0, 0)));
    }

    #[test]
    fn parses_range_with_step() {
        let c = CronExpr::parse("1-10/3 * * * *").unwrap();
        for m in [1u32, 4, 7, 10] {
            assert!(c.matches(&dt(2026, 1, 1, 0, m)), "minute {m}");
        }
        assert!(!c.matches(&dt(2026, 1, 1, 0, 2)));
    }

    #[test]
    fn parses_month_and_dow_names() {
        let c = CronExpr::parse("0 9 * jan-mar mon-fri").unwrap();
        // 2026-01-05 is a Monday.
        assert!(c.matches(&dt(2026, 1, 5, 9, 0)));
        // 2026-01-04 is a Sunday.
        assert!(!c.matches(&dt(2026, 1, 4, 9, 0)));
        assert!(!c.matches(&dt(2026, 4, 6, 9, 0)));
    }

    #[test]
    fn seven_is_sunday() {
        let c = CronExpr::parse("0 0 * * 7").unwrap();
        // 2026-07-12 is a Sunday.
        assert!(c.matches(&dt(2026, 7, 12, 0, 0)));
        assert!(!c.matches(&dt(2026, 7, 13, 0, 0)));
    }

    #[test]
    fn dow_ranges_involving_seven() {
        // "0-7" covers every day.
        let all = CronExpr::parse("0 0 * * 0-7").unwrap();
        for d in 12..=18 {
            assert!(all.matches(&dt(2026, 7, d, 0, 0)), "day {d}");
        }
        // "5-7" is Fri, Sat, Sun.
        let fss = CronExpr::parse("0 0 * * 5-7").unwrap();
        assert!(fss.matches(&dt(2026, 7, 10, 0, 0))); // Friday
        assert!(fss.matches(&dt(2026, 7, 11, 0, 0))); // Saturday
        assert!(fss.matches(&dt(2026, 7, 12, 0, 0))); // Sunday
        assert!(!fss.matches(&dt(2026, 7, 13, 0, 0))); // Monday
    }

    #[test]
    fn vixie_or_rule_when_both_restricted() {
        // "day 13 OR friday" - classic vixie semantics.
        let c = CronExpr::parse("0 0 13 * 5").unwrap();
        // 2026-02-13 is a Friday (both match).
        assert!(c.matches(&dt(2026, 2, 13, 0, 0)));
        // 2026-03-13 is a Friday.
        assert!(c.matches(&dt(2026, 3, 13, 0, 0)));
        // 2026-07-13 is a Monday -> matches via day-of-month.
        assert!(c.matches(&dt(2026, 7, 13, 0, 0)));
        // 2026-07-17 is a Friday -> matches via day-of-week.
        assert!(c.matches(&dt(2026, 7, 17, 0, 0)));
        // 2026-07-14 is a Tuesday, day 14 -> no match.
        assert!(!c.matches(&dt(2026, 7, 14, 0, 0)));
    }

    #[test]
    fn dom_restricted_dow_star() {
        let c = CronExpr::parse("0 0 13 * *").unwrap();
        assert!(c.matches(&dt(2026, 7, 13, 0, 0)));
        assert!(!c.matches(&dt(2026, 7, 17, 0, 0)));
    }

    #[test]
    fn aliases() {
        assert_eq!(
            CronExpr::parse("@hourly").unwrap(),
            CronExpr::parse("0 * * * *").unwrap()
        );
        assert_eq!(
            CronExpr::parse("@daily").unwrap(),
            CronExpr::parse("0 0 * * *").unwrap()
        );
        assert_eq!(
            CronExpr::parse("@weekly").unwrap(),
            CronExpr::parse("0 0 * * 0").unwrap()
        );
        assert_eq!(
            CronExpr::parse("@monthly").unwrap(),
            CronExpr::parse("0 0 1 * *").unwrap()
        );
        assert_eq!(
            CronExpr::parse("@yearly").unwrap(),
            CronExpr::parse("0 0 1 1 *").unwrap()
        );
        assert!(CronExpr::parse("@reboot").unwrap().reboot);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(CronExpr::parse("* * * *").is_err());
        assert!(CronExpr::parse("60 * * * *").is_err());
        assert!(CronExpr::parse("* 24 * * *").is_err());
        assert!(CronExpr::parse("* * 0 * *").is_err());
        assert!(CronExpr::parse("* * 32 * *").is_err());
        assert!(CronExpr::parse("* * * 13 *").is_err());
        assert!(CronExpr::parse("* * * * 8").is_err());
        assert!(CronExpr::parse("*/0 * * * *").is_err());
        assert!(CronExpr::parse("5-2 * * * *").is_err());
        assert!(CronExpr::parse("@fortnightly").is_err());
        assert!(CronExpr::parse("a * * * *").is_err());
    }

    #[test]
    fn next_after_simple() {
        let c = CronExpr::parse("15 2 * * *").unwrap();
        assert_eq!(
            c.next_after(dt(2026, 7, 8, 1, 0)),
            Some(dt(2026, 7, 8, 2, 15))
        );
        assert_eq!(
            c.next_after(dt(2026, 7, 8, 2, 15)),
            Some(dt(2026, 7, 9, 2, 15))
        );
        assert_eq!(
            c.next_after(dt(2026, 7, 8, 3, 0)),
            Some(dt(2026, 7, 9, 2, 15))
        );
    }

    #[test]
    fn next_after_month_rollover() {
        let c = CronExpr::parse("0 0 1 * *").unwrap();
        assert_eq!(
            c.next_after(dt(2026, 1, 31, 23, 59)),
            Some(dt(2026, 2, 1, 0, 0))
        );
        assert_eq!(
            c.next_after(dt(2026, 12, 15, 0, 0)),
            Some(dt(2027, 1, 1, 0, 0))
        );
    }

    #[test]
    fn next_after_leap_day() {
        let c = CronExpr::parse("0 12 29 2 *").unwrap();
        // Next Feb 29 after mid-2026 is in 2028.
        assert_eq!(
            c.next_after(dt(2026, 7, 1, 0, 0)),
            Some(dt(2028, 2, 29, 12, 0))
        );
    }

    #[test]
    fn next_after_impossible_returns_none() {
        let c = CronExpr::parse("0 0 31 2 *").unwrap();
        assert_eq!(c.next_after(dt(2026, 1, 1, 0, 0)), None);
    }

    #[test]
    fn next_after_dow() {
        let c = CronExpr::parse("30 8 * * mon").unwrap();
        // 2026-07-08 is a Wednesday; next Monday is 07-13.
        assert_eq!(
            c.next_after(dt(2026, 7, 8, 9, 0)),
            Some(dt(2026, 7, 13, 8, 30))
        );
    }

    #[test]
    fn next_after_truncates_seconds() {
        let c = CronExpr::parse("* * * * *").unwrap();
        let after = NaiveDate::from_ymd_opt(2026, 7, 8)
            .unwrap()
            .and_hms_opt(5, 44, 37)
            .unwrap();
        assert_eq!(c.next_after(after), Some(dt(2026, 7, 8, 5, 45)));
    }
}
