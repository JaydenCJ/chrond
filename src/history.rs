//! Structured run history (append-only JSONL) and persisted daemon state.
//!
//! Every schedule occurrence produces exactly one record — including the
//! ones that never ran (`missed`, `skipped_overlap`). This is what makes
//! "did last night's backup run?" answerable with one command.

use crate::scheduler::JobState;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub const TIME_FMT: &str = "%Y-%m-%dT%H:%M:%S";

/// Outcome of a schedule occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Command exited 0.
    Ok,
    /// Command exited non-zero.
    Failed,
    /// Command was killed after exceeding its timeout.
    Timeout,
    /// Occurrence passed while the daemon was down and was not caught up.
    Missed,
    /// Occurrence skipped because the previous run was still going
    /// (overlap=skip).
    SkippedOverlap,
    /// Command could not be spawned at all.
    SpawnError,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Ok => "ok",
            RunStatus::Failed => "failed",
            RunStatus::Timeout => "timeout",
            RunStatus::Missed => "missed",
            RunStatus::SkippedOverlap => "skipped_overlap",
            RunStatus::SpawnError => "spawn_error",
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            RunStatus::Failed | RunStatus::Timeout | RunStatus::SpawnError
        )
    }
}

/// One structured record per schedule occurrence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub job: String,
    /// The schedule occurrence this record accounts for (local time).
    pub scheduled: String,
    pub status: RunStatus,
    /// True when this execution was a catch-up of a missed occurrence.
    #[serde(default)]
    pub catchup: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Last bytes of combined stdout/stderr (capped).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_tail: String,
}

/// Append-only JSONL history plus the state file, rooted in a state dir.
pub struct Store {
    dir: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    jobs: BTreeMap<String, StateEntry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    last_scheduled: Option<String>,
}

impl Store {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(dir)?;
        fs::create_dir_all(dir.join("logs"))?;
        Ok(Store {
            dir: dir.to_path_buf(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn history_path(&self) -> PathBuf {
        self.dir.join("history.jsonl")
    }

    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    pub fn append(&self, record: &RunRecord) -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.history_path())?;
        let line = serde_json::to_string(record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        writeln!(f, "{line}")
    }

    /// Read all records, oldest first. Unparseable lines are skipped.
    pub fn read_all(&self) -> std::io::Result<Vec<RunRecord>> {
        let path = self.history_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let f = File::open(path)?;
        let mut out = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(r) = serde_json::from_str::<RunRecord>(&line) {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Query records with optional filters. Returns newest-last, capped to
    /// the newest `limit` when given.
    pub fn query(
        &self,
        job: Option<&str>,
        since: Option<NaiveDateTime>,
        failed_only: bool,
        limit: Option<usize>,
    ) -> std::io::Result<Vec<RunRecord>> {
        let mut records: Vec<RunRecord> = self
            .read_all()?
            .into_iter()
            .filter(|r| job.map_or(true, |j| r.job == j))
            .filter(|r| {
                since.map_or(true, |s| {
                    NaiveDateTime::parse_from_str(&r.scheduled, TIME_FMT)
                        .map(|t| t >= s)
                        .unwrap_or(false)
                })
            })
            .filter(|r| !failed_only || r.status.is_failure())
            .collect();
        if let Some(limit) = limit {
            let len = records.len();
            if len > limit {
                records.drain(..len - limit);
            }
        }
        Ok(records)
    }

    pub fn load_states(&self) -> std::io::Result<BTreeMap<String, JobState>> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let content = fs::read_to_string(path)?;
        let parsed: StateFile = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut out = BTreeMap::new();
        for (name, entry) in parsed.jobs {
            let last = entry
                .last_scheduled
                .and_then(|s| NaiveDateTime::parse_from_str(&s, TIME_FMT).ok());
            out.insert(
                name,
                JobState {
                    last_scheduled: last,
                },
            );
        }
        Ok(out)
    }

    pub fn save_states(&self, states: &BTreeMap<String, JobState>) -> std::io::Result<()> {
        let file = StateFile {
            jobs: states
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        StateEntry {
                            last_scheduled: v
                                .last_scheduled
                                .map(|t| t.format(TIME_FMT).to_string()),
                        },
                    )
                })
                .collect(),
        };
        let tmp = self.state_path().with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&file).unwrap())?;
        fs::rename(tmp, self.state_path())
    }
}

pub fn format_time(t: NaiveDateTime) -> String {
    t.format(TIME_FMT).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn dt(d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 7, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chrond-history-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn record(job: &str, day: u32, status: RunStatus) -> RunRecord {
        RunRecord {
            job: job.to_string(),
            scheduled: format_time(dt(day, 2, 15)),
            status,
            catchup: false,
            started: Some(format_time(dt(day, 2, 15))),
            finished: Some(format_time(dt(day, 2, 16))),
            duration_ms: Some(60_000),
            exit_code: Some(if status == RunStatus::Ok { 0 } else { 1 }),
            output_tail: "done".into(),
        }
    }

    #[test]
    fn append_and_read_roundtrip() {
        let dir = tempdir("roundtrip");
        let store = Store::open(&dir).unwrap();
        store.append(&record("backup", 6, RunStatus::Ok)).unwrap();
        store
            .append(&record("backup", 7, RunStatus::Failed))
            .unwrap();
        let all = store.read_all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].job, "backup");
        assert_eq!(all[1].status, RunStatus::Failed);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn query_filters() {
        let dir = tempdir("query");
        let store = Store::open(&dir).unwrap();
        store.append(&record("backup", 5, RunStatus::Ok)).unwrap();
        store
            .append(&record("backup", 6, RunStatus::Failed))
            .unwrap();
        store.append(&record("cleanup", 6, RunStatus::Ok)).unwrap();
        store
            .append(&record("backup", 7, RunStatus::Timeout))
            .unwrap();

        let backups = store.query(Some("backup"), None, false, None).unwrap();
        assert_eq!(backups.len(), 3);

        let failed = store.query(None, None, true, None).unwrap();
        assert_eq!(failed.len(), 2);
        assert!(failed.iter().all(|r| r.status.is_failure()));

        let since = store
            .query(Some("backup"), Some(dt(6, 0, 0)), false, None)
            .unwrap();
        assert_eq!(since.len(), 2);

        let limited = store.query(None, None, false, Some(2)).unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[1].job, "backup");
        assert_eq!(limited[1].status, RunStatus::Timeout);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn state_roundtrip() {
        let dir = tempdir("state");
        let store = Store::open(&dir).unwrap();
        let mut states = BTreeMap::new();
        states.insert(
            "backup".to_string(),
            JobState {
                last_scheduled: Some(dt(7, 2, 15)),
            },
        );
        states.insert("fresh".to_string(), JobState::default());
        store.save_states(&states).unwrap();
        let loaded = store.load_states().unwrap();
        assert_eq!(loaded, states);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_history_lines_are_skipped() {
        let dir = tempdir("corrupt");
        let store = Store::open(&dir).unwrap();
        store.append(&record("backup", 6, RunStatus::Ok)).unwrap();
        fs::write(
            store.history_path(),
            format!(
                "{}\nnot-json\n",
                serde_json::to_string(&record("backup", 6, RunStatus::Ok)).unwrap()
            ),
        )
        .unwrap();
        assert_eq!(store.read_all().unwrap().len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
