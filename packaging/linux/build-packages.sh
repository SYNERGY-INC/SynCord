#!/bin/sh
# Builds the Linux release files from already-compiled binaries:
#   celerity_<v>_amd64.deb, celerity-server_<v>_amd64.deb,
#   celerity-<v>-linux-x86_64.tar.gz, celerity-server-<v>-linux-x86_64.tar.gz
#
# Usage: packaging/linux/build-packages.sh <version> <dir with celerity + celerity-server> <output dir>
set -eu
version="$1"; bins="$(cd "$2" && pwd)"; out="$3"
here="$(cd "$(dirname "$0")" && pwd)"
icons="$here/../icons"
mkdir -p "$out"; out="$(cd "$out" && pwd)"
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT

# ---- celerity (app) .deb
root="$work/celerity"
install -D -m 755 "$bins/celerity" "$root/usr/bin/celerity"
install -D -m 644 "$here/celerity.desktop" "$root/usr/share/applications/celerity.desktop"
for s in 16 32 48 64 128 256 512; do
    install -D -m 644 "$icons/celerity-$s.png" "$root/usr/share/icons/hicolor/${s}x${s}/apps/celerity.png"
done
install -D -m 644 "$here/../../crates/client/assets/fonts/OFL.txt" "$root/usr/share/doc/celerity/inter-font-license.txt"
mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CONTROL
Package: celerity
Version: $version
Replaces: syncord
Conflicts: syncord
Provides: syncord
Architecture: amd64
Maintainer: Celerity <noreply@users.noreply.github.com>
Section: net
Priority: optional
Installed-Size: $(du -sk "$root/usr" | cut -f1)
Depends: libc6 (>= 2.35), libasound2 | libasound2t64, libxkbcommon0, libxkbcommon-x11-0
Recommends: libwayland-client0, libx11-6, libxcursor1, libxrandr2, libxi6
Homepage: https://github.com/bxpo/SynCord
Description: Lightweight text and voice chat for friends
 Celerity is a small native chat app with text channels, voice channels,
 themes and profile customization. Connects to a self-hosted Celerity server.
CONTROL
dpkg-deb --root-owner-group -Zxz --build "$root" "$out/celerity_${version}_amd64.deb" >/dev/null

# ---- celerity-server .deb (runs as a systemd service)
root="$work/celerity-server"
install -D -m 755 "$bins/celerity-server" "$root/usr/bin/celerity-server"
install -D -m 644 "$here/celerity-server.service" "$root/lib/systemd/system/celerity-server.service"
mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CONTROL
Package: celerity-server
Version: $version
Replaces: syncord-server
Conflicts: syncord-server
Provides: syncord-server
Architecture: amd64
Maintainer: Celerity <noreply@users.noreply.github.com>
Section: net
Priority: optional
Installed-Size: $(du -sk "$root/usr" | cut -f1)
Depends: libc6 (>= 2.35)
Homepage: https://github.com/bxpo/SynCord
Description: Server for Celerity, a lightweight text and voice chat
 Runs the Celerity chat server as a background service on port 7878
 (TCP for chat, UDP for voice). Settings and data live in /var/lib/celerity.
CONTROL
cat > "$root/DEBIAN/postinst" <<'SCRIPT'
#!/bin/sh
set -e
# Celerity used to be SynCord: keep a SynCord server's data (settings, invite code,
# messages, certificate) by moving its folder over the first time.
if [ -d /var/lib/private/syncord ] && [ ! -e /var/lib/private/celerity ]; then
    mv /var/lib/private/syncord /var/lib/private/celerity
    rm -f /var/lib/syncord
fi
if [ "$1" = "configure" ] && [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl enable celerity-server.service >/dev/null 2>&1 || true
    systemctl restart celerity-server.service || true
    sleep 1
    echo
    echo "Celerity Server is running on port 7878 and starts automatically at boot."
    echo "Invite code and settings:  sudo cat /var/lib/celerity/celerity-server.toml"
    echo "After editing settings:    sudo systemctl restart celerity-server"
    echo
fi
SCRIPT
cat > "$root/DEBIAN/prerm" <<'SCRIPT'
#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then
    systemctl stop celerity-server.service || true
    if [ "$1" = "remove" ]; then systemctl disable celerity-server.service >/dev/null 2>&1 || true; fi
fi
SCRIPT
cat > "$root/DEBIAN/postrm" <<'SCRIPT'
#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then systemctl daemon-reload || true; fi
if [ "$1" = "purge" ]; then rm -rf /var/lib/private/celerity /var/lib/celerity; fi
SCRIPT
chmod 755 "$root/DEBIAN/postinst" "$root/DEBIAN/prerm" "$root/DEBIAN/postrm"
dpkg-deb --root-owner-group -Zxz --build "$root" "$out/celerity-server_${version}_amd64.deb" >/dev/null

# ---- portable app archive (any distro)
dir="$work/celerity-$version-linux-x86_64"
mkdir -p "$dir/icons"
install -m 755 "$bins/celerity" "$dir/celerity"
# SynCord 0.6's updater looks for a program called syncord in the archive (same file).
ln "$dir/celerity" "$dir/syncord"
install -m 755 "$here/install.sh" "$dir/install.sh"
install -m 644 "$here/celerity.desktop" "$dir/celerity.desktop"
for s in 16 32 48 64 128 256 512; do install -m 644 "$icons/celerity-$s.png" "$dir/icons/"; done
cat > "$dir/README.txt" <<TXT
Celerity $version for Linux

Run ./celerity to start it straight away, or ./install.sh to add it to your
app menu (installs to ~/.local, no root needed). ./install.sh --uninstall removes it.

Needs ALSA (libasound2) and libxkbcommon, which almost every desktop already has.
TXT
tar -C "$work" -czf "$out/celerity-$version-linux-x86_64.tar.gz" "$(basename "$dir")"

# ---- portable server archive
dir="$work/celerity-server-$version-linux-x86_64"
mkdir -p "$dir"
install -m 755 "$bins/celerity-server" "$dir/celerity-server"
install -m 644 "$here/celerity-server.service" "$dir/celerity-server.service"
cat > "$dir/README.txt" <<TXT
Celerity Server $version for Linux

Quick start: ./celerity-server   (creates celerity-server.toml with an invite code
and celerity.db in the current folder; listens on port 7878 TCP + UDP).

To run it as a service: copy celerity-server to /usr/bin/, copy
celerity-server.service to /etc/systemd/system/, then run
  sudo systemctl enable --now celerity-server
Settings then live in /var/lib/celerity/celerity-server.toml.
On Debian/Ubuntu the .deb package does all of this for you.
TXT
tar -C "$work" -czf "$out/celerity-server-$version-linux-x86_64.tar.gz" "$(basename "$dir")"

ls -la "$out"
