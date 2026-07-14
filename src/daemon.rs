//! The foreground scheduler daemon: plans occurrences once per minute,
//! executes jobs on worker threads, persists state and history, updates
//! metrics and sends notifications.

use crate::alert::{build_message, should_notify, Notifier, NtfyNotifier};
use crate::crontab::{Crontab, JobSpec, OverlapPolicy};
use crate::history::{format_time, RunRecord, RunStatus, Store};
use crate::logrotate::append_run_log;
use crate::metrics::{self, Registry};
use crate::runner::run_job;
use crate::scheduler::{plan, JobState};
use chrono::{Local, NaiveDateTime, Timelike};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Daemon configuration assembled by the CLI.
pub struct DaemonConfig {
    pub crontab_path: PathBuf,
    pub state_dir: PathBuf,
    pub system_mode: bool,
    pub metrics_addr: Option<String>,
    pub ntfy_url: Option<String>,
    /// Stop after this long (useful for CI, smoke tests and supervised
    /// restarts). `None` = run until killed.
    pub exit_after: Option<Duration>,
}

struct Shared {
    store: Mutex<Store>,
    registry: Registry,
    running: Mutex<HashSet<String>>,
    notifier: Option<Box<dyn Notifier>>,
}

fn log_info(msg: &str) {
    println!("[chrond] {} {msg}", format_time(Local::now().naive_local()));
}

fn log_error(msg: &str) {
    eprintln!(
        "[chrond] {} ERROR {msg}",
        format_time(Local::now().naive_local())
    );
}

fn record_and_report(shared: &Shared, spec: &JobSpec, record: RunRecord) {
    shared.registry.record(
        &record.job,
        record.status,
        record.duration_ms,
        record.exit_code,
    );
    if let Err(e) = shared.store.lock().unwrap().append(&record) {
        log_error(&format!("failed to append history record: {e}"));
    }
    if let Some(notifier) = &shared.notifier {
        if should_notify(spec.notify, record.status) {
            let (title, body, priority) = build_message(&record);
            if let Err(e) = notifier.notify(&title, &body, priority) {
                log_error(&e);
            }
        }
    }
}

fn spawn_worker(
    shared: Arc<Shared>,
    spec: JobSpec,
    env: Vec<(String, String)>,
    scheduled: NaiveDateTime,
    is_catchup: bool,
    logs_dir: PathBuf,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let label = if is_catchup { " (catch-up)" } else { "" };
        log_info(&format!(
            "job {} started (scheduled {}){label}",
            spec.name,
            format_time(scheduled)
        ));
        let outcome = run_job(&spec, &env, scheduled, is_catchup);
        let record = outcome.record;
        let header = format!(
            "{} scheduled={} status={} exit={} duration={}ms{}",
            record.started.clone().unwrap_or_default(),
            record.scheduled,
            record.status.as_str(),
            record.exit_code.map_or("-".to_string(), |c| c.to_string()),
            record.duration_ms.unwrap_or(0),
            if is_catchup { " catchup=true" } else { "" }
        );
        if let Err(e) = append_run_log(
            &logs_dir,
            &spec.name,
            &header,
            &outcome.full_output,
            spec.log_max_bytes,
            spec.log_keep,
        ) {
            log_error(&format!("failed to write job log for {}: {e}", spec.name));
        }
        log_info(&format!(
            "job {} finished: {} (exit {}, {}ms)",
            spec.name,
            record.status.as_str(),
            record.exit_code.map_or("-".to_string(), |c| c.to_string()),
            record.duration_ms.unwrap_or(0)
        ));
        record_and_report(&shared, &spec, record);
        shared.running.lock().unwrap().remove(&spec.name);
    })
}

fn dispatch(
    shared: &Arc<Shared>,
    spec: &JobSpec,
    env: &[(String, String)],
    scheduled: NaiveDateTime,
    is_catchup: bool,
    logs_dir: &Path,
    workers: &mut Vec<JoinHandle<()>>,
) {
    {
        let mut running = shared.running.lock().unwrap();
        if running.contains(&spec.name) {
            if spec.overlap == OverlapPolicy::Skip {
                log_info(&format!(
                    "job {} skipped (previous run still in progress)",
                    spec.name
                ));
                record_and_report(
                    shared,
                    spec,
                    RunRecord {
                        job: spec.name.clone(),
                        scheduled: format_time(scheduled),
                        status: RunStatus::SkippedOverlap,
                        catchup: is_catchup,
                        started: None,
                        finished: None,
                        duration_ms: None,
                        exit_code: None,
                        output_tail: String::new(),
                    },
                );
                return;
            }
        } else {
            running.insert(spec.name.clone());
        }
    }
    workers.push(spawn_worker(
        shared.clone(),
        spec.clone(),
        env.to_vec(),
        scheduled,
        is_catchup,
        logs_dir.to_path_buf(),
    ));
}

/// Run the daemon until killed (or until `exit_after` elapses).
pub fn run(cfg: DaemonConfig) -> Result<(), String> {
    let content = std::fs::read_to_string(&cfg.crontab_path)
        .map_err(|e| format!("cannot read {}: {e}", cfg.crontab_path.display()))?;
    let tab = Crontab::parse(&content, cfg.system_mode)
        .map_err(|e| format!("{}: {e}", cfg.crontab_path.display()))?;

    let store = Store::open(&cfg.state_dir)
        .map_err(|e| format!("cannot open state dir {}: {e}", cfg.state_dir.display()))?;
    let logs_dir = store.logs_dir();
    let mut states: BTreeMap<String, JobState> = store
        .load_states()
        .map_err(|e| format!("cannot load state: {e}"))?;

    let registry = Registry::new();
    if let Some(addr) = &cfg.metrics_addr {
        let bound = metrics::serve(addr, registry.clone())
            .map_err(|e| format!("cannot bind metrics endpoint {addr}: {e}"))?;
        log_info(&format!(
            "metrics endpoint listening on http://{bound}/metrics"
        ));
    }

    let notifier: Option<Box<dyn Notifier>> = cfg
        .ntfy_url
        .as_deref()
        .map(|u| Box::new(NtfyNotifier::new(u)) as Box<dyn Notifier>);
    if notifier.is_some() {
        log_info("ntfy notifications enabled");
    }

    if cfg.system_mode {
        let current = std::env::var("USER").unwrap_or_default();
        for j in &tab.jobs {
            if let Some(u) = &j.user {
                if !current.is_empty() && u != &current {
                    log_info(&format!(
                        "warning: job {} declares user '{}' but chrond runs every job as '{}' (the user column is parsed for /etc/crontab compatibility only)",
                        j.name, u, current
                    ));
                }
            }
        }
    }

    let shared = Arc::new(Shared {
        store: Mutex::new(store),
        registry,
        running: Mutex::new(HashSet::new()),
        notifier,
    });
    let mut workers: Vec<JoinHandle<()>> = Vec::new();

    log_info(&format!(
        "starting: {} job(s) from {}, state in {}",
        tab.jobs.len(),
        cfg.crontab_path.display(),
        cfg.state_dir.display()
    ));

    // @reboot jobs run once at startup.
    let startup_time = Local::now().naive_local();
    for spec in tab.jobs.iter().filter(|j| j.schedule.reboot) {
        dispatch(
            &shared,
            spec,
            &tab.env,
            startup_time,
            false,
            &logs_dir,
            &mut workers,
        );
    }

    let started = Instant::now();
    let deadline = cfg.exit_after.map(|d| started + d);

    loop {
        let now = Local::now().naive_local();
        let now_min = now
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(now);
        let mut dirty = false;

        for spec in tab.jobs.iter().filter(|j| !j.schedule.reboot) {
            let state = states.entry(spec.name.clone()).or_default();
            let p = plan(spec, state, now);
            for missed in &p.missed {
                log_info(&format!(
                    "job {} missed occurrence {} (catch-up limit reached or disabled)",
                    spec.name,
                    format_time(*missed)
                ));
                record_and_report(
                    &shared,
                    spec,
                    RunRecord {
                        job: spec.name.clone(),
                        scheduled: format_time(*missed),
                        status: RunStatus::Missed,
                        catchup: false,
                        started: None,
                        finished: None,
                        duration_ms: None,
                        exit_code: None,
                        output_tail: String::new(),
                    },
                );
            }
            for occurrence in &p.run {
                let is_catchup = *occurrence < now_min;
                dispatch(
                    &shared,
                    spec,
                    &tab.env,
                    *occurrence,
                    is_catchup,
                    &logs_dir,
                    &mut workers,
                );
            }
            if state.last_scheduled != p.new_last_scheduled {
                state.last_scheduled = p.new_last_scheduled;
                dirty = true;
            }
        }

        if dirty {
            if let Err(e) = shared.store.lock().unwrap().save_states(&states) {
                log_error(&format!("failed to persist state: {e}"));
            }
        }

        workers.retain(|h| !h.is_finished());

        // Sleep until the next minute boundary (checking the deadline).
        let next_minute = now_min + chrono::Duration::minutes(1);
        loop {
            if let Some(d) = deadline {
                if Instant::now() >= d {
                    log_info("exit-after elapsed, waiting for running jobs");
                    for h in workers.drain(..) {
                        let _ = h.join();
                    }
                    log_info("shutdown complete");
                    return Ok(());
                }
            }
            if Local::now().naive_local() >= next_minute {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
