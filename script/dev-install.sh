#!/usr/bin/env bash
#
# Safely (re)install the locally-built WarpOss.app to ~/Applications.
#
# This fork is ad-hoc signed (there is no Apple Developer identity on this
# machine). Two things make a naive install crash the app, and this script
# guards against both:
#
#   1. Hardened runtime. Signing with `--options runtime` makes the kernel
#      strictly re-validate every executable page at runtime and SIGKILL the
#      process on any mismatch ("Code Signature Invalid / Invalid Page"). That
#      is fragile for a large ad-hoc debug binary under memory pressure. We sign
#      plain ad-hoc (no hardened runtime), which avoids that entire kill class.
#      Stock Warp doesn't need this because it is Developer-ID signed + notarized.
#
#   2. Replacing a running app. macOS memory-maps the executable; overwriting the
#      bundle while WarpOss is open invalidates those pages and crashes the live
#      process instantly. So we refuse to install while it is running.
#
# Usage:  ./script/dev-install.sh
#   (build first: DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
#                 PROTOC=/opt/homebrew/bin/protoc \
#                 cargo build --bin warp-oss --features gui)
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/target/debug/warp-oss"
SRC_BUNDLE="$REPO/target/debug/bundle/osx/WarpOss.app"
DEST="$HOME/Applications/WarpOss.app"
ENTITLEMENTS="$REPO/script/Debug-Entitlements.plist"

if pgrep -x warp-oss >/dev/null 2>&1; then
  echo "error: WarpOss is running. Quit it (Cmd-Q) first, or installing would" >&2
  echo "       swap the binary under the live process and crash it." >&2
  exit 1
fi

[ -f "$BIN" ] || { echo "error: no binary at $BIN — build it first." >&2; exit 1; }
[ -d "$SRC_BUNDLE" ] || { echo "error: no app bundle at $SRC_BUNDLE — run ./script/run once to create it." >&2; exit 1; }

# Stage the freshly built binary into the bundle.
cp "$BIN" "$SRC_BUNDLE/Contents/MacOS/warp-oss"

# ditto to ~/Applications strips resource forks / xattrs / quarantine that the
# iCloud-synced repo `target/` dir keeps re-adding and that break code signing.
rm -rf "$DEST"
ditto --norsrc --noextattr --noqtn "$SRC_BUNDLE" "$DEST"
xattr -cr "$DEST" 2>/dev/null || true

# Ad-hoc, deep, with entitlements — but deliberately NO --options runtime.
codesign --force --deep --sign - --entitlements "$ENTITLEMENTS" "$DEST"
codesign --verify --deep --strict "$DEST"

echo "✅ installed ad-hoc (no hardened runtime): $DEST"
echo "   launch:  open \"$DEST\""
