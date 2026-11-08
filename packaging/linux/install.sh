#!/bin/sh
# Installs SynCord for the current user (no root needed): ~/.local/bin/syncord
# plus a menu entry and icon. Run ./install.sh --uninstall to remove it.
set -e
here="$(cd "$(dirname "$0")" && pwd)"
bin="$HOME/.local/bin"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
icons="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"

if [ "$1" = "--uninstall" ]; then
    rm -f "$bin/syncord" "$apps/syncord.desktop"
    for s in 16 32 48 64 128 256 512; do rm -f "$icons/${s}x${s}/apps/syncord.png"; done
    echo "SynCord removed. Your settings are still in ~/.config/syncord."
    exit 0
fi

mkdir -p "$bin" "$apps"
install -m 755 "$here/syncord" "$bin/syncord"
for s in 16 32 48 64 128 256 512; do
    install -D -m 644 "$here/icons/syncord-$s.png" "$icons/${s}x${s}/apps/syncord.png"
done
sed "s|^Exec=.*|Exec=$bin/syncord|" "$here/syncord.desktop" > "$apps/syncord.desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$apps" 2>/dev/null || true
echo "SynCord installed. Find it in your app menu, or run: $bin/syncord"
