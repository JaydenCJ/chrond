//! Built-in size-based rotation for per-job output logs.
//!
//! Every run's output is appended to `<state>/logs/<job>.log` with a
//! header line. When the file exceeds the job's `log_max` size it is
//! rotated to `<job>.log.1`, `<job>.log.2`, ... keeping `log_keep`
//! generations — no separate logrotate configuration needed.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

fn rotated_path(base: &Path, n: u32) -> PathBuf {
    let mut os = base.as_os_str().to_os_string();
    os.push(format!(".{n}"));
    PathBuf::from(os)
}

/// Rotate `base` if it exceeds `max_bytes`: shift `.1 -> .2 -> ...`,
/// dropping the generation beyond `keep`. With `keep == 0` the file is
/// simply truncated.
pub fn rotate_if_needed(base: &Path, max_bytes: u64, keep: u32) -> std::io::Result<bool> {
    let size = match fs::metadata(base) {
        Ok(m) => m.len(),
        Err(_) => return Ok(false),
    };
    if size <= max_bytes {
        return Ok(false);
    }
    if keep == 0 {
        fs::write(base, b"")?;
        return Ok(true);
    }
    // Drop the oldest generation, then shift the rest up.
    let oldest = rotated_path(base, keep);
    let _ = fs::remove_file(&oldest);
    for n in (1..keep).rev() {
        let from = rotated_path(base, n);
        if from.exists() {
            fs::rename(&from, rotated_path(base, n + 1))?;
        }
    }
    fs::rename(base, rotated_path(base, 1))?;
    Ok(true)
}

/// Append one run's output to the job log, rotating first when needed.
pub fn append_run_log(
    logs_dir: &Path,
    job: &str,
    header: &str,
    output: &str,
    max_bytes: u64,
    keep: u32,
) -> std::io::Result<()> {
    fs::create_dir_all(logs_dir)?;
    let base = logs_dir.join(format!("{job}.log"));
    rotate_if_needed(&base, max_bytes, keep)?;
    let mut f = OpenOptions::new().create(true).append(true).open(&base)?;
    writeln!(f, "=== {header}")?;
    if !output.is_empty() {
        f.write_all(output.as_bytes())?;
        if !output.ends_with('\n') {
            writeln!(f)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chrond-logrotate-test-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_rotation_below_threshold() {
        let dir = tempdir("below");
        let base = dir.join("job.log");
        fs::write(&base, "small").unwrap();
        assert!(!rotate_if_needed(&base, 1024, 3).unwrap());
        assert!(base.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotates_and_shifts_generations() {
        let dir = tempdir("shift");
        let base = dir.join("job.log");

        fs::write(&base, "gen-A".repeat(10)).unwrap();
        assert!(rotate_if_needed(&base, 10, 3).unwrap());
        assert!(!base.exists());
        assert!(fs::read_to_string(rotated_path(&base, 1))
            .unwrap()
            .contains("gen-A"));

        fs::write(&base, "gen-B".repeat(10)).unwrap();
        assert!(rotate_if_needed(&base, 10, 3).unwrap());
        assert!(fs::read_to_string(rotated_path(&base, 1))
            .unwrap()
            .contains("gen-B"));
        assert!(fs::read_to_string(rotated_path(&base, 2))
            .unwrap()
            .contains("gen-A"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keep_limit_drops_oldest() {
        let dir = tempdir("keep");
        let base = dir.join("job.log");
        for gen in ["one", "two", "three"] {
            fs::write(&base, gen.repeat(20)).unwrap();
            rotate_if_needed(&base, 10, 2).unwrap();
        }
        assert!(rotated_path(&base, 1).exists());
        assert!(rotated_path(&base, 2).exists());
        assert!(!rotated_path(&base, 3).exists());
        // Newest rotation holds the last generation written.
        assert!(fs::read_to_string(rotated_path(&base, 1))
            .unwrap()
            .contains("three"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keep_zero_truncates() {
        let dir = tempdir("zero");
        let base = dir.join("job.log");
        fs::write(&base, "x".repeat(100)).unwrap();
        assert!(rotate_if_needed(&base, 10, 0).unwrap());
        assert_eq!(fs::metadata(&base).unwrap().len(), 0);
        assert!(!rotated_path(&base, 1).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_writes_header_and_output() {
        let dir = tempdir("append");
        append_run_log(
            &dir,
            "backup",
            "2026-07-08T02:15:00 ok 12ms",
            "did stuff",
            1024,
            3,
        )
        .unwrap();
        let content = fs::read_to_string(dir.join("backup.log")).unwrap();
        assert!(content.contains("=== 2026-07-08T02:15:00 ok 12ms"));
        assert!(content.contains("did stuff"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_triggers_rotation() {
        let dir = tempdir("append-rotate");
        append_run_log(&dir, "j", "h1", &"x".repeat(200), 100, 2).unwrap();
        append_run_log(&dir, "j", "h2", "fresh", 100, 2).unwrap();
        let base = dir.join("j.log");
        assert!(fs::read_to_string(&base).unwrap().contains("fresh"));
        assert!(rotated_path(&base, 1).exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
