#!/bin/sh
# Installs Celerity for the current user (no root needed): ~/.local/bin/celerity
# plus a menu entry and icon. Run ./install.sh --uninstall to remove it.
set -e
here="$(cd "$(dirname "$0")" && pwd)"
bin="$HOME/.local/bin"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
icons="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"

if [ "$1" = "--uninstall" ]; then
    rm -f "$bin/celerity" "$apps/celerity.desktop"
    for s in 16 32 48 64 128 256 512; do rm -f "$icons/${s}x${s}/apps/celerity.png"; done
    echo "Celerity removed. Your settings are still in ~/.config/celerity."
    exit 0
fi

mkdir -p "$bin" "$apps"
install -m 755 "$here/celerity" "$bin/celerity"
for s in 16 32 48 64 128 256 512; do
    install -D -m 644 "$here/icons/celerity-$s.png" "$icons/${s}x${s}/apps/celerity.png"
done
sed "s|^Exec=.*|Exec=$bin/celerity|" "$here/celerity.desktop" > "$apps/celerity.desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$apps" 2>/dev/null || true
echo "Celerity installed. Find it in your app menu, or run: $bin/celerity"
