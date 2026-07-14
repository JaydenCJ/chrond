//! End-to-end tests that exercise the compiled `chrond` binary: crontab
//! validation, the foreground daemon (catch-up, @reboot, failures), the
//! history queries and the Prometheus endpoint. Everything runs against
//! temporary directories and 127.0.0.1 only.

use chrono::{Duration as ChronoDuration, Local};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_chrond")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("failed to run chrond binary")
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chrond-cli-test-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn help_and_version() {
    let help = run(&["--help"]);
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    for cmd in ["run", "check", "runs", "status"] {
        assert!(text.contains(cmd), "help must mention '{cmd}'");
    }

    let version = run(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("chrond {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_command_fails_with_usage_error() {
    let out = run(&["frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown command"));
}

#[test]
fn check_validates_a_crontab() {
    let dir = tempdir("check");
    let tab = dir.join("crontab");
    fs::write(
        &tab,
        "#[chrond] name=backup catchup=on timeout=30m overlap=skip\n\
         15 2 * * * /usr/local/bin/backup.sh --full\n\
         @hourly echo tick\n",
    )
    .unwrap();
    let out = run(&["check", tab.to_str().unwrap()]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("OK (2 job(s)"));
    assert!(text.contains("job: backup"));
    assert!(text.contains("catch-up: on"));
    assert!(text.contains("next[1]:"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn check_reports_parse_errors_with_line_numbers() {
    let dir = tempdir("check-bad");
    let tab = dir.join("crontab");
    fs::write(&tab, "# fine\n61 * * * * boom\n").unwrap();
    let out = run(&["check", tab.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("line 2"), "stderr was: {err}");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn check_missing_file_is_a_readable_error() {
    let out = run(&["check", "/nonexistent/crontab"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read"));
}

#[test]
fn runs_on_empty_state_reports_no_records() {
    let dir = tempdir("runs-empty");
    let out = run(&["runs", "--state", dir.to_str().unwrap()]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("no matching run records"));
    fs::remove_dir_all(&dir).unwrap();
}

/// Full daemon pass: @reboot job, catch-up of pre-seeded missed occurrences,
/// a failing job, then history/status queries and the metrics endpoint.
#[test]
fn daemon_end_to_end() {
    let dir = tempdir("daemon");
    let state = dir.join("state");
    fs::create_dir_all(&state).unwrap();
    let tab = dir.join("crontab");
    fs::write(
        &tab,
        "#[chrond] name=hello\n\
         @reboot echo hello-from-chrond\n\
         #[chrond] name=tick catchup=on max_catchup=2\n\
         * * * * * echo tick-ran\n\
         #[chrond] name=badjob notify=never\n\
         @reboot sh -c 'echo boom >&2; exit 3'\n",
    )
    .unwrap();

    // Pre-seed state: `tick` last accounted for 4 minutes ago, so the daemon
    // sees 3 missed occurrences plus the current minute: it catches up the
    // newest max_catchup=2 and records the oldest as `missed`.
    let four_min_ago = (Local::now().naive_local() - ChronoDuration::minutes(4))
        .format("%Y-%m-%dT%H:%M:00")
        .to_string();
    fs::write(
        state.join("state.json"),
        format!(r#"{{"jobs":{{"tick":{{"last_scheduled":"{four_min_ago}"}}}}}}"#),
    )
    .unwrap();

    // Run the daemon in the foreground on a background thread and poll the
    // metrics endpoint while it is up.
    let metrics_addr = "127.0.0.1:39633";
    let out = std::thread::scope(|s| {
        let handle = s.spawn(|| {
            run(&[
                "run",
                "--file",
                tab.to_str().unwrap(),
                "--state",
                state.to_str().unwrap(),
                "--metrics",
                metrics_addr,
                "--exit-after",
                "3s",
            ])
        });
        // Give the daemon a moment to bind and run the startup jobs.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let health = http_get(metrics_addr, "/health");
        assert!(health.starts_with("HTTP/1.1 200"), "health was: {health}");
        let metrics = http_get(metrics_addr, "/metrics");
        assert!(
            metrics.contains("chrond_job_runs_total"),
            "metrics was: {metrics}"
        );
        handle.join().unwrap()
    });
    assert!(out.status.success(), "daemon exited with {:?}", out.status);

    // History must contain: hello ok, badjob failed exit 3, tick catch-ups.
    let json = run(&["runs", "--state", state.to_str().unwrap(), "--json"]);
    assert!(json.status.success());
    let lines = String::from_utf8_lossy(&json.stdout);
    assert!(lines.contains(r#""job":"hello","#), "history: {lines}");
    assert!(lines.contains("hello-from-chrond"));
    assert!(lines.contains(r#""job":"badjob","#));
    assert!(lines.contains(r#""status":"failed""#));
    assert!(lines.contains(r#""exit_code":3"#));
    assert!(lines.contains(r#""job":"tick","#));
    assert!(lines.contains(r#""catchup":true"#));
    assert!(
        lines.contains(r#""status":"missed""#),
        "3rd missed minute is recorded"
    );

    // --failed filters down to the failing job only.
    let failed = run(&["runs", "--state", state.to_str().unwrap(), "--failed"]);
    let table = String::from_utf8_lossy(&failed.stdout);
    assert!(table.contains("badjob"));
    assert!(!table.contains("hello "));

    // status shows the latest outcome and the next planned run.
    let status = run(&[
        "status",
        "--state",
        state.to_str().unwrap(),
        "--file",
        tab.to_str().unwrap(),
    ]);
    assert!(status.status.success());
    let stext = String::from_utf8_lossy(&status.stdout);
    assert!(stext.contains("hello"));
    assert!(stext.contains("at daemon startup"));
    assert!(stext.contains("tick"));
    fs::remove_dir_all(&dir).unwrap();
}

fn http_get(addr: &str, path: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect to metrics endpoint");
    write!(s, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    buf
}
