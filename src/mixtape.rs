//! „Mixtape“: Playlist als ZIP exportieren (ein Ordner mit dem Namen der Playlist,
//! darin direkt die Titeldateien). Optional werden die Titel vorher mit ffmpeg in ein
//! einheitliches Format konvertiert – **nie die Originale**: ffmpeg bekommt
//! ausschließlich eine temporäre Kopie, die danach wieder gelöscht wird.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Zielformat der Konvertierung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Opus,
    Aac,
    Mp3,
}

impl Format {
    pub const ALL: &'static [Format] = &[Format::Opus, Format::Aac, Format::Mp3];

    /// Dateiendung der Ausgabe (AAC im MP4-Container).
    pub fn ext(self) -> &'static str {
        match self {
            Format::Opus => "opus",
            Format::Aac => "m4a",
            Format::Mp3 => "mp3",
        }
    }

    /// Schlüssel zum Speichern in den Einstellungen.
    pub fn key(self) -> &'static str {
        match self {
            Format::Opus => "opus",
            Format::Aac => "aac",
            Format::Mp3 => "mp3",
        }
    }

    pub fn from_key(key: &str) -> Option<Format> {
        Format::ALL.iter().copied().find(|f| f.key() == key)
    }

    /// Hat die Datei dieses Format schon? Dann wird nicht neu kodiert (kein Qualitätsverlust).
    fn matches_ext(self, ext: &str) -> bool {
        match self {
            Format::Opus => ext == "opus",
            Format::Aac => ext == "m4a" || ext == "aac",
            Format::Mp3 => ext == "mp3",
        }
    }

    /// ffmpeg-Argumente. `with_cover`: Titelbild (falls vorhanden) übernehmen; Opus/Ogg
    /// bekommt keins.
    fn args(self, with_cover: bool) -> Vec<&'static str> {
        let mut a = vec!["-map", "0:a:0"];
        let cover = with_cover && self != Format::Opus;
        if cover {
            a.extend(["-map", "0:v?", "-c:v", "copy", "-disposition:v", "attached_pic"]);
        } else {
            a.push("-vn");
        }
        a.extend(["-map_metadata", "0"]);
        match self {
            Format::Opus => a.extend(["-c:a", "libopus", "-b:a", "128k"]),
            Format::Aac => a.extend(["-c:a", "aac", "-b:a", "192k"]),
            Format::Mp3 => a.extend(["-c:a", "libmp3lame", "-q:a", "2", "-id3v2_version", "3"]),
        }
        a
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Format::Opus => "Opus",
            Format::Aac => "AAC",
            Format::Mp3 => "MP3",
        })
    }
}

/// Konvertierung: Pfad zu ffmpeg + Zielformat.
#[derive(Debug, Clone)]
pub struct Convert {
    pub ffmpeg: PathBuf,
    pub format: Format,
}

/// Sucht ffmpeg im `PATH` und an üblichen Orten (aus Finder gestartete Apps haben nur
/// einen minimalen `PATH`, Homebrew liegt dort nicht drin).
pub fn find_ffmpeg() -> Option<PathBuf> {
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/snap/bin"] {
        dirs.push(PathBuf::from(extra));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// Ergebnis: geschriebene Titel, übersprungene (Datei nicht auffindbar/lesbar),
/// davon konvertierte.
#[derive(Debug)]
pub struct Outcome {
    pub written: usize,
    pub skipped: usize,
    pub converted: usize,
    pub target: PathBuf,
}

/// Erzeugt das ZIP unter `target` (`.zip` wird bei Bedarf angehängt). Ohne Konvertierung
/// werden die Dateien nur gespeichert (Audio ist schon komprimiert).
pub fn create(
    target: &Path,
    playlist_name: &str,
    files: &[PathBuf],
    convert: Option<&Convert>,
) -> Result<Outcome> {
    let mut target = target.to_path_buf();
    if target.extension().is_none_or(|e| !e.eq_ignore_ascii_case("zip")) {
        let mut name = target.as_os_str().to_os_string();
        name.push(".zip");
        target = PathBuf::from(name);
    }
    let mut folder = crate::db::sanitize_folder_name(playlist_name)
        .trim()
        .to_string();
    if folder.is_empty() {
        folder = "Mixtape".into();
    }

    let result = write_zip(&target, &folder, files, convert);
    if result.is_err() {
        let _ = fs::remove_file(&target); // keine halbe Datei zurücklassen
    }
    let (written, skipped, converted) = result?;
    Ok(Outcome {
        written,
        skipped,
        converted,
        target,
    })
}

/// Temporäres Verzeichnis für Kopien; wird beim Drop samt Inhalt gelöscht.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("fya-mixtape-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).with_context(|| format!("Temp-Ordner: {}", dir.display()))?;
        Ok(TempDir(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_zip(
    target: &Path,
    folder: &str,
    files: &[PathBuf],
    convert: Option<&Convert>,
) -> Result<(usize, usize, usize)> {
    let file =
        File::create(target).with_context(|| format!("ZIP anlegen: {}", target.display()))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .large_file(true);
    let tmp = match convert {
        Some(_) => Some(TempDir::new()?),
        None => None,
    };

    let mut used = HashSet::new();
    let (mut written, mut skipped, mut converted) = (0, 0, 0);
    for (idx, path) in files.iter().enumerate() {
        // Quelle prüfen (nur lesen)
        if File::open(path).is_err() {
            skipped += 1;
            continue;
        }
        let orig_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Titel".into());
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        // Datei, deren Inhalt ins ZIP kommt (Original oder konvertierte Temp-Datei) + Name
        let (source, entry_name, is_temp) = match (convert, &tmp) {
            (Some(c), Some(tmp)) if !c.format.matches_ext(&ext) => {
                let out = convert_copy(c, &tmp.0, path, idx)
                    .with_context(|| format!("Konvertieren: {orig_name}"))?;
                let stem = Path::new(&orig_name)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| orig_name.clone());
                converted += 1;
                (out, format!("{stem}.{}", c.format.ext()), true)
            }
            _ => (path.clone(), orig_name, false),
        };

        let mut src = File::open(&source)
            .with_context(|| format!("Lesen: {}", source.display()))?;
        let name = unique_name(&entry_name, &mut used);
        zip.start_file(format!("{folder}/{name}"), options)?;
        io::copy(&mut src, &mut zip)
            .with_context(|| format!("Kopieren: {}", source.display()))?;
        written += 1;
        if is_temp {
            let _ = fs::remove_file(&source); // Platz sparen, nur die Temp-Ausgabe
        }
    }
    zip.finish()?;
    Ok((written, skipped, converted))
}

/// Konvertiert eine **temporäre Kopie** von `src` und gibt den Pfad der Ausgabe (ebenfalls
/// im Temp-Ordner) zurück. Das Original wird nur gelesen (Kopieren), nie an ffmpeg
/// übergeben und nie verändert.
fn convert_copy(c: &Convert, tmp: &Path, src: &Path, idx: usize) -> Result<PathBuf> {
    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "bin".into());
    let input = tmp.join(format!("in-{idx}.{ext}"));
    let output = tmp.join(format!("out-{idx}.{}", c.format.ext()));

    // Sicherheitsnetz: Eingabe und Ausgabe liegen im Temp-Ordner, nie beim Original.
    if !input.starts_with(tmp) || !output.starts_with(tmp) || input == src || output == src {
        bail!("interner Fehler: Temp-Pfad ungültig");
    }
    fs::copy(src, &input).with_context(|| format!("Kopie anlegen: {}", src.display()))?;
    if let (Ok(a), Ok(b)) = (fs::canonicalize(src), fs::canonicalize(&input)) {
        if a == b {
            bail!("interner Fehler: Kopie zeigt auf das Original");
        }
    }

    // Erst mit Titelbild, bei Fehler ohne (nicht jedes Quellformat/jeder Container passt).
    let mut last_err = String::new();
    for with_cover in [true, false] {
        let _ = fs::remove_file(&output);
        let mut cmd = Command::new(&c.ffmpeg);
        cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&input) // <- ausschließlich die Kopie
            .args(c.format.args(with_cover))
            .arg(&output)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // kein Konsolenfenster
        }
        let out = cmd
            .output()
            .with_context(|| format!("ffmpeg starten: {}", c.ffmpeg.display()))?;
        if out.status.success() && output.is_file() {
            let _ = fs::remove_file(&input); // Eingabekopie sofort löschen
            return Ok(output);
        }
        last_err = String::from_utf8_lossy(&out.stderr)
            .lines()
            .last()
            .unwrap_or("unbekannter Fehler")
            .to_string();
    }
    let _ = fs::remove_file(&input);
    bail!("ffmpeg: {last_err}")
}

/// Dateiname im ZIP; bei Doppelungen `Name (2).ext`.
fn unique_name(name: &str, used: &mut HashSet<String>) -> String {
    if used.insert(name.to_lowercase()) {
        return name.to_string();
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.to_string(), String::new()),
    };
    (2..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|c| used.insert(c.to_lowercase()))
        .expect("unendlicher Iterator")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fya-mixtape-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn zip_mit_ordner_dateien_und_duplikaten() {
        let dir = temp("plain");
        fs::create_dir_all(dir.join("a")).unwrap();
        fs::create_dir_all(dir.join("b")).unwrap();
        fs::write(dir.join("a/song.mp3"), b"AAA").unwrap();
        fs::write(dir.join("b/song.mp3"), b"BBB").unwrap();
        fs::write(dir.join("b/other.m4a"), b"CCC").unwrap();

        let files = vec![
            dir.join("a/song.mp3"),
            dir.join("b/song.mp3"), // gleicher Dateiname -> "song (2).mp3"
            dir.join("gone/x.mp3"), // fehlt -> übersprungen
            dir.join("b/other.m4a"),
        ];
        // Ziel ohne .zip -> wird ergänzt; Name mit Sonderzeichen -> bereinigt
        let out = create(&dir.join("mix"), "Road/Trip: 1", &files, None).unwrap();
        assert_eq!((out.written, out.skipped, out.converted), (3, 1, 0));
        assert_eq!(out.target, dir.join("mix.zip"));

        let mut z = zip::ZipArchive::new(File::open(&out.target).unwrap()).unwrap();
        let mut names: Vec<String> = (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["Road_Trip_ 1/other.m4a", "Road_Trip_ 1/song (2).mp3", "Road_Trip_ 1/song.mp3"]
        );
        let mut s = String::new();
        z.by_name("Road_Trip_ 1/song (2).mp3")
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "BBB");
        let _ = fs::remove_dir_all(dir);
    }

    /// Unix-Test mit einem Fake-„ffmpeg“, der seine Eingabedatei **verändert**: Das Original
    /// muss trotzdem unberührt bleiben, weil nur eine Temp-Kopie übergeben wird.
    #[cfg(unix)]
    #[test]
    fn original_bleibt_unveraendert_nur_kopie_an_ffmpeg() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("convert");
        let orig = dir.join("lieder/song.mp3");
        fs::create_dir_all(orig.parent().unwrap()).unwrap();
        fs::write(&orig, b"ORIGINAL").unwrap();
        let same_format = dir.join("lieder/schon.opus");
        fs::write(&same_format, b"OPUSDATA").unwrap();

        let log = dir.join("log.txt");
        let fake = dir.join("ffmpeg");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nwhile [ $# -gt 0 ]; do [ \"$1\" = \"-i\" ] && in=\"$2\"; last=\"$1\"; shift; done\n\
                 echo \"$in\" >> '{}'\necho TAMPERED >> \"$in\"\necho CONVERTED > \"$last\"\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let conv = Convert { ffmpeg: fake, format: Format::Opus };
        let out = create(&dir.join("t.zip"), "Mix", &[orig.clone(), same_format.clone()], Some(&conv)).unwrap();
        assert_eq!((out.written, out.converted), (2, 1)); // .opus bleibt unkonvertiert

        // Original unverändert
        assert_eq!(fs::read(&orig).unwrap(), b"ORIGINAL");
        assert_eq!(fs::read(&same_format).unwrap(), b"OPUSDATA");

        // ffmpeg bekam genau eine Eingabe, und die war eine Kopie im Temp-Ordner
        let logged = fs::read_to_string(&log).unwrap();
        let inputs: Vec<&str> = logged.lines().collect();
        assert_eq!(inputs.len(), 1);
        let input = Path::new(inputs[0]);
        assert_ne!(input, orig);
        assert!(input.starts_with(std::env::temp_dir()));
        assert!(!input.exists(), "Temp-Kopie wurde nicht gelöscht");

        // ZIP: konvertierte Datei (Endung .opus) + unveränderte .opus
        let mut z = zip::ZipArchive::new(File::open(dir.join("t.zip")).unwrap()).unwrap();
        let mut s = String::new();
        z.by_name("Mix/song.opus").unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s.trim(), "CONVERTED");
        let mut s = String::new();
        z.by_name("Mix/schon.opus").unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "OPUSDATA");
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn ffmpeg_fehler_bricht_ab_und_entfernt_zip() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("fail");
        let orig = dir.join("song.mp3");
        fs::write(&orig, b"ORIGINAL").unwrap();
        let fake = dir.join("ffmpeg");
        fs::write(&fake, "#!/bin/sh\necho 'Unknown encoder libopus' >&2\nexit 1\n").unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let conv = Convert { ffmpeg: fake, format: Format::Opus };
        let err = create(&dir.join("t.zip"), "Mix", &[orig.clone()], Some(&conv)).unwrap_err();
        assert!(format!("{err:#}").contains("Unknown encoder libopus"));
        assert!(!dir.join("t.zip").exists());
        assert_eq!(fs::read(&orig).unwrap(), b"ORIGINAL");
        let _ = fs::remove_dir_all(dir);
    }

    /// Echter ffmpeg (falls installiert): MP3 -> Opus/AAC, Original unverändert, Tags übernommen.
    #[test]
    fn echter_ffmpeg_wenn_vorhanden() {
        let Some(ffmpeg) = find_ffmpeg() else {
            eprintln!("ffmpeg nicht installiert – Test übersprungen");
            return;
        };
        let dir = temp("real");
        let src = dir.join("Song.mp3");
        let made = Command::new(&ffmpeg)
            .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
            .args(["-metadata", "title=Testtitel", "-metadata", "artist=Tester", "-c:a", "libmp3lame"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let before = fs::read(&src).unwrap();

        for (fmt, magic) in [(Format::Opus, &b"OggS"[..]), (Format::Aac, &b"ftyp"[..])] {
            let conv = Convert { ffmpeg: ffmpeg.clone(), format: fmt };
            let zip_path = dir.join(format!("{}.zip", fmt.key()));
            let out = create(&zip_path, "Mix", &[src.clone()], Some(&conv)).unwrap();
            assert_eq!((out.written, out.converted), (1, 1), "{fmt}");

            let mut z = zip::ZipArchive::new(File::open(&zip_path).unwrap()).unwrap();
            let name = format!("Mix/Song.{}", fmt.ext());
            let mut bytes = Vec::new();
            z.by_name(&name).unwrap().read_to_end(&mut bytes).unwrap();
            assert!(bytes.len() > 500, "{fmt}: Ausgabe zu klein");
            assert!(bytes.windows(4).take(64).any(|w| w == magic), "{fmt}: falsches Format");

            // Tags (Titel/Künstler) sind in der konvertierten Datei angekommen
            let conv_file = dir.join(format!("check.{}", fmt.ext()));
            fs::write(&conv_file, &bytes).unwrap();
            use lofty::file::TaggedFileExt;
            use lofty::tag::Accessor;
            let tagged = lofty::read_from_path(&conv_file).unwrap();
            let tag = tagged.primary_tag().or_else(|| tagged.first_tag()).unwrap();
            assert_eq!(tag.title().as_deref(), Some("Testtitel"), "{fmt}");
            assert_eq!(tag.artist().as_deref(), Some("Tester"), "{fmt}");
        }
        assert_eq!(fs::read(&src).unwrap(), before, "Original wurde verändert");
        let _ = fs::remove_dir_all(dir);
    }

    /// Format-Schlüssel (so in `settings` gespeichert) lassen sich wieder einlesen.
    #[test]
    fn format_schluessel_roundtrip() {
        for f in Format::ALL {
            assert_eq!(Format::from_key(f.key()), Some(*f));
        }
        assert_eq!(Format::from_key("flac"), None);
    }
}
