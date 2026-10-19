#!/usr/bin/env bash
# Fetch the pinned TLA+ tools jar atomically and verify its release checksum.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JAR="$ROOT/target/tla2tools.jar"
VERSION="v1.8.0"
# v1.8.0 is a rolling upstream asset, not an immutable release. The upstream
# port pinned 867aefb69ffc2452031292587b389d1fc3eb43ff (2026-09-12); by
# 2026-10-19 that asset was gone, the release had rolled to
# 932d35ec2b9fdb3d3e4d0a3321689c634a1f6b43 (built 2026-10-15) and the old
# checksum could no longer be obtained. The pin was re-derived from the release
# asset GitHub serves for v1.8.0 (asset digest 15d103b7...) and every matrix arm
# was rerun. A mismatch still fails closed: inspect MANIFEST.MF, upstream commit
# history and the release asset digest before changing it. Never accept an
# unverified replacement automatically.
SHA256="15d103b7e5617780bcc904c0af12f1ad242e2b345c0301136c5be4588f17edac"
URL="https://github.com/tlaplus/tlaplus/releases/download/$VERSION/tla2tools.jar"

# `command -v java` alone accepts macOS's /usr/bin/java stub, which exists but
# cannot run a JVM; probe the runtime itself so the failure is reported here,
# before any download or arm.
if ! command -v java >/dev/null || ! java -version >/dev/null 2>&1; then
  echo "TLA+: Java 11+ is required" >&2
  exit 2
fi
if command -v sha256sum >/dev/null; then
  SHA_CHECK=(sha256sum -c -)
elif command -v shasum >/dev/null; then
  SHA_CHECK=(shasum -a 256 -c -)
else
  echo "TLA+: sha256sum or shasum is required" >&2
  exit 2
fi

check_sha256() {
  printf '%s  %s\n' "$SHA256" "$1" | "${SHA_CHECK[@]}" >/dev/null 2>&1
}

valid_jar() {
  [[ -f "$JAR" ]] && check_sha256 "$JAR"
}

valid_jar && exit 0
mkdir -p "$(dirname "$JAR")"

# One temp file at a time, always removed: a failed seed or a failed download
# must not leave target/tla2tools.jar.tmp.* behind.
tmp=""
cleanup_tmp() { if [[ -n "$tmp" ]]; then rm -f "$tmp"; fi; }
trap cleanup_tmp EXIT

# Sibling worktrees usually hold a pin-matching jar already; copying one is
# faster than the download and keeps every checkout on this machine working
# when the rolling upstream asset moves ahead of the pin. Still pin-verified:
# a sibling can seed bytes, never trust. Cover sibling worktrees, sibling
# checkouts, and (from a linked worktree) the main checkout that owns the
# shared git dir.
candidates=( "$ROOT"/../*/target/tla2tools.jar "$ROOT"/.worktrees/*/target/tla2tools.jar )
if common="$(git -C "$ROOT" rev-parse --git-common-dir 2>/dev/null)"; then
  if common_root="$(cd "$ROOT" && cd "$common/.." 2>/dev/null && pwd)"; then
    candidates+=( "$common_root/target/tla2tools.jar" )
  fi
fi
for sibling in "${candidates[@]}"; do
  [[ -f "$sibling" && "$sibling" != "$JAR" ]] || continue
  if check_sha256 "$sibling"; then
    echo "TLA+: seeding $VERSION from $(cd "$(dirname "$sibling")/.." && pwd)"
    tmp="$(mktemp "${JAR}.tmp.XXXXXX")"
    if cp "$sibling" "$tmp" && mv "$tmp" "$JAR"; then
      tmp=""
      if valid_jar; then exit 0; fi
    fi
    rm -f "$tmp"
    tmp=""
  fi
done

command -v curl >/dev/null || { echo "TLA+: curl is required to fetch $VERSION" >&2; exit 2; }
tmp="$(mktemp "${JAR}.tmp.XXXXXX")"

echo "TLA+: downloading $VERSION to $JAR"
curl -fsSL --connect-timeout 15 --max-time 600 --retry 2 --retry-delay 2 -o "$tmp" "$URL"
if ! check_sha256 "$tmp"; then
  echo "TLA+: checksum mismatch for $VERSION — the rolling upstream asset has" >&2
  echo "TLA+: likely moved past the pin; see the provenance recipe in $0" >&2
  exit 1
fi
mv "$tmp" "$JAR"
tmp=""
