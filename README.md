# FYA

Minimaler Musik-Player: Rust + iced, SQLite-Bibliothek, rodio-Wiedergabe.
Formate: mp3, aac, m4a (AAC), opus (reines Rust über `opus-rs`).

## Musikordner
Der Ordner wird nicht mehr im Code festgelegt, sondern beim ersten Start im
Ordner-Dialog gewählt. Erwartete Struktur:
`<Ordner>/<Artist>/<Album>/<Song.mp3|aac|m4a|opus>`

Das Ergebnis des Dialogs steht in der Datenbank (Tabelle `settings`, Schlüssel
`music_root`: Pfad + auf macOS das Security-Scoped Bookmark). Beim nächsten Start
wird der Ordner daraus wiederhergestellt; die Mediathek wird nur neu gescannt,
wenn sie leer ist oder der Ordner verschoben wurde. Über **Ordner ändern …** und
**Neu einlesen** in der Kopfzeile lässt sich der Ordner wechseln bzw. neu scannen.

Datenbank:
- Linux: `~/.local/share/fya/library.db`
- macOS (Sandbox): `~/Library/Containers/<Bundle-ID>/Data/Library/Application Support/fya/library.db`

## macOS: App Sandbox
Die App läuft als sandboxed .app (`macos/entitlements.plist`):

| Entitlement | Zweck |
|---|---|
| `com.apple.security.app-sandbox` | Sandbox an |
| `com.apple.security.files.user-selected.read-only` | gewählten Ordner lesen |
| `com.apple.security.files.bookmarks.app-scope` | Zugriff über Neustarts (Bookmark) |

Ablauf: Ordner-Dialog (NSOpenPanel via `rfd`) → Zugriff gilt für diese Sitzung →
direkt danach wird ein Security-Scoped Bookmark erzeugt und mit dem Pfad in der DB
gespeichert → beim Start wird es aufgelöst und `startAccessingSecurityScopedResource`
aufgerufen (`src/folder.rs`). Es braucht keine Dateizugriffs-Texte (TCC) mehr, und
auf Desktop/Dokumente/Downloads wird nichts mehr abgefragt, außer der User wählt dort
den Ordner aus.

Bauen und starten:
```
./scripts/bundle-macos.sh
open "target/FYA.app"
```
- Bundle-ID `de.silpion.fya` in `macos/Info.plist` anpassen.
- `SIGN_IDENTITY="Apple Development: …"` setzen, um mit eigenem Zertifikat zu signieren
  (Standard: ad-hoc, reicht lokal).
- Mit `cargo run` läuft die Binary **ohne** Sandbox (und damit mit normalem Pfadzugriff,
  der Dialog fragt trotzdem). Getestet wird die Sandbox nur über das .app-Bundle.
- Neu anfangen: App beenden und die DB im Container löschen.

## Systemabhängigkeiten
Aktuelles stable Rust (Edition 2024).

macOS: nichts zusätzlich.

Linux:
```
sudo apt install build-essential pkg-config libasound2-dev
```
(Fedora: `alsa-lib-devel pkgconf`). Der Ordner-Dialog nutzt das XDG Desktop Portal
– ein Portal-Backend (GNOME/KDE/GTK) muss installiert sein, bei den meisten Desktops
ist das der Fall. Für iced ggf. Vulkan-Treiber (`mesa-vulkan-drivers`).

Linux-Icon (optional, für Dock/Taskleiste unter Wayland): 
```
install -Dm644 assets/icons/app-icon.png ~/.local/share/icons/hicolor/1024x1024/apps/fya.png
install -Dm644 linux/fya.desktop ~/.local/share/applications/fya.desktop
```

## Bedienung
Ordner wählen → Künstler → Album → Titel anklicken. Leertaste = Play/Pause,
Slider unten = Spulen und Lautstärke.
