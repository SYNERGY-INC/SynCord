#!/bin/sh
# Builds SynCord.app (Intel + Apple Silicon) inside a .dmg, plus a zip of the server.
# Run on macOS with Rust installed.
#
# Usage: packaging/macos/build-dmg.sh <version> <output dir>
set -eu
version="$1"
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
mkdir -p "$2"; out="$(cd "$2" && pwd)"
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT

# 12.3: screen sharing uses ScreenCaptureKit.
export MACOSX_DEPLOYMENT_TARGET=12.3
cd "$repo"
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    rustup target add "$target" >/dev/null
    cargo build --release --locked --target "$target"
done
for bin in syncord syncord-server; do
    lipo -create -output "$work/$bin" \
        "target/aarch64-apple-darwin/release/$bin" "target/x86_64-apple-darwin/release/$bin"
done

# ---- SynCord.app
app="$work/dmg/SynCord.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$work/syncord" "$app/Contents/MacOS/SynCord"
cp "$repo/packaging/icons/syncord.icns" "$app/Contents/Resources/syncord.icns"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
# Ad-hoc signature: required for Apple Silicon. Not notarized, so the first launch
# needs right-click > Open (see README).
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

ln -s /Applications "$work/dmg/Applications"
hdiutil create -volname "SynCord $version" -srcfolder "$work/dmg" -ov -format UDZO \
    "$out/SynCord-$version-macOS.dmg" >/dev/null

# ---- server
srv="$work/SynCord-Server-$version-macOS"
mkdir -p "$srv"
cp "$work/syncord-server" "$srv/"
codesign --force --sign - "$srv/syncord-server"
cat > "$srv/README.txt" <<TXT
SynCord Server $version for macOS (Intel and Apple Silicon)

Open Terminal in this folder and run:  ./syncord-server
It creates syncord-server.toml (with your invite code) and syncord.db here,
and listens on port 7878 (TCP for chat, UDP for voice).

If macOS says the file can't be opened, run once:
  xattr -d com.apple.quarantine syncord-server
TXT
(cd "$work" && ditto -c -k --keepParent "$(basename "$srv")" "$out/SynCord-Server-$version-macOS.zip")

ls -la "$out"
