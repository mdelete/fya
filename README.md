# FYA

Minimaler Musik-Player: Rust + iced, SQLite-Bibliothek, rodio-Wiedergabe.
Formate: mp3, aac, m4a (AAC), opus (über `opus-rs`). Theme geborgt von [Halloy](https://halloy.chat/).

## Musikordner
Wird beim ersten Start im Ordner-Dialog gewählt. Erwartete Struktur:
`<Ordner>/<Artist>/<Album>/<Song.mp3|aac|m4a|opus>`

Das Ergebnis des Dialogs steht in der Datenbank (Tabelle `settings`, Schlüssel
`music_root`: Pfad + auf macOS das Security-Scoped Bookmark). Beim nächsten Start
wird der Ordner daraus wiederhergestellt; die Mediathek wird nur neu gescannt,
wenn sie leer ist oder der Ordner verschoben wurde. Über **Ordner ändern …** und
**Neu einlesen** in der Kopfzeile lässt sich der Ordner wechseln bzw. neu scannen.

Datenbank:
- Linux: `~/.local/share/fya/library.db`
- macOS: `~/Library/Application Support/fya/library.db`

## macOS
Die App läuft **ohne** App Sandbox (keine Entitlements): Sie liest und schreibt den
gewählten Musikordner direkt (Tags werden geschrieben). Der Ordner-Dialog (NSOpenPanel,
`src/folder.rs`) zeigt auch versteckte Ordner. Nach dem Auswählen wird weiterhin ein
Bookmark in der Datenbank abgelegt; ohne Sandbox genügt aber der Pfad.

Hinweis: Frühere Sandbox-Builds haben ihre Daten unter
`~/Library/Containers/<Bundle-ID>/Data/Library/Application Support/fya/` abgelegt; diese
Dateien werden nicht übernommen (Einstellungen/Playlists dort ggf. nach
`~/Library/Application Support/fya/` kopieren).

Bauen und starten:
```
./scripts/bundle-macos.sh
open "target/FYA.app"
```
- Bundle-ID `de.silpion.fya` in `macos/Info.plist` anpassen.
- `SIGN_IDENTITY="Apple Development: …"` setzen, um mit eigenem Zertifikat zu signieren
  (Standard: ad-hoc, reicht lokal).
- `cargo run` und das .app-Bundle verwenden dieselbe Datenbank.
- Neu anfangen: App beenden und `~/Library/Application Support/fya/` löschen.

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

Windows: aktuelles stable Rust mit MSVC-Toolchain (Visual Studio Build Tools, „Desktop development
with C++“). Dann `cargo build --release` → `target\release\fya.exe` (mit eingebettetem Icon, ohne
Konsolenfenster). Daten liegen unter `%APPDATA%\fya\`. Die CI (`.github/workflows/windows.yml`) baut
die .exe aber auch auf Anfrage über Actions. Beim Cross-Kompilieren von macOS/Linux
(`--target x86_64-pc-windows-gnu`, benötigt mingw-w64 (brew install mingw-w64)) wird das Icon nicht eingebettet.

## Optional: Ogg und FLAC (`--features ogg`)
Standardmäßig spielt FYA `mp3`, `aac`, `m4a` und `opus`. Mit dem Cargo-Feature `ogg` kommen
`ogg`/`oga` (Vorbis, FLAC in Ogg, Opus in Ogg) und natives `flac` dazu:

```
cargo build --release --features ogg
```

- Vorbis und FLAC decodiert symphonia (über rodio); `.ogg`-Dateien mit Opus-Inhalt erkennt die
  App am Inhalt und spielt sie mit dem eigenen Opus-Decoder.
- **Ogg-FLAC** (FLAC im Ogg-Container, meist `.oga`) lässt sich abspielen, aber `lofty` kann
  seine Tags nicht lesen: Titel ist dann der Dateiname, es gibt keine Tags und keine Laufzeit.
- **Speex** wird nicht unterstützt (kein Decoder); die Datei erscheint in der Bibliothek, beim
  Abspielen meldet die Statuszeile einen Fehler.
- Ohne das Feature werden diese Endungen beim Einlesen ignoriert. Nach dem Aktivieren die
  Bibliothek über „Neu einlesen“ aktualisieren.

## Bedienung
Ordner wählen → Künstler → Album → Titel anklicken. Leertaste = Play/Pause,
Slider unten = Spulen und Lautstärke.

### Suche

* Die Künstlerliste unter Music wird aus den Künstler-Ordnern gebaut, und „Compilations“ ist dabei bewusst ausgeblendet. Ein Künstler, der nur auf Samplern vorkommt, hat keinen Ordner und taucht in der Liste "Music" nicht auf, nur unter "Compilations" ist er zu finden.

## Links

 * [BPM Analyzer](https://github.com/not-jan/bpm-analyzer) (nicht eingebaut)
