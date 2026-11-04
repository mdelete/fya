// Windows-Release: keine Konsole hinter dem Fenster
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod audio;
mod db;
mod folder;
mod musicbrainz;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use iced::widget::text::Wrapping;
use iced::widget::{
    button, checkbox, column, container, image, mouse_area, opaque, pane_grid, row, scrollable,
    slider, stack, svg, text, text_input,
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
    .theme(|_: &PlayerApp| theme())
    .default_font(Font::MONOSPACE)
    .window(window_settings())
    .run()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Look (angelehnt an Halloy): warm-dunkles Grau, cremefarbener Text,
// Monospace-Schrift, Titelleiste transparent über dem Inhalt.
// ---------------------------------------------------------------------------

const BG: Color = Color::from_rgb(0.145, 0.141, 0.165); // Fenster
const PANEL: Color = Color::from_rgb(0.122, 0.118, 0.141); // Flächen
const SELECTED: Color = Color::from_rgb(0.098, 0.094, 0.114); // gewählte Zeile
const HOVER: Color = Color::from_rgb(0.175, 0.170, 0.196);
const BORDER: Color = Color::from_rgb(0.55, 0.52, 0.53);
const TEXT: Color = Color::from_rgb(0.94, 0.88, 0.67); // creme
/// Warnfarbe (Fehler, ⚠)
const WARN: Color = Color::from_rgb(0.95, 0.62, 0.3);
const DIM: Color = Color::from_rgb(0.54, 0.49, 0.46); // Überschriften, Zeiten

fn theme() -> Theme {
    Theme::custom(
        "Halloy",
        iced::theme::Palette {
            background: BG,
            text: TEXT,
            primary: Color::from_rgb(0.84, 0.66, 0.90), // Flieder
            success: Color::from_rgb(0.66, 0.82, 0.55),
            warning: Color::from_rgb(0.90, 0.75, 0.48),
            danger: Color::from_rgb(0.88, 0.42, 0.46),
        },
    )
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
        background: Some(PANEL.into()),
        border: Border {
            color: BORDER,
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
    SelectAlbum(Option<i64>),
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
    selected_album: Option<i64>, // None = alle Alben des Künstlers

    /// Compilations-Ansicht: Alben/Titel des Künstlers „Compilations“
    comp_columns: pane_grid::State<Column>,
    comp_artist: Option<i64>,
    comp_albums: Vec<Album>,
    comp_tracks: Vec<Track>,
    comp_selected_album: Option<i64>,
    /// Enthält die Mediathek überhaupt Künstler? (unabhängig von Filter/„Compilations“)
    library_empty: bool,

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
                ratio: 0.22,
                a: Box::new(pane_grid::Configuration::Pane(Column::Artists)),
                b: Box::new(pane_grid::Configuration::Split {
                    axis: pane_grid::Axis::Vertical,
                    ratio: 0.34,
                    a: Box::new(pane_grid::Configuration::Pane(Column::Albums)),
                    b: Box::new(pane_grid::Configuration::Pane(Column::Tracks)),
                }),
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
            selected_album: None,
            queue: Vec::new(),
            queue_index: None,
            cover: None,
            seek_preview: None,
            status: None,
        };
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
                    Task::none()
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
        self.selected_album = None;
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
        let query = self.filter.is_some().then(|| self.search.trim().to_lowercase());
        self.mb_visible = self
            .mb_albums
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                query.as_ref().is_none_or(|q| {
                    e.artist.to_lowercase().contains(q) || e.album.to_lowercase().contains(q)
                })
            })
            .map(|(i, _)| i)
            .collect();
        if self.mb_selected.is_some_and(|i| !self.mb_visible.contains(&i)) {
            self.mb_selected = None;
            self.mb_artist_image = None;
            self.mb_album_image = None;
        }
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
                    key: keyboard::Key::Named(named @ (keyboard::key::Named::ArrowUp | keyboard::key::Named::ArrowDown)),
                    modifiers,
                    ..
                }),
                _,
            ) if modifiers.alt() && !modifiers.command() => Some(Message::MovePlaylistTrack(
                if named == keyboard::key::Named::ArrowUp { -1 } else { 1 },
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
            Message::SelectArtist(id) => self.load_artist(id),
            Message::SelectAlbum(album) => self.select_album(album),
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
            Message::ShowView(view) => self.show_view(view),
            Message::SelectCompAlbum(a) => self.select_comp_album(a),
            Message::SelectPlaylist(id) => self.select_playlist(Some(id)),
            Message::Escape => {
                if self.tag_editor.as_ref().is_some_and(|ed| !ed.saving) {
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
            Message::PlaylistName(v) => {
                match &mut self.playlist_dialog {
                    Some(PlaylistDialog::New { name, error, .. })
                    | Some(PlaylistDialog::Rename { name, error, .. }) => {
                        *name = v;
                        *error = None;
                    }
                    _ => {}
                }
            }
            Message::ClosePlaylistDialog => self.playlist_dialog = None,
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
                        let queue: Vec<Track> =
                            self.pl_tracks.iter().filter(|t| !t.missing).cloned().collect();
                        if let Some(i) = queue.iter().position(|t| t.id == track_id) {
                            self.queue = queue;
                            self.queue_index = Some(i);
                        }
                    }
                    Ok(false) => {}
                    Err(e) => self.status = Some(format!("Verschieben fehlgeschlagen: {e:#}")),
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
                if let Some(PlaylistDialog::AddTrack { path, error, .. }) = &mut self.playlist_dialog
                {
                    match self.library.add_to_playlist(id, path) {
                        Ok(true) => {
                            self.playlist_dialog = None;
                            self.load_playlists();
                        }
                        Ok(false) => *error = Some("Der Titel ist schon in dieser Playlist".into()),
                        Err(e) => *error = Some(format!("{e:#}")),
                    }
                }
            }
            Message::ConfirmPlaylistDialog => match &mut self.playlist_dialog {
                Some(PlaylistDialog::New { name, add, error }) => {
                    match self.library.create_playlist(name) {
                        Ok(id) => {
                            if let Some(path) = add {
                                if let Err(e) = self.library.add_to_playlist(id, path) {
                                    *error = Some(format!("{e:#}"));
                                    return Task::none();
                                }
                            }
                            self.playlist_dialog = None;
                            self.pl_selected = Some(id);
                            self.load_playlists();
                        }
                        Err(e) => *error = Some(e.to_string()),
                    }
                }
                Some(PlaylistDialog::Rename { id, name, error }) => {
                    match self.library.rename_playlist(*id, name) {
                        Ok(()) => {
                            self.playlist_dialog = None;
                            self.load_playlists();
                        }
                        Err(e) => *error = Some(e.to_string()),
                    }
                }
                Some(PlaylistDialog::Delete { id, .. }) => {
                    let id = *id;
                    if let Err(e) = self.library.delete_playlist(id) {
                        self.status = Some(format!("Playlist löschen fehlgeschlagen: {e:#}"));
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
                        self.status = Some(format!("Entfernen fehlgeschlagen: {e:#}"));
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
                )
                    || self.tag_editor.is_some()
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
                    return Task::none();
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
                }
            }
            Message::ScanFinished(Ok(count)) => {
                self.search.clear();
                self.filter = None;
                self.refresh_mb_filter();
                self.reload_library_view();
                self.notice = (count == 0).then(|| {
                    "Keine Musikdateien gefunden. Erwartet wird die Struktur \
                     Künstler/Album/Titel (mp3, aac, m4a, opus)."
                        .to_string()
                });
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
        self.selected_album = None;
        self.albums = self.library.albums(id).unwrap_or_default();
        self.tracks = self.library.tracks_by_artist(id).unwrap_or_default();
        self.apply_filter();
    }

    /// Blendet Alben/Titel aus, die nicht zur aktiven Suche passen.
    fn apply_filter(&mut self) {
        if let Some(f) = &self.filter {
            self.albums.retain(|a| f.albums.contains(&a.id));
            self.tracks.retain(|t| f.tracks.contains(&t.id));
            self.comp_albums.retain(|a| f.albums.contains(&a.id));
            self.comp_tracks.retain(|t| f.tracks.contains(&t.id));
            self.pl_tracks.retain(|t| f.tracks.contains(&t.id));
        }
    }

    fn select_album(&mut self, album: Option<i64>) {
        self.selected_album = album;
        let Some(artist) = self.selected_artist else {
            return;
        };
        self.tracks = match album {
            Some(id) => self.library.tracks_by_album(id),
            None => self.library.tracks_by_artist(artist),
        }
        .unwrap_or_default();
        self.apply_filter();
    }

    // --- Wiedergabe --------------------------------------------------------

    fn play_at(&mut self, index: usize) {
        let Some(track) = self.queue.get(index) else {
            return;
        };
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
                self.status = None;
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
                        Column::Albums => self.album_pane(),
                        Column::Tracks => self.track_pane(),
                        Column::Playlists => unreachable!("nur in der Playlists-Ansicht"),
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
                if self.tag_editor.is_some() {
                    stack![main, self.tag_editor_dialog()].into()
                } else if self.playlist_dialog.is_some() {
                    stack![main, self.playlist_dialog_view()].into()
                } else {
                    main.into()
                }
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
                    icon(glyph, 16.0, TEXT),
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
                    background: Some(if hovered { HOVER } else { PANEL }.into()),
                    text_color: TEXT,
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
            container(icon(ICON_SEARCH, 14.0, DIM))
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
            entry(ICON_SETTINGS, "Settings", Message::ShowView(View::Settings))
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
            row![container(text(label).size(13).color(DIM)).width(110), input]
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
            row![text(label).size(13).color(DIM), input]
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
                        Color::WHITE
                    } else {
                        DIM
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
                MbState::Id(id) => (id.as_str(), TEXT),
                MbState::NotAvailable => ("n/a", DIM),
                MbState::Unknown => ("unknown", DIM),
            };
            let r = row![
                cell(&e.artist).width(Length::FillPortion(1)),
                cell(&e.album).width(Length::FillPortion(1)),
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
                (Some(_), None) => container(icon(ICON_IMAGE, 48.0, DIM))
                    .center(Length::Fill)
                    .into(),
                (None, _) => container(text(""))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into(),
            };
            // Slots teilen sich die Resthöhe; der Knopf darunter bleibt immer sichtbar.
            column![text(label).size(12).color(DIM), body]
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
                    icon(ICON_SPINNER, 18.0, TEXT)
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
    fn settings_view(&self) -> Element<'_, Message> {
        container(
            column![
                text("EINSTELLUNGEN").size(12).color(DIM),
                text("Musikbibliothek").size(12),
                text(self.root_label()).size(13),
                row![
                    button("Ordner ändern …").on_press(Message::PickFolder),
                    button("Neu einlesen").on_press(Message::Rescan),
                ]
                .spacing(8),
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

    fn album_pane(&self) -> Element<'_, Message> {
        let all = list_button(
            text("Alle Alben").into(),
            self.selected_album.is_none(),
            Message::SelectAlbum(None),
        );
        let albums = self.albums.iter().map(|a| {
            let label = match a.year {
                Some(y) => format!("{y} · {}", a.title),
                None => a.title.clone(),
            };
            list_button(
                text(label).wrapping(Wrapping::None).into(),
                self.selected_album == Some(a.id),
                Message::SelectAlbum(Some(a.id)),
            )
        });
        let list = column(std::iter::once(all).chain(albums));
        pane("ALBEN", list.into())
    }

    fn track_pane(&self) -> Element<'_, Message> {
        // Compilation: Künstler + Album; "Alle Alben": Album; sonst nur Titel
        let compilation =
            self.selected_album.is_some() && self.tracks.iter().any(|t| t.compilation);
        self.track_list(
            &self.tracks,
            compilation,
            compilation || self.selected_album.is_none(),
            false,
        )
    }

    /// Titelliste mit Nummer, Titel, optional Künstler/Album und Dauer.
    fn track_list<'a>(
        &'a self,
        tracks: &'a [Track],
        show_artist: bool,
        show_album: bool,
        removable: bool,
    ) -> Element<'a, Message> {
        let playing_id = self.current_track().map(|t| t.id);
        fn dash(o: &Option<String>) -> &str {
            o.as_deref().unwrap_or("—")
        }

        let list = column(tracks.iter().enumerate().map(|(i, t)| {
            // Fehlender Pfad (Playlist): ⚠ statt Nummer
            let number = if t.missing {
                text("⚠").color(WARN)
            } else {
                text(
                    t.track_no
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "—".to_string()),
                )
                .color(DIM)
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
            r = r.push(text(fmt_time(t.duration_secs as u64)).color(DIM));
            if removable {
                r = r.push(
                    small_x_button(Message::RemoveFromPlaylist(t.path.clone())),
                );
            }

            list_button(r.into(), playing_id == Some(t.id), Message::PlayTrack(i))
        }));

        // Rechts Platz lassen, damit die Scrollbar nicht die Dauer überdeckt.
        let list = container(list).padding(Padding {
            right: 14.0,
            ..Padding::ZERO
        });
        pane("TITEL", list.into())
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
                    text(title.as_str()).size(12).color(DIM),
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
                text("Die Titeldateien bleiben erhalten.").size(13).color(DIM),
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
                Column::Tracks => self.track_list(&self.comp_tracks, true, true, false),
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
            (None, Some(t)) => (
                t.title.clone(),
                format!("{} — {}", t.artist_name, t.album_title),
            ),
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
            column![
                text(title).wrapping(Wrapping::None),
                text(subtitle).size(12)
            ]
            .width(Length::Fill),
            icon(ICON_VOLUME, 20.0, DIM),
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

        let cover_content: Element<'_, Message> = match &self.cover {
            Some(handle) => image(handle)
                .width(72)
                .height(72)
                .content_fit(iced::ContentFit::Cover)
                .into(),
            None => icon(ICON_IMAGE, 28.0, DIM).into(),
        };
        let cover = container(cover_content)
            .center(Length::Fixed(72.0))
            .clip(true)
            .style(|_: &Theme| container::Style {
                background: Some(SELECTED.into()),
                border: Border {
                    color: DIM,
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..container::Style::default()
            });

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
                    svg::Status::Hovered => Color::WHITE,
                    svg::Status::Idle => TEXT,
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
            text(title).size(12).color(DIM),
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
                    Some(SELECTED.into())
                } else if hovered {
                    Some(HOVER.into())
                } else {
                    None
                },
                text_color: TEXT,
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
        Some(e) => text(e.as_str()).size(13).color(WARN).into(),
        None => text("").size(1).into(),
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
    button(text(glyph).size(12))
        .padding([0, 4])
        .style(|_: &Theme, status| button::Style {
            text_color: if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                Color::WHITE
            } else {
                DIM
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
