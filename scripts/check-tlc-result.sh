#!/usr/bin/env bash
# Classify only this invocation. An expected error alongside another error fails.
set -euo pipefail
[[ $# -eq 3 ]] || exit 2
status="$1" expected="$2" log="$3"
[[ -f "$log" ]] || exit 1
case "$expected" in
  pass)
    [[ "$status" == 0 ]] &&
      rg -q '^Finished in .+ at \(.+\)$' "$log" &&
      rg -Fqx 'Model checking completed. No error has been found.' "$log" &&
      rg -q '^[0-9,]+ states generated, [0-9,]+ distinct states found, 0 states left on queue\.$' "$log" &&
      ! rg -q '^Error:' "$log"
    ;;
  *)
    initverdict=""
    if [[ "$expected" == temporal:* ]]; then
      [[ "$status" == 13 ]] || exit 1
      verdict="Error: Temporal property ${expected#temporal:} was violated."
      trace='Error: The following behavior constitutes a counter-example:'
    else
      [[ "$status" == 12 ]] || exit 1
      verdict="Error: Invariant $expected is violated."
      # TLC also reports an invariant that is false in Init with this wording;
      # it prints the state block directly, without a trace heading/State 1.
      initverdict="Error: Invariant $expected is violated by the initial state:"
      trace='Error: The behavior up to this point is:'
    fi
    # TLC prefixes the trace heading with Error too; it is the only permitted
    # companion. Duplicate verdicts also reject concatenated or stale logs.
    awk -v verdict="$verdict" -v initverdict="$initverdict" -v trace="$trace" '
      /^Error:/ {
        if ($0 == verdict) verdicts++
        else if (initverdict != "" && $0 == initverdict) initverdicts++
        else if ($0 == trace) traces++
        else bad = 1
      }
      /^State 1: / { state1 = 1 }
      END {
        if (initverdicts > 0)
          exit !(verdicts == 0 && initverdicts == 1 && traces == 0 && !bad)
        exit !(verdicts == 1 && traces == 1 && !bad && state1)
      }
    ' "$log"
    # A counterexample log ends with the full footer; TLC's initial-state
    # violation prints the verdict and state block without one, so requiring
    # it there would reject exactly the expected negative control.
    if [[ -z "$initverdict" ]] || ! rg -Fqx "$initverdict" "$log"; then
      rg -q '^Finished in .+ at \(.+\)$' "$log" || exit 1
      rg -q '^[0-9,]+ states generated, [0-9,]+ distinct states found, [0-9,]+ states left on queue\.$' "$log" || exit 1
    fi
    ;;
esac
