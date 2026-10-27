#!/usr/bin/env bash
# Build deb2rpm and install it for this user:
#   ~/.local/bin/deb2rpm                                  the tool (deb2rpm --help)
#   ~/.local/share/applications/deb2rpm.desktop           Files: right-click a .deb → Open With → deb2rpm
set -euo pipefail
cd "$(dirname "$0")"

BUILD_TOOL=cargo
if command -v blaze >/dev/null 2>&1; then BUILD_TOOL=blaze; fi
# Ask for a current compiler build at the exact location installed below.
# Blaze's diagnostic-format route avoids restoring an older cached artifact.
"$BUILD_TOOL" build --release --quiet --message-format=short --target-dir "$PWD/target"
BIN="${XDG_BIN_HOME:-$HOME/.local/bin}"
mkdir -p "$BIN"
BIN_TMP=$(mktemp "$BIN/.deb2rpm.XXXXXXXX")
DESKTOP_TMP=
LAUNCHER_TMP=
trap 'rm -f -- "$BIN_TMP" "${DESKTOP_TMP:-}" "${LAUNCHER_TMP:-}"' EXIT
install -m755 target/release/deb2rpm "$BIN_TMP"
mv -f -- "$BIN_TMP" "$BIN/deb2rpm"
LAUNCHER_TMP=$(mktemp "$BIN/.deb2rpm-terminal.XXXXXXXX")
install -m755 terminal-launch.sh "$LAUNCHER_TMP"
mv -f -- "$LAUNCHER_TMP" "$BIN/deb2rpm-terminal"

APPS="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$APPS"
DESKTOP_TMP=$(mktemp "$APPS/.deb2rpm.XXXXXXXX")
# Desktop entries parse escapes twice: the string layer, then Exec quoting.
EXEC_PATH=$(printf '%s' "$BIN/deb2rpm-terminal" | sed 's/\\/\\\\\\\\/g; s/"/\\\\"/g; s/\$/\\\\$/g; s/`/\\\\`/g; s/%/%%/g')
cat > "$DESKTOP_TMP" <<EOF
[Desktop Entry]
Type=Application
Name=Convert & install (deb2rpm)
Comment=Convert this .deb to an .rpm and install it
Exec="$EXEC_PATH" --pause %f
Icon=package-x-generic
MimeType=application/vnd.debian.binary-package;application/x-deb;
NoDisplay=true
Terminal=false
EOF
mv -f -- "$DESKTOP_TMP" "$APPS/deb2rpm.desktop"
/usr/bin/update-desktop-database "$APPS" 2>/dev/null || true

echo "installed: $BIN/deb2rpm ($("$BIN/deb2rpm" --help | head -1))"
