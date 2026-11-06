// Windows-Release: keine Konsole hinter dem Fenster
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod audio;
mod db;
mod folder;
mod mixtape;
mod musicbrainz;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use iced::widget::text::Wrapping;
use iced::widget::{
    button, checkbox, column, container, image, mouse_area, opaque, pane_grid, pick_list, row,
    scrollable, slider, stack, svg, text, text_input,
};
use iced::{
    Alignment, Border, Color, Element, Event, Font, Length, Padding, Subscription, Task, Theme,
    event, keyboard, time,
};

use audio::Audio;
use db::{Album, Artist, Library, MusicRoot, Playlist, SearchHits, TagEdit, Track};
use folder::FolderAccess;
use musicbrainz::MbState;

/// Datenbank im Datenverzeichnis des Users (Linux: ~/.local/share, macOS:
/// ~/Library/Application Support – unter der Sandbox innerhalb des App-Containers).
/// Der Musikordner wird nicht mehr im Code festgelegt, sondern beim ersten Start
/// im Ordner-Dialog gewählt und in dieser Datenbank gespeichert (Tabelle `settings`).
const DB_SUBPATH: &str = "fya/library.db";

fn fmt_time(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// macOS: Icon für Dock und „Über …“-Dialog zur Laufzeit setzen. Im .app-Bundle
/// kommt es schon aus `AppIcon.icns`; so gilt es auch bei `cargo run`.
/// (iced/winit bieten dafür keine API; das Standard-„Über“-Menü von winit zeigt
/// das Anwendungs-Icon von `NSApplication`.)
#[cfg(target_os = "macos")]
fn set_macos_app_icon() {
    use objc2::AllocAnyThread;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(APP_ICON);
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    set_macos_app_icon();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,fya=debug"))
        .init();
    let db_path = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(DB_SUBPATH);

    // Legt Datei + Schema an; der Scan folgt erst nach der Ordnerwahl.
    let library = Library::open(&db_path)?;
    // Beim Start läuft die Ausgabe auf dem Systemstandard; danach wird nur noch auf
    // ausdrücklich gewählte Geräte gewechselt.
    let audio = Audio::new()?;

    // iced ruft die Boot-Funktion genau einmal auf; so reichen wir die
    // bereits erzeugten Objekte hinein.
    let init = RefCell::new(Some((library, audio, db_path)));

    iced::application(
        move || {
            let (library, audio, db_path) = init.borrow_mut().take().expect("boot nur einmal");
            PlayerApp::new(library, audio, db_path)
        },
        PlayerApp::update,
        PlayerApp::view,
    )
    .title("FYA")
    .subscription(PlayerApp::subscription)
    .theme(|app: &PlayerApp| app.theme.clone())
    .default_font(Font::MONOSPACE)
    .window(window_settings())
    .run()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Look (angelehnt an Halloy): warm-dunkles Grau, cremefarbener Text,
// Monospace-Schrift, Titelleiste transparent über dem Inhalt.
// ---------------------------------------------------------------------------

/// Farben der eigenen Flächen/Texte. Für das Standard-Theme „Halloy“ die festen Werte,
/// sonst aus der Palette des gewählten iced-Themes abgeleitet.
#[derive(Debug, Clone, Copy)]
struct Colors {
    panel: Color,    // Flächen
    selected: Color, // gewählte Zeile
    hover: Color,
    border: Color,
    text: Color,
    dim: Color, // Überschriften, Zeiten
    warn: Color,
}

const HALLOY_NAME: &str = "Halloy";

fn halloy_theme() -> Theme {
    Theme::custom(
        HALLOY_NAME,
        iced::theme::Palette {
            background: Color::from_rgb(0.145, 0.141, 0.165), // Fenster
            text: Color::from_rgb(0.94, 0.88, 0.67),          // creme
            primary: Color::from_rgb(0.84, 0.66, 0.90),       // Flieder
            success: Color::from_rgb(0.66, 0.82, 0.55),
            warning: Color::from_rgb(0.90, 0.75, 0.48),
            danger: Color::from_rgb(0.88, 0.42, 0.46),
        },
    )
}

/// Alle wählbaren Themes: Halloy (Standard) + die iced-Themes.
fn theme_options() -> Vec<Theme> {
    std::iter::once(halloy_theme())
        .chain(Theme::ALL.iter().cloned())
        .collect()
}

/// Relative Helligkeit (WCAG).
fn luminance(c: Color) -> f32 {
    let f = |v: f32| {
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
}

fn contrast(a: Color, b: Color) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Schiebt `fg` Richtung Schwarz/Weiß (je nach Polarität), bis der Kontrast zu
/// allen `surfaces` mindestens `min` beträgt.
fn ensure_contrast(fg: Color, surfaces: &[Color], min: f32) -> Color {
    // Heller als die Flächen -> Richtung Weiß, sonst Richtung Schwarz
    let avg = surfaces.iter().map(|s| luminance(*s)).sum::<f32>() / surfaces.len() as f32;
    let target = if luminance(fg) > avg { 1.0 } else { 0.0 };
    let mut out = fg;
    for step in 0..=20 {
        let t = step as f32 / 20.0;
        out = Color {
            r: fg.r + (target - fg.r) * t,
            g: fg.g + (target - fg.g) * t,
            b: fg.b + (target - fg.b) * t,
            a: 1.0,
        };
        if surfaces.iter().all(|s| contrast(out, *s) >= min) {
            break;
        }
    }
    out
}

fn colors_for(theme: &Theme) -> Colors {
    if theme.to_string() == HALLOY_NAME {
        return Colors {
            panel: Color::from_rgb(0.122, 0.118, 0.141),
            selected: Color::from_rgb(0.098, 0.094, 0.114),
            hover: Color::from_rgb(0.175, 0.170, 0.196),
            border: Color::from_rgb(0.55, 0.52, 0.53),
            text: Color::from_rgb(0.94, 0.88, 0.67),
            dim: Color::from_rgb(0.54, 0.49, 0.46),
            warn: Color::from_rgb(0.95, 0.62, 0.3),
        };
    }
    let p = theme.palette();
    let bg = theme.extended_palette().background.clone();
    let mix = |a: Color, b: Color, t: f32| Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    };
    let (panel, selected, hover) = (bg.weakest.color, bg.weaker.color, bg.weak.color);
    let surfaces = [panel, selected, hover];
    // Lesbarkeit auf allen eigenen Flächen sicherstellen (v. a. bei Solarized/Tokyo Night Light)
    let text = ensure_contrast(p.text, &surfaces, 4.5);
    let dim = ensure_contrast(mix(text, p.background, 0.4), &surfaces, 3.0);
    Colors {
        panel,
        selected,
        hover,
        border: bg.strong.color,
        text,
        dim,
        warn: ensure_contrast(p.warning, &surfaces, 3.0),
    }
}

/// Aktuelle Farben (werden beim Theme-Wechsel gesetzt; Style-Closures und
/// View-Funktionen lesen sie über [`col`], ohne sie durchreichen zu müssen).
static COLORS: std::sync::RwLock<Option<Colors>> = std::sync::RwLock::new(None);

fn col() -> Colors {
    COLORS
        .read()
        .ok()
        .and_then(|c| *c)
        .unwrap_or_else(|| colors_for(&halloy_theme()))
}

fn set_colors(theme: &Theme) {
    if let Ok(mut c) = COLORS.write() {
        *c = Some(colors_for(theme));
    }
}

fn window_settings() -> iced::window::Settings {
    #[allow(unused_mut)]
    let mut settings = iced::window::Settings {
        size: iced::Size::new(1000.0, 640.0),
        // Fenster-/Taskleisten-Icon (Linux/Windows; unter macOS kommt es aus dem App-Bundle)
        icon: iced::window::icon::from_file_data(APP_ICON, None).ok(),
        ..Default::default()
    };
    // Wayland/GNOME ordnen das Icon über die App-ID der .desktop-Datei zu
    #[cfg(target_os = "linux")]
    {
        settings.platform_specific.application_id = "fya".to_string();
    }
    #[cfg(target_os = "macos")]
    {
        settings.platform_specific = iced::window::settings::PlatformSpecific {
            title_hidden: true,
            titlebar_transparent: true,
            fullsize_content_view: true,
        };
    }
    settings
}

/// App-Icon (gleiche Datei wie für das macOS-Bundle und die Linux-Desktop-Datei)
const APP_ICON: &[u8] = include_bytes!("../assets/icons/app-icon.png");

/// Platz für die Ampel-Buttons, da der Inhalt unter der Titelleiste liegt.
#[cfg(target_os = "macos")]
const TITLEBAR_INSET: f32 = 38.0;
#[cfg(not(target_os = "macos"))]
const TITLEBAR_INSET: f32 = 8.0;

fn panel_style(_: &Theme) -> container::Style {
    container::Style {
        background: Some(col().panel.into()),
        border: Border {
            color: col().border,
            width: 1.0,
            radius: 4.0.into(),
        },
        ..container::Style::default()
    }
}

// ---------------------------------------------------------------------------
// Zustand & Nachrichten
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Message {
    SelectArtist(i64),
    /// Cover der Alben (Album-ID, Bytes) im Hintergrund geladen
    AlbumCovers(Vec<(i64, Option<Vec<u8>>)>),
    PlayTrack(usize),
    TogglePause,
    Next,
    Previous,
    Tick,
    /// Slider wird gezogen (Sekunden)
    Seek(f32),
    SeekRelease,
    Volume(f32),
    /// Einstellungsdialog (modal) öffnen/schließen
    /// Trennlinie zwischen den Spalten gezogen
    PaneResized(pane_grid::ResizeEvent),
    ShowView(View),
    SelectMbAlbum(usize),
    /// Album in der Compilations-Ansicht (None = alle)
    SelectCompAlbum(Option<i64>),
    SelectPlaylist(i64),
    ThemeSelected(Theme),
    /// Lautsprecher-Knopf im Player: Geräte-Popover öffnen/schließen
    ToggleDeviceMenu,
    DevicesLoaded(Vec<String>),
    /// Ausgabegerät ausdrücklich wählen
    SelectDevice(String),
    /// Einstellung: Mixtapes immer konvertieren (an/aus) bzw. Zielformat
    MixtapeConvert(bool),
    MixtapeFormat(mixtape::Format),
    /// Taste „p“: Titel zu Playlist hinzufügen
    OpenAddToPlaylist,
    NewPlaylist,
    PlaylistName(String),
    ConfirmPlaylistDialog,
    ClosePlaylistDialog,
    AddToPlaylist(i64),
    AddToNewPlaylist,
    RemoveFromPlaylist(PathBuf),
    AskDeletePlaylist(i64),
    AskRenamePlaylist(i64),
    /// ❣️ „Mixtape“: Playlist als ZIP exportieren – Speichern-Dialog öffnen
    Mixtape(i64),
    /// Gewählter Zielpfad (None = abgebrochen)
    MixtapeTarget(i64, Option<PathBuf>),
    MixtapeDone(Result<(usize, usize, usize, PathBuf), String>),
    /// Alt+↑ / Alt+↓: laufenden Titel in der Playlist verschieben (-1 / +1)
    MovePlaylistTrack(i32),
    /// Esc: schließt den jeweils offenen Dialog
    Escape,
    PlaylistPaneResized(pane_grid::ResizeEvent),
    CompPaneResized(pane_grid::ResizeEvent),
    /// Taste „t“: Tag-Editor für den gewählten Titel öffnen
    OpenTagEditor,
    CancelTagEditor,
    /// Datei im Finder/Dateimanager zeigen
    RevealFile(PathBuf),
    /// Eingabe in einem Feld des Tag-Editors
    EditTag(TagField, String),
    EditCompilation(bool),
    SaveTags,
    /// Ergebnis des Schreibens in die Datei
    TagsSaved(Result<(), String>),
    /// Tab-Taste im Tag-Editor (true = mit Shift)
    TabPressed(bool),
    /// Bilder geladen (Künstler, Album, Ergebnis)
    MbImages(String, String, Option<musicbrainz::Images>),
    /// Ordner-Dialog öffnen
    PickFolder,
    /// Ergebnis des Ordner-Dialogs (None = abgebrochen)
    FolderPicked(Option<PathBuf>),
    Rescan,
    ScanFinished(Result<usize, String>),
    /// Suchfeld geändert
    MusicBrainz,
    MusicBrainzDone(Result<(usize, usize, usize), String>),
    SpinnerTick,
    SearchChanged(String),
    /// Ergebnis der Hintergrundsuche (Suchbegriff, Treffer)
    SearchDone(String, Option<SearchHits>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagField {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Composer,
    Genre,
    TrackNo,
    TrackTotal,
    DiscNo,
    Year,
}

/// Zustand des modalen Tag-Editors
struct TagEditor {
    track_id: i64,
    path: PathBuf,
    /// Pfad relativ zur Musikbibliothek (zur Anzeige)
    rel_path: String,
    fields: TagEdit,
    error: Option<String>,
    saving: bool,
}

/// Modale Dialoge rund um Playlists
enum PlaylistDialog {
    /// Neue Playlist anlegen; `add` = Titel, der danach hinein soll
    New {
        name: String,
        add: Option<PathBuf>,
        error: Option<String>,
    },
    /// Titel (Taste „p“) einer Playlist hinzufügen
    AddTrack {
        path: PathBuf,
        title: String,
        error: Option<String>,
    },
    /// Playlist umbenennen
    Rename {
        id: i64,
        name: String,
        error: Option<String>,
    },
    /// Löschen bestätigen
    Delete { id: i64, name: String },
}

/// Inhalt der rechten Seite (neben der Buttonleiste)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Music,
    Compilations,
    Playlists,
    MusicBrainz,
    Settings,
    Help,
}

#[derive(Debug, Clone, Copy)]
enum Column {
    Playlists,
    Artists,
    Albums,
    Tracks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    NeedsFolder,
    Scanning,
    Ready,
}

struct PlayerApp {
    library: Library,
    db_path: PathBuf,
    audio: Audio,
    /// Ausgabegerät-Menü (Popover im Player): Menü offen?, Geräteliste
    device_menu_open: bool,
    devices: Vec<String>,

    /// Zugriff auf den gewählten Ordner (unter der Sandbox zwingend nötig)
    access: Option<FolderAccess>,
    phase: Phase,
    notice: Option<String>,
    view: View,
    /// Läuft gerade der MusicBrainz-Abgleich? (+ Abbruch-Flag des Threads)
    /// Offener Tag-Editor (modal)
    tag_editor: Option<TagEditor>,
    mb_running: bool,
    mb_cancel: Arc<AtomicBool>,
    mb_progress: Arc<musicbrainz::Progress>,
    mb_result: Option<String>,
    /// MusicBrainz-Ansicht: Albumliste, Auswahl und Bilder
    mb_albums: Vec<musicbrainz::AlbumEntry>,
    /// Indizes der durch die Suche sichtbaren Einträge von `mb_albums`
    mb_visible: Vec<usize>,
    mb_selected: Option<usize>,
    mb_artist_image: Option<image::Handle>,
    mb_album_image: Option<image::Handle>,
    spinner_angle: f32,
    search: String,
    /// Treffer der aktiven Suche (None = keine Suche)
    filter: Option<SearchHits>,
    /// Zähler, um veraltete Suchläufe abzubrechen
    search_generation: Arc<AtomicU64>,
    /// Spaltenaufteilung (Künstler | Alben | Titel), per Maus verschiebbar
    columns: pane_grid::State<Column>,

    artists: Vec<Artist>,
    albums: Vec<Album>,
    tracks: Vec<Track>, // aktuell angezeigte Titel

    selected_artist: Option<i64>,
    /// Cover der Album-Köpfe (None = keins gefunden); `cover_pending` = Laden läuft
    album_covers: HashMap<i64, Option<image::Handle>>,
    cover_pending: HashSet<i64>,

    /// Compilations-Ansicht: Alben/Titel des Künstlers „Compilations“
    comp_columns: pane_grid::State<Column>,
    comp_artist: Option<i64>,
    comp_albums: Vec<Album>,
    comp_tracks: Vec<Track>,
    comp_selected_album: Option<i64>,
    /// Enthält die Mediathek überhaupt Künstler? (unabhängig von Filter/„Compilations“)
    library_empty: bool,
    /// Gewähltes Theme (siehe Einstellungen) und alle Optionen
    theme: Theme,
    theme_options: Vec<Theme>,
    /// Mixtapes per ffmpeg konvertieren (Einstellung), Zielformat, gefundenes ffmpeg
    mixtape_convert: bool,
    mixtape_format: mixtape::Format,
    ffmpeg: Option<PathBuf>,
    /// Anzeigenamen aus den Tags: Künstler-Ordner -> Name, (Künstler-Ordner, Album-Ordner) -> Titel
    artist_names: std::collections::HashMap<String, String>,
    album_titles: std::collections::HashMap<(String, String), String>,

    /// Playlists-Ansicht
    pl_columns: pane_grid::State<Column>,
    playlists: Vec<Playlist>,
    pl_selected: Option<i64>,
    pl_tracks: Vec<Track>,
    playlist_dialog: Option<PlaylistDialog>,

    queue: Vec<Track>,
    queue_index: Option<usize>,
    /// Cover des laufenden Titels (falls in der Datei eingebettet)
    cover: Option<image::Handle>,
    seek_preview: Option<f32>,
    status: Option<String>,
}

impl PlayerApp {
    fn new(library: Library, audio: Audio, db_path: PathBuf) -> (Self, Task<Message>) {
        let mut app = Self {
            library,
            db_path,
            audio,
            device_menu_open: false,
            devices: Vec::new(),
            access: None,
            phase: Phase::NeedsFolder,
            notice: None,
            view: View::Music,
            tag_editor: None,
            mb_running: false,
            mb_cancel: Arc::new(AtomicBool::new(false)),
            mb_progress: Arc::new(musicbrainz::Progress::default()),
            mb_result: None,
            mb_albums: Vec::new(),
            mb_visible: Vec::new(),
            mb_selected: None,
            mb_artist_image: None,
            mb_album_image: None,
            spinner_angle: 0.0,
            search: String::new(),
            filter: None,
            search_generation: Arc::new(AtomicU64::new(0)),
            columns: pane_grid::State::with_configuration(pane_grid::Configuration::Split {
                axis: pane_grid::Axis::Vertical,
                ratio: 0.25,
                a: Box::new(pane_grid::Configuration::Pane(Column::Artists)),
                b: Box::new(pane_grid::Configuration::Pane(Column::Albums)),
            }),
            comp_columns: pane_grid::State::with_configuration(pane_grid::Configuration::Split {
                axis: pane_grid::Axis::Vertical,
                ratio: 0.3,
                a: Box::new(pane_grid::Configuration::Pane(Column::Albums)),
                b: Box::new(pane_grid::Configuration::Pane(Column::Tracks)),
            }),
            comp_artist: None,
            comp_albums: Vec::new(),
            comp_tracks: Vec::new(),
            comp_selected_album: None,
            library_empty: true,
            theme: halloy_theme(),
            theme_options: theme_options(),
            mixtape_convert: false,
            mixtape_format: mixtape::Format::Opus,
            ffmpeg: mixtape::find_ffmpeg(),
            artist_names: Default::default(),
            album_titles: Default::default(),
            pl_columns: pane_grid::State::with_configuration(pane_grid::Configuration::Split {
                axis: pane_grid::Axis::Vertical,
                ratio: 0.3,
                a: Box::new(pane_grid::Configuration::Pane(Column::Playlists)),
                b: Box::new(pane_grid::Configuration::Pane(Column::Tracks)),
            }),
            playlists: Vec::new(),
            pl_selected: None,
            pl_tracks: Vec::new(),
            playlist_dialog: None,
            artists: Vec::new(),
            albums: Vec::new(),
            tracks: Vec::new(),
            selected_artist: None,
            album_covers: HashMap::new(),
            cover_pending: HashSet::new(),
            queue: Vec::new(),
            queue_index: None,
            cover: None,
            seek_preview: None,
            status: None,
        };
        // Gespeichertes Theme laden (sonst Halloy)
        if let Some(name) = app.library.setting("theme") {
            if let Some(t) = app.theme_options.iter().find(|t| t.to_string() == name) {
                app.theme = t.clone();
            }
        }
        set_colors(&app.theme);
        app.mixtape_convert = app.library.setting("mixtape_convert").as_deref() == Some("1");
        if let Some(f) = app
            .library
            .setting("mixtape_format")
            .and_then(|k| mixtape::Format::from_key(&k))
        {
            app.mixtape_format = f;
        }
        let task = app.restore_folder();
        (app, task)
    }

    /// Beim Start: den in der DB gespeicherten Ordner (+ Bookmark) wieder öffnen.
    fn restore_folder(&mut self) -> Task<Message> {
        let stored = match self.library.music_root() {
            Ok(Some(root)) => root,
            Ok(None) => return Task::none(), // erster Start -> Willkommensseite
            Err(e) => {
                self.notice = Some(format!("Datenbank-Fehler: {e}"));
                return Task::none();
            }
        };

        match FolderAccess::restore(&stored.path, stored.bookmark.as_deref()) {
            Ok(restored) => {
                let moved = restored.access.path() != stored.path;
                if moved || restored.refreshed_bookmark.is_some() {
                    // Ordner wurde verschoben oder das Bookmark war veraltet
                    let _ = self.library.set_music_root(&MusicRoot {
                        path: restored.access.path().to_path_buf(),
                        bookmark: restored.refreshed_bookmark.or(stored.bookmark),
                    });
                }
                self.access = Some(restored.access);

                if moved || !self.library.has_tracks() {
                    self.start_scan()
                } else {
                    self.reload_library_view();
                    self.cover_task()
                }
            }
            Err(e) => {
                self.notice = Some(format!(
                    "Kein Zugriff mehr auf den Musikordner ({e}). Bitte den Ordner neu auswählen."
                ));
                Task::none()
            }
        }
    }

    /// Mediathek im Hintergrund neu aufbauen.
    fn start_scan(&mut self) -> Task<Message> {
        let Some(access) = &self.access else {
            return Task::none();
        };
        self.phase = Phase::Scanning;
        self.notice = None;
        let db_path = self.db_path.clone();
        let root = access.path().to_path_buf();

        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    db::rebuild_from(&db_path, &root).map_err(|e| format!("{e:#}"))
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            },
            Message::ScanFinished,
        )
    }

    fn reload_library_view(&mut self) {
        self.artist_names = self.library.artist_names().unwrap_or_default();
        self.album_titles = self.library.album_titles().unwrap_or_default();
        self.artists = self.library.artists().unwrap_or_default();
        self.library_empty = self.artists.is_empty();
        // „Compilations“ hat eine eigene Ansicht und erscheint nicht unter „Music“.
        self.comp_artist = self
            .artists
            .iter()
            .find(|a| a.name == COMPILATIONS)
            .map(|a| a.id);
        self.artists.retain(|a| a.name != COMPILATIONS);
        if let Some(f) = &self.filter {
            self.artists.retain(|a| f.artists.contains(&a.id));
        }
        self.load_compilations();
        self.load_playlists();
        self.albums.clear();
        self.tracks.clear();
        self.selected_artist = None;
        if let Some(first) = self.artists.first().map(|a| a.id) {
            self.load_artist(first);
        }
        self.phase = Phase::Ready;
    }

    /// Wechselt zur Musikansicht; verlässt man den MusicBrainz-Tab, bricht ein
    /// laufender MusicBrainz-Abgleich nach der aktuellen Abfrage ab.
    fn show_view(&mut self, view: View) {
        if view != View::MusicBrainz {
            self.mb_cancel.store(true, Ordering::Relaxed);
        }
        self.view = view;
        if view == View::Music {
            // inzwischen geladene MusicBrainz-Bilder berücksichtigen
            self.album_covers.retain(|_, c| c.is_some());
        }
        if view == View::MusicBrainz {
            self.reload_mb_albums();
        }
    }

    fn reload_mb_albums(&mut self) {
        self.mb_albums = musicbrainz::list_albums(&self.db_path).unwrap_or_else(|e| {
            log::warn!("MusicBrainz-Liste: {e:#}");
            Vec::new()
        });
        self.mb_selected = None;
        self.mb_artist_image = None;
        self.mb_album_image = None;
        self.refresh_mb_filter();
    }

    /// Wendet die Suche auf die MusicBrainz-Liste an (Künstler oder Album enthält
    /// den Suchbegriff; passt der Künstler, sind alle seine Alben sichtbar).
    fn refresh_mb_filter(&mut self) {
        let query = self
            .filter
            .is_some()
            .then(|| self.search.trim().to_lowercase());
        self.mb_visible = self
            .mb_albums
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                query.as_ref().is_none_or(|q| {
                    e.artist.to_lowercase().contains(q)
                        || e.artist_display.to_lowercase().contains(q)
                        || e.album.to_lowercase().contains(q)
                        || e.display.to_lowercase().contains(q)
                })
            })
            .map(|(i, _)| i)
            .collect();
        if self
            .mb_selected
            .is_some_and(|i| !self.mb_visible.contains(&i))
        {
            self.mb_selected = None;
            self.mb_artist_image = None;
            self.mb_album_image = None;
        }
    }

    /// Ausgabegerät ist verschwunden: Wiedergabe pausieren und auf den Systemstandard
    /// umstellen (nur in diesem Fall; sonst wird nie automatisch gewechselt).
    fn handle_device_lost(&mut self) {
        let was_playing = self.audio.is_active();
        self.status = Some(match self.audio.recover_from_lost_device() {
            Ok(()) if was_playing => {
                "Ausgabegerät nicht mehr verfügbar – Wiedergabe pausiert, Systemstandard aktiv"
                    .to_string()
            }
            Ok(()) => "Ausgabegerät nicht mehr verfügbar – Systemstandard aktiv".to_string(),
            Err(e) => format!("Ausgabegerät nicht mehr verfügbar: {e:#}"),
        });
    }

    fn stop_playback(&mut self) {
        self.audio.stop();
        self.queue.clear();
        self.queue_index = None;
        self.cover = None;
        self.seek_preview = None;
        self.status = None;
    }

    fn subscription(&self) -> Subscription<Message> {
        // Ticker nur, solange etwas läuft (Fortschritt + automatisch weiter).
        let ticker = if self.audio.is_active() {
            time::every(Duration::from_millis(250)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };

        // Leertaste = Play/Pause (nur wenn kein Widget die Taste verbraucht hat)
        let keys = event::listen_with(|event, status, _window| match (event, status) {
            (
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Space),
                    ..
                }),
                event::Status::Ignored,
            ) => Some(Message::TogglePause),
            // „t“ (ohne Modifier, nur wenn kein Textfeld die Taste verbraucht hat)
            (
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(c),
                    modifiers,
                    ..
                }),
                event::Status::Ignored,
            ) if c.as_str().eq_ignore_ascii_case("t") && modifiers.is_empty() => {
                Some(Message::OpenTagEditor)
            }
            // „p“: Titel zu Playlist hinzufügen
            (
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(c),
                    modifiers,
                    ..
                }),
                event::Status::Ignored,
            ) if c.as_str().eq_ignore_ascii_case("p") && modifiers.is_empty() => {
                Some(Message::OpenAddToPlaylist)
            }
            (
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    ..
                }),
                _,
            ) => Some(Message::Escape),
            (
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key:
                        keyboard::Key::Named(
                            named @ (keyboard::key::Named::ArrowUp
                            | keyboard::key::Named::ArrowDown),
                        ),
                    modifiers,
                    ..
                }),
                _,
            ) if modifiers.alt() && !modifiers.command() => Some(Message::MovePlaylistTrack(
                if named == keyboard::key::Named::ArrowUp {
                    -1
                } else {
                    1
                },
            )),
            (
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Tab),
                    modifiers,
                    ..
                }),
                _,
            ) => Some(Message::TabPressed(modifiers.shift())),
            _ => None,
        });

        let spinner = if self.mb_running && self.view == View::MusicBrainz {
            time::every(Duration::from_millis(30)).map(|_| Message::SpinnerTick)
        } else {
            Subscription::none()
        };

        Subscription::batch([ticker, keys, spinner])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SelectArtist(id) => {
                self.load_artist(id);
                return self.cover_task();
            }
            Message::AlbumCovers(covers) => {
                for (id, bytes) in covers {
                    self.cover_pending.remove(&id);
                    self.album_covers
                        .insert(id, bytes.map(image::Handle::from_bytes));
                }
            }
            Message::PlayTrack(i) => {
                let list = self.visible_tracks();
                let Some(clicked) = list.get(i) else {
                    return Task::none();
                };
                if clicked.missing {
                    self.status = Some(format!(
                        "Datei nicht in der Mediathek: {}",
                        clicked.path.display()
                    ));
                    return Task::none();
                }
                // Fehlende Playlist-Einträge überspringen
                let clicked_id = clicked.id;
                let queue: Vec<Track> = list.iter().filter(|t| !t.missing).cloned().collect();
                let index = queue.iter().position(|t| t.id == clicked_id).unwrap_or(0);
                self.queue = queue;
                self.play_at(index);
            }
            Message::TogglePause => {
                if self.tag_editor.is_none() && self.playlist_dialog.is_none() {
                    self.toggle_pause();
                }
            }
            Message::Next => self.next(),
            Message::Previous => self.previous(),
            Message::Tick => {
                // Gerät verschwunden (Callback-Flag, kein Treiberzugriff): pausieren +
                // Systemstandard
                if self.audio.take_device_lost() {
                    self.handle_device_lost();
                }
                if self.audio.take_finished() {
                    self.next();
                }
            }
            Message::Seek(secs) => self.seek_preview = Some(secs),
            Message::SeekRelease => {
                if let Some(secs) = self.seek_preview.take() {
                    if let Err(e) = self.audio.seek(Duration::from_secs_f32(secs)) {
                        self.status = Some(e.to_string());
                    }
                }
            }
            Message::Volume(v) => self.audio.set_volume(v),

            Message::PaneResized(e) => self.columns.resize(e.split, e.ratio),
            Message::ShowView(view) => {
                if view == View::Settings {
                    // ffmpeg könnte inzwischen installiert worden sein
                    self.ffmpeg = mixtape::find_ffmpeg();
                }
                self.show_view(view);
                if view == View::Music {
                    return self.cover_task();
                }
            }
            Message::SelectCompAlbum(a) => self.select_comp_album(a),
            Message::SelectPlaylist(id) => self.select_playlist(Some(id)),
            Message::ThemeSelected(theme) => {
                set_colors(&theme);
                if let Err(e) = self.library.set_setting("theme", &theme.to_string()) {
                    self.status = Some(format!("Theme konnte nicht gespeichert werden: {e:#}"));
                }
                self.theme = theme;
            }
            Message::MixtapeConvert(on) => {
                self.mixtape_convert = on;
                if let Err(e) = self
                    .library
                    .set_setting("mixtape_convert", if on { "1" } else { "0" })
                {
                    self.status = Some(format!(
                        "Einstellung konnte nicht gespeichert werden: {e:#}"
                    ));
                }
            }
            Message::MixtapeFormat(f) => {
                self.mixtape_format = f;
                if let Err(e) = self.library.set_setting("mixtape_format", f.key()) {
                    self.status = Some(format!(
                        "Einstellung konnte nicht gespeichert werden: {e:#}"
                    ));
                }
            }
            Message::ToggleDeviceMenu => {
                self.device_menu_open = !self.device_menu_open;
                if self.device_menu_open {
                    // Geräteliste im Hintergrund holen (Treiberabfrage kann dauern)
                    return Task::perform(
                        async {
                            tokio::task::spawn_blocking(audio::output_devices)
                                .await
                                .unwrap_or_default()
                        },
                        Message::DevicesLoaded,
                    );
                }
            }
            Message::DevicesLoaded(devices) => {
                // Das aktuelle Gerät fehlt in der Liste (z. B. getrennt, bevor der Callback
                // gemeldet hat) -> wie „Gerät verschwunden“ behandeln.
                let gone = !devices.is_empty()
                    && self
                        .audio
                        .device()
                        .is_some_and(|d| !devices.iter().any(|n| n == d));
                self.devices = devices;
                if gone {
                    self.handle_device_lost();
                }
            }
            Message::SelectDevice(name) => {
                self.device_menu_open = false;
                if let Err(e) = self.audio.set_device(Some(&name)) {
                    self.status = Some(format!("Ausgabegerät: {e:#}"));
                }
            }
            Message::Escape => {
                if self.device_menu_open {
                    self.device_menu_open = false;
                } else if self.tag_editor.as_ref().is_some_and(|ed| !ed.saving) {
                    self.tag_editor = None;
                } else {
                    self.playlist_dialog = None;
                }
            }
            Message::OpenAddToPlaylist => {
                if !matches!(
                    self.view,
                    View::Music | View::Compilations | View::Playlists
                ) || self.tag_editor.is_some()
                    || self.playlist_dialog.is_some()
                {
                    return Task::none();
                }
                if let Some(t) = self.current_track() {
                    self.playlist_dialog = Some(PlaylistDialog::AddTrack {
                        path: t.path.clone(),
                        title: t.title.clone(),
                        error: None,
                    });
                }
            }
            Message::NewPlaylist => {
                self.playlist_dialog = Some(PlaylistDialog::New {
                    name: String::new(),
                    add: None,
                    error: None,
                });
                return iced::widget::operation::focus(PLAYLIST_NAME_FIELD);
            }
            Message::AddToNewPlaylist => {
                if let Some(PlaylistDialog::AddTrack { path, .. }) = &self.playlist_dialog {
                    self.playlist_dialog = Some(PlaylistDialog::New {
                        name: String::new(),
                        add: Some(path.clone()),
                        error: None,
                    });
                    return iced::widget::operation::focus(PLAYLIST_NAME_FIELD);
                }
            }
            Message::PlaylistName(v) => match &mut self.playlist_dialog {
                Some(PlaylistDialog::New { name, error, .. })
                | Some(PlaylistDialog::Rename { name, error, .. }) => {
                    *name = v;
                    *error = None;
                }
                _ => {}
            },
            Message::ClosePlaylistDialog => self.playlist_dialog = None,
            Message::Mixtape(id) => {
                let Some(p) = self.playlists.iter().find(|p| p.id == id) else {
                    return Task::none();
                };
                if self.mixtape_convert {
                    self.ffmpeg = mixtape::find_ffmpeg();
                    if self.ffmpeg.is_none() {
                        self.status = Some(
                            "Mixtape: Konvertierung ist aktiv, aber ffmpeg wurde nicht gefunden \
                             (Einstellungen)"
                                .into(),
                        );
                        return Task::none();
                    }
                }
                let default_name = format!("{}.zip", db::sanitize_folder_name(&p.name));
                return Task::perform(
                    async move {
                        folder::pick_save_zip("Mixtape speichern unter …", &default_name).await
                    },
                    move |target| Message::MixtapeTarget(id, target),
                );
            }
            Message::MixtapeTarget(_, None) => {}
            Message::MixtapeTarget(id, Some(target)) => {
                let Some(name) = self
                    .playlists
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.name.clone())
                else {
                    return Task::none();
                };
                let files: Vec<PathBuf> = match self.library.playlist_tracks(id) {
                    Ok(t) => t.into_iter().map(|t| t.path).collect(),
                    Err(e) => {
                        self.status = Some(format!("Mixtape fehlgeschlagen: {}", db_error(&e)));
                        return Task::none();
                    }
                };
                // Konvertierung nur, wenn aktiviert und ffmpeg vorhanden (Originale bleiben unberührt)
                let convert = match (self.mixtape_convert, &self.ffmpeg) {
                    (true, Some(ffmpeg)) => Some(mixtape::Convert {
                        ffmpeg: ffmpeg.clone(),
                        format: self.mixtape_format,
                    }),
                    _ => None,
                };
                self.status = Some(if convert.is_some() {
                    format!("Mixtape „{name}“ wird erstellt und konvertiert …")
                } else {
                    format!("Mixtape „{name}“ wird erstellt …")
                });
                // ZIP schreiben im Hintergrund (viele/große Dateien)
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            mixtape::create(&target, &name, &files, convert.as_ref())
                                .map(|o| (o.written, o.skipped, o.converted, o.target))
                                .map_err(|e| format!("{e:#}"))
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    Message::MixtapeDone,
                );
            }
            Message::MixtapeDone(res) => {
                self.status = Some(match res {
                    Ok((written, skipped, converted, target)) => {
                        let file = target
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let mut extra = String::new();
                        if converted > 0 {
                            extra.push_str(&format!(", {converted} konvertiert"));
                        }
                        if skipped > 0 {
                            extra.push_str(&format!(", {skipped} nicht gefunden"));
                        }
                        format!("Mixtape gespeichert: {file} ({written} Titel{extra})")
                    }
                    Err(e) => format!("Mixtape fehlgeschlagen: {e}"),
                });
            }
            Message::AskRenamePlaylist(id) => {
                if let Some(p) = self.playlists.iter().find(|p| p.id == id) {
                    self.playlist_dialog = Some(PlaylistDialog::Rename {
                        id,
                        name: p.name.clone(),
                        error: None,
                    });
                    return iced::widget::operation::focus(PLAYLIST_NAME_FIELD);
                }
            }
            Message::MovePlaylistTrack(delta) => {
                if self.view != View::Playlists
                    || self.tag_editor.is_some()
                    || self.playlist_dialog.is_some()
                {
                    return Task::none();
                }
                let (Some(id), Some(track)) = (self.pl_selected, self.current_track()) else {
                    return Task::none();
                };
                let (track_id, path) = (track.id, track.path.clone());
                match self.library.move_in_playlist(id, &path, delta) {
                    Ok(true) => {
                        self.select_playlist(Some(id));
                        // Abspielliste an die neue Reihenfolge angleichen
                        let queue: Vec<Track> = self
                            .pl_tracks
                            .iter()
                            .filter(|t| !t.missing)
                            .cloned()
                            .collect();
                        if let Some(i) = queue.iter().position(|t| t.id == track_id) {
                            self.queue = queue;
                            self.queue_index = Some(i);
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        self.status = Some(format!("Verschieben fehlgeschlagen: {}", db_error(&e)))
                    }
                }
            }
            Message::AskDeletePlaylist(id) => {
                if let Some(p) = self.playlists.iter().find(|p| p.id == id) {
                    self.playlist_dialog = Some(PlaylistDialog::Delete {
                        id,
                        name: p.name.clone(),
                    });
                }
            }
            Message::AddToPlaylist(id) => {
                if let Some(PlaylistDialog::AddTrack { path, error, .. }) =
                    &mut self.playlist_dialog
                {
                    match self.library.add_to_playlist(id, path) {
                        Ok(true) => {
                            self.playlist_dialog = None;
                            self.load_playlists();
                        }
                        Ok(false) => *error = Some("Der Titel ist schon in dieser Playlist".into()),
                        Err(e) => *error = Some(db_error(&e)),
                    }
                }
            }
            Message::ConfirmPlaylistDialog => match &mut self.playlist_dialog {
                Some(PlaylistDialog::New { name, add, error }) => {
                    match self.library.create_playlist(name) {
                        Ok(id) => {
                            if let Some(path) = add {
                                if let Err(e) = self.library.add_to_playlist(id, path) {
                                    *error = Some(db_error(&e));
                                    return Task::none();
                                }
                            }
                            self.playlist_dialog = None;
                            self.pl_selected = Some(id);
                            self.load_playlists();
                        }
                        Err(e) => *error = Some(db_error(&e)),
                    }
                }
                Some(PlaylistDialog::Rename { id, name, error }) => {
                    match self.library.rename_playlist(*id, name) {
                        Ok(()) => {
                            self.playlist_dialog = None;
                            self.load_playlists();
                        }
                        Err(e) => *error = Some(db_error(&e)),
                    }
                }
                Some(PlaylistDialog::Delete { id, .. }) => {
                    let id = *id;
                    if let Err(e) = self.library.delete_playlist(id) {
                        self.status =
                            Some(format!("Playlist löschen fehlgeschlagen: {}", db_error(&e)));
                    }
                    self.playlist_dialog = None;
                    if self.pl_selected == Some(id) {
                        self.pl_selected = None;
                    }
                    self.load_playlists();
                }
                _ => {}
            },
            Message::RemoveFromPlaylist(path) => {
                if let Some(id) = self.pl_selected {
                    if let Err(e) = self.library.remove_from_playlist(id, &path) {
                        self.status = Some(format!("Entfernen fehlgeschlagen: {}", db_error(&e)));
                    }
                    self.select_playlist(Some(id));
                }
            }
            Message::PlaylistPaneResized(e) => self.pl_columns.resize(e.split, e.ratio),
            Message::CompPaneResized(e) => self.comp_columns.resize(e.split, e.ratio),
            Message::OpenTagEditor => {
                if !matches!(
                    self.view,
                    View::Music | View::Compilations | View::Playlists
                ) || self.tag_editor.is_some()
                    || self.playlist_dialog.is_some()
                {
                    return Task::none();
                }
                let Some(track) = self.current_track() else {
                    return Task::none();
                };
                let (track_id, path) = (track.id, track.path.clone());
                let rel_path = self
                    .access
                    .as_ref()
                    .and_then(|a| path.strip_prefix(a.path()).ok())
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                match db::read_tag_edit(&path) {
                    Ok(fields) => {
                        self.tag_editor = Some(TagEditor {
                            track_id,
                            path,
                            rel_path,
                            fields,
                            error: None,
                            saving: false,
                        });
                        return iced::widget::operation::focus(TAG_FIRST_FIELD);
                    }
                    Err(e) => self.status = Some(format!("Tags nicht lesbar: {e:#}")),
                }
            }
            Message::CancelTagEditor => {
                if self.tag_editor.as_ref().is_some_and(|ed| !ed.saving) {
                    self.tag_editor = None;
                }
            }
            Message::RevealFile(path) => folder::reveal_in_file_manager(&path),
            Message::TabPressed(shift) => {
                if self.tag_editor.is_some() || self.playlist_dialog.is_some() {
                    return if shift {
                        iced::widget::operation::focus_previous()
                    } else {
                        iced::widget::operation::focus_next()
                    };
                }
            }
            Message::EditTag(field, value) => {
                if let Some(ed) = &mut self.tag_editor {
                    let f = &mut ed.fields;
                    *match field {
                        TagField::Title => &mut f.title,
                        TagField::Artist => &mut f.artist,
                        TagField::Album => &mut f.album,
                        TagField::AlbumArtist => &mut f.album_artist,
                        TagField::Composer => &mut f.composer,
                        TagField::Genre => &mut f.genre,
                        TagField::TrackNo => &mut f.track_no,
                        TagField::TrackTotal => &mut f.track_total,
                        TagField::DiscNo => &mut f.disc_no,
                        TagField::Year => &mut f.year,
                    } = value;
                    ed.error = None;
                }
            }
            Message::EditCompilation(v) => {
                if let Some(ed) = &mut self.tag_editor {
                    ed.fields.compilation = v;
                }
            }
            Message::SaveTags => {
                let Some(ed) = &mut self.tag_editor else {
                    return Task::none();
                };
                if ed.saving {
                    return Task::none();
                }
                if let Err(e) = ed.fields.validate() {
                    ed.error = Some(e.to_string());
                    return Task::none();
                }
                ed.saving = true;
                let (path, fields) = (ed.path.clone(), ed.fields.clone());
                // Datei schreiben im Hintergrund (kann bei großen Dateien dauern)
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            db::write_tag_edit(&path, &fields).map_err(|e| format!("{e:#}"))
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    Message::TagsSaved,
                );
            }
            Message::TagsSaved(Err(e)) => {
                if let Some(ed) = &mut self.tag_editor {
                    ed.saving = false;
                    ed.error = Some(format!("Schreiben fehlgeschlagen: {e}"));
                }
            }
            Message::TagsSaved(Ok(())) => {
                if let Some(ed) = self.tag_editor.take() {
                    self.apply_saved_tags(&ed);
                }
            }
            Message::SelectMbAlbum(i) => {
                self.mb_selected = Some(i);
                self.mb_artist_image = None;
                self.mb_album_image = None;
                let Some(e) = self.mb_albums.get(i) else {
                    return Task::none();
                };
                let (artist, album) = (e.artist.clone(), e.album.clone());
                let db_path = self.db_path.clone();
                let (a, b) = (artist.clone(), album.clone());
                // Nur gespeicherte Bilder lesen (kein Netzwerk); Download erfolgt über den Knopf
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            musicbrainz::read_images(&db_path, &a, &b).ok()
                        })
                        .await
                        .ok()
                        .flatten()
                    },
                    move |imgs| Message::MbImages(artist.clone(), album.clone(), imgs),
                );
            }
            Message::MbImages(artist, album, imgs) => {
                // Auswahl inzwischen geändert?
                let still = self
                    .mb_selected
                    .and_then(|i| self.mb_albums.get(i))
                    .is_some_and(|e| e.artist == artist && e.album == album);
                if let (true, Some(imgs)) = (still, imgs) {
                    let handle = |b: Option<Vec<u8>>| {
                        b.filter(|b| !b.is_empty()).map(image::Handle::from_bytes)
                    };
                    self.mb_artist_image = handle(imgs.artist);
                    self.mb_album_image = handle(imgs.album);
                }
            }
            Message::MusicBrainz => {
                if self.mb_running {
                    return Task::none();
                }
                self.mb_running = true;
                self.mb_result = None;
                let cancel = Arc::new(AtomicBool::new(false));
                self.mb_cancel = cancel.clone();
                let db_path = self.db_path.clone();
                let progress = self.mb_progress.clone();
                progress.current.store(0, Ordering::Relaxed);
                progress.total.store(0, Ordering::Relaxed);
                let (tx, rx) = tokio::sync::oneshot::channel();
                std::thread::spawn(move || {
                    let res = musicbrainz::lookup_missing(&db_path, &cancel, &progress)
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(res);
                });
                return Task::perform(
                    async move { rx.await.unwrap_or_else(|e| Err(e.to_string())) },
                    Message::MusicBrainzDone,
                );
            }
            Message::MusicBrainzDone(res) => {
                self.mb_running = false;
                if self.view == View::MusicBrainz {
                    self.reload_mb_albums();
                }
                self.mb_result = Some(match res {
                    Ok((updated, checked, images)) => format!(
                        "{updated} neue MBIDs eingetragen ({checked} Alben geprüft), {images} Bilder"
                    ),
                    Err(e) => format!("MusicBrainz fehlgeschlagen: {e}"),
                });
            }
            Message::SpinnerTick => {
                self.spinner_angle = (self.spinner_angle + 0.2) % std::f32::consts::TAU
            }
            Message::PickFolder => {
                let start = self.access.as_ref().map(|a| a.path().to_path_buf());
                return Task::perform(folder::pick_folder(start), Message::FolderPicked);
            }
            Message::FolderPicked(None) => {}
            Message::FolderPicked(Some(path)) => {
                self.show_view(View::Music);
                self.stop_playback();

                // Zugriff sichern (macOS: + Security-Scoped Bookmark erzeugen) …
                let (access, bookmark) = FolderAccess::from_picked(path.clone());

                // … und das Ergebnis des Ordner-Dialogs in der Datenbank speichern.
                if let Err(e) = self.library.set_music_root(&MusicRoot { path, bookmark }) {
                    self.notice = Some(format!("Ordner konnte nicht gespeichert werden: {e}"));
                    return Task::none();
                }
                self.access = Some(access); // gibt den Zugriff auf den alten Ordner frei
                return self.start_scan();
            }
            Message::Rescan => {
                self.show_view(View::Music);
                if self.access.is_some() {
                    self.stop_playback();
                    return self.start_scan();
                }
            }
            Message::SearchChanged(value) => {
                self.search = value;
                let query = self.search.trim().to_string();
                // Jede Eingabe macht laufende Suchen ungültig.
                let generation = self.search_generation.fetch_add(1, Ordering::Relaxed) + 1;
                if query.chars().count() < 2 {
                    if self.filter.take().is_some() {
                        self.reload_library_view();
                        self.refresh_mb_filter();
                    }
                    return self.cover_task();
                }
                let counter = self.search_generation.clone();
                let db_path = self.db_path.clone();
                let (tx, rx) = tokio::sync::oneshot::channel();
                let q = query.clone();
                // Eigener Thread mit niedriger Priorität, damit die UI flüssig bleibt.
                std::thread::spawn(move || {
                    db::lower_thread_priority();
                    let _ = tx.send(
                        db::search(&db_path, &q, &counter, generation)
                            .ok()
                            .flatten(),
                    );
                });
                return Task::perform(async move { rx.await.ok().flatten() }, move |hits| {
                    Message::SearchDone(query.clone(), hits)
                });
            }
            Message::SearchDone(query, hits) => {
                // Veraltet (Eingabe inzwischen geändert) oder abgebrochen
                if query != self.search.trim() {
                    return Task::none();
                }
                if let Some(hits) = hits {
                    self.filter = Some(hits);
                    self.reload_library_view();
                    self.refresh_mb_filter();
                    return self.cover_task();
                }
            }
            Message::ScanFinished(Ok(count)) => {
                self.search.clear();
                self.filter = None;
                self.refresh_mb_filter();
                // Album-IDs ändern sich beim Scan -> Cover-Cache verwerfen
                self.album_covers.clear();
                self.cover_pending.clear();
                self.reload_library_view();
                self.notice = (count == 0).then(|| {
                    "Keine Musikdateien gefunden. Erwartet wird die Struktur \
                     Künstler/Album/Titel (mp3, aac, m4a, opus)."
                        .to_string()
                });
                return self.cover_task();
            }
            Message::ScanFinished(Err(e)) => {
                self.phase = Phase::NeedsFolder;
                self.notice = Some(format!("Einlesen fehlgeschlagen: {e}"));
            }
        }
        Task::none()
    }

    /// Übernimmt gespeicherte Tags in DB und angezeigte Listen (ohne Neuscan).
    fn apply_saved_tags(&mut self, ed: &TagEditor) {
        if let Err(e) = self
            .library
            .update_track_tags(ed.track_id, &ed.path, &ed.fields)
        {
            self.status = Some(format!("Datenbank-Update fehlgeschlagen: {e:#}"));
            return;
        }
        let f = &ed.fields;
        let non_empty = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
        let title = non_empty(&f.title).unwrap_or_else(|| {
            ed.path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        let track_no = f.track_no.trim().parse().ok();
        for t in self
            .tracks
            .iter_mut()
            .chain(self.comp_tracks.iter_mut())
            .chain(self.pl_tracks.iter_mut())
            .chain(self.queue.iter_mut())
            .filter(|t| t.id == ed.track_id)
        {
            t.title = title.clone();
            t.artist_tag = non_empty(&f.artist);
            t.album_tag = non_empty(&f.album);
            t.track_no = track_no;
            t.compilation = f.compilation;
        }
    }

    // --- Navigation ------------------------------------------------------

    fn load_compilations(&mut self) {
        self.comp_selected_album = None;
        (self.comp_albums, self.comp_tracks) = match self.comp_artist {
            Some(id) => (
                self.library.albums(id).unwrap_or_default(),
                self.library.tracks_by_artist(id).unwrap_or_default(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        self.apply_filter();
    }

    fn load_playlists(&mut self) {
        self.playlists = self.library.playlists().unwrap_or_default();
        let keep = self
            .pl_selected
            .filter(|id| self.playlists.iter().any(|p| p.id == *id))
            .or_else(|| self.playlists.first().map(|p| p.id));
        self.select_playlist(keep);
    }

    fn select_playlist(&mut self, id: Option<i64>) {
        self.pl_selected = id;
        self.pl_tracks = id
            .map(|id| self.library.playlist_tracks(id).unwrap_or_default())
            .unwrap_or_default();
        self.apply_filter();
    }

    fn select_comp_album(&mut self, album: Option<i64>) {
        self.comp_selected_album = album;
        let Some(artist) = self.comp_artist else {
            return;
        };
        self.comp_tracks = match album {
            Some(id) => self.library.tracks_by_album(id),
            None => self.library.tracks_by_artist(artist),
        }
        .unwrap_or_default();
        self.apply_filter();
    }

    /// Titel der aktuell sichtbaren Ansicht (Quelle für Abspielliste).
    fn visible_tracks(&self) -> &[Track] {
        match self.view {
            View::Compilations => &self.comp_tracks,
            View::Playlists => &self.pl_tracks,
            _ => &self.tracks,
        }
    }

    fn load_artist(&mut self, id: i64) {
        self.selected_artist = Some(id);
        self.albums = self.library.albums(id).unwrap_or_default();
        self.tracks = self.library.tracks_by_artist(id).unwrap_or_default();
        // Titel dieses Künstlers auf Samplern (Ordner „Compilations“) hängen hinten an
        if let Some(a) = self.artists.iter().find(|a| a.id == id) {
            if a.folder != COMPILATIONS {
                let extra = self
                    .library
                    .compilation_tracks_for(&a.name, &a.folder)
                    .unwrap_or_default();
                self.tracks.extend(extra);
            }
        }
        self.apply_filter();
    }

    /// Blendet Alben/Titel aus, die nicht zur aktiven Suche passen.
    fn apply_filter(&mut self) {
        if let Some(f) = &self.filter {
            self.albums.retain(|a| f.albums.contains(&a.id));
            self.tracks.retain(|t| f.tracks.contains(&t.id));
            self.comp_albums.retain(|a| f.albums.contains(&a.id));
            self.comp_tracks.retain(|t| f.tracks.contains(&t.id));
        }
    }

    /// Lädt (im Hintergrund) die Cover der Alben des gewählten Künstlers, die noch
    /// nicht im Cache sind: eingebettetes Cover der ersten Datei, sonst das Bild
    /// aus musicbrainz.db (nur Albumbild).
    fn cover_task(&mut self) -> Task<Message> {
        let mut wanted: Vec<(i64, PathBuf, String, String)> = Vec::new();
        let mut seen = HashSet::new();
        for t in &self.tracks {
            if seen.insert(t.album_id)
                && !self.album_covers.contains_key(&t.album_id)
                && !self.cover_pending.contains(&t.album_id)
            {
                wanted.push((
                    t.album_id,
                    t.path.clone(),
                    t.artist_name.clone(),
                    t.album_title.clone(),
                ));
            }
        }
        if wanted.is_empty() {
            return Task::none();
        }
        self.cover_pending.extend(wanted.iter().map(|w| w.0));
        let db_path = self.db_path.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    wanted
                        .into_iter()
                        .map(|(id, path, artist, album)| {
                            // Album-Kopf: nur Albumbilder (kein Künstlerfoto als Cover)
                            let bytes = db::read_cover(&path).or_else(|| {
                                musicbrainz::read_images(&db_path, &artist, &album)
                                    .ok()
                                    .and_then(|i| i.album)
                                    .filter(|b| !b.is_empty())
                            });
                            (id, bytes)
                        })
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default()
            },
            Message::AlbumCovers,
        )
    }

    // --- Wiedergabe --------------------------------------------------------

    fn play_at(&mut self, index: usize) {
        let Some(track) = self.queue.get(index) else {
            return;
        };
        // War das Gerät inzwischen weg: erst auf den Systemstandard umstellen, dann spielen
        let lost = self.audio.take_device_lost();
        if lost {
            if let Err(e) = self.audio.recover_from_lost_device() {
                self.status = Some(format!("Ausgabegerät nicht mehr verfügbar: {e:#}"));
                return;
            }
        }
        match self.audio.play(&track.path) {
            Ok(()) => {
                self.queue_index = Some(index);
                // Eingebettetes Cover; sonst Album-, dann Künstlerbild aus musicbrainz.db
                self.cover = db::read_cover(&track.path)
                    .or_else(|| {
                        musicbrainz::cover_for(
                            &self.db_path,
                            &track.artist_name,
                            &track.album_title,
                        )
                    })
                    .map(image::Handle::from_bytes);
                self.seek_preview = None;
                self.status = lost.then(|| {
                    "Ausgabegerät nicht mehr verfügbar – Systemstandard aktiv".to_string()
                });
            }
            Err(e) if folder::is_permission_denied(&e) => {
                self.status = Some(
                    "Zugriff verweigert – bitte den Musikordner über „Ordner ändern …“ neu auswählen."
                        .to_string(),
                );
            }
            Err(e) => self.status = Some(format!("Fehler beim Abspielen: {e}")),
        }
    }

    fn next(&mut self) {
        match self.queue_index {
            Some(i) if i + 1 < self.queue.len() => self.play_at(i + 1),
            Some(_) => {
                self.audio.stop();
                self.queue_index = None;
                self.cover = None;
            }
            None => {}
        }
    }

    fn previous(&mut self) {
        if let Some(i) = self.queue_index {
            // wie üblich: nach 3 s zurück an den Anfang, sonst vorheriger Titel
            if self.audio.position() > Duration::from_secs(3) || i == 0 {
                self.play_at(i);
            } else {
                self.play_at(i - 1);
            }
        }
    }

    fn toggle_pause(&mut self) {
        if self.audio.is_active() {
            self.audio.toggle_pause();
        } else if self.queue_index.is_none() && !self.visible_tracks().is_empty() {
            self.queue = self.visible_tracks().to_vec();
            self.play_at(0);
        }
    }

    fn current_track(&self) -> Option<&Track> {
        self.queue_index.and_then(|i| self.queue.get(i))
    }

    // --- UI ----------------------------------------------------------------

    fn root_label(&self) -> String {
        self.access
            .as_ref()
            .map(|a| a.path().display().to_string())
            .unwrap_or_default()
    }

    fn view(&self) -> Element<'_, Message> {
        match self.phase {
            Phase::Scanning => container(
                column![
                    text("Mediathek wird aufgebaut …").size(22),
                    text(self.root_label()).size(12),
                ]
                .spacing(8)
                .align_x(Alignment::Center),
            )
            .center(Length::Fill)
            .into(),
            Phase::NeedsFolder => self.welcome(),
            Phase::Ready if self.library_empty => self.welcome(),
            Phase::Ready => {
                let browser = pane_grid(&self.columns, |_, kind, _| {
                    pane_grid::Content::new(match kind {
                        Column::Artists => self.artist_pane(),
                        Column::Albums => self.albums_pane(),
                        Column::Tracks | Column::Playlists => {
                            unreachable!("nur in Compilations/Playlists")
                        }
                    })
                })
                .spacing(8)
                .min_size(120)
                .on_resize(6, Message::PaneResized)
                .height(Length::Fill);

                let content: Element<'_, Message> = match self.view {
                    View::Music => browser.into(),
                    View::Compilations => self.compilations_view(),
                    View::Playlists => self.playlists_view(),
                    View::MusicBrainz => self.musicbrainz_view(),
                    View::Settings => self.settings_view(),
                    View::Help => self.help_view(),
                };
                let main = row![
                    self.sidebar(),
                    column![content, self.player_bar()].spacing(8)
                ]
                .spacing(8);
                let main = container(main).padding(Padding {
                    top: TITLEBAR_INSET,
                    right: 8.0,
                    bottom: 8.0,
                    left: 8.0,
                });
                // Immer derselbe Aufbau `stack![main, overlay]`: Würde die Ansicht beim Öffnen
                // eines Dialogs/Popovers von `main` auf `stack![main, …]` wechseln, verwirft
                // iced den Zustand des Baums (u. a. die Scrollpositionen aller Listen).
                let overlay: Element<'_, Message> = if self.tag_editor.is_some() {
                    self.tag_editor_dialog()
                } else if self.playlist_dialog.is_some() {
                    self.playlist_dialog_view()
                } else if self.device_menu_open {
                    self.device_menu()
                } else {
                    iced::widget::Space::new().into()
                };
                stack![main, overlay].into()
            }
        }
    }

    /// Startseite: noch kein (gültiger) Musikordner.
    fn welcome(&self) -> Element<'_, Message> {
        let mut col = column![
            text("FYA").size(28),
            text("Wähle den Ordner mit deiner Musik (Struktur: Künstler / Album / Titel)."),
        ]
        .spacing(12)
        .align_x(Alignment::Center);

        if let Some(notice) = &self.notice {
            col = col.push(text(notice.as_str()).size(13));
        }
        col = col.push(button("Musikordner wählen …").on_press(Message::PickFolder));

        container(col).center(Length::Fill).into()
    }

    /// Schmale Buttonleiste links (wie die Kanalliste in Halloy).
    fn sidebar(&self) -> Element<'_, Message> {
        let entry = |glyph: &'static [u8], label: &'static str, on_press: Message| {
            button(
                row![
                    icon(glyph, 16.0, col().text),
                    text(label).wrapping(Wrapping::None)
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .padding(Padding {
                top: 8.0,
                right: 18.0,
                bottom: 8.0,
                left: 10.0,
            })
            .style(|_: &Theme, status| {
                let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
                button::Style {
                    background: Some(if hovered { col().hover } else { col().panel }.into()),
                    text_color: col().text,
                    border: Border {
                        radius: 4.0.into(),
                        ..Border::default()
                    },
                    ..button::Style::default()
                }
            })
            .on_press(on_press)
        };
        let search = stack![
            text_input("Suche", &self.search)
                .on_input(Message::SearchChanged)
                .padding(Padding {
                    top: 7.0,
                    right: 8.0,
                    bottom: 7.0,
                    left: 30.0,
                })
                .size(13),
            container(icon(ICON_SEARCH, 14.0, col().dim))
                .padding(Padding {
                    left: 10.0,
                    ..Padding::ZERO
                })
                .height(Length::Fill)
                .align_y(Alignment::Center),
        ];
        column![
            search,
            entry(ICON_MUSIC, "Music", Message::ShowView(View::Music)),
            entry(
                ICON_COMPILATIONS,
                "Compilations",
                Message::ShowView(View::Compilations)
            ),
            entry(
                ICON_PLAYLISTS,
                "Playlists",
                Message::ShowView(View::Playlists)
            ),
            entry(
                ICON_HEXAGON,
                "MusicBrainz",
                Message::ShowView(View::MusicBrainz)
            ),
            entry(ICON_SETTINGS, "Settings", Message::ShowView(View::Settings)),
            entry(ICON_HELP, "Help", Message::ShowView(View::Help))
        ]
        .spacing(4)
        .width(Length::Fixed(157.0))
        .into()
    }

    /// Modaler Tag-Editor (Taste „t“); Klick daneben oder Esc schließt.
    fn tag_editor_dialog(&self) -> Element<'_, Message> {
        let Some(ed) = &self.tag_editor else {
            return container(text("")).into();
        };
        let f = &ed.fields;
        let saving = ed.saving;

        let field = |label: &'static str, value: &str, kind: TagField| {
            let mut input = text_input("", value).padding(6).size(14);
            if !saving {
                input = input
                    .on_input(move |v| Message::EditTag(kind, v))
                    .on_submit(Message::SaveTags);
            }
            if kind == TagField::Title {
                input = input.id(TAG_FIRST_FIELD);
            }
            row![
                container(text(label).size(13).color(col().dim)).width(110),
                input
            ]
            .spacing(8)
            .align_y(Alignment::Center)
        };
        let small = |label: &'static str, value: &str, kind: TagField| {
            let mut input = text_input("", value).padding(6).size(14).width(70);
            if !saving {
                input = input
                    .on_input(move |v| Message::EditTag(kind, v))
                    .on_submit(Message::SaveTags);
            }
            row![text(label).size(13).color(col().dim), input]
                .spacing(8)
                .align_y(Alignment::Center)
        };

        let mut compilation = checkbox(f.compilation).label("Compilation");
        if !saving {
            compilation = compilation.on_toggle(Message::EditCompilation);
        }

        let mut col = column![
            text("Metadaten bearbeiten").size(20),
            // Relativer Pfad; Klick zeigt die Datei im Finder
            button(text(ed.rel_path.as_str()).size(12))
                .padding(0)
                .style(|_: &Theme, status| button::Style {
                    text_color: if matches!(
                        status,
                        button::Status::Hovered | button::Status::Pressed
                    ) {
                        col().text
                    } else {
                        col().dim
                    },
                    ..button::Style::default()
                })
                .on_press(Message::RevealFile(ed.path.clone())),
            field("Titel", &f.title, TagField::Title),
            field("Künstler", &f.artist, TagField::Artist),
            field("Album", &f.album, TagField::Album),
            field("Albumkünstler", &f.album_artist, TagField::AlbumArtist),
            field("Komponist", &f.composer, TagField::Composer),
            field("Genre", &f.genre, TagField::Genre),
            row![
                small("Track", &f.track_no, TagField::TrackNo),
                small("von", &f.track_total, TagField::TrackTotal),
                small("Disc", &f.disc_no, TagField::DiscNo),
                small("Jahr", &f.year, TagField::Year),
            ]
            .spacing(16),
            compilation,
        ]
        .spacing(10);
        if let Some(err) = &ed.error {
            col = col.push(
                text(err.as_str())
                    .size(13)
                    .color(Color::from_rgb(0.95, 0.45, 0.4)),
            );
        }
        col = col.push(
            row![
                button("Abbrechen").on_press_maybe((!saving).then_some(Message::CancelTagEditor)),
                button(if saving { "Speichern …" } else { "Speichern" })
                    .on_press_maybe((!saving).then_some(Message::SaveTags)),
            ]
            .spacing(8),
        );

        let dialog = container(col).padding(20).max_width(560).style(panel_style);

        // Abgedunkelter Hintergrund; Klick daneben schließt, Klicks auf den Dialog nicht.
        opaque(
            mouse_area(
                container(opaque(dialog))
                    .center(Length::Fill)
                    .style(|_: &Theme| container::Style {
                        background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.6).into()),
                        ..container::Style::default()
                    }),
            )
            .on_press(Message::CancelTagEditor),
        )
    }

    /// MusicBrainz-Ansicht: links die Albumliste, rechts (fest, ohne Scrollbar)
    /// Künstler- und Albumbild des gewählten Albums.
    fn musicbrainz_view(&self) -> Element<'_, Message> {
        let list = column(self.mb_visible.iter().map(|&i| {
            let e = &self.mb_albums[i];
            let (state, color) = match &e.state {
                MbState::Id(id) => (id.as_str(), col().text),
                MbState::NotAvailable => ("n/a", col().dim),
                MbState::Unknown => ("unknown", col().dim),
            };
            let r = row![
                cell(&e.artist_display).width(Length::FillPortion(1)),
                cell(&e.display).width(Length::FillPortion(1)),
                // MBID (UUID, 36 Zeichen): fest, klein und schmal; Monospace ≈ 0,6 em/Zeichen
                text(state)
                    .size(if matches!(e.state, MbState::Id(_)) {
                        MBID_SIZE
                    } else {
                        // „n/a“ / „unknown“: normale Schriftgröße
                        16.0
                    })
                    .color(color)
                    .wrapping(Wrapping::None)
                    .width(Length::Fixed(MBID_SIZE * 0.6 * 36.0 + 2.0)),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            list_button(
                r.into(),
                self.mb_selected == Some(i),
                Message::SelectMbAlbum(i),
            )
        }));
        let list = container(list).padding(Padding {
            right: 14.0,
            ..Padding::ZERO
        });

        const SIDE: f32 = 240.0;
        let slot = |label: &'static str, handle: &Option<image::Handle>| -> Element<'_, Message> {
            let body: Element<'_, Message> = match (self.mb_selected, handle) {
                (Some(_), Some(h)) => image(h.clone())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(iced::ContentFit::Contain)
                    .into(),
                (Some(_), None) => container(icon(ICON_IMAGE, 48.0, col().dim))
                    .center(Length::Fill)
                    .into(),
                (None, _) => container(text(""))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into(),
            };
            // Slots teilen sich die Resthöhe; der Knopf darunter bleibt immer sichtbar.
            column![text(label).size(12).color(col().dim), body]
                .spacing(6)
                .height(Length::FillPortion(1))
                .into()
        };
        // Knopf zentriert; darunter Spinner bzw. Ergebnistext (ebenfalls zentriert)
        let mut run = column![
            button("MusicBrainz abfragen")
                .on_press_maybe((!self.mb_running).then_some(Message::MusicBrainz))
        ]
        .spacing(8)
        .align_x(Alignment::Center);
        if self.mb_running {
            let current = self.mb_progress.current.load(Ordering::Relaxed);
            let total = self.mb_progress.total.load(Ordering::Relaxed);
            run = run.push(
                row![
                    icon(ICON_SPINNER, 18.0, col().text)
                        .rotation(iced::Rotation::Floating(self.spinner_angle.into())),
                    text(format!("{current}/{total}")).size(13),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        } else if let Some(msg) = &self.mb_result {
            run = run.push(text(msg.as_str()).size(12).align_x(Alignment::Center));
        }

        let images = container(
            column![
                slot("KÜNSTLER", &self.mb_artist_image),
                slot("ALBUM", &self.mb_album_image),
                container(run).center_x(Length::Fill),
            ]
            .spacing(12),
        )
        .padding(8)
        .width(Length::Fixed(SIDE + 16.0))
        .height(Length::Fill)
        .style(panel_style);

        row![pane("ALBEN", list.into()), images]
            .spacing(8)
            .height(Length::Fill)
            .into()
    }

    /// Einstellungen (rechte Seite): Ort der Musikbibliothek, MusicBrainz.
    /// Hilfe: alle Tasten + Danksagungen.
    fn help_view(&self) -> Element<'_, Message> {
        let c = col();
        let key = |keys: &'static str, what: &'static str| -> Element<'static, Message> {
            row![
                container(text(keys).size(13).color(c.text))
                    .padding([2, 8])
                    .width(Length::Fixed(190.0))
                    .style(move |_: &Theme| container::Style {
                        background: Some(c.selected.into()),
                        border: Border {
                            color: c.border,
                            width: 1.0,
                            radius: 4.0.into(),
                        },
                        ..container::Style::default()
                    }),
                text(what).size(14),
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into()
        };
        let heading = |t: &'static str| text(t).size(12).color(c.dim);
        let credit = |name: &'static str, what: &'static str, license: &'static str| {
            column![
                text(name).size(15),
                text(what).size(13),
                text(license).size(12).color(c.dim),
            ]
            .spacing(2)
        };

        let content = column![
            heading("TASTEN"),
            column![
                key("Leertaste", "Wiedergabe starten / pausieren"),
                key("t", "Metadaten (Tags) des laufenden Titels bearbeiten (Music, Compilations, Playlists)"),
                key("p", "Laufenden Titel zu einer Playlist hinzufügen"),
                key("Alt + ↑ / Alt + ↓", "Laufenden Titel in der Playlist nach oben / unten verschieben"),
                key("Tab / Umschalt + Tab", "In Dialogen zum nächsten / vorherigen Feld wechseln"),
                key("Enter", "In Dialogen speichern bzw. bestätigen"),
                key("Esc", "Dialog schließen"),
            ]
            .spacing(8),
            heading("MAUS"),
            column![
                key("Klick auf Titel", "Titel abspielen (die sichtbare Liste ist die Wiedergabeliste)"),
                key("Suchfeld", "Sucht ab zwei Zeichen in Künstler, Album und Titel"),
                key("Trennlinien ziehen", "Spaltenbreiten ändern"),
                key("Lautsprecher-Knopf (Player)", "Ausgabegerät wählen (z. B. Bluetooth-Lautsprecher). Es wird nie automatisch gewechselt; verschwindet das Gerät, pausiert die Wiedergabe und der Systemstandard wird aktiv"),
                key("❣️ (Playlists)", "Mixtape: Playlist als ZIP speichern (optional per ffmpeg konvertiert, siehe Einstellungen)"),
                key("✎ / ✕ (Playlists)", "Playlist umbenennen / löschen bzw. Titel entfernen"),
            ]
            .spacing(8),
            heading("ACKNOWLEDGEMENTS"),
            text("FYA nutzt unter anderem freie Software von:").size(13),
            column![
                credit("iced", "Plattformübergreifende GUI-Bibliothek für Rust (iced.rs)", "MIT-Lizenz"),
                credit("rodio", "Audio-Wiedergabe (github.com/RustAudio/rodio)", "MIT oder Apache-2.0"),
                credit("lofty", "Lesen und Schreiben von Audio-Tags (github.com/Serial-ATA/lofty-rs)", "MIT oder Apache-2.0"),
                credit("opus-rs", "Opus-Decoder in Rust (github.com/restsend/opus-rs)", "BSD-3-Clause"),
                credit("Lucide", "Icons (lucide.dev)", "ISC-Lizenz"),
            ]
            .spacing(10),
        ]
        .spacing(14);

        pane("HILFE", content.into())
    }

    /// „Mixtapes immer konvertieren in: [Format]“ – nur aktiv, wenn ffmpeg gefunden wurde.
    fn mixtape_setting(&self) -> Element<'_, Message> {
        let available = self.ffmpeg.is_some();
        let mut checkbox =
            checkbox(self.mixtape_convert && available).label("Mixtapes immer konvertieren in:");
        let mut formats = row![].spacing(10).align_y(Alignment::Center);
        if available {
            checkbox = checkbox.on_toggle(Message::MixtapeConvert);
            formats = formats.push(pick_list(
                mixtape::Format::ALL,
                Some(self.mixtape_format),
                Message::MixtapeFormat,
            ));
        } else {
            // ohne ffmpeg: nicht bedienbar, Format nur anzeigen + Hinweis
            formats = formats
                .push(text(self.mixtape_format.to_string()).color(col().dim))
                .push(text("Benötigt ffmpeg").size(13).color(col().warn));
        }
        row![checkbox, formats]
            .spacing(10)
            .align_y(Alignment::Center)
            .into()
    }

    fn settings_view(&self) -> Element<'_, Message> {
        container(
            column![
                text("EINSTELLUNGEN").size(12).color(col().dim),
                text("Musikbibliothek").size(12),
                text(self.root_label()).size(13),
                row![
                    button("Ordner ändern …").on_press(Message::PickFolder),
                    button("Neu einlesen").on_press(Message::Rescan),
                ]
                .spacing(8),
                text("Design").size(12),
                pick_list(
                    self.theme_options.as_slice(),
                    Some(&self.theme),
                    Message::ThemeSelected,
                ),
                text("Mixtape").size(12),
                self.mixtape_setting(),
            ]
            .spacing(12),
        )
        .padding(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(panel_style)
        .into()
    }

    fn artist_pane(&self) -> Element<'_, Message> {
        let list = column(self.artists.iter().map(|a| {
            list_button(
                text(a.name.as_str()).wrapping(Wrapping::None).into(),
                self.selected_artist == Some(a.id),
                Message::SelectArtist(a.id),
            )
        }));
        pane("KÜNSTLER", list.into())
    }

    /// Music: alle Alben des Künstlers untereinander. Pro Album ein Kopf (Cover,
    /// Titel, Jahr) und darunter die Titel; zwischen den Alben eine Trennlinie
    /// mit 80 % Breite.
    fn albums_pane(&self) -> Element<'_, Message> {
        // Titel (mit globalem Index in `self.tracks`) nach Album gruppieren
        let mut groups: Vec<(i64, Vec<(usize, &Track)>)> = Vec::new();
        for (i, t) in self.tracks.iter().enumerate() {
            match groups.last_mut() {
                Some((id, items)) if *id == t.album_id => items.push((i, t)),
                _ => groups.push((t.album_id, vec![(i, t)])),
            }
        }

        let mut content = column![].spacing(14);
        let last = groups.len().saturating_sub(1);
        for (n, (album_id, items)) in groups.into_iter().enumerate() {
            let album = self
                .albums
                .iter()
                .chain(self.comp_albums.iter())
                .find(|a| a.id == album_id);
            let title = album
                .map(|a| a.title.clone())
                .unwrap_or_else(|| items[0].1.album_title.clone());
            let mut titles = column![text(title).size(16)].spacing(2);
            if let Some(year) = album.and_then(|a| a.year) {
                titles = titles.push(text(year.to_string()).size(12).color(col().dim));
            }
            let header = row![
                cover_box(self.album_covers.get(&album_id).and_then(|c| c.as_ref())),
                titles
            ]
            .spacing(12)
            .align_y(Alignment::Center);

            // Sampler-Alben enthalten hier nur Titel dieses Künstlers -> keine Künstler-Spalte
            let is_comp = self.comp_albums.iter().any(|a| a.id == album_id);
            let show_artist = !is_comp && items.iter().any(|(_, t)| t.compilation);
            content = content.push(
                column![header, self.track_rows(items, show_artist, false, false)].spacing(8),
            );
            if n < last {
                content = content.push(row![
                    iced::widget::Space::new().width(Length::FillPortion(1)),
                    container(text(""))
                        .width(Length::FillPortion(8))
                        .height(1)
                        .style(|_: &Theme| container::Style {
                            background: Some(col().border.into()),
                            ..container::Style::default()
                        }),
                    iced::widget::Space::new().width(Length::FillPortion(1)),
                ]);
            }
        }

        // Rechts Platz lassen, damit die Scrollbar nicht die Dauer überdeckt.
        let content = container(content).padding(Padding {
            right: 14.0,
            ..Padding::ZERO
        });
        pane("ALBEN", content.into())
    }

    /// Zeilen mit Nummer, Titel, optional Künstler/Album und Dauer. Jeder Eintrag
    /// trägt den Index in der abspielbaren Liste (`PlayTrack`).
    fn track_rows<'a>(
        &'a self,
        items: Vec<(usize, &'a Track)>,
        show_artist: bool,
        show_album: bool,
        removable: bool,
    ) -> Element<'a, Message> {
        let playing_id = self.current_track().map(|t| t.id);
        fn dash(o: &Option<String>) -> &str {
            o.as_deref().unwrap_or("—")
        }

        column(items.into_iter().map(|(i, t)| {
            // Fehlender Pfad (Playlist): ⚠ statt Nummer
            let number = if t.missing {
                text("⚠").color(col().warn)
            } else {
                text(
                    t.track_no
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "—".to_string()),
                )
                .color(col().dim)
            };
            let mut r = row![
                container(number).width(32),
                cell(&t.title).width(Length::FillPortion(3)),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            if show_artist {
                r = r.push(cell(dash(&t.artist_tag)).width(Length::FillPortion(2)));
            }
            if show_album {
                r = r.push(cell(dash(&t.album_tag)).width(Length::FillPortion(2)));
            }
            r = r.push(text(fmt_time(t.duration_secs as u64)).color(col().dim));
            if removable {
                r = r.push(small_x_button(Message::RemoveFromPlaylist(t.path.clone())));
            }

            list_button(r.into(), playing_id == Some(t.id), Message::PlayTrack(i))
        }))
        .into()
    }

    /// Titelliste (Compilations, Playlists) als eigene Spalte.
    fn track_list<'a>(
        &'a self,
        tracks: &'a [Track],
        show_artist: bool,
        show_album: bool,
        removable: bool,
    ) -> Element<'a, Message> {
        let rows = self.track_rows(
            tracks.iter().enumerate().collect(),
            show_artist,
            show_album,
            removable,
        );
        // Rechts Platz lassen, damit die Scrollbar nicht die Dauer überdeckt.
        let list = container(rows).padding(Padding {
            right: 14.0,
            ..Padding::ZERO
        });
        pane("TITEL", list.into())
    }

    /// Popover über dem Lautsprecher-Knopf: Systemstandard + alle Ausgabegeräte.
    /// (iced hat kein fertiges Popover: Vollflächen-`mouse_area` zum Schließen + Menü.)
    fn device_menu(&self) -> Element<'_, Message> {
        let check = |on: bool| container(text(if on { "✓" } else { "" })).width(20);

        let mut items = column![];
        for name in &self.devices {
            items = items.push(list_button(
                row![
                    check(self.audio.device() == Some(name.as_str())),
                    text(name.as_str()).wrapping(Wrapping::None),
                ]
                .align_y(Alignment::Center)
                .into(),
                false,
                Message::SelectDevice(name.clone()),
            ));
        }
        if self.devices.is_empty() {
            items = items.push(
                text("Keine Ausgabegeräte gefunden")
                    .size(13)
                    .color(col().dim),
            );
        }

        let menu = container(
            column![
                text("AUSGABEGERÄT").size(12).color(col().dim),
                scrollable(items).height(Length::Shrink),
            ]
            .spacing(6),
        )
        .padding(10)
        .width(Length::Fixed(320.0))
        .max_height(360)
        .style(panel_style);

        // Klick daneben schließt; das Menü selbst ist `opaque`.
        let dismiss = mouse_area(container(text("")).width(Length::Fill).height(Length::Fill))
            .on_press(Message::ToggleDeviceMenu);
        let popover = container(opaque(menu))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::End)
            .align_y(Alignment::End)
            // etwa über dem Lautsprecher-Knopf (rechts in der Player-Leiste)
            .padding(Padding {
                right: 170.0,
                bottom: 106.0,
                ..Padding::ZERO
            });
        stack![dismiss, popover].into()
    }

    /// Modale Playlist-Dialoge (Neu / Hinzufügen / Löschen).
    fn playlist_dialog_view(&self) -> Element<'_, Message> {
        let Some(dialog) = &self.playlist_dialog else {
            return container(text("")).into();
        };
        let err = err_text;
        let content: Element<'_, Message> = match dialog {
            PlaylistDialog::New { name, error, .. } => column![
                text("Neue Playlist").size(20),
                text_input("Name", name)
                    .id(PLAYLIST_NAME_FIELD)
                    .on_input(Message::PlaylistName)
                    .on_submit(Message::ConfirmPlaylistDialog)
                    .padding(6),
                err(error),
                row![
                    button("Abbrechen").on_press(Message::ClosePlaylistDialog),
                    button("Anlegen").on_press(Message::ConfirmPlaylistDialog),
                ]
                .spacing(8),
            ]
            .spacing(10)
            .into(),
            PlaylistDialog::Rename { name, error, .. } => column![
                text("Playlist umbenennen").size(20),
                text_input("Name", name)
                    .id(PLAYLIST_NAME_FIELD)
                    .on_input(Message::PlaylistName)
                    .on_submit(Message::ConfirmPlaylistDialog)
                    .padding(6),
                err(error),
                row![
                    button("Abbrechen").on_press(Message::ClosePlaylistDialog),
                    button("Umbenennen").on_press(Message::ConfirmPlaylistDialog),
                ]
                .spacing(8),
            ]
            .spacing(10)
            .into(),
            PlaylistDialog::AddTrack { title, error, .. } => {
                let list: Element<'_, Message> = if self.playlists.is_empty() {
                    column![].into()
                } else {
                    scrollable(column(self.playlists.iter().map(|p| {
                        list_button(
                            text(p.name.as_str()).wrapping(Wrapping::None).into(),
                            false,
                            Message::AddToPlaylist(p.id),
                        )
                    })))
                    .height(Length::Shrink)
                    .into()
                };
                column![
                    text("Zu Playlist hinzufügen").size(20),
                    text(title.as_str()).size(12).color(col().dim),
                    container(list).max_height(260),
                    button("+ Neue Playlist …").on_press(Message::AddToNewPlaylist),
                    err(error),
                    button("Abbrechen").on_press(Message::ClosePlaylistDialog),
                ]
                .spacing(10)
                .into()
            }
            PlaylistDialog::Delete { name, .. } => column![
                text(format!("Playlist „{name}“ löschen?")).size(20),
                text("Die Titeldateien bleiben erhalten.")
                    .size(13)
                    .color(col().dim),
                row![
                    button("Abbrechen").on_press(Message::ClosePlaylistDialog),
                    button("Löschen").on_press(Message::ConfirmPlaylistDialog),
                ]
                .spacing(8),
            ]
            .spacing(10)
            .into(),
        };
        modal(
            container(content)
                .padding(20)
                .max_width(460)
                .style(panel_style)
                .into(),
            Message::ClosePlaylistDialog,
        )
    }

    /// Playlists: zwei Spalten (Playlist | Titel).
    fn playlists_view(&self) -> Element<'_, Message> {
        pane_grid(&self.pl_columns, |_, kind, _| {
            pane_grid::Content::new(match kind {
                Column::Tracks => self.track_list(&self.pl_tracks, true, true, true),
                _ => {
                    let list: Element<'_, Message> = if self.playlists.is_empty() {
                        column![].into()
                    } else {
                        column(self.playlists.iter().map(|p| {
                            list_button(
                                row![
                                    text(p.name.as_str())
                                        .wrapping(Wrapping::None)
                                        .width(Length::Fill),
                                    small_glyph_button("❣️", Message::Mixtape(p.id)),
                                    small_glyph_button("✎", Message::AskRenamePlaylist(p.id)),
                                    small_x_button(Message::AskDeletePlaylist(p.id)),
                                ]
                                .align_y(Alignment::Center)
                                .into(),
                                self.pl_selected == Some(p.id),
                                Message::SelectPlaylist(p.id),
                            )
                        }))
                        .into()
                    };
                    let content = column![
                        button("+ Neue Playlist").on_press(Message::NewPlaylist),
                        list
                    ]
                    .spacing(8);
                    pane("PLAYLIST", content.into())
                }
            })
        })
        .spacing(8)
        .min_size(120)
        .on_resize(6, Message::PlaylistPaneResized)
        .height(Length::Fill)
        .into()
    }

    /// Compilations: zwei Spalten (Alben des Künstlers „Compilations“ | Titel).
    fn compilations_view(&self) -> Element<'_, Message> {
        pane_grid(&self.comp_columns, |_, kind, _| {
            pane_grid::Content::new(match kind {
                Column::Tracks => self.track_list(
                    &self.comp_tracks,
                    true,
                    // Album nur bei „Alle Alben“; bei einem gewählten Album ist es immer gleich
                    self.comp_selected_album.is_none(),
                    false,
                ),
                _ => {
                    let all = list_button(
                        text("Alle Alben").into(),
                        self.comp_selected_album.is_none(),
                        Message::SelectCompAlbum(None),
                    );
                    let albums = self.comp_albums.iter().map(|a| {
                        let label = match a.year {
                            Some(y) => format!("{y} · {}", a.title),
                            None => a.title.clone(),
                        };
                        list_button(
                            text(label).wrapping(Wrapping::None).into(),
                            self.comp_selected_album == Some(a.id),
                            Message::SelectCompAlbum(Some(a.id)),
                        )
                    });
                    pane(
                        "COMPILATIONS",
                        column(std::iter::once(all).chain(albums)).into(),
                    )
                }
            })
        })
        .spacing(8)
        .min_size(120)
        .on_resize(6, Message::CompPaneResized)
        .height(Length::Fill)
        .into()
    }

    fn player_bar(&self) -> Element<'_, Message> {
        let current = self.current_track();
        let (title, subtitle) = match (&self.status, current) {
            (Some(err), _) => (err.clone(), String::new()),
            (None, Some(t)) => (t.title.clone(), {
                // Anzeigenamen aus den Tags statt der (bereinigten) Ordnernamen
                // Sammler-Ordner „Compilations“: Künstler des Titels aus dem Tag
                let artist = match (&t.artist_tag, t.artist_name == COMPILATIONS) {
                    (Some(tag), true) => tag,
                    _ => self
                        .artist_names
                        .get(&t.artist_name)
                        .unwrap_or(&t.artist_name),
                };
                let album = self
                    .album_titles
                    .get(&(t.artist_name.clone(), t.album_title.clone()))
                    .unwrap_or(&t.album_title);
                format!("{artist} — {album}")
            }),
            (None, None) => ("Kein Titel ausgewählt".to_string(), String::new()),
        };

        let dur = current.map(|t| t.duration_secs).unwrap_or(0) as f32;
        let pos = self
            .seek_preview
            .unwrap_or_else(|| self.audio.position().as_secs_f32());

        let controls = row![
            icon_button(ICON_PREVIOUS, Message::Previous),
            icon_button(
                if self.audio.is_playing() {
                    ICON_PAUSE
                } else {
                    ICON_PLAY
                },
                Message::TogglePause,
            ),
            icon_button(ICON_NEXT, Message::Next),
            // Texte abschneiden statt in den Lautstärke-Slider hineinzulaufen
            container(column![
                text(title).wrapping(Wrapping::None),
                text(subtitle).size(12).wrapping(Wrapping::None)
            ])
            .width(Length::Fill)
            .clip(true),
            icon_button(ICON_SPEAKER, Message::ToggleDeviceMenu),
            icon(ICON_VOLUME, 20.0, col().dim),
            slider(0.0..=1.0, self.audio.volume(), Message::Volume)
                .step(0.01 as f32)
                .width(120),
        ]
        .spacing(12)
        .align_y(Alignment::Center);

        let progress = row![
            text(fmt_time(pos as u64)).size(12),
            slider(0.0..=dur.max(1.0), pos.min(dur.max(1.0)), Message::Seek)
                .on_release(Message::SeekRelease)
                .step(0.5 as f32)
                .width(Length::Fill),
            text(fmt_time(dur as u64)).size(12),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        let cover = cover_box(self.cover.as_ref());

        container(
            row![
                cover,
                column![progress, controls].spacing(8).width(Length::Fill)
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .padding(10)
        .width(Length::Fill)
        .style(panel_style)
        .into()
    }
}

// ---------------------------------------------------------------------------
// UI-Helfer
// ---------------------------------------------------------------------------

// Icons von https://lucide.dev (ISC-Lizenz, siehe assets/icons/LICENSE)
/// Name des Sammel-Künstlers (Ordner „Compilations“) – eigene Ansicht
const COMPILATIONS: &str = "Compilations";

/// ID des Namensfeldes im „Neue Playlist“-Dialog
const PLAYLIST_NAME_FIELD: &str = "playlist-name";

/// ID des ersten Feldes im Tag-Editor (Fokus beim Öffnen)
const TAG_FIRST_FIELD: &str = "tag-editor-title";

/// Schriftgröße der MBID-Spalte (UUID = 36 Zeichen)
const MBID_SIZE: f32 = 10.0;

const ICON_PREVIOUS: &[u8] = include_bytes!("../assets/icons/skip-back.svg");
const ICON_NEXT: &[u8] = include_bytes!("../assets/icons/skip-forward.svg");
const ICON_PLAY: &[u8] = include_bytes!("../assets/icons/play.svg");
const ICON_PAUSE: &[u8] = include_bytes!("../assets/icons/pause.svg");
const ICON_VOLUME: &[u8] = include_bytes!("../assets/icons/volume-2.svg");
const ICON_IMAGE: &[u8] = include_bytes!("../assets/icons/image.svg");
const ICON_SEARCH: &[u8] = include_bytes!("../assets/icons/search.svg");
const ICON_SPINNER: &[u8] = include_bytes!("../assets/icons/loader-circle.svg");
const ICON_MUSIC: &[u8] = include_bytes!("../assets/icons/music.svg");
const ICON_HEXAGON: &[u8] = include_bytes!("../assets/icons/hexagon.svg");
const ICON_COMPILATIONS: &[u8] = include_bytes!("../assets/icons/list-music.svg");
const ICON_PLAYLISTS: &[u8] = include_bytes!("../assets/icons/list-ordered.svg");
const ICON_HELP: &[u8] = include_bytes!("../assets/icons/circle-help.svg");
const ICON_SPEAKER: &[u8] = include_bytes!("../assets/icons/speaker.svg");
const ICON_SETTINGS: &[u8] = include_bytes!("../assets/icons/settings.svg");

/// Einfarbiges SVG-Icon (Lucide-Icons nutzen `currentColor`, daher Farbfilter).
fn icon<'a>(data: &'static [u8], size: f32, color: Color) -> svg::Svg<'a> {
    svg(svg::Handle::from_memory(data))
        .width(size)
        .height(size)
        .style(move |_: &Theme, _| svg::Style { color: Some(color) })
}

/// Icon als Button ohne Rahmen und Hintergrund; beim Darüberfahren heller.
fn icon_button(data: &'static [u8], on_press: Message) -> Element<'static, Message> {
    button(
        svg(svg::Handle::from_memory(data))
            .width(22)
            .height(22)
            .style(|_: &Theme, status| svg::Style {
                color: Some(match status {
                    svg::Status::Hovered => col().text,
                    svg::Status::Idle => col().text,
                }),
            }),
    )
    .padding(4)
    .style(|_: &Theme, _| button::Style::default())
    .on_press(on_press)
    .into()
}

/// Spalte mit Überschrift und scrollbarem Inhalt.
fn pane<'a>(title: &'a str, content: Element<'a, Message>) -> Element<'a, Message> {
    container(
        column![
            text(title).size(12).color(col().dim),
            scrollable(content).height(Length::Fill).width(Length::Fill),
        ]
        .spacing(6),
    )
    .padding(8)
    .width(Length::Fill)
    .height(Length::Fill)
    .style(panel_style)
    .into()
}

/// Klickbare Listenzeile; `selected` hebt sie hervor.
fn list_button<'a>(
    content: Element<'a, Message>,
    selected: bool,
    on_press: Message,
) -> Element<'a, Message> {
    button(content)
        .width(Length::Fill)
        .padding([4, 8])
        .style(move |_: &Theme, status| {
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: if selected {
                    Some(col().selected.into())
                } else if hovered {
                    Some(col().hover.into())
                } else {
                    None
                },
                text_color: col().text,
                border: Border {
                    radius: 4.0.into(),
                    ..Border::default()
                },
                ..button::Style::default()
            }
        })
        .on_press(on_press)
        .into()
}

/// Fehlerzeile in Dialogen (leer, wenn kein Fehler).
fn err_text(e: &Option<String>) -> Element<'_, Message> {
    match e {
        Some(e) => text(e.as_str()).size(13).color(col().warn).into(),
        None => text("").size(1).into(),
    }
}

/// Cover-Kachel (72 px, wie im Player); ohne Bild ein Platzhalter-Icon.
fn cover_box<'a>(handle: Option<&image::Handle>) -> Element<'a, Message> {
    let content: Element<'a, Message> = match handle {
        Some(handle) => image(handle.clone())
            .width(72)
            .height(72)
            .content_fit(iced::ContentFit::Cover)
            .into(),
        None => icon(ICON_IMAGE, 28.0, col().dim).into(),
    };
    container(content)
        .center(Length::Fixed(72.0))
        .clip(true)
        .style(|_: &Theme| container::Style {
            background: Some(col().selected.into()),
            border: Border {
                color: col().dim,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// Fehlertext für Datenbankfehler; „gesperrt“ bekommt eine verständliche Meldung.
fn db_error(e: &anyhow::Error) -> String {
    let busy = e.chain().any(|c| {
        matches!(
            c.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(f, _))
                if matches!(
                    f.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    });
    if busy {
        "Die Datenbank ist gerade belegt (MusicBrainz-Abgleich oder Einlesen läuft). \
         Bitte kurz warten und erneut versuchen."
            .to_string()
    } else {
        format!("{e:#}")
    }
}

/// Abgedunkelter Hintergrund mit zentriertem Dialog; Klick daneben schließt.
fn modal<'a>(dialog: Element<'a, Message>, on_close: Message) -> Element<'a, Message> {
    opaque(
        mouse_area(
            container(opaque(dialog))
                .center(Length::Fill)
                .style(|_: &Theme| container::Style {
                    background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.6).into()),
                    ..container::Style::default()
                }),
        )
        .on_press(on_close),
    )
}

/// Kleines Zeichen als Knopf ohne Rahmen; beim Darüberfahren heller.
fn small_glyph_button(glyph: &'static str, on_press: Message) -> Element<'static, Message> {
    button(text(glyph).size(18))
        .padding([2, 6])
        .style(|_: &Theme, status| button::Style {
            text_color: if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                col().text
            } else {
                col().dim
            },
            ..button::Style::default()
        })
        .on_press(on_press)
        .into()
}

/// Kleines „✕“ (Entfernen/Löschen).
fn small_x_button(on_press: Message) -> Element<'static, Message> {
    small_glyph_button("✕", on_press)
}

/// Einzeiliger, abgeschnittener Text.
fn cell(s: &str) -> container::Container<'_, Message> {
    container(text(s).wrapping(Wrapping::None)).clip(true)
}

#[cfg(test)]
mod db_error_tests {
    use super::*;

    #[test]
    fn gesperrte_db_bekommt_verstaendliche_meldung() {
        let busy = anyhow::Error::from(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(5), // SQLITE_BUSY
            Some("database is locked".into()),
        ));
        assert!(db_error(&busy).contains("gerade belegt"));
        assert_eq!(
            db_error(&anyhow::anyhow!("Bitte einen Namen eingeben")),
            "Bitte einen Namen eingeben"
        );
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    /// Text und gedimmter Text müssen auf den Flächen jedes Themes lesbar sein.
    #[test]
    fn alle_themes_lesbar() {
        let mut schlecht = Vec::new();
        for theme in theme_options() {
            let c = colors_for(&theme);
            for (name, surface) in [
                ("panel", c.panel),
                ("selected", c.selected),
                ("hover", c.hover),
            ] {
                let (t, d) = (contrast(c.text, surface), contrast(c.dim, surface));
                if t < 4.0 || d < 2.0 {
                    schlecht.push(format!("{theme}/{name}: text {t:.1} dim {d:.1}"));
                }
            }
        }
        assert!(
            schlecht.is_empty(),
            "zu geringer Kontrast:\n{}",
            schlecht.join("\n")
        );
        assert_eq!(theme_options().len(), Theme::ALL.len() + 1);
    }
}
