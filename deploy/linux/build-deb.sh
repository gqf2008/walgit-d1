#!/usr/bin/env bash
# build-deb.sh — assemble a .deb from prebuilt binaries (dpkg-deb only, no cargo).
# Usage: deploy/linux/build-deb.sh <bin-dir> <version>
#   bin-dir: directory with walgit, walgit-server, walgit-tray (release or debug)
#   version: e.g. 0.2.0 (a leading 'v' is stripped)
# Output: walgit_<version>_amd64.deb in the current directory.
set -euo pipefail

BIN_DIR="${1:?usage: build-deb.sh <bin-dir> <version>}"
VERSION="${2:?usage: build-deb.sh <bin-dir> <version>}"
VERSION="${VERSION#v}"
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

for b in walgit walgit-server walgit-tray; do
    [ -x "$BIN_DIR/$b" ] || { echo "missing binary: $BIN_DIR/$b" >&2; exit 1; }
done
[ -f "$REPO_ROOT/walgit.example.toml" ] || { echo "missing walgit.example.toml" >&2; exit 1; }

# dpkg version fields only allow [0-9A-Za-z.+-~] — guard before writing control.
case "$VERSION" in
    ''|*[!0-9A-Za-z.+-~]*)
        echo "invalid deb version: $VERSION" >&2; exit 1 ;;
esac

PKG="$(mktemp -d)"
trap 'rm -rf "$PKG"' EXIT
mkdir -p "$PKG/DEBIAN" "$PKG/usr/bin" "$PKG/usr/share/walgit" "$PKG/usr/share/applications"

cp "$BIN_DIR"/walgit "$BIN_DIR"/walgit-server "$BIN_DIR"/walgit-tray "$PKG/usr/bin/"
cp "$REPO_ROOT/walgit.example.toml" "$PKG/usr/share/walgit/"
# The D43 unconfigured template: postinst initializes ~/.walgit state for the
# installing user. Programs stay in /usr/bin; no binary is copied into $HOME.
cp "$REPO_ROOT/deploy/tray/macos/walgit.toml.template" "$PKG/usr/share/walgit/walgit.toml.template"
cat > "$PKG/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
# Initialize the per-user state dir for the installing user (idempotent,
# existing files never overwritten). Programs stay in /usr/bin.
# Runs under sudo dpkg -i; SUDO_USER names the human.
set -eu
user="${SUDO_USER:-}"
if [ -z "$user" ] || ! id -u "$user" >/dev/null 2>&1; then
    echo "walgit: no SUDO_USER — skipping ~/.walgit bootstrap" >&2
    exit 0
fi
home="$(getent passwd "$user" | cut -d: -f6)"
base="$home/.walgit"
mkdir -p "$base"
[ -f "$base/walgit.toml" ] || cp /usr/share/walgit/walgit.toml.template "$base/walgit.toml"
chown -R "$user" "$base" 2>/dev/null || true
exit 0
POSTINST
chmod 755 "$PKG/DEBIAN/postinst"
cat > "$PKG/usr/share/applications/walgit-tray.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=walgit-tray
Comment=walgit service tray — start/stop, update detection, click to upgrade
Exec=walgit-tray
Terminal=false
Categories=Utility;Development;
DESKTOP

cat > "$PKG/DEBIAN/control" <<CONTROL
Package: walgit
Version: $VERSION
Section: utils
Priority: optional
Architecture: amd64
Depends: libgtk-3-0, libayatana-appindicator3-1, libxdo3
Maintainer: qingfeng gao <gao.qingfeng@gmail.com>
Homepage: https://github.com/gqf2008/walgit-d1
Description: Git at any scale, on object storage
 walgit serves git (smart HTTP v0/v2, receive-pack, upload-pack, bundle-uri,
 LFS) and a browsing web UI from an object-store bucket. This package installs
 the CLI, the server and the tray app.
CONTROL

dpkg-deb --build --root-owner-group "$PKG" "walgit_${VERSION}_amd64.deb" >/dev/null
echo "built walgit_${VERSION}_amd64.deb"
