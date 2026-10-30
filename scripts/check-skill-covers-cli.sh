#!/usr/bin/env bash
# Guard the user-facing surface: every top-level CLI command must be
# explained in the public agent guide.
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

# The extraction is the guard's whole input: if it comes back empty
# (clap's help layout changed, the wrong binary was built, `awk` matched
# nothing) the loops below would check nothing and still print OK. That is the
# "guard that cannot fail" shape, so an empty or implausibly small surface is a
# hard error here rather than a green step.
[ -n "$commands" ] || {
    echo "check-skill-covers-cli: no top-level command extracted from \`$bin --help\`" >&2
    exit 2
}
# 14 items are documented today (2 exemptions -> 12 checked); the floor leaves
# room for an exemption or two while catching a shrunken extraction. It is a
# floor on the *extraction*, never on what the guide may document: a command
# that disappears from the CLI comes out of this count in the same change.
min_checked="${WALGIT_MIN_CHECKED:-11}"

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

while IFS= read -r command; do
    [ -n "$command" ] && check_command "$command"
done <<<"$commands"

if [ "$checked" -lt "$min_checked" ]; then
    echo "check-skill-covers-cli: only $checked items were checked (floor $min_checked)" >&2
    exit 2
fi

if [ "$missing" -ne 0 ]; then
    echo "check-skill-covers-cli: update $skill or add a reasoned exemption" >&2
    exit 1
fi

echo "check-skill-covers-cli: OK ($checked documented items)"
