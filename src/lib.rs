//! chrond — a memory-safe cron daemon with missed-job catch-up, overlap
//! control, queryable structured run history, built-in log rotation, and
//! Prometheus / ntfy alerting.
//!
//! The library crate exposes the parsing, scheduling and storage layers so
//! they can be unit-tested and reused; the `chrond` binary wires them into
//! a CLI (see `src/main.rs`).

pub mod alert;
pub mod cli;
pub mod cronexpr;
pub mod crontab;
pub mod daemon;
pub mod history;
pub mod logrotate;
pub mod metrics;
pub mod runner;
pub mod scheduler;
