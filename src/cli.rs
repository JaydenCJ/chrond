//! Command-line interface: argument parsing and the `check`, `runs` and
//! `status` subcommands. Kept dependency-free on purpose.

use crate::crontab::{parse_duration, Crontab};
use crate::daemon::{self, DaemonConfig};
use crate::history::{RunRecord, Store, TIME_FMT};
use chrono::{Duration as ChronoDuration, Local, NaiveDateTime};
use std::path::PathBuf;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
chrond — memory-safe cron with catch-up, run history, log rotation and alerting

USAGE:
    chrond <COMMAND> [OPTIONS]

COMMANDS:
    run       Run the scheduler daemon in the foreground
    check     Validate a crontab file and preview upcoming runs
    runs      Query the structured run history
    status    Show the latest outcome and next run per job

OPTIONS:
    -h, --help       Print this help
    -V, --version    Print version

Run 'chrond <COMMAND> --help' for command-specific options.";

const RUN_HELP: &str = "\
chrond run — run the scheduler daemon in the foreground

USAGE:
    chrond run --file <CRONTAB> [OPTIONS]

OPTIONS:
    -f, --file <PATH>       Crontab file to load (required)
    -s, --state <DIR>       State directory for history/state/logs
                            [default: $XDG_STATE_HOME/chrond or ~/.local/state/chrond]
        --system            Parse system crontab format (sixth user column)
        --metrics <ADDR>    Serve Prometheus /metrics and /health on ADDR
                            (bind 127.0.0.1:PORT unless you know better)
        --ntfy <URL>        Send ntfy notifications to this topic URL
        --exit-after <DUR>  Stop after DUR (e.g. 30s, 5m); for CI and smoke tests
    -h, --help              Print this help";

const CHECK_HELP: &str = "\
chrond check — validate a crontab file and preview upcoming runs

USAGE:
    chrond check <CRONTAB> [OPTIONS]

OPTIONS:
    -f, --file <PATH>    Crontab file (alternative to the positional argument)
        --system         Parse system crontab format (sixth user column)
    -n, --next <N>       Show the next N occurrences per job [default: 3]
    -h, --help           Print this help";

const RUNS_HELP: &str = "\
chrond runs — query the structured run history

USAGE:
    chrond runs [OPTIONS]

OPTIONS:
    -s, --state <DIR>    State directory [default: $XDG_STATE_HOME/chrond or ~/.local/state/chrond]
        --job <NAME>     Only records for this job
        --since <DUR>    Only records scheduled within the last DUR (e.g. 24h, 7d)
        --failed         Only failures (failed, timeout, spawn_error)
        --json           Output JSON lines instead of a table
        --limit <N>      Show at most the newest N records [default: 20]
    -h, --help           Print this help";

const STATUS_HELP: &str = "\
chrond status — show the latest outcome and next run per job

USAGE:
    chrond status [OPTIONS]

OPTIONS:
    -s, --state <DIR>    State directory [default: $XDG_STATE_HOME/chrond or ~/.local/state/chrond]
    -f, --file <PATH>    Crontab file (enables the NEXT RUN column)
        --system         Parse system crontab format (sixth user column)
    -h, --help           Print this help";

/// Errors carrying the intended process exit code.
pub struct CliError {
    pub message: String,
    pub code: i32,
}

fn usage_err(message: impl Into<String>) -> CliError {
    CliError {
        message: message.into(),
        code: 2,
    }
}

fn run_err(message: impl Into<String>) -> CliError {
    CliError {
        message: message.into(),
        code: 1,
    }
}

fn default_state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CHROND_STATE") {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("chrond");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/state/chrond");
    }
    PathBuf::from(".chrond-state")
}

struct Args {
    tokens: Vec<String>,
    pos: usize,
}

impl Args {
    fn next_value(&mut self, flag: &str) -> Result<String, CliError> {
        self.pos += 1;
        self.tokens
            .get(self.pos)
            .cloned()
            .ok_or_else(|| usage_err(format!("{flag} requires a value")))
    }
}

/// Entry point used by `main`. Returns the process exit code.
pub fn dispatch(argv: Vec<String>) -> i32 {
    match run_cli(argv) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("chrond: {}", e.message);
            e.code
        }
    }
}

fn run_cli(argv: Vec<String>) -> Result<(), CliError> {
    let Some(command) = argv.first().cloned() else {
        println!("{HELP}");
        return Err(usage_err("missing command"));
    };
    match command.as_str() {
        "-h" | "--help" | "help" => {
            println!("{HELP}");
            Ok(())
        }
        "-V" | "--version" | "version" => {
            println!("chrond {VERSION}");
            Ok(())
        }
        "run" => cmd_run(Args {
            tokens: argv,
            pos: 0,
        }),
        "check" => cmd_check(Args {
            tokens: argv,
            pos: 0,
        }),
        "runs" => cmd_runs(Args {
            tokens: argv,
            pos: 0,
        }),
        "status" => cmd_status(Args {
            tokens: argv,
            pos: 0,
        }),
        other => Err(usage_err(format!(
            "unknown command '{other}' (try 'chrond --help')"
        ))),
    }
}

fn cmd_run(mut args: Args) -> Result<(), CliError> {
    let mut file: Option<PathBuf> = None;
    let mut state = default_state_dir();
    let mut system_mode = false;
    let mut metrics_addr = None;
    let mut ntfy_url = None;
    let mut exit_after = None;
    while args.pos + 1 < args.tokens.len() {
        args.pos += 1;
        let tok = args.tokens[args.pos].clone();
        match tok.as_str() {
            "-f" | "--file" => file = Some(PathBuf::from(args.next_value(&tok)?)),
            "-s" | "--state" => state = PathBuf::from(args.next_value(&tok)?),
            "--system" => system_mode = true,
            "--metrics" => metrics_addr = Some(args.next_value(&tok)?),
            "--ntfy" => ntfy_url = Some(args.next_value(&tok)?),
            "--exit-after" => {
                let v = args.next_value(&tok)?;
                exit_after = Some(parse_duration(&v).map_err(usage_err)?);
            }
            "-h" | "--help" => {
                println!("{RUN_HELP}");
                return Ok(());
            }
            other => return Err(usage_err(format!("unknown option '{other}' for run"))),
        }
    }
    let file = file.ok_or_else(|| usage_err("run requires --file <CRONTAB>"))?;
    daemon::run(DaemonConfig {
        crontab_path: file,
        state_dir: state,
        system_mode,
        metrics_addr,
        ntfy_url,
        exit_after,
    })
    .map_err(run_err)
}

fn cmd_check(mut args: Args) -> Result<(), CliError> {
    let mut file: Option<PathBuf> = None;
    let mut system_mode = false;
    let mut next_n: usize = 3;
    while args.pos + 1 < args.tokens.len() {
        args.pos += 1;
        let tok = args.tokens[args.pos].clone();
        match tok.as_str() {
            "-f" | "--file" => file = Some(PathBuf::from(args.next_value(&tok)?)),
            "--system" => system_mode = true,
            "-n" | "--next" => {
                let v = args.next_value(&tok)?;
                next_n = v
                    .parse()
                    .map_err(|_| usage_err(format!("--next expects a number, got '{v}'")))?;
            }
            "-h" | "--help" => {
                println!("{CHECK_HELP}");
                return Ok(());
            }
            other if !other.starts_with('-') && file.is_none() => {
                file = Some(PathBuf::from(other));
            }
            other => return Err(usage_err(format!("unknown option '{other}' for check"))),
        }
    }
    let file = file.ok_or_else(|| usage_err("check requires a crontab file"))?;
    let content = std::fs::read_to_string(&file)
        .map_err(|e| run_err(format!("cannot read {}: {e}", file.display())))?;
    let tab = Crontab::parse(&content, system_mode)
        .map_err(|e| run_err(format!("{}: {e}", file.display())))?;

    println!(
        "{}: OK ({} job(s), {} environment assignment(s))",
        file.display(),
        tab.jobs.len(),
        tab.env.len()
    );
    let now = Local::now().naive_local();
    for job in &tab.jobs {
        println!();
        println!("  job: {}", job.name);
        println!("    schedule: {}", job.schedule_str);
        println!("    command:  {}", job.command);
        if job.catchup {
            println!("    catch-up: on (max {})", job.max_catchup);
        }
        if let Some(t) = job.timeout {
            println!("    timeout:  {}s", t.as_secs());
        }
        if job.schedule.reboot {
            println!("    next:     at daemon startup (@reboot)");
            continue;
        }
        let mut cursor = now;
        for i in 0..next_n {
            match job.schedule.next_after(cursor) {
                Some(t) => {
                    println!("    next[{}]:  {}", i + 1, t.format(TIME_FMT));
                    cursor = t;
                }
                None => {
                    println!("    next[{}]:  never (schedule can never match)", i + 1);
                    break;
                }
            }
        }
    }
    Ok(())
}

fn parse_since(v: &str) -> Result<NaiveDateTime, CliError> {
    let dur = parse_duration(v).map_err(usage_err)?;
    Ok(Local::now().naive_local() - ChronoDuration::seconds(dur.as_secs() as i64))
}

fn cmd_runs(mut args: Args) -> Result<(), CliError> {
    let mut state = default_state_dir();
    let mut job: Option<String> = None;
    let mut since: Option<NaiveDateTime> = None;
    let mut failed = false;
    let mut json = false;
    let mut limit: usize = 20;
    while args.pos + 1 < args.tokens.len() {
        args.pos += 1;
        let tok = args.tokens[args.pos].clone();
        match tok.as_str() {
            "-s" | "--state" => state = PathBuf::from(args.next_value(&tok)?),
            "--job" => job = Some(args.next_value(&tok)?),
            "--since" => since = Some(parse_since(&args.next_value(&tok)?)?),
            "--failed" => failed = true,
            "--json" => json = true,
            "--limit" => {
                let v = args.next_value(&tok)?;
                limit = v
                    .parse()
                    .map_err(|_| usage_err(format!("--limit expects a number, got '{v}'")))?;
            }
            "-h" | "--help" => {
                println!("{RUNS_HELP}");
                return Ok(());
            }
            other => return Err(usage_err(format!("unknown option '{other}' for runs"))),
        }
    }
    let store = Store::open(&state)
        .map_err(|e| run_err(format!("cannot open state dir {}: {e}", state.display())))?;
    let records = store
        .query(job.as_deref(), since, failed, Some(limit))
        .map_err(|e| run_err(format!("cannot read history: {e}")))?;
    if json {
        for r in &records {
            println!("{}", serde_json::to_string(r).unwrap());
        }
        return Ok(());
    }
    if records.is_empty() {
        println!(
            "no matching run records in {}",
            store.history_path().display()
        );
        return Ok(());
    }
    print_runs_table(&records);
    Ok(())
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let cut: String = line.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}\u{2026}")
    }
}

fn print_runs_table(records: &[RunRecord]) {
    let name_w = records
        .iter()
        .map(|r| r.job.len())
        .chain(std::iter::once(3))
        .max()
        .unwrap();
    println!(
        "{:<name_w$}  {:<19}  {:<15}  {:>4}  {:>9}  OUTPUT",
        "JOB", "SCHEDULED", "STATUS", "EXIT", "DURATION"
    );
    for r in records {
        let status = if r.catchup {
            format!("{} (catch-up)", r.status.as_str())
        } else {
            r.status.as_str().to_string()
        };
        println!(
            "{:<name_w$}  {:<19}  {:<15}  {:>4}  {:>9}  {}",
            r.job,
            r.scheduled,
            status,
            r.exit_code.map_or("-".to_string(), |c| c.to_string()),
            r.duration_ms.map_or("-".to_string(), |d| format!("{d}ms")),
            first_line(&r.output_tail, 40)
        );
    }
}

fn cmd_status(mut args: Args) -> Result<(), CliError> {
    let mut state = default_state_dir();
    let mut file: Option<PathBuf> = None;
    let mut system_mode = false;
    while args.pos + 1 < args.tokens.len() {
        args.pos += 1;
        let tok = args.tokens[args.pos].clone();
        match tok.as_str() {
            "-s" | "--state" => state = PathBuf::from(args.next_value(&tok)?),
            "-f" | "--file" => file = Some(PathBuf::from(args.next_value(&tok)?)),
            "--system" => system_mode = true,
            "-h" | "--help" => {
                println!("{STATUS_HELP}");
                return Ok(());
            }
            other => return Err(usage_err(format!("unknown option '{other}' for status"))),
        }
    }
    let store = Store::open(&state)
        .map_err(|e| run_err(format!("cannot open state dir {}: {e}", state.display())))?;
    let records = store
        .read_all()
        .map_err(|e| run_err(format!("cannot read history: {e}")))?;

    let mut latest: std::collections::BTreeMap<String, &RunRecord> = Default::default();
    for r in &records {
        latest.insert(r.job.clone(), r);
    }

    let tab = match &file {
        Some(f) => {
            let content = std::fs::read_to_string(f)
                .map_err(|e| run_err(format!("cannot read {}: {e}", f.display())))?;
            Some(
                Crontab::parse(&content, system_mode)
                    .map_err(|e| run_err(format!("{}: {e}", f.display())))?,
            )
        }
        None => None,
    };

    let mut names: Vec<String> = latest.keys().cloned().collect();
    if let Some(tab) = &tab {
        for j in &tab.jobs {
            if !names.contains(&j.name) {
                names.push(j.name.clone());
            }
        }
    }
    if names.is_empty() {
        println!(
            "no job history in {} (and no crontab given)",
            state.display()
        );
        return Ok(());
    }

    let now = Local::now().naive_local();
    let name_w = names
        .iter()
        .map(|n| n.len())
        .chain(std::iter::once(3))
        .max()
        .unwrap();
    println!(
        "{:<name_w$}  {:<19}  {:<15}  {:>4}  NEXT RUN",
        "JOB", "LAST SCHEDULED", "STATUS", "EXIT"
    );
    for name in &names {
        let (last, status, exit) = match latest.get(name) {
            Some(r) => (
                r.scheduled.clone(),
                r.status.as_str().to_string(),
                r.exit_code.map_or("-".to_string(), |c| c.to_string()),
            ),
            None => ("-".to_string(), "never ran".to_string(), "-".to_string()),
        };
        let next = tab
            .as_ref()
            .and_then(|t| t.jobs.iter().find(|j| &j.name == name))
            .map(|j| {
                if j.schedule.reboot {
                    "at daemon startup".to_string()
                } else {
                    j.schedule
                        .next_after(now)
                        .map(|t| t.format(TIME_FMT).to_string())
                        .unwrap_or_else(|| "never".to_string())
                }
            })
            .unwrap_or_else(|| "-".to_string());
        println!("{name:<name_w$}  {last:<19}  {status:<15}  {exit:>4}  {next}");
    }
    Ok(())
}
