//! Sprachwahl: gespeicherte Einstellung, sonst Systemsprache, sonst Englisch.

use std::fmt;

/// Eine unterstützte Oberflächensprache (Dateiname in `locales/` = `code`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lang {
    pub code: &'static str,
    pub name: &'static str,
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

pub const LANGS: &[Lang] = &[
    Lang {
        code: "de",
        name: "Deutsch",
    },
    Lang {
        code: "en",
        name: "English",
    },
];

pub const FALLBACK: Lang = LANGS[1];

/// Einstellungsschlüssel in der Tabelle `settings`.
pub const SETTING_KEY: &str = "language";

/// `de`, `de-DE`, `de_DE.UTF-8` … -> unterstützte Sprache.
pub fn from_code(code: &str) -> Option<Lang> {
    let primary = code
        .split(['-', '_', '.', '@'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    LANGS.iter().copied().find(|l| l.code == primary)
}

/// Gespeicherte Sprache, sonst Systemsprache, sonst Englisch.
pub fn detect(saved: Option<&str>) -> Lang {
    saved
        .and_then(from_code)
        .or_else(|| sys_locale::get_locale().as_deref().and_then(from_code))
        .unwrap_or(FALLBACK)
}

/// Stellt die Sprache für alle folgenden `t!()`-Aufrufe ein.
pub fn apply(lang: Lang) {
    rust_i18n::set_locale(lang.code);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Alle Sprachdateien müssen dieselben Schlüssel haben (fehlende Übersetzung = Fehler).
    #[test]
    fn locale_files_have_the_same_keys() {
        fn keys(code: &str) -> std::collections::BTreeSet<String> {
            let path = format!("{}/locales/{code}.yml", env!("CARGO_MANIFEST_DIR"));
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .filter_map(|l| {
                    l.strip_prefix('"')?
                        .split_once("\": ")
                        .map(|(k, _)| k.to_string())
                })
                .collect()
        }
        let reference = keys(LANGS[0].code);
        assert!(!reference.is_empty());
        for l in LANGS {
            assert_eq!(
                keys(l.code),
                reference,
                "Schlüssel in {}.yml weichen ab",
                l.code
            );
        }
    }

    #[test]
    fn codes_are_parsed() {
        assert_eq!(from_code("de_DE.UTF-8").unwrap().code, "de");
        assert_eq!(from_code("en-US").unwrap().code, "en");
        assert_eq!(from_code("EN").unwrap().code, "en");
        assert!(from_code("fr-FR").is_none());
        assert_eq!(detect(Some("de")).code, "de");
    }
}
