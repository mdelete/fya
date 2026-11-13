# FYA

Minimaler Musik-Player: Rust + iced, SQLite-Bibliothek, rodio-Wiedergabe.
Formate: mp3, aac, m4a (AAC), opus (über `opus-rs`).

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
`~/Library/Containers/<Bundle-ID>/Data/Library/Application Support/fya/` abgelegt.

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

## Systemabhängigkeiten zum Bauen des Projekts
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

## Cargo-Features
| Feature | Standard | Wirkung |
|---|---|---|
| `bpm` | an | BPM-Erkennung der gespielten Titel (`bpm-analyzer`): Einstellung „BPM … analysieren“ und BPM-Anzeige im Player. Ohne das Feature entfallen die Abhängigkeit samt DSP-Crates (Binary ca. 0,5 MB kleiner) und diese Oberfläche; die BPM-**Tags** (Tag-Editor, Suche `:120`) bleiben. |
| `ogg` | aus | Ogg (Vorbis, FLAC, Opus) und natives FLAC, siehe unten. |
| `controls` | aus | Medientasten und „Now Playing“ des Betriebssystems (`souvlaki`), siehe unten. |

```
cargo build --release                                    # Standard (bpm)
cargo build --release --no-default-features              # ohne BPM-Erkennung
cargo build --release --features ogg                     # Standard + Ogg/FLAC
cargo build --release --features controls                # Standard + Medientasten
cargo build --release --features "ogg controls"          # mehrere Features
```

## Optional: Medientasten (`--features controls`)
Mit dem Feature `controls` meldet sich FYA beim Betriebssystem an: Medientasten (Play/Pause,
Weiter, Zurück, Stop), Spulen sowie Titel, Album, Cover und Position in der
„Now Playing“-Anzeige. Ohne das Feature wird der gesamte Code (`src/media_controls.rs`)
nicht gebaut.

- macOS: Now-Playing/Kontrollzentrum und Medientasten (`MPRemoteCommandCenter`).
- Windows: Medienüberlagerung (SMTC). Wird erst nach dem Öffnen des Fensters angemeldet.
  Das Cover wird dort bisher nicht sicher angezeigt.
- Linux/BSD: MPRIS über D-Bus (z. B. `playerctl`, GNOME/KDE-Mediensteuerung). Benötigt
  `libdbus-1-dev` (Fedora: `dbus-devel`) zum Bauen; für die Zuordnung zur App wird die
  `linux/fya.desktop` (siehe oben) verwendet.
- Die Abhängigkeit `souvlaki` kommt aus dem Git-Branch `1.0`, daher braucht der Build Netzzugriff auf GitHub.
- Unter macOS gibt `souvlaki` bei jedem Titel mit Cover ein einzelnes `?` auf der Konsole aus
  (vergessenes `println!` im Upstream-Code).

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
Ordner wählen → Künstler → Album → Titel anklicken. Die sichtbare Titelliste ist die
Wiedergabeliste. Die Hilfe-Seite (F6) in der App listet dasselbe.

### Tastatur
| Taste | Wirkung |
|---|---|
| Leertaste | Wiedergabe starten / pausieren |
| F1 … F6 | Tab wechseln: Music, Compilations, Playlists, MusicBrainz, Settings, Help |
| Tab / Umschalt + Tab | Music / Compilations: nächster / vorheriger Künstler bzw. Album; in Dialogen nächstes / vorheriges Feld |
| `t` | Tags des laufenden Titels bearbeiten (Music, Compilations, Playlists) |
| `p` | Laufenden Titel zu einer Playlist hinzufügen |
| Alt + ↑ / ↓ | Laufenden Titel in der Playlist verschieben |
| Esc | Dialog schließen |
| Enter | In Dialogen speichern bzw. bestätigen |

Die Buchstabentasten und die Leertaste greifen nur, wenn kein Textfeld den Fokus hat.
Mit dem Feature `controls` kommen die Medientasten des Systems hinzu.

### Maus
- Klick auf Titel: abspielen.
- Slider im Player: Spulen und Lautstärke; der Lautsprecher-Knopf wählt das Ausgabegerät
  (es wird nie automatisch gewechselt; verschwindet das Gerät, pausiert die Wiedergabe).
- Trennlinie zwischen Künstler und Album ziehen: Spaltenbreite ändern (auch in Compilations und Playlists).
- ✎ / ✕ in Playlists: umbenennen / löschen bzw. Titel entfernen; ❣️: Mixtape als ZIP speichern
  (optional per ffmpeg konvertiert, siehe Settings).

### Gespeicherter Zustand
In der Datenbank (Tabelle `settings`) merkt sich FYA: Musikordner, Theme, Sprache,
Mixtape-/BPM-Einstellungen, Fenstergröße, die Trennlinie Künstler | Album und die
Lautstärke. Die Fensterposition wird bewusst nicht gespeichert (nach Multi-Monitor-Betrieb
könnte das Fenster sonst außerhalb des sichtbaren Bereichs liegen).

### Suche

Das Suchfeld sucht ab zwei Zeichen in Künstler, Album und Titel.
`:120` oder `:120-130` filtert nach BPM.

* Die Künstlerliste unter Music wird aus den Künstler-Ordnern gebaut, und „Compilations“ ist dabei bewusst ausgeblendet. Ein Künstler, der nur auf Samplern vorkommt, hat keinen Ordner und taucht in der Liste "Music" nicht auf, nur unter "Compilations" ist er zu finden.

## Links

* https://github.com/longbridge/gpui-fast
* https://github.com/longbridge/gpui-kit
