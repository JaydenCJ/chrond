//! Crontab file parsing.
//!
//! chrond reads standard crontab files (comments, `KEY=value` environment
//! lines, five-field job lines, `@aliases`). System crontab format
//! (`/etc/crontab`, with a sixth user column) is supported via
//! `system_mode`. Per-job behavior is configured with an annotation
//! comment placed on the line directly above the job:
//!
//! ```text
//! #[chrond] name=nightly-backup catchup=on max_catchup=3 timeout=30m overlap=skip notify=on_failure
//! 15 2 * * * /usr/local/bin/backup.sh
//! ```

use crate::cronexpr::CronExpr;
use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

/// What to do when a job is still running at its next scheduled time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlapPolicy {
    /// Start another instance (vixie-cron behavior, default).
    Allow,
    /// Skip the new occurrence and record it as `skipped_overlap`.
    Skip,
}

/// When to send a push notification for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyPolicy {
    Never,
    OnFailure,
    Always,
}

/// A single parsed job.
#[derive(Debug, Clone)]
pub struct JobSpec {
    pub name: String,
    pub schedule: CronExpr,
    pub schedule_str: String,
    pub command: String,
    /// User column from system crontabs (parsed, not yet acted upon).
    pub user: Option<String>,
    pub timeout: Option<Duration>,
    pub overlap: OverlapPolicy,
    pub catchup: bool,
    pub max_catchup: u32,
    pub notify: NotifyPolicy,
    /// Rotate the job's output log once it exceeds this size.
    pub log_max_bytes: u64,
    /// How many rotated log generations to keep.
    pub log_keep: u32,
    pub line: usize,
}

/// A parsed crontab: environment assignments plus jobs.
#[derive(Debug, Clone, Default)]
pub struct Crontab {
    pub env: Vec<(String, String)>,
    pub jobs: Vec<JobSpec>,
}

/// Parse error with a line number.
#[derive(Debug, Clone)]
pub struct CrontabError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for CrontabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for CrontabError {}

#[derive(Debug, Default, Clone)]
struct Annotation {
    name: Option<String>,
    timeout: Option<Duration>,
    overlap: Option<OverlapPolicy>,
    catchup: Option<bool>,
    max_catchup: Option<u32>,
    notify: Option<NotifyPolicy>,
    log_max_bytes: Option<u64>,
    log_keep: Option<u32>,
}

/// Parse a human duration such as `30s`, `5m`, `2h`, `1d`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let (num, mult) = match unit {
        "s" => (num, 1u64),
        "m" => (num, 60),
        "h" => (num, 3600),
        "d" => (num, 86400),
        _ if s.chars().all(|c| c.is_ascii_digit()) => (s, 1),
        _ => return Err(format!("invalid duration '{s}' (use e.g. 30s, 5m, 2h, 1d)")),
    };
    let n: u64 = num.parse().map_err(|_| format!("invalid duration '{s}'"))?;
    Ok(Duration::from_secs(n * mult))
}

/// Parse a human size such as `512K`, `1M`, `2048`.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size".into());
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let (num, mult) = match unit.to_ascii_uppercase().as_str() {
        "K" => (num, 1024u64),
        "M" => (num, 1024 * 1024),
        "G" => (num, 1024 * 1024 * 1024),
        _ if s.chars().all(|c| c.is_ascii_digit()) => (s, 1),
        _ => return Err(format!("invalid size '{s}' (use e.g. 512K, 1M)")),
    };
    let n: u64 = num.parse().map_err(|_| format!("invalid size '{s}'"))?;
    Ok(n * mult)
}

fn parse_annotation(rest: &str, line: usize) -> Result<Annotation, CrontabError> {
    let mut ann = Annotation::default();
    for token in rest.split_whitespace() {
        let (key, value) = token.split_once('=').ok_or_else(|| CrontabError {
            line,
            message: format!("annotation token '{token}' is not key=value"),
        })?;
        let err = |m: String| CrontabError { line, message: m };
        match key {
            "name" => {
                if value.is_empty()
                    || !value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
                {
                    return Err(err(format!(
                        "job name '{value}' must be non-empty [A-Za-z0-9._-]"
                    )));
                }
                ann.name = Some(value.to_string());
            }
            "timeout" => ann.timeout = Some(parse_duration(value).map_err(err)?),
            "overlap" => {
                ann.overlap = Some(match value {
                    "allow" => OverlapPolicy::Allow,
                    "skip" => OverlapPolicy::Skip,
                    other => return Err(err(format!("overlap must be allow|skip, got '{other}'"))),
                })
            }
            "catchup" => {
                ann.catchup = Some(match value {
                    "on" | "true" | "yes" => true,
                    "off" | "false" | "no" => false,
                    other => return Err(err(format!("catchup must be on|off, got '{other}'"))),
                })
            }
            "max_catchup" => {
                let n: u32 = value
                    .parse()
                    .map_err(|_| err(format!("max_catchup must be a number, got '{value}'")))?;
                if n == 0 {
                    return Err(err("max_catchup must be >= 1".into()));
                }
                ann.max_catchup = Some(n);
            }
            "notify" => {
                ann.notify = Some(match value {
                    "never" => NotifyPolicy::Never,
                    "on_failure" => NotifyPolicy::OnFailure,
                    "always" => NotifyPolicy::Always,
                    other => {
                        return Err(err(format!(
                            "notify must be never|on_failure|always, got '{other}'"
                        )))
                    }
                })
            }
            "log_max" => ann.log_max_bytes = Some(parse_size(value).map_err(err)?),
            "log_keep" => {
                ann.log_keep = Some(
                    value
                        .parse()
                        .map_err(|_| err(format!("log_keep must be a number, got '{value}'")))?,
                )
            }
            other => {
                return Err(CrontabError {
                    line,
                    message: format!("unknown annotation key '{other}'"),
                })
            }
        }
    }
    Ok(ann)
}

fn is_env_line(line: &str) -> bool {
    if let Some((key, _)) = line.split_once('=') {
        let key = key.trim();
        !key.is_empty()
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !key.chars().next().unwrap().is_ascii_digit()
    } else {
        false
    }
}

fn split_schedule_and_command(
    line: &str,
    system_mode: bool,
    lineno: usize,
) -> Result<(String, Option<String>, String), CrontabError> {
    let fields_needed = if line.starts_with('@') { 1 } else { 5 };
    let mut rest = line;
    let mut schedule_parts: Vec<&str> = Vec::new();
    for _ in 0..fields_needed {
        let trimmed = rest.trim_start();
        let end = trimmed
            .find(char::is_whitespace)
            .ok_or_else(|| CrontabError {
                line: lineno,
                message: "missing command after schedule".into(),
            })?;
        schedule_parts.push(&trimmed[..end]);
        rest = &trimmed[end..];
    }
    let mut user = None;
    if system_mode {
        let trimmed = rest.trim_start();
        let end = trimmed
            .find(char::is_whitespace)
            .ok_or_else(|| CrontabError {
                line: lineno,
                message: "missing command after user column (system crontab)".into(),
            })?;
        user = Some(trimmed[..end].to_string());
        rest = &trimmed[end..];
    }
    let command = rest.trim().to_string();
    if command.is_empty() {
        return Err(CrontabError {
            line: lineno,
            message: "missing command".into(),
        });
    }
    Ok((schedule_parts.join(" "), user, command))
}

/// Derive a default job name from its command (first word's basename).
fn default_name(command: &str, index: usize) -> String {
    let first = command.split_whitespace().next().unwrap_or("job");
    let base = first.rsplit('/').next().unwrap_or(first);
    let clean: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect();
    if clean.is_empty() {
        format!("job{index}")
    } else {
        format!("{clean}-{index}")
    }
}

impl Crontab {
    /// Parse crontab file content. `system_mode` expects the sixth user
    /// column as in `/etc/crontab`.
    pub fn parse(content: &str, system_mode: bool) -> Result<Self, CrontabError> {
        let mut tab = Crontab::default();
        let mut pending: Option<Annotation> = None;
        let mut seen_names: HashSet<String> = HashSet::new();

        for (i, raw) in content.lines().enumerate() {
            let lineno = i + 1;
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("#[chrond]") {
                pending = Some(parse_annotation(rest, lineno)?);
                continue;
            }
            if line.starts_with('#') {
                continue;
            }
            if !line.starts_with('@') && is_env_line(line) {
                let (k, v) = line.split_once('=').unwrap();
                let v = v.trim();
                let v = v
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .unwrap_or(v);
                tab.env.push((k.trim().to_string(), v.to_string()));
                continue;
            }
            let (schedule_str, user, command) =
                split_schedule_and_command(line, system_mode, lineno)?;
            let schedule = CronExpr::parse(&schedule_str).map_err(|e| CrontabError {
                line: lineno,
                message: e.to_string(),
            })?;
            let ann = pending.take().unwrap_or_default();
            let index = tab.jobs.len() + 1;
            let name = ann.name.unwrap_or_else(|| default_name(&command, index));
            if !seen_names.insert(name.clone()) {
                return Err(CrontabError {
                    line: lineno,
                    message: format!("duplicate job name '{name}' (set a unique name= annotation)"),
                });
            }
            tab.jobs.push(JobSpec {
                name,
                schedule,
                schedule_str,
                command,
                user,
                timeout: ann.timeout,
                overlap: ann.overlap.unwrap_or(OverlapPolicy::Allow),
                catchup: ann.catchup.unwrap_or(false),
                max_catchup: ann.max_catchup.unwrap_or(1),
                notify: ann.notify.unwrap_or(NotifyPolicy::OnFailure),
                log_max_bytes: ann.log_max_bytes.unwrap_or(1024 * 1024),
                log_keep: ann.log_keep.unwrap_or(4),
                line: lineno,
            });
        }
        Ok(tab)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_env_jobs_and_comments() {
        let content = "\
# a comment
SHELL=/bin/sh
PATH=/usr/bin:/bin

15 2 * * * /usr/local/bin/backup.sh --full
@hourly echo hourly-tick
";
        let tab = Crontab::parse(content, false).unwrap();
        assert_eq!(
            tab.env,
            vec![
                ("SHELL".to_string(), "/bin/sh".to_string()),
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ]
        );
        assert_eq!(tab.jobs.len(), 2);
        assert_eq!(tab.jobs[0].command, "/usr/local/bin/backup.sh --full");
        assert_eq!(tab.jobs[0].name, "backup.sh-1");
        assert_eq!(tab.jobs[1].command, "echo hourly-tick");
    }

    #[test]
    fn parses_annotation() {
        let content = "\
#[chrond] name=nightly timeout=30m overlap=skip catchup=on max_catchup=3 notify=always log_max=512K log_keep=7
15 2 * * * backup.sh
5 * * * * other.sh
";
        let tab = Crontab::parse(content, false).unwrap();
        let j = &tab.jobs[0];
        assert_eq!(j.name, "nightly");
        assert_eq!(j.timeout, Some(Duration::from_secs(1800)));
        assert_eq!(j.overlap, OverlapPolicy::Skip);
        assert!(j.catchup);
        assert_eq!(j.max_catchup, 3);
        assert_eq!(j.notify, NotifyPolicy::Always);
        assert_eq!(j.log_max_bytes, 512 * 1024);
        assert_eq!(j.log_keep, 7);
        // The annotation applies only to the next job line.
        let k = &tab.jobs[1];
        assert!(!k.catchup);
        assert_eq!(k.overlap, OverlapPolicy::Allow);
        assert_eq!(k.notify, NotifyPolicy::OnFailure);
    }

    #[test]
    fn system_mode_parses_user_column() {
        let content = "17 * * * * root cd / && run-parts /etc/cron.hourly\n";
        let tab = Crontab::parse(content, true).unwrap();
        assert_eq!(tab.jobs[0].user.as_deref(), Some("root"));
        assert_eq!(tab.jobs[0].command, "cd / && run-parts /etc/cron.hourly");
    }

    #[test]
    fn user_mode_keeps_full_command() {
        let content = "17 * * * * root cd / && run-parts /etc/cron.hourly\n";
        let tab = Crontab::parse(content, false).unwrap();
        assert_eq!(tab.jobs[0].user, None);
        assert_eq!(
            tab.jobs[0].command,
            "root cd / && run-parts /etc/cron.hourly"
        );
    }

    #[test]
    fn rejects_duplicate_names() {
        let content = "\
#[chrond] name=x
* * * * * a
#[chrond] name=x
* * * * * b
";
        let err = Crontab::parse(content, false).unwrap_err();
        assert!(err.message.contains("duplicate"));
    }

    #[test]
    fn reports_line_numbers() {
        let content = "# ok\n\n61 * * * * boom\n";
        let err = Crontab::parse(content, false).unwrap_err();
        assert_eq!(err.line, 3);
    }

    #[test]
    fn rejects_missing_command() {
        let err = Crontab::parse("* * * * *\n", false).unwrap_err();
        assert!(err.message.contains("missing command"));
    }

    #[test]
    fn rejects_bad_annotation() {
        assert!(Crontab::parse("#[chrond] overlap=maybe\n* * * * * x\n", false).is_err());
        assert!(Crontab::parse("#[chrond] nope=1\n* * * * * x\n", false).is_err());
        assert!(Crontab::parse("#[chrond] timeout=fast\n* * * * * x\n", false).is_err());
        assert!(Crontab::parse("#[chrond] max_catchup=0\n* * * * * x\n", false).is_err());
    }

    #[test]
    fn duration_and_size_parsers() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert!(parse_duration("abc").is_err());
        assert_eq!(parse_size("512K").unwrap(), 512 * 1024);
        assert_eq!(parse_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("100").unwrap(), 100);
        assert!(parse_size("1X").is_err());
    }

    #[test]
    fn env_line_with_quotes() {
        let tab = Crontab::parse("MAILTO=\"ops@example.com\"\n", false).unwrap();
        assert_eq!(tab.env[0].1, "ops@example.com");
    }
}
