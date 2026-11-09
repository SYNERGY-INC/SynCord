#!/bin/sh
# Builds Celerity.app (Intel + Apple Silicon) inside a .dmg, plus a zip of the server.
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
for bin in celerity celerity-server; do
    lipo -create -output "$work/$bin" \
        "target/aarch64-apple-darwin/release/$bin" "target/x86_64-apple-darwin/release/$bin"
done

# ---- Celerity.app
app="$work/dmg/Celerity.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$work/celerity" "$app/Contents/MacOS/Celerity"
cp "$repo/packaging/icons/celerity.icns" "$app/Contents/Resources/celerity.icns"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
# Ad-hoc signature: required for Apple Silicon. Not notarized, so the first launch
# needs right-click > Open (see README).
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

ln -s /Applications "$work/dmg/Applications"
hdiutil create -volname "Celerity $version" -srcfolder "$work/dmg" -ov -format UDZO \
    "$out/Celerity-$version-macOS.dmg" >/dev/null

# The same app named SynCord.app, for SynCord 0.6's updater (it looks for that name and
# installs it in place; Celerity's own updater then renames it to Celerity.app).
mkdir -p "$work/legacy"
ditto "$app" "$work/legacy/SynCord.app"
codesign --verify --deep --strict "$work/legacy/SynCord.app"
hdiutil create -volname "Celerity $version" -srcfolder "$work/legacy" -ov -format UDZO \
    "$out/Celerity-$version-macOS-update-from-SynCord.dmg" >/dev/null

# ---- server
srv="$work/Celerity-Server-$version-macOS"
mkdir -p "$srv"
cp "$work/celerity-server" "$srv/"
codesign --force --sign - "$srv/celerity-server"
cat > "$srv/README.txt" <<TXT
Celerity Server $version for macOS (Intel and Apple Silicon)

Open Terminal in this folder and run:  ./celerity-server
It creates celerity-server.toml (with your invite code) and celerity.db here,
and listens on port 7878 (TCP for chat, UDP for voice).

If macOS says the file can't be opened, run once:
  xattr -d com.apple.quarantine celerity-server
TXT
(cd "$work" && ditto -c -k --keepParent "$(basename "$srv")" "$out/Celerity-Server-$version-macOS.zip")

ls -la "$out"
