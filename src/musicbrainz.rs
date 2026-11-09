//! MusicBrainz-Abgleich.
//!
//! `mbid`: NULL = noch nicht abgefragt, '' (leer) = abgefragt, aber nichts gefunden
//! (wird nicht erneut angefragt).
//!
//! Treffer landen in `library.db` (Spalte `mbid` bei Künstlern und Alben) und in
//! der eigenen Datenbank `musicbrainz.db` neben der Bibliothek. Letztere wird nie
//! neu angelegt oder gelöscht (auch nicht beim erneuten Einlesen des Musikordners)
//! und dient als Gedächtnis: nach einem Scan werden die IDs daraus zurückgespielt.

use rust_i18n::t;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

const ENDPOINT: &str = "https://musicbrainz.org/ws/2/release-group/";
const USER_AGENT: &str = concat!("fya/", env!("CARGO_PKG_VERSION"), " (local music player)");
/// MusicBrainz erlaubt im Schnitt eine Anfrage pro Sekunde.
const PAUSE: Duration = Duration::from_millis(1100);

/// Das Bild (BLOB) ist für Cover/Künstlerfotos vorgesehen und bleibt vorerst leer.
const MB_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS artists (
    name   TEXT NOT NULL PRIMARY KEY,
    mbid   TEXT NOT NULL,
    image  BLOB
);
CREATE TABLE IF NOT EXISTS albums (
    name   TEXT NOT NULL,
    artist TEXT NOT NULL,
    mbid   TEXT NOT NULL,
    image  BLOB,
    PRIMARY KEY (artist, name)
);
";

/// Pfad von `musicbrainz.db` (liegt neben `library.db`).
pub fn store_path(library_db: &Path) -> PathBuf {
    library_db.with_file_name("musicbrainz.db")
}

/// Öffnet `musicbrainz.db`; nur fehlende Tabellen werden angelegt.
fn open_store(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| t!("db.open", path = path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(MB_SCHEMA)?;
    Ok(conn)
}

/// Spielt gespeicherte MBIDs aus `musicbrainz.db` in die frisch aufgebaute
/// Mediathek zurück (Zuordnung über Künstler-/Albumname).
pub fn restore_into(library: &Connection, library_db: &Path) -> Result<()> {
    let store = store_path(library_db);
    drop(open_store(&store)?); // sicherstellen, dass Datei + Tabellen existieren
    library.execute("ATTACH DATABASE ?1 AS mb", [store.to_string_lossy()])?;
    let res = library.execute_batch(
        "UPDATE artists SET mbid = (SELECT m.mbid FROM mb.artists m WHERE m.name = artists.name)
           WHERE mbid IS NULL;
         UPDATE albums SET mbid = (SELECT m.mbid FROM mb.albums m
              WHERE m.name = albums.title
                AND m.artist = (SELECT name FROM artists WHERE id = albums.artist_id))
           WHERE mbid IS NULL;",
    );
    library.execute_batch("DETACH DATABASE mb")?;
    Ok(res?)
}

/// Für diese Einträge gibt es nie Anfragen (Sammelordner / unbekanntes Album).
fn never_request(artist: &str, album: &str) -> bool {
    // Auch nie in musicbrainz.db aufnehmen (siehe lookup_missing, mark_artists_without_mbid, list_albums).
    artist == "Compilations" || album == "Unknown Album"
}

/// Maskiert Anführungszeichen und Backslashes für die Lucene-Suche.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

struct Album {
    id: i64,
    artist_id: i64,
    artist: String,
    /// Ordnername (Schlüssel in beiden Datenbanken)
    title: String,
    /// Titel für die Abfrage (Album-Tag, siehe `db::album_display_titles`)
    query_title: String,
    /// Künstlername für die Abfrage (Tag, siehe `db::artist_display_names`)
    query_artist: String,
    lib_mbid: Option<String>,
}

/// Fragt alle Alben ohne mbid ab. Bricht nach der nächsten abgeschlossenen
/// Anfrage ab, sobald `cancel` gesetzt ist. Nach jeder erfolgreich ermittelten
/// MBID werden die zugehörigen Bilder geladen. Rückgabe: (eingetragen, geprüft, Bilder).
pub fn lookup_missing(
    db_path: &Path,
    cancel: &AtomicBool,
    progress: &Progress,
) -> Result<(usize, usize, usize)> {
    let lib = Connection::open(db_path)
        .with_context(|| t!("db.open", path = db_path.display()))?;
    lib.busy_timeout(Duration::from_secs(5))?;
    let store = open_store(&store_path(db_path))?;

    let display = crate::db::album_display_titles(&lib)?;
    let artist_display = crate::db::artist_display_names(&lib)?;

    let albums: Vec<Album> = {
        let mut st = lib.prepare(
            "SELECT al.id, ar.id, ar.name, al.title, al.mbid FROM albums al
             JOIN artists ar ON ar.id = al.artist_id
             ORDER BY ar.sort_name, al.title",
        )?;
        st.query_map([], |r| {
            Ok(Album {
                id: r.get(0)?,
                artist_id: r.get(1)?,
                artist: r.get(2)?,
                title: r.get(3)?,
                query_title: String::new(),
                query_artist: String::new(),
                lib_mbid: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|mut a| {
            a.query_title = display
                .get(&(a.artist.clone(), a.title.clone()))
                .cloned()
                .unwrap_or_else(|| a.title.clone());
            a.query_artist = artist_display
                .get(&a.artist)
                .cloned()
                .unwrap_or_else(|| a.artist.clone());
            a
        })
        .collect()
    };
    // „Unbekannt“ = noch nicht in musicbrainz.db (oder in library.db ohne mbid).
    // Alben mit mbid in library.db, die in musicbrainz.db fehlen, werden erneut abgefragt.
    let known: std::collections::HashSet<(String, String)> = {
        let mut st = store.prepare("SELECT artist, name FROM albums")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let albums: Vec<Album> = albums
        .into_iter()
        .filter(|a| !never_request(&a.artist, &a.title))
        .filter(|a| {
            a.lib_mbid.is_none() || !known.contains(&(a.artist.clone(), a.title.clone()))
        })
        .collect();
    progress.current.store(0, Ordering::Relaxed);
    progress.total.store(albums.len(), Ordering::Relaxed);
    log::debug!("MusicBrainz: {} Alben ohne mbid", albums.len());

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into();
    let (mut updated, mut checked, mut images) = (0, 0, 0);
    let mut throttle = Throttle(None);
    let mut cancelled = false;

    for al in &albums {
        let _tick = Tick(&progress.current);
        if never_request(&al.artist, &al.title) {
            log::debug!("{} – {}: übersprungen (keine Anfrage)", al.artist, al.title);
            continue;
        }
        // Schon in musicbrainz.db bekannt? Dann ohne Anfrage übernehmen.
        let known: Option<String> = store
            .query_row(
                "SELECT mbid FROM albums WHERE artist = ?1 AND name = ?2",
                params![al.artist, al.title],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(mbid) = known {
            log::debug!(
                "{} – {}: mbid {mbid} aus musicbrainz.db",
                al.artist,
                al.title
            );
            lib.execute(
                "UPDATE albums SET mbid = ?1 WHERE id = ?2",
                params![mbid, al.id],
            )?;
            updated += 1;
            continue;
        }

        throttle.wait(); // Rate-Limit einhalten
        let query = format!(
            "artist:\"{}\" AND releasegroup:\"{}\"",
            escape(&al.query_artist),
            escape(&al.query_title)
        );
        log::debug!("GET {ENDPOINT}?query={query}&fmt=json");

        let result = agent
            .get(ENDPOINT)
            .header("User-Agent", USER_AGENT)
            .query("query", &query)
            .query("fmt", "json")
            .call()
            .map_err(anyhow::Error::from)
            .and_then(|mut resp| {
                let status = resp.status();
                let json: serde_json::Value = resp.body_mut().read_json()?;
                Ok((status, json))
            });
        checked += 1;

        match result {
            Ok((status, json)) => {
                let Some(groups) = json["release-groups"].as_array() else {
                    log::debug!("  -> HTTP {status}, unerwartete Antwort, wird nicht markiert");
                    continue;
                };
                log::debug!("  -> HTTP {status}, {} Ergebnis(se)", groups.len());
                if groups.is_empty() {
                    // Kein Ergebnis: mit '' markieren, damit nicht erneut angefragt wird
                    // (auch in musicbrainz.db, damit die Markierung einen Scan überlebt).
                    lib.execute("UPDATE albums SET mbid = '' WHERE id = ?1", params![al.id])?;
                    store.execute(
                        "INSERT INTO albums(name, artist, mbid) VALUES (?1, ?2, '')
                         ON CONFLICT(artist, name) DO NOTHING",
                        params![al.title, al.artist],
                    )?;
                    log::debug!("  -> {} – {}: kein Ergebnis, markiert", al.artist, al.title);
                }
                // Mindestens ein Treffer: der erste (höchster Score) wird übernommen.
                if let Some(mbid) = groups.first().and_then(|g| g["id"].as_str()) {
                    let artist_mbid = groups[0]["artist-credit"][0]["artist"]["id"].as_str();
                    lib.execute(
                        "UPDATE albums SET mbid = ?1 WHERE id = ?2",
                        params![mbid, al.id],
                    )?;
                    store.execute(
                        "INSERT INTO albums(name, artist, mbid) VALUES (?1, ?2, ?3)
                         ON CONFLICT(artist, name) DO UPDATE SET mbid = excluded.mbid",
                        params![al.title, al.artist, mbid],
                    )?;
                    if let Some(a) = artist_mbid {
                        lib.execute(
                            "UPDATE artists SET mbid = ?1 WHERE id = ?2",
                            params![a, al.artist_id],
                        )?;
                        store.execute(
                            "INSERT INTO artists(name, mbid) VALUES (?1, ?2)
                             ON CONFLICT(name) DO UPDATE SET mbid = excluded.mbid",
                            params![al.artist, a],
                        )?;
                    }
                    log::debug!(
                        "  -> {} – {}: mbid {mbid}, Künstler-mbid {artist_mbid:?}",
                        al.artist,
                        al.title
                    );
                    updated += 1;
                    // Bilder direkt nach erfolgreicher MBID-Abfrage
                    images += ensure_images(&store, &agent, &mut throttle, &al.artist, &al.title)?;
                }
            }
            Err(e) => log::debug!("  -> Fehler: {e:#}"),
        }

        if cancel.load(Ordering::Relaxed) {
            log::debug!("MusicBrainz: abgebrochen");
            cancelled = true;
            break;
        }
    }
    mark_artists_without_mbid(&lib, &store)?;

    // Rückstand: Alben mit MBID, aber ohne Bild (z. B. aus früheren Läufen)
    if !cancelled {
        let pending: Vec<(String, String)> = {
            let mut st = store.prepare(
                "SELECT artist, name FROM albums WHERE mbid <> '' AND image IS NULL ORDER BY artist, name",
            )?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        log::debug!("MusicBrainz: {} Alben mit MBID ohne Bild", pending.len());
        progress.total.fetch_add(pending.len(), Ordering::Relaxed);
        for (artist, album) in pending {
            let _tick = Tick(&progress.current);
            images += ensure_images(&store, &agent, &mut throttle, &artist, &album)?;
            if cancel.load(Ordering::Relaxed) {
                log::debug!("MusicBrainz: abgebrochen");
                break;
            }
        }
    }
    log::debug!("MusicBrainz: fertig, {updated} eingetragen, {checked} Anfragen, {images} Bilder");
    Ok((updated, checked, images))
}

/// Künstler ohne mbid, deren Alben alle abgefragt sind (kein Album mit NULL),
/// bekommen '' – sie liefern auch künftig keine ID.
fn mark_artists_without_mbid(lib: &Connection, store: &Connection) -> Result<()> {
    let names: Vec<String> = {
        let mut st = lib.prepare(
            "SELECT name FROM artists ar WHERE mbid IS NULL AND name <> 'Compilations'
               AND NOT EXISTS (SELECT 1 FROM albums al WHERE al.artist_id = ar.id AND al.mbid IS NULL)",
        )?;
        st.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for name in names {
        lib.execute(
            "UPDATE artists SET mbid = '' WHERE name = ?1 AND mbid IS NULL",
            [&name],
        )?;
        store.execute(
            "INSERT INTO artists(name, mbid) VALUES (?1, '') ON CONFLICT(name) DO NOTHING",
            [&name],
        )?;
        log::debug!("Künstler {name}: keine mbid verfügbar, markiert");
    }
    Ok(())
}

/// Stand der MusicBrainz-ID eines Albums.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MbState {
    Id(String),
    /// abgefragt, nichts gefunden
    NotAvailable,
    /// noch nicht abgefragt
    Unknown,
}

#[derive(Clone, Debug)]
pub struct AlbumEntry {
    pub artist: String,
    /// Ordnername (Schlüssel in der Datenbank)
    pub album: String,
    /// Anzeigetitel (Album-Tag)
    pub display: String,
    /// Anzeigename des Künstlers (Tag)
    pub artist_display: String,
    pub state: MbState,
}

/// Alle Alben aus `musicbrainz.db` plus Alben der Mediathek, die dort noch
/// fehlen (Status „unbekannt“), sortiert nach Künstler und Album.
pub fn list_albums(library_db: &Path) -> Result<Vec<AlbumEntry>> {
    let store = open_store(&store_path(library_db))?;
    store.execute("ATTACH DATABASE ?1 AS lib", [library_db.to_string_lossy()])?;
    let lib_ro = Connection::open_with_flags(library_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let display = crate::db::album_display_titles(&lib_ro)?;
    let artist_names = crate::db::artist_display_names(&lib_ro)?;
    let mut st = store.prepare(
        "SELECT artist, name, mbid FROM main.albums
          WHERE artist <> 'Compilations' AND name <> 'Unknown Album'
         UNION ALL
         SELECT ar.name, al.title, NULL FROM lib.albums al
           JOIN lib.artists ar ON ar.id = al.artist_id
          WHERE ar.name <> 'Compilations' AND al.title <> 'Unknown Album'
            AND NOT EXISTS (SELECT 1 FROM main.albums m WHERE m.artist = ar.name AND m.name = al.title)
         ORDER BY 1 COLLATE NOCASE, 2 COLLATE NOCASE",
    )?;
    let rows = st.query_map([], |r| {
        let mbid: Option<String> = r.get(2)?;
        let (artist, album): (String, String) = (r.get(0)?, r.get(1)?);
        Ok(AlbumEntry {
            display: display
                .get(&(artist.clone(), album.clone()))
                .cloned()
                .unwrap_or_else(|| album.clone()),
            artist_display: artist_names.get(&artist).cloned().unwrap_or_else(|| artist.clone()),
            artist,
            album,
            state: match mbid {
                None => MbState::Unknown,
                Some(s) if s.is_empty() => MbState::NotAvailable,
                Some(s) => MbState::Id(s),
            },
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Cover Art Archive. Die gespeicherten Album-MBIDs sind Release-Group-IDs; der
/// Endpunkt `/release/{id}/front` kennt sie nicht (404), daher `/release-group/`.
/// `front-500` ist die 500-px-Vorschau (das Original kann >10 MB groß sein).
fn album_image_url(mbid: &str) -> String {
    format!("https://coverartarchive.org/release-group/{mbid}/front-500")
}

/// Prozent-Kodierung für Dateinamen in URLs.
fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// JSON-GET; Err(Fetched::Missing) bei HTTP 4xx, Err(Fetched::Failed) sonst.
fn get_json(agent: &ureq::Agent, url: &str) -> std::result::Result<serde_json::Value, Fetched> {
    log::debug!("GET {url}");
    match agent.get(url).header("User-Agent", USER_AGENT).call() {
        Ok(mut resp) => resp.body_mut().read_json().map_err(|e| {
            log::debug!("  -> JSON-Fehler: {e}");
            Fetched::Failed
        }),
        Err(ureq::Error::StatusCode(code)) if (400..500).contains(&code) => {
            log::debug!("  -> HTTP {code}");
            Err(Fetched::Missing)
        }
        Err(e) => {
            log::debug!("  -> Fehler: {e}");
            Err(Fetched::Failed)
        }
    }
}

/// Künstlerbild: Das Cover Art Archive hat keine Künstlerbilder. Stattdessen
/// liefert MusicBrainz über `url-rels` entweder eine `image`-Relation (Wikimedia
/// Commons) oder einen Wikidata-Link; dessen Eigenschaft P18 nennt die
/// Commons-Datei, die als 500-px-Vorschau geladen wird.
fn fetch_artist_image(agent: &ureq::Agent, throttle: &mut Throttle, mbid: &str) -> Fetched {
    throttle.wait(); // zählt zum MusicBrainz-Limit
    let mb = match get_json(
        agent,
        &format!("https://musicbrainz.org/ws/2/artist/{mbid}?inc=url-rels&fmt=json"),
    ) {
        Ok(v) => v,
        Err(f) => return f,
    };
    let rels = mb["relations"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let resource = |kind: &str| {
        rels.iter()
            .find(|r| r["type"] == kind)
            .and_then(|r| r["url"]["resource"].as_str())
    };

    // 1. direkte image-Relation auf eine Commons-Datei (Name bereits kodiert)
    let mut file: Option<String> = resource("image")
        .and_then(|u| u.split_once("/wiki/File:"))
        .map(|(_, name)| name.to_string());

    // 2. sonst Wikidata P18
    if file.is_none() {
        if let Some(qid) = resource("wikidata").and_then(|u| u.rsplit('/').next()) {
            let wd = match get_json(
                agent,
                &format!("https://www.wikidata.org/wiki/Special:EntityData/{qid}.json"),
            ) {
                Ok(v) => v,
                Err(f) => return f,
            };
            file = wd["entities"][qid]["claims"]["P18"][0]["mainsnak"]["datavalue"]["value"]
                .as_str()
                .map(percent_encode);
        }
    }
    match file {
        Some(name) => fetch_image(
            agent,
            &format!("https://commons.wikimedia.org/wiki/Special:FilePath/{name}?width=500"),
        ),
        None => {
            log::debug!("  -> kein Künstlerbild in MusicBrainz/Wikidata");
            Fetched::Missing
        }
    }
}

/// Bilder zu einem Album. Leerer Vec = abgefragt, aber keins vorhanden.
#[derive(Clone, Debug, Default)]
pub struct Images {
    pub artist: Option<Vec<u8>>,
    pub album: Option<Vec<u8>>,
}

enum Fetched {
    Image(Vec<u8>),
    /// endgültig nicht vorhanden (HTTP 4xx)
    Missing,
    /// vorübergehender Fehler: später erneut versuchen
    Failed,
}

fn fetch_image(agent: &ureq::Agent, url: &str) -> Fetched {
    log::debug!("GET {url}");
    match agent.get(url).header("User-Agent", USER_AGENT).call() {
        Ok(mut resp) => {
            let status = resp.status();
            match resp
                .body_mut()
                .with_config()
                .limit(20_000_000)
                .read_to_vec()
            {
                Ok(bytes) if !bytes.is_empty() => {
                    log::debug!("  -> HTTP {status}, {} Bytes", bytes.len());
                    Fetched::Image(bytes)
                }
                Ok(_) => Fetched::Missing,
                Err(e) => {
                    log::debug!("  -> Lesefehler: {e}");
                    Fetched::Failed
                }
            }
        }
        Err(ureq::Error::StatusCode(code)) if (400..500).contains(&code) => {
            log::debug!("  -> HTTP {code}, kein Bild");
            Fetched::Missing
        }
        Err(e) => {
            log::debug!("  -> Fehler: {e}");
            Fetched::Failed
        }
    }
}

/// Liest die gespeicherten Bilder aus `musicbrainz.db` (ohne Netzwerk).
/// Leerer Vec = abgefragt, aber nicht vorhanden; None = noch nicht geladen.
pub fn read_images(library_db: &Path, artist: &str, album: &str) -> Result<Images> {
    let store = open_store(&store_path(library_db))?;
    let get = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> Result<Option<Vec<u8>>> {
        Ok(store
            .query_row(sql, p, |r| r.get::<_, Option<Vec<u8>>>(0))
            .optional()?
            .flatten())
    };
    Ok(Images {
        artist: get("SELECT image FROM artists WHERE name = ?1", &[&artist])?,
        album: get(
            "SELECT image FROM albums WHERE artist = ?1 AND name = ?2",
            &[&artist, &album],
        )?,
    })
}

/// Fortschritt eines Laufs (aus der UI lesbar): bearbeitete / insgesamt zu
/// bearbeitende Alben. `total` wächst einmal, wenn der Bild-Rückstand bekannt ist.
#[derive(Default)]
pub struct Progress {
    pub current: AtomicUsize,
    pub total: AtomicUsize,
}

/// Zählt beim Verlassen des Schleifenkörpers (auch bei `continue`/`break`) hoch.
struct Tick<'a>(&'a AtomicUsize);

impl Drop for Tick<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// Bild für den Player aus `musicbrainz.db`: zuerst das Albumbild, sonst das
/// Künstlerbild; None, wenn keins gespeichert ist (dann bleibt der Platzhalter).
pub fn cover_for(library_db: &Path, artist: &str, album: &str) -> Option<Vec<u8>> {
    let imgs = read_images(library_db, artist, album).ok()?;
    [imgs.album, imgs.artist]
        .into_iter()
        .flatten()
        .find(|b| !b.is_empty())
}

/// Begrenzt MusicBrainz-Anfragen auf eine pro `PAUSE`.
struct Throttle(Option<Instant>);

impl Throttle {
    fn wait(&mut self) {
        if let Some(last) = self.0 {
            let elapsed = last.elapsed();
            if elapsed < PAUSE {
                std::thread::sleep(PAUSE - elapsed);
            }
        }
        self.0 = Some(Instant::now());
    }
}

/// Lädt fehlende Bilder (Album + Künstler) zu einem Eintrag mit MBID und
/// speichert sie als BLOB in `musicbrainz.db`. „Nicht vorhanden“ wird als leerer
/// BLOB vermerkt, bei vorübergehenden Fehlern bleibt der Wert NULL (später erneut).
/// Rückgabe: Anzahl neu gespeicherter Einträge (auch „nicht vorhanden“).
fn ensure_images(
    store: &Connection,
    agent: &ureq::Agent,
    throttle: &mut Throttle,
    artist: &str,
    album: &str,
) -> Result<usize> {
    if never_request(artist, album) {
        return Ok(0);
    }
    let row = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> Result<Option<(String, Option<Vec<u8>>)>> {
        Ok(store
            .query_row(sql, p, |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    };
    let mut stored = 0;

    if let Some((mbid, None)) = row(
        "SELECT mbid, image FROM albums WHERE artist = ?1 AND name = ?2",
        &[&artist, &album],
    )? {
        if !mbid.is_empty() {
            let blob = match fetch_image(agent, &album_image_url(&mbid)) {
                Fetched::Image(b) => Some(b),
                Fetched::Missing => Some(Vec::new()),
                Fetched::Failed => None,
            };
            if let Some(b) = blob {
                store.execute(
                    "UPDATE albums SET image = ?1 WHERE artist = ?2 AND name = ?3",
                    params![b, artist, album],
                )?;
                stored += 1;
            }
        }
    }
    if let Some((mbid, None)) = row("SELECT mbid, image FROM artists WHERE name = ?1", &[&artist])? {
        if !mbid.is_empty() {
            let blob = match fetch_artist_image(agent, throttle, &mbid) {
                Fetched::Image(b) => Some(b),
                Fetched::Missing => Some(Vec::new()),
                Fetched::Failed => None,
            };
            if let Some(b) = blob {
                store.execute("UPDATE artists SET image = ?1 WHERE name = ?2", params![b, artist])?;
                stored += 1;
            }
        }
    }
    Ok(stored)
}
