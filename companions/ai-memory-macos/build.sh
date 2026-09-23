#!/usr/bin/env bash
# Build AI Memory.app: Swift menu bar + staged ai-memory runtime (binary + hooks).
set -euo pipefail

COMPANION="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$COMPANION/../.." && pwd)"
DIST="${1:-$COMPANION/dist/AI Memory.app}"
CONFIG="${CONFIGURATION:-release}"

echo "building ai-memory ($CONFIG) from $ROOT"
if [[ "$CONFIG" == "release" ]]; then
  cargo build --release --bin ai-memory --manifest-path "$ROOT/Cargo.toml"
  BINARY="$ROOT/target/release/ai-memory"
  SWIFT_FLAGS=(-c release)
else
  cargo build --bin ai-memory --manifest-path "$ROOT/Cargo.toml"
  BINARY="$ROOT/target/debug/ai-memory"
  SWIFT_FLAGS=(-c debug)
fi

echo "building AIMemoryMenu"
# SwiftUI @State needs libSwiftUIMacros; Command Line Tools alone do not ship it.
XCODE_DEVELOPER="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
if [[ ! -d "$XCODE_DEVELOPER/Platforms/MacOSX.platform" ]]; then
  echo "error: install Xcode or set DEVELOPER_DIR to Xcode.app/Contents/Developer" >&2
  exit 1
fi
export DEVELOPER_DIR="$XCODE_DEVELOPER"

swift build "${SWIFT_FLAGS[@]}" --package-path "$COMPANION"
BIN_PATH="$(swift build "${SWIFT_FLAGS[@]}" --package-path "$COMPANION" --show-bin-path)"

APP="$DIST"
echo "staging $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
mkdir -p "$APP/Contents/Resources/runtime/packaging/launchd"

cp "$BIN_PATH/AIMemoryMenu" "$APP/Contents/MacOS/AIMemoryMenu"
cp "$COMPANION/Info.plist" "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

cp "$BINARY" "$APP/Contents/Resources/runtime/ai-memory"
chmod 755 "$APP/Contents/Resources/runtime/ai-memory"
rsync -a --delete "$ROOT/hooks/" "$APP/Contents/Resources/runtime/hooks/"
cp "$ROOT/packaging/launchd/com.github.akitaonrails.ai-memory.plist" \
  "$APP/Contents/Resources/runtime/packaging/launchd/"

echo "built $APP"
echo "runtime: $APP/Contents/Resources/runtime/ai-memory"
echo "open with: open \"$APP\""
