//! SQLite-Mediendatenbank: Schema, Scan der Ordnerstruktur, Abfragen.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::{Path, PathBuf};

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag, TagExt};

/// Dateiendungen, die als Song erkannt werden (`m4a` = AAC im MP4-Container).
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "aac", "m4a", "opus"];

#[derive(Clone, Debug)]
pub struct Artist {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Album {
    pub id: i64,
    pub title: String,
    pub year: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: i64,
    pub path: PathBuf,
    pub title: String,
    pub track_no: Option<u32>,
    pub duration_secs: u32,
    pub album_title: String,
    pub artist_name: String,
    /// Tags der Datei (None = nicht vorhanden)
    pub artist_tag: Option<String>,
    pub album_tag: Option<String>,
    pub compilation: bool,
    /// Nur Playlists: Pfad steht nicht (mehr) in der Mediathek
    pub missing: bool,
}

pub struct Library {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS artists (
    id    INTEGER PRIMARY KEY,
    name  TEXT NOT NULL,
    sort_name TEXT NOT NULL DEFAULT '',  -- MusicBrainz-Sortierschlüssel, siehe sort_key()
    mbid  TEXT,  -- MusicBrainz-ID des Künstlers
    path  TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS albums (
    id         INTEGER PRIMARY KEY,
    artist_id  INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
    title      TEXT NOT NULL,
    year       INTEGER,
    mbid       TEXT,  -- MusicBrainz-ID (UUID, Textform)
    path       TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS tracks (
    id             INTEGER PRIMARY KEY,
    album_id       INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
    path           TEXT NOT NULL UNIQUE,
    format         TEXT NOT NULL,
    title          TEXT NOT NULL,
    artist_tag     TEXT,
    album_tag      TEXT,
    track_no       INTEGER,
    track_total    INTEGER,
    album_artist_tag TEXT,
    composer       TEXT,
    compilation    INTEGER NOT NULL DEFAULT 0,
    disc_no        INTEGER NOT NULL DEFAULT 1,
    year           INTEGER,
    duration_secs  INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    text  TEXT,
    blob  BLOB
);
-- Playlists überleben den Scan (der nur tracks/albums/artists neu aufbaut); Titel
-- werden über den Dateipfad referenziert, nicht über die track-ID.
CREATE TABLE IF NOT EXISTS playlists (
    id    INTEGER PRIMARY KEY,
    name  TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS playlist_tracks (
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    path        TEXT NOT NULL,
    PRIMARY KEY (playlist_id, position)
);
CREATE INDEX IF NOT EXISTS idx_albums_artist ON albums(artist_id);
CREATE INDEX IF NOT EXISTS idx_tracks_album  ON tracks(album_id);
";

/// Migration älterer Datenbanken auf das aktuelle Schema.
/// (Siehe CLAUDE.md: Migrationscode bleibt, bis er ausdrücklich entfernt werden soll.)
/// Derzeit nichts zu tun – bei der nächsten Schemaänderung hier wieder einbauen
/// (neue Spalten per `ALTER TABLE`, ggf. Daten nachfüllen).
fn migrate(_conn: &Connection) -> Result<()> {
    Ok(())
}

const TRACK_SELECT: &str = "
SELECT t.id, t.path, t.title, t.track_no, t.duration_secs, al.title, ar.name,
       t.artist_tag, t.album_tag, t.compilation
FROM tracks t
JOIN albums  al ON al.id = t.album_id
JOIN artists ar ON ar.id = al.artist_id";

/// Vom Ordner-Dialog gewählter Musikordner, in der DB gespeichert.
#[derive(Clone, Debug)]
pub struct MusicRoot {
    pub path: PathBuf,
    /// macOS: Security-Scoped Bookmark, damit die Sandbox den Ordner nach einem
    /// Neustart wieder freigibt.
    pub bookmark: Option<Vec<u8>>,
}

/// Baut die Mediathek aus `root/<artist>/<album>/<song>` neu auf. Öffnet eine
/// eigene Verbindung und kann deshalb auf einem Worker-Thread laufen.
/// Rückgabe: Anzahl der Titel.
pub fn rebuild_from(db_path: &Path, root: &Path) -> Result<usize> {
    if !root.is_dir() {
        bail!("Musikordner nicht gefunden: {}", root.display());
    }
    let mut conn = Connection::open(db_path)
        .with_context(|| format!("Datenbank öffnen: {}", db_path.display()))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    let n = scan(&mut conn, root)?;
    // IDs aus musicbrainz.db zurückspielen (die Datei selbst bleibt unangetastet).
    if let Err(e) = crate::musicbrainz::restore_into(&conn, db_path) {
        eprintln!("MusicBrainz-IDs konnten nicht zurückgespielt werden: {e:#}");
    }
    Ok(n)
}

fn scan(conn: &mut Connection, root: &Path) -> Result<usize> {
    // Alles in einer Transaktion: bei einem Fehler bleibt die alte Mediathek.
    let tx = conn.transaction()?;
    tx.execute_batch("DELETE FROM tracks; DELETE FROM albums; DELETE FROM artists;")?;
    let mut n_tracks = 0usize;

    for artist_dir in sorted_entries(root, |p| p.is_dir())? {
        let artist_name = file_name(&artist_dir);
        tx.execute(
            "INSERT INTO artists(name, sort_name, path) VALUES (?1, ?2, ?3)",
            params![
                artist_name,
                sort_key(&artist_name),
                artist_dir.to_string_lossy()
            ],
        )?;
        let artist_id = tx.last_insert_rowid();
        eprintln!("  {artist_name}");

        for album_dir in sorted_entries(&artist_dir, |p| p.is_dir())? {
            let files = sorted_entries(&album_dir, is_audio_file)?;
            if files.is_empty() {
                continue;
            }
            tx.execute(
                "INSERT INTO albums(artist_id, title, path) VALUES (?1, ?2, ?3)",
                params![
                    artist_id,
                    file_name(&album_dir),
                    album_dir.to_string_lossy()
                ],
            )?;
            let album_id = tx.last_insert_rowid();

            for file in files {
                let mut meta = read_meta(&file);
                fill_tags_from_filename(&file, &mut meta);
                let format = extension(&file);
                let title = meta.title.clone().unwrap_or_else(|| file_stem(&file));
                tx.execute(
                    "INSERT INTO tracks(album_id, path, format, title, artist_tag, album_tag,
                                        album_artist_tag, composer, track_no, track_total,
                                        compilation, disc_no, year, duration_secs)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        album_id,
                        file.to_string_lossy(),
                        format,
                        title,
                        meta.artist,
                        meta.album,
                        meta.album_artist,
                        meta.composer,
                        meta.track_no,
                        meta.track_total,
                        meta.compilation,
                        meta.disc_no.unwrap_or(1),
                        meta.year,
                        meta.duration_secs,
                    ],
                )?;
                if let Some(year) = meta.year {
                    tx.execute(
                        "UPDATE albums SET year = ?1 WHERE id = ?2 AND year IS NULL",
                        params![year, album_id],
                    )?;
                }
                n_tracks += 1;
            }
        }
    }

    // Künstler-Ordner ohne verwertbare Alben wieder entfernen.
    tx.execute(
        "DELETE FROM artists WHERE id NOT IN (SELECT artist_id FROM albums)",
        [],
    )?;
    tx.commit()?;
    eprintln!("Scan fertig: {n_tracks} Titel.");
    Ok(n_tracks)
}

impl Library {
    /// Öffnet die Datenbank; Datei und Schema werden bei Bedarf angelegt.
    /// Befüllt wird sie erst, nachdem im Ordner-Dialog ein Musikordner
    /// gewählt wurde (siehe [`rebuild_from`]).
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("Datenbank öffnen: {}", db_path.display()))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Library { conn })
    }

    /// Einfache Einstellung (Textwert) aus der Tabelle `settings`.
    pub fn setting(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT text FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings(key, text) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET text = excluded.text",
            params![key, value],
        )?;
        Ok(())
    }

    /// Der zuletzt gewählte Musikordner (Ergebnis des Ordner-Dialogs).
    pub fn music_root(&self) -> Result<Option<MusicRoot>> {
        Ok(self
            .conn
            .query_row(
                "SELECT text, blob FROM settings WHERE key = 'music_root'",
                [],
                |r| {
                    let path: String = r.get(0)?;
                    Ok(MusicRoot {
                        path: PathBuf::from(path),
                        bookmark: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// Speichert das Ergebnis des Ordner-Dialogs (Pfad + ggf. Bookmark).
    pub fn set_music_root(&self, root: &MusicRoot) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings(key, text, blob) VALUES ('music_root', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET text = excluded.text, blob = excluded.blob",
            params![root.path.to_string_lossy(), root.bookmark],
        )?;
        Ok(())
    }

    pub fn has_tracks(&self) -> bool {
        self.conn
            .query_row("SELECT EXISTS(SELECT 1 FROM tracks)", [], |r| {
                r.get::<_, bool>(0)
            })
            .unwrap_or(false)
    }

    pub fn artists(&self) -> Result<Vec<Artist>> {
        let mut st = self
            .conn
            .prepare("SELECT id, name FROM artists ORDER BY sort_name, name")?;
        let rows = st.query_map([], |r| {
            Ok(Artist {
                id: r.get(0)?,
                name: r.get(1)?,
            })
        })?;
        let mut artists: Vec<Artist> = rows.collect::<rusqlite::Result<_>>()?;
        // Anzeigename aus den Tags (Ordnernamen kennen `/ : ?` nicht)
        let display = artist_display_names(&self.conn)?;
        for a in &mut artists {
            if let Some(shown) = display.get(&a.name) {
                a.name = shown.clone();
            }
        }
        Ok(artists)
    }

    /// Anzeigenamen (Tag) für Künstler-Ordner, siehe [`artist_display_names`].
    pub fn artist_names(&self) -> Result<std::collections::HashMap<String, String>> {
        artist_display_names(&self.conn)
    }

    /// Anzeigetitel (Tag) für Album-Ordner, siehe [`album_display_titles`].
    pub fn album_titles(&self) -> Result<std::collections::HashMap<(String, String), String>> {
        album_display_titles(&self.conn)
    }

    /// Alben eines Künstlers. `title` ist der Anzeigetitel: Ordnernamen können
    /// `: ? " / …` nicht enthalten (dort steht `_`); stimmt der häufigste Album-Tag
    /// der Titel bis auf solche Zeichen mit dem Ordnernamen überein, wird der Tag
    /// angezeigt. Der Schlüssel in der DB (`albums.title`) bleibt der Ordnername.
    pub fn albums(&self, artist_id: i64) -> Result<Vec<Album>> {
        let mut st = self.conn.prepare(
            "SELECT al.id, al.title, al.year,
                    (SELECT t.album_tag FROM tracks t
                      WHERE t.album_id = al.id AND t.album_tag IS NOT NULL AND t.album_tag <> ''
                      GROUP BY t.album_tag ORDER BY COUNT(*) DESC LIMIT 1)
             FROM albums al WHERE al.artist_id = ?1
             ORDER BY COALESCE(al.year, 9999), al.title COLLATE NOCASE",
        )?;
        let rows = st.query_map([artist_id], |r| {
            let folder: String = r.get(1)?;
            let tag: Option<String> = r.get(3)?;
            Ok(Album {
                id: r.get(0)?,
                title: display_title(folder, tag),
                year: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn tracks_by_album(&self, album_id: i64) -> Result<Vec<Track>> {
        self.query_tracks(
            &format!(
                "{TRACK_SELECT} WHERE t.album_id = ?1
                 ORDER BY t.disc_no, COALESCE(t.track_no, 9999), t.title COLLATE NOCASE"
            ),
            album_id,
        )
    }

    pub fn playlists(&self) -> Result<Vec<Playlist>> {
        let mut st = self
            .conn
            .prepare("SELECT id, name FROM playlists ORDER BY name COLLATE NOCASE")?;
        let rows = st.query_map([], |r| {
            Ok(Playlist {
                id: r.get(0)?,
                name: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Titel einer Playlist in Playlist-Reihenfolge. Pfade, die nicht (mehr) in der
    /// Mediathek stehen, kommen als Platzhalter mit `missing = true` (negative ID).
    pub fn playlist_tracks(&self, playlist_id: i64) -> Result<Vec<Track>> {
        let mut st = self.conn.prepare(
            "SELECT t.id, pt.path, t.title, t.track_no, t.duration_secs, al.title, ar.name,
                    t.artist_tag, t.album_tag, t.compilation
             FROM playlist_tracks pt
             LEFT JOIN tracks  t  ON t.path = pt.path
             LEFT JOIN albums  al ON al.id = t.album_id
             LEFT JOIN artists ar ON ar.id = al.artist_id
             WHERE pt.playlist_id = ?1 ORDER BY pt.position",
        )?;
        let mut out = Vec::new();
        let mut rows = st.query([playlist_id])?;
        while let Some(r) = rows.next()? {
            let path = PathBuf::from(r.get::<_, String>(1)?);
            let id: Option<i64> = r.get(0)?;
            out.push(match id {
                Some(id) => Track {
                    id,
                    title: r.get(2)?,
                    track_no: r.get(3)?,
                    duration_secs: r.get(4)?,
                    album_title: r.get(5)?,
                    artist_name: r.get(6)?,
                    artist_tag: r.get::<_, Option<String>>(7)?.filter(|s| !s.trim().is_empty()),
                    album_tag: r.get::<_, Option<String>>(8)?.filter(|s| !s.trim().is_empty()),
                    compilation: r.get(9)?,
                    path,
                    missing: false,
                },
                None => Track {
                    id: -(out.len() as i64) - 1,
                    title: file_stem(&path),
                    track_no: None,
                    duration_secs: 0,
                    album_title: String::new(),
                    artist_name: String::new(),
                    artist_tag: None,
                    album_tag: None,
                    compilation: false,
                    path,
                    missing: true,
                },
            });
        }
        Ok(out)
    }

    /// Legt eine Playlist an (Name eindeutig, ohne Groß-/Kleinschreibung).
    pub fn create_playlist(&self, name: &str) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() {
            bail!("Bitte einen Namen eingeben");
        }
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlists WHERE name = ?1 COLLATE NOCASE)",
            [name],
            |r| r.get(0),
        )?;
        if exists {
            bail!("Eine Playlist „{name}“ gibt es schon");
        }
        self.conn
            .execute("INSERT INTO playlists(name) VALUES (?1)", [name])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Benennt eine Playlist um (Name eindeutig, ohne Groß-/Kleinschreibung).
    pub fn rename_playlist(&self, id: i64, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("Bitte einen Namen eingeben");
        }
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlists WHERE name = ?1 COLLATE NOCASE AND id <> ?2)",
            params![name, id],
            |r| r.get(0),
        )?;
        if exists {
            bail!("Eine Playlist „{name}“ gibt es schon");
        }
        self.conn
            .execute("UPDATE playlists SET name = ?1 WHERE id = ?2", params![name, id])?;
        Ok(())
    }

    /// Verschiebt einen Titel um `delta` (-1 = nach oben, +1 = nach unten).
    /// Rückgabe: false, wenn er schon am Rand steht oder nicht enthalten ist.
    pub fn move_in_playlist(&self, id: i64, path: &Path, delta: i32) -> Result<bool> {
        let rows: Vec<(i64, String)> = {
            let mut st = self.conn.prepare(
                "SELECT position, path FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position",
            )?;
            st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let path = path.to_string_lossy();
        let Some(i) = rows.iter().position(|(_, p)| *p == path) else {
            return Ok(false);
        };
        let j = i as i64 + delta as i64;
        if j < 0 || j >= rows.len() as i64 {
            return Ok(false);
        }
        let (pos_a, pos_b) = (rows[i].0, rows[j as usize].0);
        // Positionen tauschen (Primärschlüssel -> über eine freie Zwischenposition)
        let tx = self.conn.unchecked_transaction()?;
        let upd = "UPDATE playlist_tracks SET position = ?1 WHERE playlist_id = ?2 AND position = ?3";
        tx.execute(upd, params![-1, id, pos_a])?;
        tx.execute(upd, params![pos_a, id, pos_b])?;
        tx.execute(upd, params![pos_b, id, -1])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn delete_playlist(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM playlist_tracks WHERE playlist_id = ?1", [id])?;
        self.conn.execute("DELETE FROM playlists WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Hängt einen Titel (per Pfad) ans Ende an. Rückgabe: false, wenn er schon enthalten ist.
    pub fn add_to_playlist(&self, id: i64, path: &Path) -> Result<bool> {
        let path = path.to_string_lossy();
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlist_tracks WHERE playlist_id = ?1 AND path = ?2)",
            params![id, path],
            |r| r.get(0),
        )?;
        if exists {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT INTO playlist_tracks(playlist_id, position, path)
             VALUES (?1, COALESCE((SELECT MAX(position) + 1 FROM playlist_tracks WHERE playlist_id = ?1), 0), ?2)",
            params![id, path],
        )?;
        Ok(true)
    }

    pub fn remove_from_playlist(&self, id: i64, path: &Path) -> Result<()> {
        self.conn.execute(
            "DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND path = ?2",
            params![id, path.to_string_lossy()],
        )?;
        Ok(())
    }

    pub fn tracks_by_artist(&self, artist_id: i64) -> Result<Vec<Track>> {
        self.query_tracks(
            &format!(
                "{TRACK_SELECT} WHERE al.artist_id = ?1
                 ORDER BY COALESCE(al.year, 9999), al.title COLLATE NOCASE,
                          t.disc_no, COALESCE(t.track_no, 9999), t.title COLLATE NOCASE"
            ),
            artist_id,
        )
    }

    fn query_tracks(&self, sql: &str, id: i64) -> Result<Vec<Track>> {
        let mut st = self.conn.prepare(sql)?;
        let rows = st.query_map([id], |r| {
            let path: String = r.get(1)?;
            Ok(Track {
                id: r.get(0)?,
                path: PathBuf::from(path),
                title: r.get(2)?,
                track_no: r.get(3)?,
                duration_secs: r.get(4)?,
                album_title: r.get(5)?,
                artist_name: r.get(6)?,
                artist_tag: r
                    .get::<_, Option<String>>(7)?
                    .filter(|s| !s.trim().is_empty()),
                album_tag: r
                    .get::<_, Option<String>>(8)?
                    .filter(|s| !s.trim().is_empty()),
                compilation: r.get(9)?,
                missing: false,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// Sortierschlüssel nach MusicBrainz-Regeln: führender Artikel ("The", "A",
/// "An") wird ans Ende gestellt ("The Beatles" -> "beatles, the"), Groß-/
/// Kleinschreibung ist egal. Angezeigt wird weiter der Originalname.
fn sort_key(name: &str) -> String {
    let name = name.trim();
    let lower = name.to_lowercase();
    for article in ["the", "an", "a"] {
        if let Some(rest) = lower.strip_prefix(article) {
            if rest.starts_with(char::is_whitespace) && !rest.trim().is_empty() {
                return format!("{}, {article}", rest.trim());
            }
        }
    }
    lower
}

#[cfg(test)]
mod tests {
    use super::sort_key;

    #[test]
    fn artist_sorting() {
        let mut v = vec![
            "The Beatles",
            "Aerosmith",
            "A Tribe Called Quest",
            "the who",
            "A-ha",
            "Beck",
            "The",
            "Anthrax",
        ];
        v.sort_by_cached_key(|n| sort_key(n));
        assert_eq!(
            v,
            [
                "A-ha",
                "Aerosmith",
                "Anthrax",
                "The Beatles",
                "Beck",
                "The",
                "A Tribe Called Quest",
                "the who"
            ]
        );
    }
}

// ---------------------------------------------------------------------------
// Metadaten
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Meta {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    composer: Option<String>,
    track_no: Option<u32>,
    track_total: Option<u32>,
    compilation: bool,
    disc_no: Option<u32>,
    year: Option<u32>,
    duration_secs: u32,
}

/// Liest Tags (ID3v2, ID3v1, MP4-Atome, Vorbis-Comments/OpusTags) und Dauer.
fn read_meta(path: &Path) -> Meta {
    let tagged = match lofty::read_from_path(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("    Warnung: {} nicht lesbar ({e})", path.display());
            return Meta::default();
        }
    };

    let mut meta = Meta {
        duration_secs: tagged.properties().duration().as_secs() as u32,
        ..Meta::default()
    };

    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        meta.title = tag
            .title()
            .map(|s| s.into_owned())
            .filter(|s| !s.trim().is_empty());
        meta.artist = tag.artist().map(|s| s.into_owned());
        meta.album = tag.album().map(|s| s.into_owned());
        meta.album_artist = text_item(tag, &ItemKey::AlbumArtist);
        meta.composer = text_item(tag, &ItemKey::Composer);
        meta.track_no = tag.track();
        meta.track_total = tag.track_total();
        meta.compilation =
            tag.get(&ItemKey::FlagCompilation)
                .is_some_and(|item| match item.value() {
                    ItemValue::Text(t) | ItemValue::Locator(t) => {
                        matches!(t.trim().to_lowercase().as_str(), "1" | "true" | "yes")
                    }
                    ItemValue::Binary(b) => b.first().is_some_and(|&x| x == 1 || x == b'1'),
                });
        meta.disc_no = tag.disk();
        meta.year = tag.year();
    }
    meta
}

/// Hat die Datei weder Titel noch Album noch Künstler, werden diese aus dem
/// Dateinamen (`ARTIST - TITLE.*` oder `ARTIST - ALBUM - TITLE.*`) gelesen, in
/// die Datei geschrieben und in `meta` übernommen. Passt kein Schema: nichts tun.
fn fill_tags_from_filename(path: &Path, meta: &mut Meta) {
    if meta.title.is_some() || meta.album.is_some() || meta.artist.is_some() {
        return;
    }
    let stem = file_stem(path);
    let parts: Vec<&str> = stem.split(" - ").map(str::trim).collect();
    if parts.iter().any(|p| p.is_empty()) {
        return;
    }
    let (artist, album, title) = match parts[..] {
        [artist, title] => (artist, None, title),
        [artist, album, title] => (artist, Some(album), title),
        _ => return,
    };
    match write_tags(path, artist, album, title) {
        Ok(()) => {
            meta.artist = Some(artist.to_string());
            meta.album = album.map(str::to_string);
            meta.title = Some(title.to_string());
        }
        Err(e) => eprintln!(
            "    Warnung: Tags für {} nicht geschrieben ({e})",
            path.display()
        ),
    }
}

/// Schreibt Künstler, (optional) Album und Titel in die Datei; legt den
/// Tag-Container an, falls die Datei noch keinen hat. Bestehende andere Tags
/// bleiben erhalten.
pub fn write_tags(path: &Path, artist: &str, album: Option<&str>, title: &str) -> Result<()> {
    let mut tagged = lofty::read_from_path(path)?;
    if tagged.primary_tag().is_none() && tagged.first_tag().is_none() {
        let tag_type = tagged.primary_tag_type();
        tagged.insert_tag(Tag::new(tag_type));
    }
    let tag = match tagged.primary_tag_mut() {
        Some(t) => t,
        None => tagged.first_tag_mut().context("kein Tag-Container")?,
    };
    tag.set_artist(artist.to_string());
    if let Some(album) = album {
        tag.set_album(album.to_string());
    }
    tag.set_title(title.to_string());
    tag.save_to_path(path, WriteOptions::default())?;
    Ok(())
}

/// Eingebettetes Cover (ID3 APIC, MP4 covr, Vorbis/Opus PICTURE): bevorzugt das
/// Frontcover, sonst das erste Bild. Rohbytes (JPEG/PNG …).
pub fn read_cover(path: &Path) -> Option<Vec<u8>> {
    use lofty::picture::PictureType;
    let tagged = lofty::read_from_path(path).ok()?;
    let pictures: Vec<_> = tagged.tags().iter().flat_map(|t| t.pictures()).collect();
    pictures
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first())
        .map(|p| p.data().to_vec())
}

fn text_item(tag: &lofty::tag::Tag, key: &ItemKey) -> Option<String> {
    tag.get_string(key)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Dateisystem-Helfer
// ---------------------------------------------------------------------------

fn sorted_entries(dir: &Path, keep: impl Fn(&Path) -> bool) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("Verzeichnis lesen: {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        // versteckte Einträge (".DS_Store", "._foo.mp3" …) ignorieren
        .filter(|p| !file_name(p).starts_with('.'))
        .filter(|p| keep(p))
        .collect();
    v.sort_by_key(|p| file_name(p).to_lowercase());
    Ok(v)
}

fn is_audio_file(p: &Path) -> bool {
    p.is_file() && AUDIO_EXTENSIONS.contains(&extension(p).as_str())
}

fn extension(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn file_stem(p: &Path) -> String {
    p.file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Anzeigetitel aller Alben: (Künstler-Ordner, Album-Ordner) -> Titel (siehe
/// [`Library::albums`]). Für MusicBrainz-Abfragen und -Anzeige.
pub fn album_display_titles(
    conn: &Connection,
) -> Result<std::collections::HashMap<(String, String), String>> {
    let mut st = conn.prepare(
        "SELECT ar.name, al.title,
                (SELECT t.album_tag FROM tracks t
                  WHERE t.album_id = al.id AND t.album_tag IS NOT NULL AND t.album_tag <> ''
                  GROUP BY t.album_tag ORDER BY COUNT(*) DESC LIMIT 1)
         FROM albums al JOIN artists ar ON ar.id = al.artist_id",
    )?;
    let rows = st.query_map([], |r| {
        let (artist, folder): (String, String) = (r.get(0)?, r.get(1)?);
        let tag: Option<String> = r.get(2)?;
        Ok(((artist, folder.clone()), display_title(folder, tag)))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod artist_name_tests {
    use super::*;

    #[test]
    fn kuenstler_tag_nur_bei_sanitisierten_zeichen() {
        assert!(tag_matches_folder("AC/DC", "AC_DC"));
        assert!(tag_matches_folder("R.E.M.", "R.E.M_"));
        assert!(tag_matches_folder("V.A.S.T.", "V.A.S.T_"));
        assert!(tag_matches_folder("Then: The Years", "Then_ The Years"));
        assert!(tag_matches_folder("Sunn O)))", "Sunn O)))"));
        assert!(!tag_matches_folder("Kanye West feat. Jay-Z", "Kanye West"));
    }
}

/// Ersetzt Zeichen, die in Ordnernamen nicht vorkommen dürfen, durch `_`.
fn sanitize_folder_name(s: &str) -> String {
    s.chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect()
}

/// Anzeigetitel eines Albums: der Tag, wenn er dem Ordnernamen bis auf
/// ersetzte Sonderzeichen entspricht, sonst der Ordnername.
fn display_title(folder: String, tag: Option<String>) -> String {
    match tag {
        Some(tag) if tag != folder && tag_matches_folder(&tag, &folder) => tag.trim().to_string(),
        _ => folder,
    }
}

/// Entspricht der Tag dem Ordnernamen bis auf ersetzte Sonderzeichen? Ein
/// Punkt am Ende (`R.E.M.`) wird in Ordnernamen ebenfalls zu `_` (`R.E.M_`).
fn tag_matches_folder(tag: &str, folder: &str) -> bool {
    let (tag, folder) = (tag.trim(), folder.trim());
    let mut sanitized = sanitize_folder_name(tag);
    if sanitized.ends_with('.') {
        sanitized.pop();
        sanitized.push('_');
    }
    sanitized == folder
        || sanitize_folder_name(tag).trim_end_matches('.') == folder.trim_end_matches('.')
}

/// Anzeigenamen der Künstler: Ordnername -> Tag-Name (nur wo er abweicht). Gewählt
/// wird der häufigste Künstler-/Albumkünstler-Tag der Titel, der dem Ordnernamen
/// bis auf ersetzte Sonderzeichen entspricht (z. B. `AC_DC` -> `AC/DC`).
pub fn artist_display_names(
    conn: &Connection,
) -> Result<std::collections::HashMap<String, String>> {
    let mut st = conn.prepare(
        "SELECT ar.name, COALESCE(NULLIF(t.album_artist_tag, ''), NULLIF(t.artist_tag, '')) AS tag,
                COUNT(*) AS c
         FROM artists ar
         JOIN albums al ON al.artist_id = ar.id
         JOIN tracks t ON t.album_id = al.id
         WHERE tag IS NOT NULL
         GROUP BY ar.name, tag ORDER BY c DESC",
    )?;
    let mut map = std::collections::HashMap::new();
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let (folder, tag): (String, String) = (r.get(0)?, r.get(1)?);
        if tag != folder && tag_matches_folder(&tag, &folder) {
            map.entry(folder).or_insert_with(|| tag.trim().to_string());
        }
    }
    Ok(map)
}

/// Treffer einer Suche (IDs der sichtbaren Künstler/Alben/Titel).
#[derive(Debug, Clone, Default)]
pub struct SearchHits {
    pub artists: std::collections::HashSet<i64>,
    pub albums: std::collections::HashSet<i64>,
    pub tracks: std::collections::HashSet<i64>,
}

/// Sucht `query` (ohne Groß-/Kleinschreibung) in Künstler, Album und Titel.
/// Passt der Künstler, sind alle seine Alben und Titel sichtbar; sonst nur
/// passende Alben/Titel (samt ihrem Album bzw. Künstler als Zugang).
/// Eigene Verbindung, läuft auf einem Worker-Thread. Bricht ab (None), sobald
/// `generation` nicht mehr `my_generation` ist (neuere Eingabe).
pub fn search(
    db_path: &Path,
    query: &str,
    generation: &std::sync::atomic::AtomicU64,
    my_generation: u64,
) -> Result<Option<SearchHits>> {
    use std::sync::atomic::Ordering::Relaxed;
    let q = query.to_lowercase();
    let hit = |s: &str| s.to_lowercase().contains(&q);

    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Datenbank öffnen: {}", db_path.display()))?;
    let mut st = conn.prepare(
        "SELECT ar.id, ar.name, al.id, al.title, t.id, t.title, t.artist_tag, t.album_tag
         FROM albums al
         JOIN artists ar ON ar.id = al.artist_id
         LEFT JOIN tracks t ON t.album_id = al.id",
    )?;
    let mut rows = st.query([])?;

    // Erst Treffer sammeln, Künstlertreffer danach auffüllen.
    let mut hits = SearchHits::default();
    let mut all: Vec<(i64, i64, Option<i64>)> = Vec::new(); // (artist, album, track)
    let mut artist_match = std::collections::HashSet::new();
    let mut n = 0u32;
    while let Some(r) = rows.next()? {
        n += 1;
        if n % 512 == 0 && generation.load(Relaxed) != my_generation {
            return Ok(None);
        }
        let (ar_id, ar_name): (i64, String) = (r.get(0)?, r.get(1)?);
        let (al_id, al_title): (i64, String) = (r.get(2)?, r.get(3)?);
        let t_id: Option<i64> = r.get(4)?;
        let t_title: Option<String> = r.get(5)?;
        let t_artist: Option<String> = r.get(6)?;
        let t_album: Option<String> = r.get(7)?;
        all.push((ar_id, al_id, t_id));

        if hit(&ar_name) {
            artist_match.insert(ar_id);
        }
        let album_hit = hit(&al_title) || t_album.as_deref().is_some_and(&hit);
        let track_hit =
            t_title.as_deref().is_some_and(&hit) || t_artist.as_deref().is_some_and(&hit);
        if album_hit {
            hits.albums.insert(al_id);
            hits.artists.insert(ar_id);
        }
        if let (true, Some(t)) = (track_hit || album_hit, t_id) {
            hits.tracks.insert(t);
            hits.albums.insert(al_id);
            hits.artists.insert(ar_id);
        }
    }
    for (ar, al, t) in all {
        if artist_match.contains(&ar) {
            hits.artists.insert(ar);
            hits.albums.insert(al);
            hits.tracks.extend(t);
        }
    }
    Ok(Some(hits))
}

/// Setzt den aktuellen Thread auf niedrige Priorität (Hintergrund).
pub fn lower_thread_priority() {
    #[cfg(unix)]
    unsafe extern "C" {
        fn setpriority(which: i32, who: u32, prio: i32) -> i32;
    }
    // macOS: PRIO_DARWIN_THREAD (3) + PRIO_DARWIN_BG (0x1000) betrifft nur diesen Thread.
    #[cfg(target_os = "macos")]
    unsafe {
        setpriority(3, 0, 0x1000);
    }
    // Linux: PRIO_PROCESS mit who = 0 wirkt auf den aufrufenden Thread.
    #[cfg(all(unix, not(target_os = "macos")))]
    unsafe {
        setpriority(0, 0, 19);
    }
    // Windows: THREAD_PRIORITY_LOWEST (-2) nur für den aufrufenden Thread.
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut core::ffi::c_void;
            fn SetThreadPriority(thread: *mut core::ffi::c_void, priority: i32) -> i32;
        }
        unsafe {
            SetThreadPriority(GetCurrentThread(), -2);
        }
    }
}

/// Bearbeitbare Metadaten einer Datei (als Text, wie im Editor-Dialog).
#[derive(Clone, Debug, Default)]
pub struct TagEdit {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub composer: String,
    pub genre: String,
    pub track_no: String,
    pub track_total: String,
    pub disc_no: String,
    pub year: String,
    pub compilation: bool,
}

fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn parse_num(label: &str, s: &str) -> Result<Option<u32>> {
    match s.trim() {
        "" => Ok(None),
        t => t
            .parse::<u32>()
            .map(Some)
            .map_err(|_| anyhow::anyhow!("{label}: „{t}“ ist keine Zahl")),
    }
}

/// Liest die Tags der Datei für den Editor.
pub fn read_tag_edit(path: &Path) -> Result<TagEdit> {
    let tagged = lofty::read_from_path(path)?;
    let mut e = TagEdit::default();
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let s = |v: Option<std::borrow::Cow<'_, str>>| v.map(|c| c.into_owned()).unwrap_or_default();
        let n = |v: Option<u32>| v.map(|n| n.to_string()).unwrap_or_default();
        e.title = s(tag.title());
        e.artist = s(tag.artist());
        e.album = s(tag.album());
        e.genre = s(tag.genre());
        e.album_artist = text_item(tag, &ItemKey::AlbumArtist).unwrap_or_default();
        e.composer = text_item(tag, &ItemKey::Composer).unwrap_or_default();
        e.track_no = n(tag.track());
        e.track_total = n(tag.track_total());
        e.disc_no = n(tag.disk());
        e.year = n(tag.year());
        e.compilation = tag
            .get(&ItemKey::FlagCompilation)
            .is_some_and(|item| match item.value() {
                ItemValue::Text(t) | ItemValue::Locator(t) => {
                    matches!(t.trim().to_lowercase().as_str(), "1" | "true" | "yes")
                }
                ItemValue::Binary(b) => b.first().is_some_and(|&x| x == 1 || x == b'1'),
            });
    }
    Ok(e)
}

impl TagEdit {
    /// Prüft die Zahlenfelder (vor dem Schreiben).
    pub fn validate(&self) -> Result<()> {
        parse_num("Track", &self.track_no)?;
        parse_num("Track gesamt", &self.track_total)?;
        parse_num("Disc", &self.disc_no)?;
        parse_num("Jahr", &self.year)?;
        Ok(())
    }
}

/// Schreibt die bearbeiteten Metadaten in die Datei; leere Felder entfernen
/// den jeweiligen Eintrag. Andere Tags (Cover usw.) bleiben erhalten.
pub fn write_tag_edit(path: &Path, e: &TagEdit) -> Result<()> {
    e.validate()?;
    let mut tagged = lofty::read_from_path(path)?;
    if tagged.primary_tag().is_none() && tagged.first_tag().is_none() {
        let tag_type = tagged.primary_tag_type();
        tagged.insert_tag(Tag::new(tag_type));
    }
    let tag = match tagged.primary_tag_mut() {
        Some(t) => t,
        None => tagged.first_tag_mut().context("kein Tag-Container")?,
    };

    match opt(&e.title) {
        Some(v) => tag.set_title(v),
        None => tag.remove_title(),
    }
    match opt(&e.artist) {
        Some(v) => tag.set_artist(v),
        None => tag.remove_artist(),
    }
    match opt(&e.album) {
        Some(v) => tag.set_album(v),
        None => tag.remove_album(),
    }
    match opt(&e.genre) {
        Some(v) => tag.set_genre(v),
        None => tag.remove_genre(),
    }
    for (key, val) in [
        (ItemKey::AlbumArtist, &e.album_artist),
        (ItemKey::Composer, &e.composer),
    ] {
        match opt(val) {
            Some(v) => {
                tag.insert_text(key, v);
            }
            None => tag.remove_key(&key),
        }
    }
    match parse_num("Track", &e.track_no)? {
        Some(v) => tag.set_track(v),
        None => tag.remove_track(),
    }
    match parse_num("Track gesamt", &e.track_total)? {
        Some(v) => tag.set_track_total(v),
        None => tag.remove_track_total(),
    }
    match parse_num("Disc", &e.disc_no)? {
        Some(v) => tag.set_disk(v),
        None => tag.remove_disk(),
    }
    match parse_num("Jahr", &e.year)? {
        Some(v) => tag.set_year(v),
        None => tag.remove_year(),
    }
    if e.compilation {
        tag.insert_text(ItemKey::FlagCompilation, "1".to_string());
    } else {
        tag.remove_key(&ItemKey::FlagCompilation);
    }

    tag.save_to_path(path, WriteOptions::default())?;
    Ok(())
}

impl Library {
    /// Übernimmt bearbeitete Tags in die Mediathek (ohne Neuscan).
    pub fn update_track_tags(&self, id: i64, path: &Path, e: &TagEdit) -> Result<()> {
        let title = opt(&e.title).unwrap_or_else(|| file_stem(path));
        self.conn.execute(
            "UPDATE tracks SET title = ?1, artist_tag = ?2, album_tag = ?3,
                    album_artist_tag = ?4, composer = ?5, track_no = ?6, track_total = ?7,
                    compilation = ?8, disc_no = ?9, year = ?10
             WHERE id = ?11",
            params![
                title,
                opt(&e.artist),
                opt(&e.album),
                opt(&e.album_artist),
                opt(&e.composer),
                parse_num("Track", &e.track_no)?,
                parse_num("Track gesamt", &e.track_total)?,
                e.compilation,
                parse_num("Disc", &e.disc_no)?.unwrap_or(1),
                parse_num("Jahr", &e.year)?,
                id
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod playlist_tests {
    use super::*;

    #[test]
    fn playlist_roundtrip_mit_fehlendem_pfad() {
        let dir = std::env::temp_dir().join(format!("fya-pl-test-{}", std::process::id()));
        let lib = Library::open(&dir.join("library.db")).unwrap();

        let id = lib.create_playlist("  Mix  ").unwrap();
        assert!(lib.create_playlist("mix").is_err(), "Name eindeutig (NOCASE)");
        assert!(lib.create_playlist("   ").is_err());

        let p = Path::new("/nicht/vorhanden/a.mp3");
        assert!(lib.add_to_playlist(id, p).unwrap());
        assert!(!lib.add_to_playlist(id, p).unwrap(), "Duplikat");

        let tracks = lib.playlist_tracks(id).unwrap();
        assert_eq!(tracks.len(), 1);
        assert!(tracks[0].missing);
        assert_eq!(tracks[0].title, "a");

        lib.remove_from_playlist(id, p).unwrap();
        assert!(lib.playlist_tracks(id).unwrap().is_empty());

        // Umsortieren + Umbenennen
        let (a, b) = (Path::new("/x/a.mp3"), Path::new("/x/b.mp3"));
        lib.add_to_playlist(id, a).unwrap();
        lib.add_to_playlist(id, b).unwrap();
        assert!(!lib.move_in_playlist(id, a, -1).unwrap(), "am Anfang");
        assert!(lib.move_in_playlist(id, a, 1).unwrap());
        let titles: Vec<_> = lib.playlist_tracks(id).unwrap().into_iter().map(|t| t.title).collect();
        assert_eq!(titles, ["b", "a"]);
        let other = lib.create_playlist("Andere").unwrap();
        assert!(lib.rename_playlist(id, "andere").is_err(), "Name vergeben");
        lib.rename_playlist(id, "Mix").unwrap(); // eigener Name ist erlaubt
        lib.rename_playlist(id, "Neu").unwrap();
        assert!(lib.playlists().unwrap().iter().any(|p| p.name == "Neu"));
        lib.delete_playlist(other).unwrap();
        lib.delete_playlist(id).unwrap();
        assert!(lib.playlists().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod display_title_tests {
    use super::*;

    #[test]
    fn tag_ersetzt_nur_bei_sanitisierten_sonderzeichen() {
        let t = |f: &str, tag: Option<&str>| display_title(f.into(), tag.map(Into::into));
        assert_eq!(
            t("Secret History_ Best of the Divine Comedy", Some("Secret History: Best of the Divine Comedy")),
            "Secret History: Best of the Divine Comedy"
        );
        assert_eq!(t("Who_s Next_", Some("Who's Next?")), "Who_s Next_"); // ' wird nicht ersetzt -> kein Treffer
        assert_eq!(t("What_", Some("What?")), "What?");
        // Abweichender Tag bleibt unberücksichtigt
        assert_eq!(t("Greatest Hits", Some("Something Else")), "Greatest Hits");
        assert_eq!(t("Album", None), "Album");
    }
}

#[cfg(test)]
mod setting_tests {
    use super::*;

    #[test]
    fn setting_roundtrip() {
        let dir = std::env::temp_dir().join(format!("fya-set-test-{}", std::process::id()));
        let lib = Library::open(&dir.join("library.db")).unwrap();
        assert_eq!(lib.setting("theme"), None);
        lib.set_setting("theme", "Dracula").unwrap();
        lib.set_setting("theme", "Nord").unwrap();
        assert_eq!(lib.setting("theme").as_deref(), Some("Nord"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
