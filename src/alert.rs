//! Push notifications for job outcomes.
//!
//! The only built-in transport is [ntfy](https://ntfy.sh) — self-hostable,
//! a plain HTTP POST per message. Tests point the notifier at a local mock
//! server; nothing ever leaves the machine unless the user configures a URL.

use crate::crontab::NotifyPolicy;
use crate::history::{RunRecord, RunStatus};
use std::time::Duration;

/// Notification transport abstraction (real: ntfy; tests may use any
/// local HTTP listener).
pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, body: &str, priority: &str) -> Result<(), String>;
}

/// Sends messages to an ntfy topic URL, e.g. `https://ntfy.sh/my-alerts`
/// or a self-hosted `http://127.0.0.1:8080/chrond`.
pub struct NtfyNotifier {
    url: String,
    agent: ureq::Agent,
}

impl NtfyNotifier {
    pub fn new(url: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .build();
        NtfyNotifier {
            url: url.to_string(),
            agent,
        }
    }
}

impl Notifier for NtfyNotifier {
    fn notify(&self, title: &str, body: &str, priority: &str) -> Result<(), String> {
        self.agent
            .post(&self.url)
            .set("Title", title)
            .set("Priority", priority)
            .set("Tags", "alarm_clock")
            .send_string(body)
            .map(|_| ())
            .map_err(|e| format!("ntfy notification failed: {e}"))
    }
}

/// Whether this record should produce a notification under the job's policy.
pub fn should_notify(policy: NotifyPolicy, status: RunStatus) -> bool {
    match policy {
        NotifyPolicy::Never => false,
        NotifyPolicy::Always => !matches!(status, RunStatus::Missed | RunStatus::SkippedOverlap),
        NotifyPolicy::OnFailure => status.is_failure(),
    }
}

/// Build the (title, body, priority) triple for a run record.
pub fn build_message(record: &RunRecord) -> (String, String, &'static str) {
    let title = match record.status {
        RunStatus::Ok => format!("chrond: {} succeeded", record.job),
        RunStatus::Failed => format!("chrond: {} FAILED", record.job),
        RunStatus::Timeout => format!("chrond: {} TIMED OUT", record.job),
        RunStatus::SpawnError => format!("chrond: {} could not start", record.job),
        RunStatus::Missed => format!("chrond: {} missed a run", record.job),
        RunStatus::SkippedOverlap => format!("chrond: {} skipped (overlap)", record.job),
    };
    let mut body = format!("scheduled {}", record.scheduled);
    if let Some(code) = record.exit_code {
        body.push_str(&format!(", exit {code}"));
    }
    if let Some(ms) = record.duration_ms {
        body.push_str(&format!(", took {ms}ms"));
    }
    if !record.output_tail.is_empty() {
        let tail: String = record.output_tail.chars().rev().take(300).collect();
        let tail: String = tail.chars().rev().collect();
        body.push('\n');
        body.push_str(&tail);
    }
    let priority = if record.status.is_failure() {
        "high"
    } else {
        "default"
    };
    (title, body, priority)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(status: RunStatus) -> RunRecord {
        RunRecord {
            job: "backup".into(),
            scheduled: "2026-07-08T02:15:00".into(),
            status,
            catchup: false,
            started: None,
            finished: None,
            duration_ms: Some(1500),
            exit_code: Some(if status == RunStatus::Ok { 0 } else { 3 }),
            output_tail: "tar: /data: file changed".into(),
        }
    }

    #[test]
    fn policy_matrix() {
        use NotifyPolicy::*;
        assert!(!should_notify(Never, RunStatus::Failed));
        assert!(should_notify(OnFailure, RunStatus::Failed));
        assert!(should_notify(OnFailure, RunStatus::Timeout));
        assert!(should_notify(OnFailure, RunStatus::SpawnError));
        assert!(!should_notify(OnFailure, RunStatus::Ok));
        assert!(!should_notify(OnFailure, RunStatus::Missed));
        assert!(should_notify(Always, RunStatus::Ok));
        assert!(should_notify(Always, RunStatus::Failed));
        assert!(!should_notify(Always, RunStatus::SkippedOverlap));
    }

    /// The real notifier posts to a local mock ntfy server; nothing leaves
    /// the machine.
    #[test]
    fn ntfy_notifier_posts_title_priority_and_body() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let mut request = String::new();
            loop {
                let n = stream.read(&mut buf).unwrap();
                request.push_str(&String::from_utf8_lossy(&buf[..n]));
                // The request is complete once the body after the blank line
                // reaches Content-Length.
                if let Some(head_end) = request.find("\r\n\r\n") {
                    let content_length = request
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + content_length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            request
        });

        let notifier = NtfyNotifier::new(&format!("http://{addr}/chrond-alerts"));
        notifier
            .notify(
                "chrond: backup FAILED",
                "scheduled 2026-07-08T02:15:00, exit 3",
                "high",
            )
            .unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /chrond-alerts"));
        assert!(request.contains("Title: chrond: backup FAILED"));
        assert!(request.contains("Priority: high"));
        assert!(request.contains("exit 3"));
    }

    #[test]
    fn message_contents() {
        let (title, body, priority) = build_message(&record(RunStatus::Failed));
        assert_eq!(title, "chrond: backup FAILED");
        assert!(body.contains("exit 3"));
        assert!(body.contains("1500ms"));
        assert!(body.contains("file changed"));
        assert_eq!(priority, "high");

        let (title, _, priority) = build_message(&record(RunStatus::Ok));
        assert!(title.contains("succeeded"));
        assert_eq!(priority, "default");
    }
}
