# FYA – Hinweise für Agenten

Rust-Musikplayer (iced UI, SQLite via rusqlite, lofty für Tags, rodio für Audio).
Code-Kommentare und UI-Texte sind überwiegend deutsch.

## Datenbank-Migrationen

- Das DB-Schema (`SCHEMA` in `src/db.rs`) ist **noch nicht final**.
- Wird das Schema geändert, **immer auch Migrationscode** in `migrate()` (`src/db.rs`)
  schreiben, damit bestehende Datenbanken des Entwicklers weiter funktionieren
  (neue Spalten per `ALTER TABLE`, ggf. Daten nachfüllen). Neue Spalten außerdem in `SCHEMA` aufnehmen.
- Migrationscode muss erst **ab dem Release** dauerhaft im Code bleiben. Vorher entfernt
  der Entwickler ihn auf Zuruf („alten Migrationscode entfernen“) – dann alles löschen,
  was nur für ältere Schemas nötig war, und `migrate()` nicht eigenmächtig entfernen.
- Das heißt **nicht**, dass es nie wieder Migrationscode geben darf: bei jeder neuen
  Schemaänderung wieder einbauen.
- Die Mediathek (`tracks`, `albums`, `artists`) wird beim Scan komplett neu aufgebaut;
  Einstellungen (Tabelle `settings`, z. B. Musikordner + Bookmark) bleiben erhalten.

## Sonstiges

- Bauen/Testen: `cargo build`, `cargo test`. macOS-App-Bundle: `scripts/bundle-macos.sh`.
- Die macOS-App läuft **ohne** App Sandbox (keine Entitlements), damit sie frei auf den
  Musikordner zugreifen und externe Programme starten kann.
- Nach Fertigstellung einer Codeänderung immer `cargo fmt` aufrufen.
