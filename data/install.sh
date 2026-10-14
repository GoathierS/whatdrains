#!/bin/sh
# Installs a prebuilt What Drains release into ~/.local (or $PREFIX).
# Usage: ./install.sh            install and start recording
#        ./install.sh --uninstall
set -eu

cd "$(dirname "$0")"
PREFIX="${PREFIX:-$HOME/.local}"
BINDIR="$PREFIX/bin"
DATADIR="$PREFIX/share"
UNITDIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
APP_ID=io.github.GoathierS.WhatDrains

if [ "${1:-}" = "--uninstall" ]; then
    systemctl --user disable --now whatdrainsd.service 2>/dev/null || true
    rm -f "$BINDIR/whatdrains" "$BINDIR/whatdrainsd" \
        "$DATADIR/applications/$APP_ID.desktop" \
        "$DATADIR/metainfo/$APP_ID.metainfo.xml" \
        "$DATADIR/icons/hicolor/scalable/apps/$APP_ID.svg" \
        "$UNITDIR/whatdrainsd.service"
    systemctl --user daemon-reload
    echo "What Drains removed. Your history is still in ~/.local/state/whatdrains."
    exit 0
fi

install -Dm755 whatdrains "$BINDIR/whatdrains"
install -Dm755 whatdrainsd "$BINDIR/whatdrainsd"
install -Dm644 "$APP_ID.desktop" "$DATADIR/applications/$APP_ID.desktop"
install -Dm644 "$APP_ID.metainfo.xml" "$DATADIR/metainfo/$APP_ID.metainfo.xml"
install -Dm644 "$APP_ID.svg" "$DATADIR/icons/hicolor/scalable/apps/$APP_ID.svg"
mkdir -p "$UNITDIR"
sed "s|@BINDIR@|$BINDIR|" whatdrainsd.service.in > "$UNITDIR/whatdrainsd.service"

systemctl --user daemon-reload
systemctl --user enable --now whatdrainsd.service
echo "What Drains is installed and recording. Open it from your app grid."
