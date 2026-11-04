#!/usr/bin/env bash
# Baut "FYA.app" und signiert sie mit App-Sandbox-Entitlements.
#
#   ./scripts/bundle-macos.sh                       # ad-hoc Signatur
#   SIGN_IDENTITY="Apple Development: Name (TEAMID)" ./scripts/bundle-macos.sh
#
# Die App läuft in der App Sandbox (macos/entitlements.plist). Den Musikordner
# wählt der User im Ordner-Dialog; der Zugriff bleibt über ein Security-Scoped
# Bookmark in der Datenbank erhalten. Für lokale Tests genügt die ad-hoc
# Signatur; für Verteilung/Notarisierung ein Developer-ID-Zertifikat verwenden.
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

codesign --force --deep --sign "$IDENTITY" --identifier "$BUNDLE_ID" \
    --entitlements macos/entitlements.plist "$APP"
codesign --verify --verbose "$APP"

echo "Entitlements:"
codesign -d --entitlements - "$APP" 2>/dev/null | grep -A1 "com.apple" || true

echo "Fertig: $APP"
echo "Datenbank (im Container):"
echo "  ~/Library/Containers/$BUNDLE_ID/Data/Library/Application Support/fya/library.db"
