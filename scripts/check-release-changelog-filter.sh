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

matches=(
    'feat!: x'
    'fix!: x'
    'feat(s)!: x'
    'fix(s)!: x'
    'feat: x'
    'fix(s): x'
    'docs: x'
)
non_matches=(
    'feature: x'
    'feat x'
    'featx: y'
    'feat(s: x'
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
