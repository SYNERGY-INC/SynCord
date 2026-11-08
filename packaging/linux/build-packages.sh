#!/bin/sh
# Builds the Linux release files from already-compiled binaries:
#   syncord_<v>_amd64.deb, syncord-server_<v>_amd64.deb,
#   syncord-<v>-linux-x86_64.tar.gz, syncord-server-<v>-linux-x86_64.tar.gz
#
# Usage: packaging/linux/build-packages.sh <version> <dir with syncord + syncord-server> <output dir>
set -eu
version="$1"; bins="$(cd "$2" && pwd)"; out="$3"
here="$(cd "$(dirname "$0")" && pwd)"
icons="$here/../icons"
mkdir -p "$out"; out="$(cd "$out" && pwd)"
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT

# ---- syncord (app) .deb
root="$work/syncord"
install -D -m 755 "$bins/syncord" "$root/usr/bin/syncord"
install -D -m 644 "$here/syncord.desktop" "$root/usr/share/applications/syncord.desktop"
for s in 16 32 48 64 128 256 512; do
    install -D -m 644 "$icons/syncord-$s.png" "$root/usr/share/icons/hicolor/${s}x${s}/apps/syncord.png"
done
install -D -m 644 "$here/../../crates/client/assets/fonts/OFL.txt" "$root/usr/share/doc/syncord/inter-font-license.txt"
mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CONTROL
Package: syncord
Version: $version
Architecture: amd64
Maintainer: SynCord <noreply@users.noreply.github.com>
Section: net
Priority: optional
Installed-Size: $(du -sk "$root/usr" | cut -f1)
Depends: libc6 (>= 2.35), libasound2 | libasound2t64, libxkbcommon0, libxkbcommon-x11-0
Recommends: libwayland-client0, libx11-6, libxcursor1, libxrandr2, libxi6
Homepage: https://github.com/bxpo/SynCord
Description: Lightweight text and voice chat for friends
 SynCord is a small native chat app with text channels, voice channels,
 themes and profile customization. Connects to a self-hosted SynCord server.
CONTROL
dpkg-deb --root-owner-group -Zxz --build "$root" "$out/syncord_${version}_amd64.deb" >/dev/null

# ---- syncord-server .deb (runs as a systemd service)
root="$work/syncord-server"
install -D -m 755 "$bins/syncord-server" "$root/usr/bin/syncord-server"
install -D -m 644 "$here/syncord-server.service" "$root/lib/systemd/system/syncord-server.service"
mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CONTROL
Package: syncord-server
Version: $version
Architecture: amd64
Maintainer: SynCord <noreply@users.noreply.github.com>
Section: net
Priority: optional
Installed-Size: $(du -sk "$root/usr" | cut -f1)
Depends: libc6 (>= 2.35)
Homepage: https://github.com/bxpo/SynCord
Description: Server for SynCord, a lightweight text and voice chat
 Runs the SynCord chat server as a background service on port 7878
 (TCP for chat, UDP for voice). Settings and data live in /var/lib/syncord.
CONTROL
cat > "$root/DEBIAN/postinst" <<'SCRIPT'
#!/bin/sh
set -e
if [ "$1" = "configure" ] && [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl enable syncord-server.service >/dev/null 2>&1 || true
    systemctl restart syncord-server.service || true
    sleep 1
    echo
    echo "SynCord Server is running on port 7878 and starts automatically at boot."
    echo "Invite code and settings:  sudo cat /var/lib/syncord/syncord-server.toml"
    echo "After editing settings:    sudo systemctl restart syncord-server"
    echo
fi
SCRIPT
cat > "$root/DEBIAN/prerm" <<'SCRIPT'
#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then
    systemctl stop syncord-server.service || true
    if [ "$1" = "remove" ]; then systemctl disable syncord-server.service >/dev/null 2>&1 || true; fi
fi
SCRIPT
cat > "$root/DEBIAN/postrm" <<'SCRIPT'
#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then systemctl daemon-reload || true; fi
if [ "$1" = "purge" ]; then rm -rf /var/lib/private/syncord /var/lib/syncord; fi
SCRIPT
chmod 755 "$root/DEBIAN/postinst" "$root/DEBIAN/prerm" "$root/DEBIAN/postrm"
dpkg-deb --root-owner-group -Zxz --build "$root" "$out/syncord-server_${version}_amd64.deb" >/dev/null

# ---- portable app archive (any distro)
dir="$work/syncord-$version-linux-x86_64"
mkdir -p "$dir/icons"
install -m 755 "$bins/syncord" "$dir/syncord"
install -m 755 "$here/install.sh" "$dir/install.sh"
install -m 644 "$here/syncord.desktop" "$dir/syncord.desktop"
for s in 16 32 48 64 128 256 512; do install -m 644 "$icons/syncord-$s.png" "$dir/icons/"; done
cat > "$dir/README.txt" <<TXT
SynCord $version for Linux

Run ./syncord to start it straight away, or ./install.sh to add it to your
app menu (installs to ~/.local, no root needed). ./install.sh --uninstall removes it.

Needs ALSA (libasound2) and libxkbcommon, which almost every desktop already has.
TXT
tar -C "$work" -czf "$out/syncord-$version-linux-x86_64.tar.gz" "$(basename "$dir")"

# ---- portable server archive
dir="$work/syncord-server-$version-linux-x86_64"
mkdir -p "$dir"
install -m 755 "$bins/syncord-server" "$dir/syncord-server"
install -m 644 "$here/syncord-server.service" "$dir/syncord-server.service"
cat > "$dir/README.txt" <<TXT
SynCord Server $version for Linux

Quick start: ./syncord-server   (creates syncord-server.toml with an invite code
and syncord.db in the current folder; listens on port 7878 TCP + UDP).

To run it as a service: copy syncord-server to /usr/bin/, copy
syncord-server.service to /etc/systemd/system/, then run
  sudo systemctl enable --now syncord-server
Settings then live in /var/lib/syncord/syncord-server.toml.
On Debian/Ubuntu the .deb package does all of this for you.
TXT
tar -C "$work" -czf "$out/syncord-server-$version-linux-x86_64.tar.gz" "$(basename "$dir")"

ls -la "$out"
