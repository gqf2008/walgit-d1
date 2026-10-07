#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workflow="$repo_root/.github/workflows/release.yml"

# Keep this guard tied to the workflow's actual filter rather than a copy that
# can drift: extract the ERE between the single quotes after `grep -E`.
regex="$(sed -n "s/^[[:space:]]*git log .*grep -E '\\([^']*\\)'.*$/\\1/p" "$workflow")"
if [ -z "$regex" ] || [ "$(printf '%s\n' "$regex" | wc -l | tr -d ' ')" -ne 1 ]; then
    printf 'FAIL: unable to extract exactly one changelog regex from %s\n' "$workflow" >&2
    exit 1
fi

# Type coverage: the filter's job is "list every Conventional Commits subject".
# An incomplete enumeration drops whole commit types **silently** — v0.8.12's
# release body came out empty because its only commit was `style(...)`, which the
# list did not name (2026-10-07, thread release-changelog-types). So the type set
# in the workflow's alternation must equal this one, in both directions: a type
# dropped from the workflow turns this guard red, and a type invented there does
# too (the behavioural cases below then pin the ERE shape).
expected_types='build chore ci docs feat fix perf refactor revert style test'
actual_types="$(
    printf '%s\n' "$regex" \
        | sed -n 's/^\^(\([^)]*\)).*/\1/p' \
        | tr '|' '\n' \
        | sort \
        | tr '\n' ' ' \
        | sed 's/ $//'
)"
if [ "$actual_types" != "$expected_types" ]; then
    printf 'FAIL: changelog filter types drifted\n  expected: %s\n  actual:   %s\n' \
        "$expected_types" "$actual_types" >&2
    exit 1
fi

matches=(
    'feat!: x'
    'fix!: x'
    'feat(s)!: x'
    'fix(s)!: x'
    'feat: x'
    'fix(s): x'
    'docs: x'
    'style: x'
    'style(s): x'
    'refactor: x'
    'perf: x'
    'test: x'
    'build: x'
    'ci: x'
    'chore: x'
    'revert: x'
)
non_matches=(
    'feature: x'
    'feat x'
    'featx: y'
    'feat(s: x'
    'styles: x'
)

for case in "${matches[@]}"; do
    if ! printf '%s\n' "$case" | grep -Eq "$regex"; then
        printf 'FAIL: %s\n' "$case" >&2
        exit 1
    fi
done

for case in "${non_matches[@]}"; do
    if printf '%s\n' "$case" | grep -Eq "$regex"; then
        printf 'FAIL: %s\n' "$case" >&2
        exit 1
    fi
done

printf 'PASS: changelog filter regex (%d cases)\n' "$(( ${#matches[@]} + ${#non_matches[@]} ))"
