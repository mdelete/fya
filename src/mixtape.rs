//! „Mixtape“: Playlist als ZIP exportieren (ein Ordner mit dem Namen der Playlist,
//! darin direkt die Titeldateien).

use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Ergebnis: (geschriebene Titel, übersprungene = Datei nicht auffindbar/lesbar).
pub struct Outcome {
    pub written: usize,
    pub skipped: usize,
    pub target: PathBuf,
}

/// Erzeugt das ZIP unter `target` (`.zip` wird bei Bedarf angehängt). Audiodateien sind
/// bereits komprimiert, deshalb wird nur gespeichert (schnell, kaum größer).
pub fn create(target: &Path, playlist_name: &str, files: &[PathBuf]) -> Result<Outcome> {
    let mut target = target.to_path_buf();
    if target.extension().is_none_or(|e| !e.eq_ignore_ascii_case("zip")) {
        let mut name = target.as_os_str().to_os_string();
        name.push(".zip");
        target = PathBuf::from(name);
    }
    let mut folder = crate::db::sanitize_folder_name(playlist_name).trim().to_string();
    if folder.is_empty() {
        folder = "Mixtape".into();
    }

    let result = write_zip(&target, &folder, files);
    if result.is_err() {
        let _ = std::fs::remove_file(&target); // keine halbe Datei zurücklassen
    }
    let (written, skipped) = result?;
    Ok(Outcome { written, skipped, target })
}

fn write_zip(target: &Path, folder: &str, files: &[PathBuf]) -> Result<(usize, usize)> {
    let file = File::create(target)
        .with_context(|| format!("ZIP anlegen: {}", target.display()))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .large_file(true);

    let mut used = HashSet::new();
    let (mut written, mut skipped) = (0, 0);
    for path in files {
        let Ok(mut src) = File::open(path) else {
            skipped += 1;
            continue;
        };
        let name = unique_name(path, &mut used);
        zip.start_file(format!("{folder}/{name}"), options)?;
        io::copy(&mut src, &mut zip)
            .with_context(|| format!("Kopieren: {}", path.display()))?;
        written += 1;
    }
    zip.finish()?;
    Ok((written, skipped))
}

/// Dateiname im ZIP; bei Doppelungen `Name (2).ext`.
fn unique_name(path: &Path, used: &mut HashSet<String>) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Titel".into());
    if used.insert(name.to_lowercase()) {
        return name;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.clone(), String::new()),
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

    #[test]
    fn zip_mit_ordner_dateien_und_duplikaten() {
        let dir = std::env::temp_dir().join(format!("fya-mixtape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::write(dir.join("a/song.mp3"), b"AAA").unwrap();
        std::fs::write(dir.join("b/song.mp3"), b"BBB").unwrap();
        std::fs::write(dir.join("b/other.m4a"), b"CCC").unwrap();

        let files = vec![
            dir.join("a/song.mp3"),
            dir.join("b/song.mp3"),   // gleicher Dateiname -> "song (2).mp3"
            dir.join("gone/x.mp3"),   // fehlt -> übersprungen
            dir.join("b/other.m4a"),
        ];
        // Ziel ohne .zip -> wird ergänzt; Name mit Sonderzeichen -> bereinigt
        let out = create(&dir.join("mix"), "Road/Trip: 1", &files).unwrap();
        assert_eq!((out.written, out.skipped), (3, 1));
        assert_eq!(out.target, dir.join("mix.zip"));

        let mut z = zip::ZipArchive::new(File::open(&out.target).unwrap()).unwrap();
        let mut names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().unwrap().to_string()).collect();
        names.sort();
        assert_eq!(
            names,
            ["Road_Trip_ 1/other.m4a", "Road_Trip_ 1/song (2).mp3", "Road_Trip_ 1/song.mp3"]
        );
        let mut s = String::new();
        z.by_name("Road_Trip_ 1/song (2).mp3").unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "BBB");
        let _ = std::fs::remove_dir_all(dir);
    }
}
