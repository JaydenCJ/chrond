//! Job execution: spawn the command through a shell, capture output,
//! enforce the per-job timeout.

use crate::crontab::JobSpec;
use crate::history::{format_time, RunRecord, RunStatus};
use chrono::{Local, NaiveDateTime};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Cap on the output tail stored per run record.
const OUTPUT_TAIL_BYTES: usize = 8 * 1024;

/// How long to wait for remaining pipe output after the job process has
/// exited or been killed (background children may still hold the pipe).
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Result of executing one occurrence, plus the full captured output for
/// the job log.
pub struct RunOutcome {
    pub record: RunRecord,
    pub full_output: String,
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Keep the last `max` bytes on a char boundary.
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("...{}", &s[start..])
}

/// Execute one occurrence of a job. Blocking; the daemon calls this from a
/// worker thread.
pub fn run_job(
    spec: &JobSpec,
    env: &[(String, String)],
    scheduled: NaiveDateTime,
    is_catchup: bool,
) -> RunOutcome {
    let shell = env
        .iter()
        .rev()
        .find(|(k, _)| k == "SHELL")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "/bin/sh".to_string());

    let started_at = Local::now().naive_local();
    let start = Instant::now();

    let mut cmd = Command::new(&shell);
    cmd.arg("-c")
        .arg(&spec.command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Run the job in its own process group so a timeout can kill the whole
    // tree, not just the shell.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    for (k, v) in env {
        if k != "SHELL" {
            cmd.env(k, v);
        }
    }
    cmd.env("SHELL", &shell);
    cmd.env("CHROND_JOB", &spec.name);
    cmd.env("CHROND_SCHEDULED", format_time(scheduled));

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RunOutcome {
                record: RunRecord {
                    job: spec.name.clone(),
                    scheduled: format_time(scheduled),
                    status: RunStatus::SpawnError,
                    catchup: is_catchup,
                    started: Some(format_time(started_at)),
                    finished: Some(format_time(Local::now().naive_local())),
                    duration_ms: Some(0),
                    exit_code: None,
                    output_tail: format!("failed to spawn '{shell}': {e}"),
                },
                full_output: String::new(),
            }
        }
    };

    // Drain stdout/stderr on separate threads to avoid pipe deadlock. The
    // results come back over channels so collection can be time-bounded
    // (a background child of the job may hold the pipe open forever).
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let (err_tx, err_rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = out_tx.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = err_tx.send(buf);
    });

    let pid = child.id();
    let kill_tree = |child: &mut std::process::Child| {
        // Kill the whole process group first (the job ran with setpgid),
        // then the direct child as a fallback.
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = child.kill();
    };

    let mut timed_out = false;
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if let Some(limit) = spec.timeout {
                    if start.elapsed() >= limit {
                        timed_out = true;
                        kill_tree(&mut child);
                        break child.wait().ok();
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };

    let stdout = String::from_utf8_lossy(
        &out_rx
            .recv_timeout(OUTPUT_DRAIN_TIMEOUT)
            .unwrap_or_default(),
    )
    .into_owned();
    let stderr = String::from_utf8_lossy(
        &err_rx
            .recv_timeout(OUTPUT_DRAIN_TIMEOUT)
            .unwrap_or_default(),
    )
    .into_owned();
    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }

    let finished_at = Local::now().naive_local();
    let duration_ms = start.elapsed().as_millis() as u64;
    let exit_code = exit_status.and_then(|s| s.code());
    let status = if timed_out {
        RunStatus::Timeout
    } else if exit_code == Some(0) {
        RunStatus::Ok
    } else {
        RunStatus::Failed
    };

    RunOutcome {
        record: RunRecord {
            job: spec.name.clone(),
            scheduled: format_time(scheduled),
            status,
            catchup: is_catchup,
            started: Some(format_time(started_at)),
            finished: Some(format_time(finished_at)),
            duration_ms: Some(duration_ms),
            exit_code,
            output_tail: tail(combined.trim_end(), OUTPUT_TAIL_BYTES),
        },
        full_output: combined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crontab::Crontab;
    use chrono::NaiveDate;

    fn scheduled() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 7, 8)
            .unwrap()
            .and_hms_opt(2, 15, 0)
            .unwrap()
    }

    fn job(line: &str) -> JobSpec {
        Crontab::parse(line, false).unwrap().jobs.remove(0)
    }

    #[test]
    fn successful_run() {
        let spec = job("* * * * * echo hello-from-job\n");
        let out = run_job(&spec, &[], scheduled(), false);
        assert_eq!(out.record.status, RunStatus::Ok);
        assert_eq!(out.record.exit_code, Some(0));
        assert!(out.record.output_tail.contains("hello-from-job"));
        assert!(out.record.duration_ms.is_some());
    }

    #[test]
    fn failed_run_captures_exit_code_and_stderr() {
        let spec = job("* * * * * sh -c 'echo boom >&2; exit 3'\n");
        let out = run_job(&spec, &[], scheduled(), false);
        assert_eq!(out.record.status, RunStatus::Failed);
        assert_eq!(out.record.exit_code, Some(3));
        assert!(out.record.output_tail.contains("boom"));
    }

    #[test]
    fn timeout_kills_the_job() {
        let spec = job("#[chrond] timeout=1s\n* * * * * sleep 30\n");
        let start = Instant::now();
        let out = run_job(&spec, &[], scheduled(), false);
        assert_eq!(out.record.status, RunStatus::Timeout);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn env_is_passed_to_job() {
        let spec = job("* * * * * printenv GREETING\n");
        let env = vec![("GREETING".to_string(), "konnichiwa".to_string())];
        let out = run_job(&spec, &env, scheduled(), false);
        assert_eq!(out.record.status, RunStatus::Ok);
        assert!(out.record.output_tail.contains("konnichiwa"));
    }

    #[test]
    fn chrond_context_vars_are_set() {
        let spec = job("* * * * * printenv CHROND_JOB CHROND_SCHEDULED\n");
        let out = run_job(&spec, &[], scheduled(), false);
        assert!(out.record.output_tail.contains("printenv"));
        assert!(out.record.output_tail.contains("2026-07-08T02:15:00"));
    }

    #[test]
    fn spawn_error_is_reported() {
        let spec = job("* * * * * whatever\n");
        let env = vec![("SHELL".to_string(), "/nonexistent/shell".to_string())];
        let out = run_job(&spec, &env, scheduled(), false);
        assert_eq!(out.record.status, RunStatus::SpawnError);
        assert!(out.record.output_tail.contains("failed to spawn"));
    }

    #[test]
    fn tail_truncates_on_char_boundary() {
        let s = "あいうえお".repeat(100);
        let t = tail(&s, 32);
        assert!(t.len() <= 40);
        assert!(t.starts_with("..."));
    }
}
