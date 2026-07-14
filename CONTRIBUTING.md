# Contributing to chrond

Thanks for your interest in improving chrond. Issues, discussions and pull requests are all welcome.

## Getting started

Prerequisites: Rust 1.75 or newer (stable toolchain).

```bash
git clone https://github.com/JaydenCJ/chrond.git
cd chrond
cargo build
cargo test
bash scripts/smoke.sh
```

`scripts/smoke.sh` runs the daemon end to end against a temporary state directory and asserts on the run history and the metrics endpoint. It finishes in under a minute and must print `SMOKE OK`.

## Before you open a pull request

1. `cargo fmt` — formatting is enforced.
2. `cargo clippy --all-targets -- -D warnings` — clippy must be clean.
3. `cargo test` — unit tests and the CLI integration tests must pass.
4. `bash scripts/smoke.sh` — the smoke test must print `SMOKE OK`.
5. Add tests for behavior changes. Scheduling and parsing logic lives in pure modules (`cronexpr`, `crontab`, `scheduler`, `logrotate`) that are easy to unit-test; please keep it that way.

## Ground rules

- Keep dependencies minimal. chrond currently depends on `chrono`, `serde`, `serde_json` and `ureq` only; adding a dependency needs a clear justification in the PR description.
- No network calls at startup, no telemetry. The only outbound traffic is the ntfy notification the user explicitly configures.
- Code comments and doc comments are written in English.
- Compatibility first: standard crontab files must keep parsing. chrond-specific behavior goes into `#[chrond]` annotations, never into new syntax on the job line itself.

## Reporting bugs

Please include your crontab (redact commands if needed), the `chrond --version` output, the relevant `chrond runs --json` records, and daemon log lines. Scheduling bugs are much easier to fix with a concrete timestamp scenario ("state said X, at time Y it did Z").

## Security

If you find a security issue (e.g. privilege or command-injection related), please do not open a public issue. Use GitHub's private vulnerability reporting on this repository instead.
