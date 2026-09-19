#!/usr/bin/env bash
# Guard the user-facing surface: every top-level CLI command and every
# `walgit mcp` flag must be explained in the public agent guide.
set -euo pipefail

cd "$(dirname "$0")/.."

bin="${WALGIT_BIN:-target/debug/walgit}"
skill="${WALGIT_SKILL_MD:-web/SKILL.md}"

[ -x "$bin" ] || { echo "check-skill-covers-cli: missing executable: $bin" >&2; exit 2; }
[ -f "$skill" ] || { echo "check-skill-covers-cli: missing skill: $skill" >&2; exit 2; }

exemption_reason() {
    case "$1" in
        synth)
            printf '%s' "internal deterministic synthetic-repository generator; not a user-facing host workflow"
            ;;
        help)
            printf '%s' "clap's built-in help command; usage is the CLI's own --help output"
            ;;
        --help)
            printf '%s' "clap's built-in help flag; not a walgit feature"
            ;;
        *)
            return 1
            ;;
    esac
}

commands="$({
    "$bin" --help
} | awk '
    /^Commands:/ { in_commands = 1; next }
    in_commands && /^Options:/ { exit }
    in_commands && $1 ~ /^[a-z][a-z0-9-]*$/ { print $1 }
')"

flags="$({
    "$bin" mcp --help
} | grep -oE -- '--[a-z0-9-]+' | sort -u)"

missing=0
checked=0

check_command() {
    local command="$1"
    if reason="$(exemption_reason "$command")"; then
        printf 'EXEMPT command %-12s %s\n' "$command" "$reason"
        return
    fi
    checked=$((checked + 1))
    if grep -Fq -- "walgit $command" "$skill"; then
        printf 'OK     command %s\n' "$command"
    else
        printf 'MISSING command walgit %s\n' "$command" >&2
        missing=1
    fi
}

check_flag() {
    local flag="$1"
    if reason="$(exemption_reason "$flag")"; then
        printf 'EXEMPT flag    %-28s %s\n' "$flag" "$reason"
        return
    fi
    checked=$((checked + 1))
    if grep -Fq -- "$flag" "$skill"; then
        printf 'OK     flag    %s\n' "$flag"
    else
        printf 'MISSING flag    %s\n' "$flag" >&2
        missing=1
    fi
}

while IFS= read -r command; do
    [ -n "$command" ] && check_command "$command"
done <<<"$commands"

while IFS= read -r flag; do
    [ -n "$flag" ] && check_flag "$flag"
done <<<"$flags"

if [ "$missing" -ne 0 ]; then
    echo "check-skill-covers-cli: update $skill or add a reasoned exemption" >&2
    exit 1
fi

echo "check-skill-covers-cli: OK ($checked documented items)"
