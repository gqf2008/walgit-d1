#!/usr/bin/env bash
# Exhaustive bounded TLC, not simulation. Each invocation has its own log and
# metadir; a different arm's expected error must never satisfy a negative control.
#
# Each case owns its config, log and state directory. Bound the JVM pool
# explicitly: the default is two jobs, two workers and 2 GiB heap per job.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

command -v rg >/dev/null || { echo 'TLA+: ripgrep (rg) is required' >&2; exit 2; }

# Portable per-invocation cap: GNU timeout when present (Linux), gtimeout when
# Homebrew installed it, otherwise a background watchdog. Stock macOS has no
# `timeout` and coreutils is not a prerequisite for checking the models.
# `command -v timeout` alone is not enough: under Git Bash it finds Windows'
# System32 timeout.exe, whose CLI is a different program (`timeout 600 cmd` is
# a hard error there). Probe for GNU rather than for the name.
have_gnu_timeout() {
  command -v timeout >/dev/null 2>&1 &&
    timeout --version 2>/dev/null | grep -qi 'GNU coreutils'
}

run_timed() {
  local limit="$1"; shift
  if have_gnu_timeout; then
    # -k mirrors the watchdog's TERM-then-KILL escalation: a JVM stuck in a
    # shutdown hook must not outlive its cap.
    timeout -k 10 "$limit" "$@"
    return $?
  fi
  if command -v gtimeout >/dev/null 2>&1; then
    gtimeout -k 10 "$limit" "$@"
    return $?
  fi
  "$@" &
  local child=$!
  (
    sleeper=""
    # Install the trap before forking `sleep`: a parent kill in the window
    # between the fork and the trap assignment would otherwise kill this
    # subshell with the default disposition and orphan the sleep, which then
    # holds the log pipe open for the whole cap.
    trap 'if [[ -n "$sleeper" ]]; then kill "$sleeper" 2>/dev/null; fi; exit 0' TERM
    sleep "$limit" &
    sleeper=$!
    wait "$sleeper" || exit 0
    kill -TERM "$child" 2>/dev/null || exit 0
    sleep 2
    kill -KILL "$child" 2>/dev/null || true
  ) &
  local watchdog=$!
  local status=0
  wait "$child" || status=$?
  kill "$watchdog" 2>/dev/null || true
  wait "$watchdog" 2>/dev/null || true
  return "$status"
}

# --case: one pooled TLC arm, self-exec'd by the pool below.
# Env from the parent: SPEC_JAR SPEC_LOGS SPEC_WORK SPEC_WORKERS SPEC_HEAP.
# Writes $SPEC_LOGS/<name>.result (the arm's summary-file fragment) and exits
# nonzero on an unexpected verdict so xargs propagates failure.
if [[ "${1:-}" == "--case" ]]; then
  name="$2" module="$3" config="$4" expected="$5"
  log="$SPEC_LOGS/$name.log" meta="$SPEC_WORK/$name" result="$SPEC_LOGS/$name.result"
  mkdir -p "$meta"
  effective="$SPEC_LOGS/$name.cfg"
  [[ "$config" == "$effective" ]] || cp "$config" "$effective"
  config="$effective"
  limit="${WALGIT_TLC_TIMEOUT:-600}"
  # A suite-level deadline (WALGIT_SPEC_BUDGET) shortens this arm's cap to the
  # remaining budget, so the suite ends with per-arm verdicts instead of being
  # killed by an outer CI job cap with no evidence.
  if [[ -n "${SPEC_DEADLINE:-}" ]]; then
    remaining=$(( SPEC_DEADLINE - $(date +%s) ))
    if (( remaining <= 0 )); then
      {
        printf '== %s (expect %s)\n' "$name" "$expected"
        printf 'FAIL %s: suite budget exhausted before this arm started\n' "$name"
      } > "$result"
      echo "FAIL $name (suite budget exhausted)"
      exit 1
    fi
    if (( remaining < limit )); then limit="$remaining"; fi
  fi
  status=0
  run_timed "$limit" java -Xmx"$SPEC_HEAP" -XX:+UseParallelGC -XX:ActiveProcessorCount="$SPEC_WORKERS" \
    -DTLA-Library="$root/docs/spec" -cp "$SPEC_JAR" tlc2.TLC \
    -noGenerateSpecTE -metadir "$meta" -workers "$SPEC_WORKERS" -fp 0 \
    -config "$config" "$module" > "$log" 2>&1 || status=$?
  if scripts/check-tlc-result.sh "$status" "$expected" "$log"; then
    {
      printf '== %s (expect %s)\n' "$name" "$expected"
      printf 'PASS %s (exit %s)\n' "$name" "$status"
      rg 'states generated,|depth of the complete|Finished in' "$log" || true
    } > "$result"
    echo "PASS $name"
    rm -rf "$meta" 2>/dev/null || true
    exit 0
  fi
  {
    printf '== %s (expect %s)\n' "$name" "$expected"
    printf 'FAIL %s: exit %s, expected %s; %s\n' "$name" "$status" "$expected" "$log"
  } > "$result"
  echo "FAIL $name ($log)"
  exit 1
fi

mode="${1:-fast}"
case "$mode" in
  fast|fragments) default_workers=2; default_heap=2g ;;
  full) default_workers=8; default_heap=8g ;;
  *) echo "usage: $0 [fast|full|fragments]" >&2; exit 2 ;;
esac
workers="${WALGIT_TLC_WORKERS:-$default_workers}"
heap="${WALGIT_TLC_HEAP:-$default_heap}"
[[ "$heap" =~ ^[1-9][0-9]*[mMgG]$ ]] || { echo "WALGIT_TLC_HEAP must use m or g units" >&2; exit 2; }
[[ "$workers" =~ ^[1-9][0-9]*$ ]] || { echo 'WALGIT_TLC_WORKERS must be positive' >&2; exit 2; }
default_jobs=2
if [[ "$mode" == full ]]; then default_jobs=1; fi
jobs="${WALGIT_TLC_JOBS:-$default_jobs}"
[[ "$jobs" =~ ^[1-9][0-9]*$ ]] || { echo 'WALGIT_TLC_JOBS must be positive' >&2; exit 2; }
# Full mode runs the large reference arms; the checked-in MC arm needs more
# than the 600 s fast-mode cap (~91M states at 8 workers), so its default is
# one hour. Everything else keeps 600.
default_timeout=600
if [[ "$mode" == full ]]; then default_timeout=3600; fi
timeout_s="${WALGIT_TLC_TIMEOUT:-$default_timeout}"
[[ "$timeout_s" =~ ^[1-9][0-9]*$ ]] || { echo 'WALGIT_TLC_TIMEOUT must be positive integer seconds' >&2; exit 2; }
export WALGIT_TLC_TIMEOUT="$timeout_s"
if [[ -n "${WALGIT_SPEC_BUDGET:-}" ]]; then
  [[ "$WALGIT_SPEC_BUDGET" =~ ^[1-9][0-9]*$ ]] || { echo 'WALGIT_SPEC_BUDGET must be positive integer seconds' >&2; exit 2; }
  export SPEC_DEADLINE="$(( $(date +%s) + WALGIT_SPEC_BUDGET ))"
fi
mkdir -p target/test-logs
logs="$(mktemp -d "$root/target/test-logs/spec-$mode-$(date +%Y%m%d-%H%M%S).XXXXXX")"
echo "log: $logs/summary.log"
: > "$logs/summary.log"
if ! scripts/test-spec-runner.sh > "$logs/runner.log" 2>&1; then
  echo "FAIL result-classifier regressions ($logs/runner.log)" | tee -a "$logs/summary.log"
  tail -60 "$logs/runner.log"
  exit 1
fi
# A malformed/empty matrix or duplicate label must not silently skip cases or
# overwrite another invocation's evidence.
awk '
  /^#/ || NF == 0 { next }
  NF != 7 || seen[$1]++ { print "invalid/duplicate TLC case at line " NR > "/dev/stderr"; exit 1 }
  { count++ }
  END { if (count == 0) exit 1 }
' docs/spec/tlc/cases.tsv

# Every requested model must name a matrix module: a typo in a list otherwise
# silently runs the other tokens and exits green. Checked before any download
# or JVM work.
if [[ -n "${WALGIT_SPEC_MODELS:-}" ]]; then
  for model in $WALGIT_SPEC_MODELS; do
    if ! awk -v m="$model" '!/^#/ && $2 == m { found = 1 } END { exit !found }' docs/spec/tlc/cases.tsv; then
      echo "WALGIT_SPEC_MODELS matched no matrix case: $model" >&2
      exit 2
    fi
  done
fi
scripts/ensure-tla-tools.sh
jar="$root/target/tla2tools.jar"
work="$(mktemp -d "$root/target/tlc.XXXXXX")"
trap 'rm -rf "$work"' EXIT

export SPEC_JAR="$jar" SPEC_LOGS="$logs" SPEC_WORK="$work" SPEC_WORKERS="$workers" SPEC_HEAP="$heap"

# NUL-delimited arguments also preserve checkout paths containing whitespace.
queue="$logs/queue.txt"
: > "$queue"
order=()
enqueue() {
  printf '%s\0%s\0%s\0%s\0' "$1" "$2" "$3" "$4" >> "$queue"
  order+=("$1")
}

# TLC semantically checks every loaded module too. Keep an explicit SANY log
# for the frozen reference, even in the fragment-only developer loop.
sany_status=0
java -cp "$jar" tla2sany.SANY docs/spec/WALContract.tla > "$logs/sany.log" 2>&1 || sany_status=$?
if [[ "$sany_status" -ne 0 ]] || rg -q "\*\*\* (Errors|Parse Error)|Fatal errors|Semantic errors" "$logs/sany.log"; then
  echo "FAIL reference SANY (exit $sany_status; $logs/sany.log)" >&2
  exit 1
fi
if [[ "$mode" != fragments && -z "${WALGIT_SPEC_MODELS:-}" ]]; then
  enqueue reference-fast "$root/docs/spec/tlc/MC.tla" "$root/docs/spec/tlc/MC_fast.cfg" pass
  enqueue reference-bug "$root/docs/spec/tlc/MC_bug.tla" "$root/docs/spec/tlc/MC_bug.cfg" HistPreconds
fi

while read -r name module config scenario fault spec expected; do
  [[ -z "$name" || "$name" == \#* ]] && continue
  if [[ -n "${WALGIT_SPEC_MODELS:-}" ]]; then
    keep=0
    for model in $WALGIT_SPEC_MODELS; do
      [[ "$module" == "$model" ]] && keep=1
    done
    [[ "$keep" -eq 1 ]] || continue
  fi
  cfg="$logs/$name.cfg"
  dir="$root/docs/spec/scratch"
  [[ -f "$dir/$module.tla" ]] || dir="$root/docs/spec/tlc"
  awk -v scenario="$scenario" -v fault="$fault" -v spec="$spec" -v expected="$expected" -v name="$name" '
    /^SPECIFICATION / { hasSpec = 1 }
    /^  Scenario = / { hasScenario = 1 }
    /^  Broken = / { hasBroken = 1 }
    /^  Mode = / { hasMode = 1 }
    /^  Fault = / { hasFault = 1 }
    /^SPECIFICATION / && spec != "-" { firedSpec = 1; print "SPECIFICATION " spec; next }
    /^  Scenario = / && scenario != "-" { firedScenario = 1; print "  Scenario = \"" scenario "\""; next }
    /^  Broken = / && scenario != "-" { firedBroken = 1; print "  Broken = " (scenario ~ /^broken/ ? "TRUE" : "FALSE"); next }
    /^  Mode = / && scenario != "-" { firedMode = 1; print "  Mode = \"" (scenario ~ /prune$/ ? "prune" : "conserve") "\""; next }
    /^  Fault = / && fault != "-" { firedFault = 1; print "  Fault = \"" fault "\""; next }
    /^(INVARIANTS?|PROPERTIES|PROPERTY)( |$)/ && expected != "pass" {
      if (expected ~ /^temporal:/) print "PROPERTY " substr(expected, 10)
      else print "INVARIANT " expected
      exit
    }
    { print }
    END {
      # A requested override whose anchor line is absent (spacing/case drift in
      # the base cfg) silently runs a different model and can still PASS.
      if (spec != "-" && (!hasSpec || !firedSpec)) {
        print "override did not apply: SPECIFICATION for " name > "/dev/stderr"; bad = 1
      }
      if (scenario != "-") {
        if (!(hasScenario || hasBroken || hasMode)) {
          print "override did not apply: no Scenario/Broken/Mode anchor in cfg for " name > "/dev/stderr"; bad = 1
        }
        if (hasScenario && !firedScenario) {
          print "override did not apply: Scenario for " name > "/dev/stderr"; bad = 1
        }
        if (hasBroken && !firedBroken) {
          print "override did not apply: Broken for " name > "/dev/stderr"; bad = 1
        }
        if (hasMode && !firedMode) {
          print "override did not apply: Mode for " name > "/dev/stderr"; bad = 1
        }
      }
      if (fault != "-" && (!hasFault || !firedFault)) {
        print "override did not apply: Fault for " name > "/dev/stderr"; bad = 1
      }
      if ((spec == "LiveSpec" || spec == "HealthySpec") && expected == "pass")
        print "PROPERTIES Terminates TransportSettles"
      if (spec == "HealthySpec" && expected == "pass") print "PROPERTY EventuallyCommitted"
      if (bad) exit 1
    }
  ' "$dir/$config.cfg" > "$cfg"
  enqueue "$name" "$dir/$module.tla" "$cfg" "$expected"
done < docs/spec/tlc/cases.tsv

# Fan the arms out. Unlike the serial loop this does not stop at the first
# failure: every arm runs, every verdict is recorded, and the exit is the sum —
# partial evidence beats a truncated matrix.
pool_status=0
xargs -0 -P "$jobs" -n 4 "$0" --case < "$queue" || pool_status=$?

failures=0
for name in "${order[@]}"; do
  if [[ ! -f "$logs/$name.result" ]]; then
    printf 'FAIL %s: no result written (crashed arm?); %s\n' "$name" "$logs/$name.log" | tee -a "$logs/summary.log"
    failures=$((failures + 1))
    continue
  fi
  cat "$logs/$name.result" >> "$logs/summary.log"
  if rg -q '^FAIL ' "$logs/$name.result"; then
    failures=$((failures + 1))
    tail -60 "$logs/$name.log"
  fi
done

# spec-full's reference arms enumerate big state spaces with -workers=8 each;
# running them concurrently would only divide the same cores. Keep them serial.
if [[ "$mode" == full && "$failures" -eq 0 && "$pool_status" -eq 0 && -z "${WALGIT_SPEC_MODELS:-}" ]]; then
  for config in MC MC_notrim MC_live; do
    arm_status=0
    "$0" --case "$config" "$root/docs/spec/tlc/MC.tla" "$root/docs/spec/tlc/$config.cfg" pass || arm_status=$?
    cat "$logs/$config.result" >> "$logs/summary.log" 2>/dev/null || true
    if [[ "$arm_status" -ne 0 ]]; then
      failures=$((failures + 1))
      tail -60 "$logs/$config.log"
      break
    fi
  done
fi

if [[ "$failures" -ne 0 || "$pool_status" -ne 0 ]]; then
  printf 'spec-%s: %s failed arm(s); evidence in %s\n' "$mode" "$failures" "$logs" | tee -a "$logs/summary.log"
  exit 1
fi
printf 'spec-%s: all required passes, mutations and witnesses verified (%s)\n' "$mode" "$logs" | tee -a "$logs/summary.log"
