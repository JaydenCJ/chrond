//! Prometheus metrics: an in-process registry plus a tiny HTTP exposition
//! server (text format 0.0.4). No external HTTP framework; the endpoint
//! only ever binds where the user asks (default suggestion: 127.0.0.1).

use crate::history::RunStatus;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Default, Clone)]
struct JobMetrics {
    runs_total: BTreeMap<&'static str, u64>,
    last_run_epoch: Option<u64>,
    last_duration_seconds: Option<f64>,
    last_exit_code: Option<i32>,
}

/// Thread-safe metrics registry shared between the daemon loop and the
/// exposition server.
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<Mutex<BTreeMap<String, JobMetrics>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one finished (or skipped/missed) occurrence.
    pub fn record(
        &self,
        job: &str,
        status: RunStatus,
        duration_ms: Option<u64>,
        exit_code: Option<i32>,
    ) {
        let mut map = self.inner.lock().unwrap();
        let m = map.entry(job.to_string()).or_default();
        *m.runs_total.entry(status.as_str()).or_insert(0) += 1;
        if !matches!(status, RunStatus::Missed | RunStatus::SkippedOverlap) {
            m.last_run_epoch = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs());
            m.last_duration_seconds = duration_ms.map(|ms| ms as f64 / 1000.0);
            m.last_exit_code = exit_code;
        }
    }

    /// Render the Prometheus text exposition format.
    pub fn render(&self) -> String {
        let map = self.inner.lock().unwrap();
        let mut out = String::new();
        out.push_str(
            "# HELP chrond_job_runs_total Schedule occurrences accounted for, by job and status.\n",
        );
        out.push_str("# TYPE chrond_job_runs_total counter\n");
        for (job, m) in map.iter() {
            for (status, count) in &m.runs_total {
                out.push_str(&format!(
                    "chrond_job_runs_total{{job=\"{job}\",status=\"{status}\"}} {count}\n"
                ));
            }
        }
        out.push_str("# HELP chrond_job_last_run_timestamp_seconds Unix time of the last execution per job.\n");
        out.push_str("# TYPE chrond_job_last_run_timestamp_seconds gauge\n");
        for (job, m) in map.iter() {
            if let Some(ts) = m.last_run_epoch {
                out.push_str(&format!(
                    "chrond_job_last_run_timestamp_seconds{{job=\"{job}\"}} {ts}\n"
                ));
            }
        }
        out.push_str(
            "# HELP chrond_job_last_run_duration_seconds Duration of the last execution per job.\n",
        );
        out.push_str("# TYPE chrond_job_last_run_duration_seconds gauge\n");
        for (job, m) in map.iter() {
            if let Some(d) = m.last_duration_seconds {
                out.push_str(&format!(
                    "chrond_job_last_run_duration_seconds{{job=\"{job}\"}} {d}\n"
                ));
            }
        }
        out.push_str("# HELP chrond_job_last_exit_code Exit code of the last execution per job.\n");
        out.push_str("# TYPE chrond_job_last_exit_code gauge\n");
        for (job, m) in map.iter() {
            if let Some(c) = m.last_exit_code {
                out.push_str(&format!("chrond_job_last_exit_code{{job=\"{job}\"}} {c}\n"));
            }
        }
        out.push_str(&format!("chrond_jobs {}\n", map.len()));
        out
    }
}

fn handle_conn(mut stream: TcpStream, registry: &Registry) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let (status, content_type, body) = match path {
        "/metrics" => (
            "200 OK",
            "text/plain; version=0.0.4; charset=utf-8",
            registry.render(),
        ),
        "/health" => ("200 OK", "text/plain; charset=utf-8", "ok\n".to_string()),
        _ => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "not found\n".to_string(),
        ),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Bind and serve `/metrics` and `/health` on a background thread.
/// Returns the bound address (useful with port 0).
pub fn serve(addr: &str, registry: Registry) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            handle_conn(stream, &registry);
        }
    });
    Ok(local)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn render_counts_by_status() {
        let r = Registry::new();
        r.record("backup", RunStatus::Ok, Some(1200), Some(0));
        r.record("backup", RunStatus::Ok, Some(900), Some(0));
        r.record("backup", RunStatus::Failed, Some(50), Some(3));
        r.record("cleanup", RunStatus::Missed, None, None);
        let text = r.render();
        assert!(text.contains("chrond_job_runs_total{job=\"backup\",status=\"ok\"} 2"));
        assert!(text.contains("chrond_job_runs_total{job=\"backup\",status=\"failed\"} 1"));
        assert!(text.contains("chrond_job_runs_total{job=\"cleanup\",status=\"missed\"} 1"));
        assert!(text.contains("chrond_job_last_exit_code{job=\"backup\"} 3"));
        assert!(text.contains("chrond_jobs 2"));
        // A missed occurrence must not set a last-run timestamp.
        assert!(!text.contains("chrond_job_last_run_timestamp_seconds{job=\"cleanup\"}"));
    }

    #[test]
    fn http_server_serves_metrics_and_health() {
        let r = Registry::new();
        r.record("job1", RunStatus::Ok, Some(10), Some(0));
        let addr = serve("127.0.0.1:0", r).unwrap();

        let get = |path: &str| -> String {
            let mut s = TcpStream::connect(addr).unwrap();
            write!(s, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
            let mut buf = String::new();
            s.read_to_string(&mut buf).unwrap();
            buf
        };

        let metrics = get("/metrics");
        assert!(metrics.starts_with("HTTP/1.1 200 OK"));
        assert!(metrics.contains("chrond_job_runs_total{job=\"job1\",status=\"ok\"} 1"));

        let health = get("/health");
        assert!(health.starts_with("HTTP/1.1 200 OK"));
        assert!(health.contains("ok"));

        let missing = get("/nope");
        assert!(missing.starts_with("HTTP/1.1 404"));
    }
}
