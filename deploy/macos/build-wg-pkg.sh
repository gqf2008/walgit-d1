#!/usr/bin/env bash
# build-wg-pkg.sh — build the standalone wg macOS installer package.
#
# Usage: deploy/macos/build-wg-pkg.sh <wg-binary> <version> <out-dir>
#
# The package installs exactly one file, `wg`, into /usr/local/bin — a
# directory already on the default PATH — so `sudo installer -pkg … -target /`
# makes the client usable without editing shell profiles.
#
# Signing (Developer ID Installer + notarization) is driven by secrets:
#   APPLE_INSTALLER_CERT_P12 / APPLE_INSTALLER_CERT_PASSWORD
#       base64 Developer ID Installer certificate. When BOTH are absent the
#       script prints a warning and exits 0 WITHOUT producing a package: an
#       unsigned .pkg is never published, and releases keep shipping the DMG
#       and the signed .zip. Setting only one of them is an error.
#   APPLE_ID / APPLE_TEAM_ID / APPLE_APP_PASSWORD
#       notarytool credentials; required whenever the certificate is present.
#
# Local dry run (unsigned package + structural self-check, never published):
#   WALGIT_SKIP_SIGN=1 deploy/macos/build-wg-pkg.sh ./wg 0.14.0 /tmp/out
#
# Other environment:
#   WALGIT_PKG_ARCH             force aarch64|x86_64 instead of detecting it
#   NOTARY_S3_ACCELERATION=0    disable notarytool S3 acceleration
#
# Output: <out-dir>/wg-<arch>-macos.pkg; stdout carries only that path.
set -euo pipefail

# Assigned once the temp workspace exists; the EXIT trap is written to tolerate
# a failure before either is set (set -u is on).
TMP=""
KEYCHAIN=""

die() { echo "build-wg-pkg: $*" >&2; exit 1; }
warn() { echo "build-wg-pkg: WARNING: $*" >&2; }

usage() {
    sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'
}

detect_arch() {
    local bin="$1" base archs
    if [ -n "${WALGIT_PKG_ARCH:-}" ]; then
        case "$WALGIT_PKG_ARCH" in
            aarch64|x86_64) echo "$WALGIT_PKG_ARCH"; return 0 ;;
            *) die "WALGIT_PKG_ARCH must be aarch64 or x86_64 (got '$WALGIT_PKG_ARCH')" ;;
        esac
    fi
    # A single-arch Mach-O is authoritative; a universal binary is ambiguous
    # and must be split by the caller.
    if command -v lipo >/dev/null 2>&1; then
        archs="$(lipo -archs "$bin" 2>/dev/null || true)"
        case "$archs" in
            arm64) echo aarch64; return 0 ;;
            x86_64) echo x86_64; return 0 ;;
            '') : ;;
            *) die "expected a single-arch Mach-O binary, got '$archs': $bin" ;;
        esac
    fi
    base="$(basename "$bin")"
    case "$base" in
        *aarch64*|*arm64*) echo aarch64; return 0 ;;
        *x86_64*|*amd64*) echo x86_64; return 0 ;;
    esac
    # Last resort for local smoke tests that feed a non-Mach-O stub as `wg`:
    # assume the host architecture. Real release inputs are Mach-O and were
    # answered by lipo above.
    warn "cannot read a Mach-O architecture from $bin; assuming the host architecture"
    case "$(uname -m)" in
        arm64) echo aarch64; return 0 ;;
        x86_64) echo x86_64; return 0 ;;
    esac
    die "cannot determine the architecture of $bin; set WALGIT_PKG_ARCH=aarch64|x86_64"
}

require_signing_env() {
    local missing='' var
    for var in APPLE_INSTALLER_CERT_P12 APPLE_INSTALLER_CERT_PASSWORD \
        APPLE_ID APPLE_TEAM_ID APPLE_APP_PASSWORD; do
        if [ -z "${!var:-}" ]; then
            missing="$missing $var"
        fi
    done
    [ -z "$missing" ] || die "missing signing environment:$missing"
}

import_installer_identity() {
    local cert="$TMP/installer.p12"
    KEYCHAIN="$TMP/wg-pkg-signing.keychain-db"
    local keychain_password="wg-pkg-signing-$$"
    security create-keychain -p "$keychain_password" "$KEYCHAIN" >&2
    security set-keychain-settings -lut 21600 "$KEYCHAIN" >&2
    security unlock-keychain -p "$keychain_password" "$KEYCHAIN" >&2
    if ! printf '%s' "$APPLE_INSTALLER_CERT_P12" | base64 --decode >"$cert" 2>/dev/null; then
        # BSD base64 on older macOS only knows -D.
        printf '%s' "$APPLE_INSTALLER_CERT_P12" | base64 -D >"$cert"
    fi
    security import "$cert" -k "$KEYCHAIN" -P "$APPLE_INSTALLER_CERT_PASSWORD" \
        -T /usr/bin/productsign -T /usr/bin/codesign >&2
    rm -f "$cert"
    security set-key-partition-list \
        -S apple-tool:,apple:,codesign: -s \
        -k "$keychain_password" "$KEYCHAIN" >&2
    local identity
    identity="$(security find-identity -v -p basic "$KEYCHAIN" \
        | sed -n 's/.*"\(Developer ID Installer:.*\)"/\1/p' | head -n 1)"
    [ -n "$identity" ] || die "no Developer ID Installer identity in the temporary keychain (is APPLE_INSTALLER_CERT_P12 an Installer certificate, not an Application one?)"
    printf '%s\n' "$identity"
}

notarize() {
    local pkg="$1" attempt
    local args=(submit "$pkg" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
        --password "$APPLE_APP_PASSWORD" --wait)
    if [ "${NOTARY_S3_ACCELERATION:-1}" = "0" ]; then
        args+=(--no-s3-acceleration)
    fi
    for attempt in 1 2 3; do
        if xcrun notarytool "${args[@]}" >&2; then
            return 0
        fi
        [ "$attempt" -lt 3 ] || die "notarytool failed for $pkg after 3 attempts"
        echo "notarytool attempt $attempt/3 failed; retrying in 15s" >&2
        sleep 15
    done
}

stapler_with_retry() {
    local sub="$1" pkg="$2" attempt
    for attempt in 1 2 3; do
        if xcrun stapler "$sub" "$pkg" >&2; then
            return 0
        fi
        [ "$attempt" -lt 3 ] || die "stapler $sub failed for $pkg after 3 attempts"
        echo "stapler $sub attempt $attempt/3 failed; retrying in 5s" >&2
        sleep 5
    done
}

# Structural checks run for every package this script writes (unsigned dry
# run included): one payload file, the documented identifier/version/install
# location, and an executable after installation.
verify_pkg() {
    local pkg="$1" version="$2" expect_signature="$3"
    local expand="$TMP/expand"
    local listing unexpected mode signature
    listing="$(pkgutil --payload-files "$pkg")" \
        || die "pkgutil cannot read the payload of $pkg"
    # `com.apple.provenance` is stamped by macOS on every newly created file;
    # pkgbuild stores it as the AppleDouble metadata companion `./._wg`. The
    # Installer folds it back into xattrs and installs no extra file, so it is
    # the one entry tolerated next to `./wg`.
    unexpected="$(printf '%s\n' "$listing" \
        | grep -v -x -F -e '.' -e './wg' -e './._wg' || true)"
    [ -z "$unexpected" ] || die "unexpected payload entries: $unexpected"
    printf '%s\n' "$listing" | grep -qx -F './wg' \
        || die "payload does not contain ./wg"
    rm -rf "$expand"
    pkgutil --expand "$pkg" "$expand" >&2 || die "cannot expand $pkg"
    grep -Fq 'identifier="dev.walgit.wg"' "$expand/PackageInfo" \
        || die "wrong package identifier (expected dev.walgit.wg)"
    grep -Fq "version=\"$version\"" "$expand/PackageInfo" \
        || die "wrong package version (expected $version)"
    grep -Fq 'install-location="/usr/local/bin"' "$expand/PackageInfo" \
        || die "wrong install location (expected /usr/local/bin)"
    mode="$(lsbom "$expand/Bom" | awk '$1=="./wg" {print $2; exit}')"
    [ "$mode" = "100755" ] \
        || die "./wg would install with mode $mode, expected 100755"
    if [ "$expect_signature" = "1" ]; then
        signature="$(pkgutil --check-signature "$pkg" 2>&1)" \
            || die "pkgutil --check-signature rejected $pkg"
        printf '%s\n' "$signature" >&2
        printf '%s\n' "$signature" | grep -q 'Developer ID Installer' \
            || die "package is not signed by a Developer ID Installer identity"
        spctl --assess --type install --verbose=2 "$pkg" >&2 \
            || die "spctl rejected $pkg (not notarized for install?)"
        stapler_with_retry validate "$pkg"
    fi
}

main() {
    [ "$#" -eq 3 ] || { usage >&2; exit 1; }
    local bin="$1" version="$2" out_dir="$3"
    [ -n "$bin" ] || die "empty wg binary path"
    [ -f "$bin" ] || die "missing wg binary: $bin"
    [ -x "$bin" ] || die "wg binary is not executable: $bin"
    version="${version#v}"
    [ -n "$version" ] || die "empty version"
    case "$version" in
        *[!0-9A-Za-z.+-]*) die "invalid version (allowed: 0-9 A-Z a-z . + -): $version" ;;
    esac

    local arch pkg
    arch="$(detect_arch "$bin")"
    pkg="$out_dir/wg-$arch-macos.pkg"

    local skip_sign="${WALGIT_SKIP_SIGN:-0}"
    if [ "$skip_sign" != "1" ]; then
        if [ -z "${APPLE_INSTALLER_CERT_P12:-}" ] \
            && [ -z "${APPLE_INSTALLER_CERT_PASSWORD:-}" ]; then
            warn "APPLE_INSTALLER_CERT_P12/APPLE_INSTALLER_CERT_PASSWORD are not set; skipping $pkg"
            warn "no unsigned .pkg is written — the macOS DMG and the signed .zip remain the published artifacts"
            exit 0
        fi
        require_signing_env
    fi

    TMP="$(mktemp -d)"
    KEYCHAIN=""
    trap 'if [ -n "$KEYCHAIN" ]; then security delete-keychain "$KEYCHAIN" 2>/dev/null || true; fi; if [ -n "$TMP" ]; then rm -rf "$TMP"; fi' EXIT

    local stage="$TMP/root"
    mkdir -p "$stage"
    cp "$bin" "$stage/wg"
    chmod 0755 "$stage/wg"

    local unsigned="$TMP/unsigned.pkg"
    pkgbuild --root "$stage" \
        --identifier dev.walgit.wg \
        --install-location /usr/local/bin \
        --version "$version" \
        --ownership recommended \
        "$unsigned" >&2

    local built="$unsigned"
    if [ "$skip_sign" != "1" ]; then
        local identity signed="$TMP/signed.pkg"
        # Run in this shell (not a command substitution): the function sets
        # KEYCHAIN for productsign and the EXIT trap.
        import_installer_identity >"$TMP/identity"
        identity="$(cat "$TMP/identity")"
        echo "signing with: $identity" >&2
        productsign --sign "$identity" --keychain "$KEYCHAIN" --timestamp \
            "$unsigned" "$signed" >&2
        notarize "$signed"
        stapler_with_retry staple "$signed"
        built="$signed"
    fi

    verify_pkg "$built" "$version" "$([ "$skip_sign" = "1" ] && echo 0 || echo 1)"

    mkdir -p "$out_dir"
    mv "$built" "$pkg"
    printf '%s\n' "$pkg"
}

main "$@"
