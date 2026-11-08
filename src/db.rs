//! SQLite-Mediendatenbank: Schema, Scan der Ordnerstruktur, Abfragen.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::{Path, PathBuf};

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag, TagExt};

/// Dateiendungen, die als Song erkannt werden (`m4a` = AAC im MP4-Container). Mit dem
/// Cargo-Feature `ogg` zusätzlich Ogg (Vorbis, FLAC, Opus) und natives FLAC.
pub const AUDIO_EXTENSIONS: &[&str] = if cfg!(feature = "ogg") {
    &["mp3", "aac", "m4a", "opus", "ogg", "oga", "flac"]
} else {
    &["mp3", "aac", "m4a", "opus"]
};

#[derive(Clone, Debug)]
pub struct Artist {
    pub id: i64,
    /// Anzeigename (Tag, siehe `artist_display_names`)
    pub name: String,
    /// Ordnername (Schlüssel)
    pub folder: String,
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
    pub album_id: i64,
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
    duration_secs  INTEGER NOT NULL DEFAULT 0,
    bpm            INTEGER  -- aus dem BPM-Tag (gerundet), NULL = kein Tag
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
fn migrate(conn: &Connection) -> Result<()> {
    // tracks.bpm: neu. Bestehende Zeilen bleiben NULL, bis die Mediathek neu eingelesen wird
    // (die Tags stehen nur in den Dateien).
    add_column(conn, "tracks", "bpm", "INTEGER")?;
    Ok(())
}

/// Fügt die Spalte hinzu, falls sie fehlt. Rückgabe: wurde sie neu angelegt?
fn add_column(conn: &Connection, table: &str, column: &str, def: &str) -> Result<bool> {
    let exists = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|c| c == column);
    if exists {
        return Ok(false);
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {def}"),
        [],
    )?;
    Ok(true)
}

const TRACK_SELECT: &str = "
SELECT t.id, t.path, t.title, t.track_no, t.duration_secs, al.title, ar.name,
       t.artist_tag, t.album_tag, t.compilation, t.album_id
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
                                        compilation, disc_no, year, duration_secs, bpm)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                        meta.bpm,
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
        // Kurze Wartezeit: Hält ein Hintergrund-Thread (MusicBrainz, Scan) die Schreibsperre,
        // soll die Oberfläche nicht lange hängen, sondern schnell eine Meldung zeigen.
        conn.busy_timeout(std::time::Duration::from_millis(750))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Library { conn })
    }

    /// Einfache Einstellung (Textwert) aus der Tabelle `settings`.
    pub fn setting(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT text FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
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
                folder: String::new(),
            })
        })?;
        let mut artists: Vec<Artist> = rows.collect::<rusqlite::Result<_>>()?;
        // Anzeigename aus den Tags (Ordnernamen kennen `/ : ?` nicht)
        let display = artist_display_names(&self.conn)?;
        for a in &mut artists {
            a.folder = a.name.clone();
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
                    t.artist_tag, t.album_tag, t.compilation, t.album_id
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
                    artist_tag: r
                        .get::<_, Option<String>>(7)?
                        .filter(|s| !s.trim().is_empty()),
                    album_tag: r
                        .get::<_, Option<String>>(8)?
                        .filter(|s| !s.trim().is_empty()),
                    compilation: r.get(9)?,
                    album_id: r.get::<_, Option<i64>>(10)?.unwrap_or(0),
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
                    album_id: 0,
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
        self.conn.execute(
            "UPDATE playlists SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
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
        let upd =
            "UPDATE playlist_tracks SET position = ?1 WHERE playlist_id = ?2 AND position = ?3";
        tx.execute(upd, params![-1, id, pos_a])?;
        tx.execute(upd, params![pos_a, id, pos_b])?;
        tx.execute(upd, params![pos_b, id, -1])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn delete_playlist(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM playlist_tracks WHERE playlist_id = ?1", [id])?;
        self.conn
            .execute("DELETE FROM playlists WHERE id = ?1", [id])?;
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

    /// Titel des Künstlers auf Samplern (Ordner „Compilations“): Künstler-Tag
    /// entspricht Anzeigenamen oder Ordnername (ohne Groß-/Kleinschreibung).
    pub fn compilation_tracks_for(&self, display: &str, folder: &str) -> Result<Vec<Track>> {
        self.query_tracks_with(
            &format!(
                "{TRACK_SELECT} WHERE ar.name = 'Compilations'
                   AND (t.artist_tag = ?1 COLLATE NOCASE OR t.artist_tag = ?2 COLLATE NOCASE)
                 ORDER BY COALESCE(al.year, 9999), al.title COLLATE NOCASE,
                          t.disc_no, COALESCE(t.track_no, 9999), t.title COLLATE NOCASE"
            ),
            params![display, folder],
        )
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
        self.query_tracks_with(sql, [id])
    }

    fn query_tracks_with(&self, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Track>> {
        let mut st = self.conn.prepare(sql)?;
        let rows = st.query_map(p, |r| {
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
                album_id: r.get(10)?,
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
    bpm: Option<u32>,
    duration_secs: u32,
}

/// Liest Tags (ID3v2, ID3v1, MP4-Atome, Vorbis-Comments/OpusTags) und Dauer.
fn read_meta(path: &Path) -> Meta {
    let tagged = match open_tagged(path) {
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
        // Tags getrimmt lesen (ID3v1 füllt seine festen Felder oft mit Leerzeichen auf)
        meta.title = clean_text(tag.title());
        meta.artist = clean_text(tag.artist());
        meta.album = clean_text(tag.album());
        meta.album_artist = text_item(tag, ItemKey::AlbumArtist);
        meta.composer = text_item(tag, ItemKey::Composer);
        meta.track_no = tag.track();
        meta.track_total = tag.track_total();
        meta.compilation =
            tag.get(ItemKey::FlagCompilation)
                .is_some_and(|item| match item.value() {
                    ItemValue::Text(t) | ItemValue::Locator(t) => {
                        matches!(t.trim().to_lowercase().as_str(), "1" | "true" | "yes")
                    }
                    ItemValue::Binary(b) => b.first().is_some_and(|&x| x == 1 || x == b'1'),
                });
        meta.disc_no = tag.disk();
        meta.year = tag.date().map(|d| u32::from(d.year));
        // Dezimal-BPM zuerst, sonst die ganzzahlige; als ganze Zahl (gerundet) gespeichert
        meta.bpm = tag
            .get_string(ItemKey::Bpm)
            .or_else(|| tag.get_string(ItemKey::IntegerBpm))
            .and_then(|b| parse_bpm(b).ok().flatten())
            .map(|v| v.round() as u32);
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
    let mut tagged = open_tagged(path)?;
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
    let tagged = open_tagged(path).ok()?;
    let pictures: Vec<_> = tagged.tags().iter().flat_map(|t| t.pictures()).collect();
    pictures
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first())
        .map(|p| p.data().to_vec())
}

/// Öffnet eine Datei zum Lesen/Schreiben der Tags. Anders als `lofty::read_from_path`, das
/// nur nach der Endung entscheidet (`.ogg` wäre immer Vorbis, `.oga` unbekannt), wird der
/// Dateityp zuerst am Inhalt erkannt; die Endung dient nur als Rückfall. So lassen sich auch
/// Opus-Dateien mit der Endung `.ogg` lesen. (Ogg-FLAC kann `lofty` nicht – siehe README.)
fn open_tagged(path: &Path) -> Result<lofty::file::TaggedFile> {
    Ok(lofty::probe::Probe::open(path)?.guess_file_type()?.read()?)
}

fn text_item(tag: &lofty::tag::Tag, key: ItemKey) -> Option<String> {
    clean_text(tag.get_string(key))
}

/// Tag-Text ohne Leerzeichen/Nullbytes an den Rändern; leer -> None. ID3v1 hat feste
/// Feldbreiten (30 Zeichen), die ältere Programme mit Leerzeichen auffüllen; `lofty`
/// entfernt davon nur Nullbytes.
fn clean_text<S: AsRef<str>>(value: Option<S>) -> Option<String> {
    value
        .map(|s| {
            s.as_ref()
                .trim_matches(|c: char| c.is_whitespace() || c == '\0')
                .to_string()
        })
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
pub fn sanitize_folder_name(s: &str) -> String {
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
    // Titel in „Compilations“: (Compilations-Künstler-ID, Album, Titel, Künstler-Tag)
    let mut comp_rows: Vec<(i64, i64, i64, String)> = Vec::new();
    let mut artist_names: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
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
        artist_names.entry(ar_id).or_insert_with(|| ar_name.clone());
        if ar_name == "Compilations" {
            if let (Some(t), Some(tag)) = (t_id, t_artist.as_ref()) {
                comp_rows.push((ar_id, al_id, t, tag.clone()));
            }
        }

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

    // Sampler: Titel eines Künstlers (über den Künstler-Tag) gehören zu diesem Künstler.
    // Passt der Künstler oder der Titel selbst, ist der Titel samt Album sichtbar, und der
    // Künstler erscheint in der Künstlerliste.
    let find_artist = |tag: &str| find_artist_for_tag(&artist_names, tag);
    for (comp_id, al, t, tag) in comp_rows {
        let Some(artist) = find_artist(&tag) else {
            continue;
        };
        if artist_match.contains(&artist) || hits.tracks.contains(&t) {
            hits.tracks.insert(t);
            hits.albums.insert(al);
            hits.artists.insert(artist);
            hits.artists.insert(comp_id);
        }
    }
    Ok(Some(hits))
}

/// Künstler (ID) zu einem Künstler-Tag: Name gleich (ohne Groß-/Kleinschreibung) oder bis auf
/// ersetzte Sonderzeichen gleich dem Ordnernamen. „Compilations“ selbst zählt nicht.
fn find_artist_for_tag(
    artists: &std::collections::HashMap<i64, String>,
    tag: &str,
) -> Option<i64> {
    artists
        .iter()
        .filter(|(_, name)| name.as_str() != "Compilations")
        .find(|(_, name)| tag.eq_ignore_ascii_case(name) || tag_matches_folder(tag, name))
        .map(|(id, _)| *id)
}

/// BPM-Suche im Suchfeld: beginnt die Eingabe mit `:`, wird nach BPM gefiltert.
/// `:X` (ganze Zahl, höchstens dreistellig) ergibt `X-5 … X+5`, `:X-Y` (zwei solche
/// Zahlen) `X … Y` (bei vertauschter Reihenfolge sortiert). Alles andere (kein Doppelpunkt,
/// Buchstaben, mehr als drei Stellen, mehr Teile) ergibt `None`.
pub fn parse_bpm_query(query: &str) -> Option<(u32, u32)> {
    let rest = query.trim().strip_prefix(':')?;
    let num = |s: &str| -> Option<u32> {
        (!s.is_empty() && s.len() <= 3 && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    match rest.split_once('-') {
        None => {
            let x = num(rest)?;
            Some((x.saturating_sub(5), x + 5))
        }
        Some((a, b)) => {
            let (x, y) = (num(a)?, num(b)?);
            Some((x.min(y), x.max(y)))
        }
    }
}

/// Titel mit `bpm BETWEEN lo AND hi` samt Album und Künstler. Es erscheinen nur die
/// passenden Titel (nicht das ganze Album). Titel im Ordner „Compilations“ gehören über
/// den Künstler-Tag zu ihrem Künstler (wie bei [`search`]). Eigene Verbindung, läuft auf
/// einem Worker-Thread.
pub fn search_bpm(db_path: &Path, lo: u32, hi: u32) -> Result<SearchHits> {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Datenbank öffnen: {}", db_path.display()))?;
    let artist_names: std::collections::HashMap<i64, String> = conn
        .prepare("SELECT id, name FROM artists")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut hits = SearchHits::default();
    let mut st = conn.prepare(
        "SELECT ar.id, ar.name, al.id, t.id, t.artist_tag
         FROM tracks t
         JOIN albums al ON al.id = t.album_id
         JOIN artists ar ON ar.id = al.artist_id
         WHERE t.bpm BETWEEN ?1 AND ?2",
    )?;
    let mut rows = st.query(params![lo, hi])?;
    while let Some(r) = rows.next()? {
        let (ar_id, ar_name): (i64, String) = (r.get(0)?, r.get(1)?);
        let (al_id, t_id): (i64, i64) = (r.get(2)?, r.get(3)?);
        let tag: Option<String> = r.get(4)?;
        hits.tracks.insert(t_id);
        hits.albums.insert(al_id);
        hits.artists.insert(ar_id);
        if ar_name == "Compilations" {
            if let Some(artist) = tag.and_then(|t| find_artist_for_tag(&artist_names, &t)) {
                hits.artists.insert(artist);
            }
        }
    }
    Ok(hits)
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
    /// BPM als Text (Zahl, ggf. mit Nachkommastellen)
    pub bpm: String,
    pub compilation: bool,
}

fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// BPM aus dem Eingabefeld: leer = keiner, sonst Zahl > 0 und ≤ 999 (Komma oder Punkt).
fn parse_bpm(s: &str) -> Result<Option<f32>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let v: f32 = t
        .replace(',', ".")
        .parse()
        .map_err(|_| anyhow::anyhow!("BPM: „{t}“ ist keine Zahl"))?;
    if !v.is_finite() || v <= 0.0 || v > 999.0 {
        bail!("BPM: „{t}“ ist ungültig (größer 0, höchstens 999)");
    }
    Ok(Some(v))
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
    let tagged = open_tagged(path)?;
    let mut e = TagEdit::default();
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let s = |v: Option<std::borrow::Cow<'_, str>>| clean_text(v).unwrap_or_default();
        let n = |v: Option<u32>| v.map(|n| n.to_string()).unwrap_or_default();
        e.title = s(tag.title());
        e.artist = s(tag.artist());
        e.album = s(tag.album());
        e.genre = s(tag.genre());
        e.album_artist = text_item(tag, ItemKey::AlbumArtist).unwrap_or_default();
        e.composer = text_item(tag, ItemKey::Composer).unwrap_or_default();
        e.track_no = n(tag.track());
        e.track_total = n(tag.track_total());
        e.disc_no = n(tag.disk());
        e.year = n(tag.date().map(|d| u32::from(d.year)));
        // Formate mit Dezimal-BPM (Vorbis/Opus, MP4-Freiform) zuerst, sonst die ganzzahlige
        // (ID3 `TBPM`, MP4 `tmpo`)
        e.bpm = tag
            .get_string(ItemKey::Bpm)
            .or_else(|| tag.get_string(ItemKey::IntegerBpm))
            .map(|b| b.trim().to_string())
            .unwrap_or_default();
        e.compilation = tag
            .get(ItemKey::FlagCompilation)
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
        if let Some(y) = parse_num("Jahr", &self.year)? {
            if !(1..=9999).contains(&y) {
                bail!("Jahr: „{y}“ ist ungültig (1–9999)");
            }
        }
        parse_bpm(&self.bpm)?;
        Ok(())
    }
}

/// Schreibt die bearbeiteten Metadaten in die Datei; leere Felder entfernen
/// den jeweiligen Eintrag. Andere Tags (Cover usw.) bleiben erhalten.
pub fn write_tag_edit(path: &Path, e: &TagEdit) -> Result<()> {
    e.validate()?;
    let mut tagged = open_tagged(path)?;
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
            None => tag.remove_key(key),
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
        Some(v) => {
            // Nur das Jahr ersetzen; Monat/Tag eines vorhandenen Datums bleiben erhalten
            let year = u16::try_from(v).ok().filter(|y| (1..=9999).contains(y));
            let Some(year) = year else {
                bail!("Jahr: „{v}“ ist ungültig (1–9999)");
            };
            let mut date = tag.date().unwrap_or_default();
            date.year = year;
            tag.set_date(date);
        }
        None => tag.remove_date(),
    }
    // BPM: Dezimalwert dort, wo das Format ihn kennt, und der gerundete Ganzzahlwert dort,
    // wo es nur ganze BPM gibt (ID3 `TBPM`, MP4 `tmpo`); nicht passende Felder bleiben aus
    match parse_bpm(&e.bpm)? {
        Some(v) => {
            tag.insert_text(ItemKey::Bpm, v.to_string());
            tag.insert_text(ItemKey::IntegerBpm, (v.round() as u32).to_string());
        }
        None => {
            tag.remove_key(ItemKey::Bpm);
            tag.remove_key(ItemKey::IntegerBpm);
        }
    }
    if e.compilation {
        tag.insert_text(ItemKey::FlagCompilation, "1".to_string());
    } else {
        tag.remove_key(ItemKey::FlagCompilation);
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
                    compilation = ?8, disc_no = ?9, year = ?10, bpm = ?11
             WHERE id = ?12",
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
                parse_bpm(&e.bpm)?.map(|v| v.round() as i64),
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
        assert!(
            lib.create_playlist("mix").is_err(),
            "Name eindeutig (NOCASE)"
        );
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
        let titles: Vec<_> = lib
            .playlist_tracks(id)
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect();
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
            t(
                "Secret History_ Best of the Divine Comedy",
                Some("Secret History: Best of the Divine Comedy")
            ),
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

#[cfg(test)]
mod compilation_tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// Bibliothek mit „The Prodigy“ (1 Album, 1 Titel) und einem Sampler, auf dem ein
    /// Prodigy-Titel und ein Titel eines anderen Künstlers liegen.
    fn fixture(name: &str) -> (Library, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("fya-comp-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lib = Library::open(&dir.join("library.db")).unwrap();
        let c = &lib.conn;
        c.execute_batch(
            "INSERT INTO artists(id, name, sort_name, path) VALUES (1,'The Prodigy','prodigy','/p'), (2,'Compilations','compilations','/c'), (3,'Other','other','/o');
             INSERT INTO albums(id, artist_id, title, path) VALUES (10,1,'Fat','/p/fat'), (20,2,'Now 1','/c/now1'), (21,2,'Sampler Two','/c/two'), (30,3,'Else','/o/else');
             INSERT INTO tracks(id, album_id, path, format, title, artist_tag, album_tag) VALUES
               (100,10,'/p/fat/a.mp3','mp3','Firestarter','The Prodigy','Fat'),
               (200,20,'/c/now1/a.mp3','mp3','Breathe','the prodigy','Now 1'),
               (201,20,'/c/now1/b.mp3','mp3','Other Song','Someone Else','Now 1'),
               (210,21,'/c/two/a.mp3','mp3','Voodoo People','The Prodigy','Sampler Two'),
               (300,30,'/o/else/a.mp3','mp3','Zzz','Other','Else');",
        )
        .unwrap();
        (lib, dir)
    }

    #[test]
    fn sampler_titel_des_kuenstlers() {
        let (lib, dir) = fixture("tracks");
        let t = lib
            .compilation_tracks_for("The Prodigy", "The Prodigy")
            .unwrap();
        let titles: Vec<_> = t.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["Breathe", "Voodoo People"]); // nur Titel dieses Künstlers
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn suche_beruecksichtigt_sampler() {
        let (lib, dir) = fixture("search");
        let gen_ = AtomicU64::new(0);
        let db = dir.join("library.db");

        // Künstlertreffer: eigene Titel + Sampler-Titel des Künstlers, nicht der andere Titel
        let h = search(&db, "prod", &gen_, 0).unwrap().unwrap();
        assert!(h.artists.contains(&1));
        for t in [100, 200, 210] {
            assert!(h.tracks.contains(&t), "Titel {t} fehlt");
        }
        assert!(!h.tracks.contains(&201) && !h.tracks.contains(&300));

        // Treffer auf Sampler-Album: Künstler auf dem Sampler erscheint mit seinem Titel
        let h = search(&db, "sampler two", &gen_, 0).unwrap().unwrap();
        assert!(h.artists.contains(&1) && h.tracks.contains(&210));
        assert!(!h.tracks.contains(&100));
        drop(lib);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod busy_tests {
    use super::*;
    use std::time::Instant;

    /// Hält ein anderer Schreiber die Sperre, scheitert die UI-Verbindung schnell
    /// (statt Sekunden zu blockieren) und der Fehler ist als „belegt“ erkennbar.
    #[test]
    fn gesperrte_db_scheitert_schnell() {
        let dir = std::env::temp_dir().join(format!("fya-busy-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("library.db");
        let lib = Library::open(&path).unwrap();
        let pl = lib.create_playlist("X").unwrap();

        let other = Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        let start = Instant::now();
        let err = lib.add_to_playlist(pl, Path::new("/x.mp3")).unwrap_err();
        assert!(
            start.elapsed().as_secs_f32() < 2.0,
            "blockiert zu lange: {:?}",
            start.elapsed()
        );
        assert!(err.chain().any(|c| matches!(
            c.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(f, _)) if f.code == rusqlite::ErrorCode::DatabaseBusy
        )));
        other.execute_batch("COMMIT").unwrap();
        // danach geht es wieder
        assert!(lib.add_to_playlist(pl, Path::new("/x.mp3")).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod year_tests {
    use super::*;

    /// Jahr lesen/schreiben/entfernen mit lofty 0.25 (`date()` statt `year()`), für
    /// MP3, Opus und M4A; ein vorhandenes Datum behält Monat und Tag. Braucht ffmpeg
    /// (sonst übersprungen).
    #[test]
    fn jahr_lesen_schreiben_entfernen() {
        let Some(ffmpeg) = crate::mixtape::find_ffmpeg() else {
            eprintln!("ffmpeg nicht installiert – übersprungen");
            return;
        };
        let dir = std::env::temp_dir().join(format!("fya-year-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        for (ext, codec) in [("mp3", "libmp3lame"), ("opus", "libopus"), ("m4a", "aac")] {
            let file = dir.join(format!("t.{ext}"));
            let ok = std::process::Command::new(&ffmpeg)
                .args([
                    "-nostdin",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "anullsrc=r=48000:cl=stereo",
                    "-t",
                    "1",
                ])
                .args([
                    "-metadata",
                    "date=2001-05-06",
                    "-metadata",
                    "title=T",
                    "-c:a",
                    codec,
                ])
                .arg(&file)
                .status()
                .unwrap()
                .success();
            assert!(ok, "{ext}: ffmpeg");

            // lesen
            assert_eq!(read_meta(&file).year, Some(2001), "{ext}: Jahr lesen");
            let mut e = read_tag_edit(&file).unwrap();
            assert_eq!(e.year, "2001", "{ext}");

            // ändern: nur das Jahr
            e.year = "1999".into();
            write_tag_edit(&file, &e).unwrap();
            assert_eq!(
                read_tag_edit(&file).unwrap().year,
                "1999",
                "{ext}: Jahr geändert"
            );
            assert_eq!(read_meta(&file).year, Some(1999), "{ext}");
            // Monat/Tag des ursprünglichen Datums bleiben erhalten
            {
                use lofty::file::TaggedFileExt;
                let tagged = lofty::read_from_path(&file).unwrap();
                let tag = tagged.primary_tag().or_else(|| tagged.first_tag()).unwrap();
                let date = tag.date().unwrap();
                assert_eq!(
                    (date.year, date.month, date.day),
                    (1999, Some(5), Some(6)),
                    "{ext}"
                );
            }

            // ungültig: nichts wird geschrieben
            for bad in ["0", "10000", "abc"] {
                e.year = bad.into();
                assert!(
                    write_tag_edit(&file, &e).is_err(),
                    "{ext}: {bad} muss scheitern"
                );
            }
            assert_eq!(read_tag_edit(&file).unwrap().year, "1999", "{ext}");

            // entfernen
            e.year = String::new();
            write_tag_edit(&file, &e).unwrap();
            assert_eq!(read_meta(&file).year, None, "{ext}: Jahr entfernt");
            assert_eq!(
                read_tag_edit(&file).unwrap().title,
                "T",
                "{ext}: andere Tags bleiben"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod bpm_tag_tests {
    use super::*;

    /// BPM-Tag im Editor: lesen, schreiben (ganzzahlig und dezimal, je nach Format),
    /// ungültige Eingaben, entfernen. Braucht ffmpeg (sonst übersprungen).
    #[test]
    fn bpm_lesen_schreiben_entfernen() {
        let Some(ffmpeg) = crate::mixtape::find_ffmpeg() else {
            eprintln!("ffmpeg nicht installiert – übersprungen");
            return;
        };
        let dir = std::env::temp_dir().join(format!("fya-bpmtag-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // (Endung, Codec, Dezimal-BPM bleibt erhalten?)
        for (ext, codec, decimal) in [("mp3", "libmp3lame", false), ("opus", "libopus", true), ("m4a", "aac", true)] {
            let file = dir.join(format!("t.{ext}"));
            let ok = std::process::Command::new(&ffmpeg)
                .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-t", "1"])
                .args(["-metadata", "title=T", "-c:a", codec])
                .arg(&file)
                .status()
                .unwrap()
                .success();
            assert!(ok, "{ext}: ffmpeg");
            let mut e = read_tag_edit(&file).unwrap();
            assert_eq!(e.bpm, "", "{ext}: anfangs kein BPM");

            // ganzzahlig
            e.bpm = "128".into();
            write_tag_edit(&file, &e).unwrap();
            assert_eq!(read_tag_edit(&file).unwrap().bpm, "128", "{ext}: ganzzahlig");

            // dezimal (Komma erlaubt): ID3 kennt nur ganze BPM -> gerundet
            e.bpm = "127,6".into();
            write_tag_edit(&file, &e).unwrap();
            let expect = if decimal { "127.6" } else { "128" };
            assert_eq!(read_tag_edit(&file).unwrap().bpm, expect, "{ext}: dezimal");

            // ungültig: nichts wird geschrieben
            for bad in ["abc", "0", "-5", "1000", "NaN"] {
                e.bpm = bad.into();
                assert!(write_tag_edit(&file, &e).is_err(), "{ext}: „{bad}“ muss scheitern");
                assert!(e.validate().is_err());
            }
            assert_eq!(read_tag_edit(&file).unwrap().bpm, expect, "{ext}: unverändert");

            // entfernen; andere Tags bleiben
            e.bpm = String::new();
            write_tag_edit(&file, &e).unwrap();
            let back = read_tag_edit(&file).unwrap();
            assert_eq!(back.bpm, "", "{ext}: entfernt");
            assert_eq!(back.title, "T", "{ext}: andere Tags bleiben");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod bpm_db_tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fya-bpmdb-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn suchsyntax_fuer_bpm() {
        // :X -> X-5 … X+5
        assert_eq!(parse_bpm_query(":120"), Some((115, 125)));
        assert_eq!(parse_bpm_query("  :120  "), Some((115, 125)));
        assert_eq!(parse_bpm_query(":3"), Some((0, 8))); // nie negativ
        assert_eq!(parse_bpm_query(":999"), Some((994, 1004)));
        // :X-Y -> X … Y (sortiert)
        assert_eq!(parse_bpm_query(":120-130"), Some((120, 130)));
        assert_eq!(parse_bpm_query(":130-120"), Some((120, 130)));
        assert_eq!(parse_bpm_query(":7-9"), Some((7, 9)));
        // nicht interpretiert: nur ganze, höchstens dreistellige Zahlen
        for bad in [
            "", ":", ":-", ":1000", ":1234", ":12a", ":abc", ":12.5", ":12,5", ":-5", ":5-", ":1-2-3",
            ":12 - 14", ": 12", "120", "a:120", ":１２０",
        ] {
            assert_eq!(parse_bpm_query(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn bpm_suche_zeigt_nur_passende_titel_auch_aus_samplern() {
        let dir = temp("search");
        let lib = Library::open(&dir.join("library.db")).unwrap();
        lib.conn
            .execute_batch(
                "INSERT INTO artists(id, name, sort_name, path) VALUES (1,'The Prodigy','prodigy','/p'), (2,'Compilations','compilations','/c'), (3,'Other','other','/o');
                 INSERT INTO albums(id, artist_id, title, path) VALUES (10,1,'Fat','/p/fat'), (20,2,'Now 1','/c/now1'), (30,3,'Else','/o/else');
                 INSERT INTO tracks(id, album_id, path, format, title, artist_tag, album_tag, bpm) VALUES
                   (100,10,'/p/fat/a.mp3','mp3','Firestarter','The Prodigy','Fat',128),
                   (101,10,'/p/fat/b.mp3','mp3','Slow','The Prodigy','Fat',90),
                   (102,10,'/p/fat/c.mp3','mp3','Ohne Tag','The Prodigy','Fat',NULL),
                   (200,20,'/c/now1/a.mp3','mp3','Breathe','the prodigy','Now 1',130),
                   (201,20,'/c/now1/b.mp3','mp3','Other Song','Someone Else','Now 1',129),
                   (300,30,'/o/else/a.mp3','mp3','Zzz','Other','Else',140);",
            )
            .unwrap();
        let db = dir.join("library.db");

        // :128 -> 123…133
        let h = search_bpm(&db, 123, 133).unwrap();
        let mut tracks: Vec<_> = h.tracks.iter().copied().collect();
        tracks.sort();
        assert_eq!(tracks, [100, 200, 201], "nur passende Titel, nicht ganze Alben");
        assert!(h.albums.contains(&10) && h.albums.contains(&20) && !h.albums.contains(&30));
        // Künstler: Prodigy (eigener Titel + Sampler-Titel per Tag), Compilations (Album)
        assert!(h.artists.contains(&1) && h.artists.contains(&2) && !h.artists.contains(&3));

        // Bereichsgrenzen sind inklusiv (BETWEEN)
        let h = search_bpm(&db, 128, 129).unwrap();
        assert!(h.tracks.contains(&100) && h.tracks.contains(&201) && !h.tracks.contains(&200));
        // nichts passt
        assert!(search_bpm(&db, 200, 210).unwrap().tracks.is_empty());
        // Titel ohne BPM nie
        assert!(!search_bpm(&db, 0, 999).unwrap().tracks.contains(&102));
        drop(lib);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Alte Datenbank ohne `tracks.bpm`: `Library::open` legt die Spalte an, vorhandene
    /// Zeilen bleiben NULL und die Daten bleiben erhalten.
    #[test]
    fn migration_legt_bpm_spalte_an() {
        let dir = temp("migrate");
        let path = dir.join("library.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT NOT NULL, sort_name TEXT NOT NULL DEFAULT '', mbid TEXT, path TEXT NOT NULL UNIQUE);
                 CREATE TABLE albums (id INTEGER PRIMARY KEY, artist_id INTEGER NOT NULL, title TEXT NOT NULL, year INTEGER, mbid TEXT, path TEXT NOT NULL UNIQUE);
                 CREATE TABLE tracks (id INTEGER PRIMARY KEY, album_id INTEGER NOT NULL, path TEXT NOT NULL UNIQUE, format TEXT NOT NULL, title TEXT NOT NULL, artist_tag TEXT, album_tag TEXT, track_no INTEGER, track_total INTEGER, album_artist_tag TEXT, composer TEXT, compilation INTEGER NOT NULL DEFAULT 0, disc_no INTEGER NOT NULL DEFAULT 1, year INTEGER, duration_secs INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO artists(id,name,path) VALUES (1,'A','/a');
                 INSERT INTO albums(id,artist_id,title,path) VALUES (1,1,'Alb','/a/alb');
                 INSERT INTO tracks(id,album_id,path,format,title) VALUES (1,1,'/a/alb/x.mp3','mp3','X');",
            )
            .unwrap();
        }
        let lib = Library::open(&path).unwrap();
        let bpm: Option<i64> = lib
            .conn
            .query_row("SELECT bpm FROM tracks WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bpm, None);
        // zweites Öffnen ist unproblematisch (Spalte existiert schon)
        drop(lib);
        let lib = Library::open(&path).unwrap();
        assert_eq!(lib.tracks_by_artist(1).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Scan liest den BPM-Tag (MP3 `TBPM` ganzzahlig, Opus `BPM` dezimal -> gerundet) in die
    /// Datenbank; der Tag-Editor schreibt Änderungen in die Datenbank. Braucht ffmpeg.
    #[test]
    fn scan_und_tag_editor_uebernehmen_bpm() {
        let Some(ffmpeg) = crate::mixtape::find_ffmpeg() else {
            eprintln!("ffmpeg nicht installiert – übersprungen");
            return;
        };
        let dir = temp("scan");
        let root = dir.join("musik");
        let album = root.join("Künstler").join("Album");
        std::fs::create_dir_all(&album).unwrap();
        for (name, codec, tag) in [
            ("a.mp3", "libmp3lame", "TBPM=128"),
            ("b.opus", "libopus", "BPM=127.6"),
            ("c.mp3", "libmp3lame", "title=OhneBpm"),
        ] {
            let ok = std::process::Command::new(&ffmpeg)
                .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-t", "1"])
                .args(["-metadata", tag, "-c:a", codec])
                .arg(album.join(name))
                .status()
                .unwrap()
                .success();
            assert!(ok, "{name}: ffmpeg");
        }
        let db = dir.join("library.db");
        Library::open(&db).unwrap();
        assert_eq!(rebuild_from(&db, &root).unwrap(), 3);

        let lib = Library::open(&db).unwrap();
        let bpm_of = |file: &str| -> Option<i64> {
            lib.conn
                .query_row(
                    "SELECT bpm FROM tracks WHERE path LIKE ?1",
                    [format!("%{file}")],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(bpm_of("a.mp3"), Some(128));
        assert_eq!(bpm_of("b.opus"), Some(128), "127.6 gerundet");
        assert_eq!(bpm_of("c.mp3"), None);
        assert_eq!(search_bpm(&db, 123, 133).unwrap().tracks.len(), 2);

        // Tag-Editor: Änderung landet auch in der Datenbank
        let (id, path): (i64, String) = lib
            .conn
            .query_row("SELECT id, path FROM tracks WHERE path LIKE '%c.mp3'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let mut e = read_tag_edit(Path::new(&path)).unwrap();
        e.bpm = "99,6".into();
        lib.update_track_tags(id, Path::new(&path), &e).unwrap();
        assert_eq!(bpm_of("c.mp3"), Some(100));
        e.bpm = String::new();
        lib.update_track_tags(id, Path::new(&path), &e).unwrap();
        assert_eq!(bpm_of("c.mp3"), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod trim_tests {
    use super::*;

    #[test]
    fn clean_text_schneidet_leerzeichen_und_nullbytes() {
        assert_eq!(clean_text(Some("Try Again                     ")).as_deref(), Some("Try Again"));
        assert_eq!(clean_text(Some("  Alb um\0\0 ")).as_deref(), Some("Alb um"));
        assert_eq!(clean_text(Some("a  b")).as_deref(), Some("a  b")); // innen unverändert
        assert_eq!(clean_text(Some("      ")), None);
        assert_eq!(clean_text(Some("")), None);
        assert_eq!(clean_text(None::<&str>), None);
    }

    /// MP3 mit reinem ID3v1-Tag, dessen Felder mit Leerzeichen aufgefüllt sind (wie
    /// „Try Again.mp3“): Scan und Tag-Editor liefern getrimmte Werte. Braucht ffmpeg.
    #[test]
    fn id3v1_mit_leerzeichen_wird_getrimmt_gelesen() {
        let Some(ffmpeg) = crate::mixtape::find_ffmpeg() else {
            eprintln!("ffmpeg nicht installiert – übersprungen");
            return;
        };
        let dir = std::env::temp_dir().join(format!("fya-trim-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.mp3");
        let ok = std::process::Command::new(ffmpeg)
            .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "anullsrc=r=44100:cl=stereo", "-t", "1"])
            .args(["-write_id3v2", "0", "-write_id3v1", "0", "-c:a", "libmp3lame"])
            .arg(&file)
            .status()
            .unwrap()
            .success();
        assert!(ok);

        // ID3v1-Block (128 Byte) von Hand anhängen, Felder mit Leerzeichen aufgefüllt
        let pad = |s: &str, n: usize| format!("{s:<n$}");
        let mut tag = Vec::new();
        tag.extend_from_slice(b"TAG");
        tag.extend_from_slice(pad("Try Again", 30).as_bytes());
        tag.extend_from_slice(pad("Aaliyah", 30).as_bytes());
        tag.extend_from_slice(pad("Romeo Must Die Soundtrack", 30).as_bytes());
        tag.extend_from_slice(pad("2000", 4).as_bytes());
        tag.extend_from_slice(pad("", 30).as_bytes()); // Kommentar
        tag.push(255); // kein Genre
        assert_eq!(tag.len(), 128);
        let mut bytes = std::fs::read(&file).unwrap();
        // ffmpeg legt trotzdem einen (leeren) ID3v2-Kopf an: entfernen, damit nur ID3v1 bleibt
        if bytes.starts_with(b"ID3") {
            let size = bytes[6..10].iter().fold(0usize, |n, b| (n << 7) | usize::from(b & 0x7f));
            bytes.drain(..10 + size);
        }
        bytes.extend_from_slice(&tag);
        std::fs::write(&file, bytes).unwrap();

        let meta = read_meta(&file);
        assert_eq!(meta.title.as_deref(), Some("Try Again"));
        assert_eq!(meta.artist.as_deref(), Some("Aaliyah"));
        assert_eq!(meta.album.as_deref(), Some("Romeo Must Die Soundtrack"));

        let e = read_tag_edit(&file).unwrap();
        assert_eq!(
            (e.title.as_str(), e.artist.as_str(), e.album.as_str()),
            ("Try Again", "Aaliyah", "Romeo Must Die Soundtrack")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(all(test, feature = "ogg"))]
mod ogg_tests {
    use super::*;

    /// Feature `ogg`: Scan nimmt .ogg (Vorbis), .oga (FLAC in Ogg), .flac und .ogg mit Opus
    /// auf; Tags (Titel, BPM) werden gelesen und im Tag-Editor angezeigt. Braucht ffmpeg.
    #[test]
    fn scan_erkennt_ogg_und_flac_mit_tags() {
        let Some(ffmpeg) = crate::mixtape::find_ffmpeg() else {
            eprintln!("ffmpeg nicht installiert – übersprungen");
            return;
        };
        let dir = std::env::temp_dir().join(format!("fya-ogg-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let album = dir.join("musik/Künstler/Album");
        std::fs::create_dir_all(&album).unwrap();
        for (name, args) in [
            ("a.ogg", &["-c:a", "vorbis", "-strict", "-2", "-f", "ogg"][..]),
            ("b.oga", &["-c:a", "flac", "-f", "ogg"][..]),
            ("c.flac", &["-c:a", "flac"][..]),
            ("d.ogg", &["-c:a", "libopus", "-f", "ogg"][..]),
        ] {
            let ok = std::process::Command::new(&ffmpeg)
                .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-t", "1"])
                .args(["-metadata", &format!("title=T-{name}"), "-metadata", "BPM=128"])
                .args(args)
                .arg(album.join(name))
                .status()
                .unwrap()
                .success();
            assert!(ok, "{name}: ffmpeg");
        }
        let db = dir.join("library.db");
        Library::open(&db).unwrap();
        assert_eq!(rebuild_from(&db, &dir.join("musik")).unwrap(), 4, "alle vier aufgenommen");

        let lib = Library::open(&db).unwrap();
        let rows: Vec<(String, Option<i64>)> = lib
            .conn
            .prepare("SELECT title, bpm FROM tracks ORDER BY path")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        // lofty liest keine Ogg-FLAC-Tags: b.oga wird aufgenommen und ist abspielbar, hat aber
        // keine Tags (Titel = Dateiname). Alle anderen werden gelesen, auch Opus in einer .ogg
        // (Erkennung am Inhalt statt an der Endung).
        let expect: Vec<(String, Option<i64>)> = vec![
            ("T-a.ogg".into(), Some(128)),
            ("b".into(), None),
            ("T-c.flac".into(), Some(128)),
            ("T-d.ogg".into(), Some(128)),
        ];
        assert_eq!(rows, expect);

        // Tag-Editor: Lesen und Schreiben für Vorbis, natives FLAC und Opus in .ogg
        for name in ["a.ogg", "c.flac", "d.ogg"] {
            let file = album.join(name);
            let mut e = read_tag_edit(&file).unwrap();
            assert_eq!((e.title.as_str(), e.bpm.as_str()), (format!("T-{name}").as_str(), "128"), "{name}");
            e.title = format!("Neu-{name}");
            e.bpm = "97".into();
            write_tag_edit(&file, &e).unwrap_or_else(|err| panic!("{name}: schreiben: {err:#}"));
            let back = read_tag_edit(&file).unwrap();
            assert_eq!((back.title.as_str(), back.bpm.as_str()), (format!("Neu-{name}").as_str(), "97"), "{name}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
