#!/usr/bin/env bash
#
# Install / update the ONE canonical WarpOss.app at ~/Applications/WarpOss.app.
#
# There is exactly one WarpOss on this machine: ~/Applications/WarpOss.app.
# Build artifacts under target/ are excluded from Spotlight (target/.metadata_never_index)
# so they never show up as extra copies in the app switcher / ctrl-space.
#
# This fork is ad-hoc signed (no Apple Developer identity here). Two things make a
# naive install crash the app, and this script guards against both:
#
#   1. Hardened runtime. Signing with `--options runtime` makes the kernel
#      strictly re-validate every executable page at runtime and SIGKILL the
#      process on any mismatch ("Code Signature Invalid / Invalid Page") -- fragile
#      for a large ad-hoc debug binary. We sign plain ad-hoc (no hardened runtime).
#
#   2. Replacing a running app. macOS memory-maps the executable; overwriting the
#      bundle while WarpOss is open invalidates those pages and crashes the live
#      process. So we refuse to install while it is running.
#
# Usage:
#   DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer PROTOC=/opt/homebrew/bin/protoc \
#     cargo build --bin warp-oss --features gui        # everything else is default-on
#   ./script/dev-install.sh
#   open ~/Applications/WarpOss.app
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/target/debug/warp-oss"
SEED_BUNDLE="$REPO/target/debug/bundle/osx/WarpOss.app"   # only used for first-time creation
DEST="$HOME/Applications/WarpOss.app"
ENTITLEMENTS="$REPO/script/Debug-Entitlements.plist"

if pgrep -x warp-oss >/dev/null 2>&1; then
  echo "error: WarpOss is running. Quit it (Cmd-Q) first, or installing would" >&2
  echo "       swap the binary under the live process and crash it." >&2
  exit 1
fi

[ -f "$BIN" ] || { echo "error: no binary at $BIN -- build it first (cargo build --bin warp-oss --features gui)." >&2; exit 1; }

# Keep target/ out of Spotlight so build-output bundles never appear as extra apps.
touch "$REPO/target/.metadata_never_index" 2>/dev/null || true

if [ ! -d "$DEST" ]; then
  # First-time install: seed the bundle structure from a `./script/run`/`cargo bundle` output.
  [ -d "$SEED_BUNDLE" ] || { echo "error: no bundle at $SEED_BUNDLE -- run ./script/run once to create the initial bundle." >&2; exit 1; }
  ditto --norsrc --noextattr --noqtn "$SEED_BUNDLE" "$DEST"
fi

# Update just the executable in the canonical bundle (Info.plist/Resources are stable
# across builds), then re-sign in place. No dependency on the target/ bundle.
cp "$BIN" "$DEST/Contents/MacOS/warp-oss"
xattr -cr "$DEST" 2>/dev/null || true
codesign --force --deep --sign - --entitlements "$ENTITLEMENTS" "$DEST"
codesign --verify --deep --strict "$DEST"

echo "✅ updated the one WarpOss (ad-hoc, no hardened runtime): $DEST"
echo "   launch:  open \"$DEST\""
