//! Pure scheduling decisions: given a job spec, its persisted state and the
//! current time, decide which occurrences to run now, which missed
//! occurrences to catch up, and which to record as missed.
//!
//! No I/O happens here; the daemon loop applies these decisions.

use crate::crontab::JobSpec;
use chrono::{NaiveDateTime, Timelike};

/// Persisted per-job state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobState {
    /// The last schedule occurrence that was accounted for (run, caught up,
    /// or recorded as missed).
    pub last_scheduled: Option<NaiveDateTime>,
}

/// The outcome of one planning pass for one job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Occurrences to execute now, oldest first. Contains at most
    /// `max_catchup` catch-up occurrences plus the currently-due one.
    pub run: Vec<NaiveDateTime>,
    /// Missed occurrences that will NOT be executed (catch-up disabled or
    /// beyond `max_catchup`); recorded in history as `missed`.
    pub missed: Vec<NaiveDateTime>,
    /// New value for `last_scheduled` after this pass.
    pub new_last_scheduled: Option<NaiveDateTime>,
}

/// Upper bound on occurrences examined per pass, so a `* * * * *` job that
/// was down for a year cannot flood memory.
const MAX_OCCURRENCES: usize = 1000;

fn truncate_minute(t: NaiveDateTime) -> NaiveDateTime {
    t.with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(t)
}

/// Compute the plan for one job.
///
/// * First sighting of a job (no state): nothing runs; `last_scheduled` is
///   initialized to the current minute so future passes have a baseline.
///   (`@reboot` jobs are handled by the daemon at startup, not here.)
/// * Occurrences in `(last_scheduled, now)` are "missed" (the daemon was
///   down or busy): run the newest `max_catchup` of them when `catchup` is
///   enabled, record the rest as missed.
/// * An occurrence equal to the current minute is normally due and always
///   runs.
pub fn plan(spec: &JobSpec, state: &JobState, now: NaiveDateTime) -> Plan {
    let now_min = truncate_minute(now);
    let baseline = match state.last_scheduled {
        Some(t) => t,
        None => {
            return Plan {
                run: vec![],
                missed: vec![],
                new_last_scheduled: Some(now_min),
            }
        }
    };

    let mut occurrences: Vec<NaiveDateTime> = Vec::new();
    let mut cursor = baseline;
    while let Some(next) = spec.schedule.next_after(cursor) {
        if next > now_min {
            break;
        }
        occurrences.push(next);
        cursor = next;
        if occurrences.len() >= MAX_OCCURRENCES {
            break;
        }
    }

    if occurrences.is_empty() {
        return Plan {
            run: vec![],
            missed: vec![],
            new_last_scheduled: Some(baseline),
        };
    }

    let new_last = *occurrences.last().unwrap();
    let due_now: Vec<NaiveDateTime> = occurrences
        .iter()
        .copied()
        .filter(|o| *o == now_min)
        .collect();
    let past: Vec<NaiveDateTime> = occurrences
        .iter()
        .copied()
        .filter(|o| *o < now_min)
        .collect();

    let (mut run, missed) = if spec.catchup {
        let keep = spec.max_catchup as usize;
        let split = past.len().saturating_sub(keep);
        (past[split..].to_vec(), past[..split].to_vec())
    } else {
        (Vec::new(), past)
    };
    run.extend(due_now);

    Plan {
        run,
        missed,
        new_last_scheduled: Some(new_last),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crontab::Crontab;
    use chrono::NaiveDate;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn job(crontab_snippet: &str) -> JobSpec {
        Crontab::parse(crontab_snippet, false)
            .unwrap()
            .jobs
            .remove(0)
    }

    #[test]
    fn first_sighting_initializes_state_without_running() {
        let spec = job("* * * * * echo hi\n");
        let p = plan(&spec, &JobState::default(), dt(2026, 7, 8, 10, 30));
        assert!(p.run.is_empty());
        assert!(p.missed.is_empty());
        assert_eq!(p.new_last_scheduled, Some(dt(2026, 7, 8, 10, 30)));
    }

    #[test]
    fn due_now_runs() {
        let spec = job("30 10 * * * echo hi\n");
        let state = JobState {
            last_scheduled: Some(dt(2026, 7, 8, 10, 29)),
        };
        let p = plan(&spec, &state, dt(2026, 7, 8, 10, 30));
        assert_eq!(p.run, vec![dt(2026, 7, 8, 10, 30)]);
        assert!(p.missed.is_empty());
        assert_eq!(p.new_last_scheduled, Some(dt(2026, 7, 8, 10, 30)));
    }

    #[test]
    fn nothing_due_keeps_state() {
        let spec = job("0 0 * * * echo hi\n");
        let state = JobState {
            last_scheduled: Some(dt(2026, 7, 8, 0, 0)),
        };
        let p = plan(&spec, &state, dt(2026, 7, 8, 10, 30));
        assert!(p.run.is_empty());
        assert!(p.missed.is_empty());
        assert_eq!(p.new_last_scheduled, Some(dt(2026, 7, 8, 0, 0)));
    }

    #[test]
    fn missed_without_catchup_is_recorded_not_run() {
        // Daily 02:15 job, daemon was down for 26 hours.
        let spec = job("15 2 * * * backup.sh\n");
        let state = JobState {
            last_scheduled: Some(dt(2026, 7, 7, 2, 15)),
        };
        let p = plan(&spec, &state, dt(2026, 7, 8, 4, 15));
        assert!(p.run.is_empty());
        assert_eq!(p.missed, vec![dt(2026, 7, 8, 2, 15)]);
        assert_eq!(p.new_last_scheduled, Some(dt(2026, 7, 8, 2, 15)));
    }

    #[test]
    fn missed_with_catchup_runs() {
        let spec = job("#[chrond] catchup=on\n15 2 * * * backup.sh\n");
        let state = JobState {
            last_scheduled: Some(dt(2026, 7, 7, 2, 15)),
        };
        let p = plan(&spec, &state, dt(2026, 7, 8, 4, 15));
        assert_eq!(p.run, vec![dt(2026, 7, 8, 2, 15)]);
        assert!(p.missed.is_empty());
    }

    #[test]
    fn catchup_capped_by_max_catchup_keeps_newest() {
        // Hourly job, down for 5 hours, max_catchup=2: run the 2 newest
        // missed occurrences, record the 2 oldest as missed.
        let spec = job("#[chrond] catchup=on max_catchup=2\n0 * * * * tick\n");
        let state = JobState {
            last_scheduled: Some(dt(2026, 7, 8, 0, 0)),
        };
        let p = plan(&spec, &state, dt(2026, 7, 8, 4, 30));
        assert_eq!(p.run, vec![dt(2026, 7, 8, 3, 0), dt(2026, 7, 8, 4, 0)]);
        assert_eq!(p.missed, vec![dt(2026, 7, 8, 1, 0), dt(2026, 7, 8, 2, 0)]);
        assert_eq!(p.new_last_scheduled, Some(dt(2026, 7, 8, 4, 0)));
    }

    #[test]
    fn catchup_plus_due_now() {
        // Hourly job, down since 01:30, planning exactly at 04:00: catch up
        // 02:00 and 03:00 (max 2 by default -> only 03:00), plus run 04:00.
        let spec = job("#[chrond] catchup=on max_catchup=1\n0 * * * * tick\n");
        let state = JobState {
            last_scheduled: Some(dt(2026, 7, 8, 1, 0)),
        };
        let p = plan(&spec, &state, dt(2026, 7, 8, 4, 0));
        assert_eq!(p.run, vec![dt(2026, 7, 8, 3, 0), dt(2026, 7, 8, 4, 0)]);
        assert_eq!(p.missed, vec![dt(2026, 7, 8, 2, 0)]);
    }

    #[test]
    fn occurrence_flood_is_capped() {
        let spec = job("#[chrond] catchup=on\n* * * * * tick\n");
        // Down for a year: ~525k occurrences, capped at 1000.
        let state = JobState {
            last_scheduled: Some(dt(2025, 7, 8, 0, 0)),
        };
        let p = plan(&spec, &state, dt(2026, 7, 8, 0, 0));
        assert_eq!(p.run.len(), 1);
        assert!(p.missed.len() < 1001);
        // State advances so the next pass makes progress.
        assert!(p.new_last_scheduled.unwrap() > dt(2025, 7, 8, 0, 0));
    }

    #[test]
    fn seconds_are_ignored() {
        let spec = job("30 10 * * * echo hi\n");
        let state = JobState {
            last_scheduled: Some(dt(2026, 7, 8, 10, 29)),
        };
        let now = NaiveDate::from_ymd_opt(2026, 7, 8)
            .unwrap()
            .and_hms_opt(10, 30, 42)
            .unwrap();
        let p = plan(&spec, &state, now);
        assert_eq!(p.run, vec![dt(2026, 7, 8, 10, 30)]);
    }
}
