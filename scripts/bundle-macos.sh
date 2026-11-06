#!/usr/bin/env bash
# Baut "FYA.app" (ohne App Sandbox) und signiert sie.
#
#   ./scripts/bundle-macos.sh                       # ad-hoc Signatur
#   SIGN_IDENTITY="Apple Development: Name (TEAMID)" ./scripts/bundle-macos.sh
#
# Die App läuft bewusst ohne App Sandbox: Sie darf frei auf den Musikordner zugreifen
# und externe Programme starten. Für lokale Tests genügt die ad-hoc Signatur; für
# Verteilung/Notarisierung ein Developer-ID-Zertifikat verwenden.
set -euo pipefail
cd "$(dirname "$0")/.."

APP="target/FYA.app"
BUNDLE_ID=$(/usr/libexec/PlistBuddy -c "Print :CFBundleIdentifier" macos/Info.plist)
IDENTITY="${SIGN_IDENTITY:--}"

cargo build --release

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp macos/Info.plist "$APP/Contents/Info.plist"
cp target/release/fya "$APP/Contents/MacOS/fya"

# App-Icon: .icns aus assets/icons/app-icon.png (1024 px, transparent) erzeugen
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
    sips -z $s $s assets/icons/app-icon.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
    sips -z $((s * 2)) $((s * 2)) assets/icons/app-icon.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

codesign --force --deep --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$APP"
codesign --verify --verbose "$APP"

echo "Fertig: $APP"
echo "Datenbank:"
echo "  ~/Library/Application Support/fya/library.db"
