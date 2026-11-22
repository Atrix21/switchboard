#!/usr/bin/env bash
# Builds switchboard_<ver>_amd64.deb in dist/. The desktop app is included if it
# builds (needs libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev libxdo-dev).
set -euo pipefail
cd "$(dirname "$0")/../.."
VER=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cargo build --release -p switchboard
APP=1; cargo build --release -p switchboard-app || { APP=0; echo "desktop app not built (missing dev packages); packaging the service only"; }
PKG=dist/deb/switchboard_${VER}_amd64
rm -rf "$PKG"; mkdir -p "$PKG/DEBIAN" "$PKG/usr/bin" "$PKG/usr/lib/udev/rules.d" "$PKG/usr/share/applications" "$PKG/etc/xdg/autostart" "$PKG/usr/share/doc/switchboard"
install -m755 target/release/switchboard "$PKG/usr/bin/"
DEPENDS="libc6"
if [ "$APP" = 1 ]; then
  install -m755 target/release/switchboard-app "$PKG/usr/bin/"
  DEPENDS="libc6, libwebkit2gtk-4.1-0, libgtk-3-0, libayatana-appindicator3-1"
  cat > "$PKG/usr/share/applications/switchboard.desktop" <<DESK
[Desktop Entry]
Type=Application
Name=Switchboard
Comment=Share one mouse and keyboard across your computers
Exec=switchboard-app
Icon=input-mouse
Categories=Utility;
DESK
  cat > "$PKG/etc/xdg/autostart/switchboard.desktop" <<DESK
[Desktop Entry]
Type=Application
Name=Switchboard
Exec=switchboard-app --background
Icon=input-mouse
X-GNOME-Autostart-enabled=true
DESK
fi
# Let the logged-in user create the virtual pointer (no root at runtime).
echo 'KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput"' > "$PKG/usr/lib/udev/rules.d/60-switchboard-uinput.rules"
cp README.md "$PKG/usr/share/doc/switchboard/"
cat > "$PKG/DEBIAN/control" <<CTL
Package: switchboard
Version: $VER
Architecture: amd64
Maintainer: Switchboard <noreply@localhost>
Depends: $DEPENDS
Recommends: ddcutil
Section: utils
Priority: optional
Description: Share one mouse and keyboard across the computers on your desk
 Move the pointer between computers by crossing screen edges; the keyboard,
 clipboard and a shared monitor's input follow. Local network only, encrypted.
CTL
cat > "$PKG/DEBIAN/postinst" <<'POST'
#!/bin/sh
set -e
udevadm control --reload 2>/dev/null || true
udevadm trigger --name-match=uinput 2>/dev/null || true
POST
chmod 755 "$PKG/DEBIAN/postinst"
dpkg-deb --build --root-owner-group "$PKG" >/dev/null
echo "Built $PKG.deb"
