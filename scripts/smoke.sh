#!/usr/bin/env bash
# Smoke test: builds chrond, runs the daemon in the foreground against a
# real crontab (with @reboot jobs, a pre-seeded missed occurrence and a
# failing job), then asserts on the run history, the CLI queries and the
# Prometheus endpoint. Self-contained: temp dirs + 127.0.0.1 only.
set -euo pipefail

cd "$(dirname "$0")/.."

fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }

echo "[smoke] building..."
cargo build --quiet
BIN=target/debug/chrond

WORK=$(mktemp -d "${TMPDIR:-/tmp}/chrond-smoke.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
STATE="$WORK/state"
mkdir -p "$STATE"

# --- 1. version/help sanity -------------------------------------------------
"$BIN" --version | grep -q '^chrond 0\.1\.0$' || fail "--version mismatch"
"$BIN" --help | grep -q 'COMMANDS:' || fail "--help missing sections"

# --- 2. crontab with catch-up, overlap, timeout, a failure ------------------
cat > "$WORK/crontab" <<'EOF'
SHELL=/bin/sh
#[chrond] name=hello
@reboot echo hello-from-chrond
#[chrond] name=tick catchup=on max_catchup=2
* * * * * echo tick-ran
#[chrond] name=badjob notify=never
@reboot sh -c 'echo boom >&2; exit 3'
EOF

echo "[smoke] chrond check"
"$BIN" check "$WORK/crontab" | tee "$WORK/check.out"
grep -q 'OK (3 job(s)' "$WORK/check.out" || fail "check did not validate 3 jobs"
grep -q 'catch-up: on (max 2)' "$WORK/check.out" || fail "check did not show catch-up"

# An invalid crontab must fail with a line number, exit code 1.
printf '61 * * * * boom\n' > "$WORK/bad"
if "$BIN" check "$WORK/bad" 2> "$WORK/bad.err"; then fail "invalid crontab accepted"; fi
grep -q 'line 1' "$WORK/bad.err" || fail "parse error lacks line number"

# --- 3. daemon run: catch-up + @reboot + metrics -----------------------------
# Pre-seed `tick` as last accounted for 4 minutes ago: the daemon must catch
# up the 2 newest missed occurrences and record the oldest as `missed`.
SEED=$(date -d '4 minutes ago' '+%Y-%m-%dT%H:%M:00')
printf '{"jobs":{"tick":{"last_scheduled":"%s"}}}' "$SEED" > "$STATE/state.json"

METRICS=127.0.0.1:39634
echo "[smoke] chrond run (foreground, --exit-after 3s, metrics on $METRICS)"
"$BIN" run --file "$WORK/crontab" --state "$STATE" --metrics "$METRICS" --exit-after 3s > "$WORK/daemon.log" 2>&1 &
DPID=$!
sleep 1.5
HEALTH=$(curl -s -o /dev/null -w '%{http_code}' "http://$METRICS/health")
[ "$HEALTH" = 200 ] || fail "GET /health -> $HEALTH (want 200)"
echo "[smoke] GET /health -> 200"
curl -s "http://$METRICS/metrics" > "$WORK/metrics.out"
grep -q 'chrond_job_runs_total{job="hello",status="ok"} 1' "$WORK/metrics.out" \
  || fail "metrics missing hello counter"
grep -q 'chrond_job_runs_total{job="badjob",status="failed"} 1' "$WORK/metrics.out" \
  || fail "metrics missing badjob failure counter"
echo "[smoke] GET /metrics -> counters present"
wait "$DPID" || fail "daemon exited non-zero"
grep -q 'shutdown complete' "$WORK/daemon.log" || fail "daemon did not shut down cleanly"

# --- 4. history assertions ----------------------------------------------------
echo "[smoke] chrond runs / status"
"$BIN" runs --state "$STATE" --json > "$WORK/runs.json"
grep -q '"job":"hello".*"status":"ok"' "$WORK/runs.json" || fail "hello run not recorded ok"
grep -q 'hello-from-chrond' "$WORK/runs.json" || fail "job output tail not captured"
grep -q '"job":"badjob".*"status":"failed".*"exit_code":3' "$WORK/runs.json" \
  || fail "badjob failure/exit code not recorded"
grep -q '"job":"tick".*"catchup":true' "$WORK/runs.json" || fail "catch-up runs not recorded"
grep -q '"job":"tick".*"status":"missed"' "$WORK/runs.json" || fail "missed occurrence not recorded"

"$BIN" runs --state "$STATE" --failed | grep -q 'badjob' || fail "--failed filter missing badjob"
if "$BIN" runs --state "$STATE" --failed | grep -qE '^hello '; then
  fail "--failed filter leaked successful runs"
fi
"$BIN" status --state "$STATE" --file "$WORK/crontab" | tee "$WORK/status.out" >/dev/null
grep -q 'at daemon startup' "$WORK/status.out" || fail "status missing @reboot next-run"
grep -qE 'tick.*ok' "$WORK/status.out" || fail "status missing tick outcome"

# --- 5. per-job logs written by the built-in rotation ------------------------
grep -q 'hello-from-chrond' "$STATE/logs/hello.log" || fail "job log not written"

echo "SMOKE OK"
