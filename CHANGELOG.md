# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-07-08

### Added

- Crontab parser: classic five-field vixie syntax (lists, ranges, steps, month/weekday names, `7` as Sunday, the vixie day-of-month/day-of-week OR rule), `@hourly`/`@daily`/`@weekly`/`@monthly`/`@yearly`/`@reboot` aliases, `KEY=value` environment lines, and `/etc/crontab` system format (`--system`).
- `#[chrond]` per-job annotations: `name`, `catchup`, `max_catchup`, `overlap=allow|skip`, `timeout`, `notify=never|on_failure|always`, `log_max`, `log_keep`.
- Missed-job catch-up: occurrences missed while the daemon was down are replayed on restart up to `max_catchup`; the rest are recorded as `missed`.
- Structured run history: one JSONL record per schedule occurrence (`ok`, `failed`, `timeout`, `missed`, `skipped_overlap`, `spawn_error`) with timestamps, exit code, duration and an output tail.
- CLI: `chrond run` (foreground daemon with `--exit-after` for supervised runs), `chrond check` (validate + preview next occurrences), `chrond runs` (query history with `--job`, `--since`, `--failed`, `--json`, `--limit`), `chrond status` (latest outcome + next run per job).
- Per-job timeout enforcement that kills the whole process group, and per-job overlap control.
- Built-in size-based log rotation for per-job output logs (`log_max`, `log_keep`).
- Prometheus text-format metrics endpoint (`/metrics`) and `/health`, bound only to the address given via `--metrics`.
- ntfy push notifications (`--ntfy`), self-hostable, with per-job notify policy.
- Test suite: 59 unit tests, 7 CLI integration tests (including a daemon end-to-end pass), and `scripts/smoke.sh`.

[0.1.0]: https://github.com/JaydenCJ/chrond/releases/tag/v0.1.0
