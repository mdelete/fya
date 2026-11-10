//! Medientasten und „Now Playing“ des Betriebssystems über `souvlaki`.
//! macOS = MPRemoteCommandCenter, Linux/BSD = MPRIS (D-Bus), Windows = SMTC.
//!
//! Das ganze Modul gehört zum Cargo-Feature `controls` (siehe `mod` in `main.rs`).

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use iced::futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use iced::futures::{Stream, StreamExt, stream};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, OsMediaControls,
    SeekDirection,
};

/// Befehl des Betriebssystems (Medientaste, Sperrbildschirm, `playerctl` …).
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
    /// Absolute Position
    SeekTo(Duration),
    /// Relativ vor- oder zurückspulen
    SeekBy {
        forward: bool,
        by: Duration,
    },
}

/// Wiedergabestatus für die Anzeige im System.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    Stopped,
    Paused,
    Playing,
}

/// Angezeigter Titel.
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Duration,
    /// Eingebettetes Bild (JPEG/PNG …)
    pub cover: Option<Vec<u8>>,
}

/// Schritt für „ein Stück vor/zurück“ ohne Angabe (Medientaste, `playerctl position 5+`).
const SEEK_STEP: Duration = Duration::from_secs(5);
/// Weicht die Position um mehr ab, als es der Lauf der Zeit erklärt, wurde gespult.
const DRIFT: Duration = Duration::from_millis(1500);

type Channel = (
    UnboundedSender<Command>,
    Mutex<Option<UnboundedReceiver<Command>>>,
);

/// Kanal Betriebssystem -> App. Gemeinsam, weil die Subscription (`commands`) und die
/// Controls (unter Windows erst nach dem Öffnen des Fensters) unabhängig voneinander starten.
fn channel() -> &'static Channel {
    static CHANNEL: OnceLock<Channel> = OnceLock::new();
    CHANNEL.get_or_init(|| {
        let (tx, rx) = unbounded();
        (tx, Mutex::new(Some(rx)))
    })
}

/// Befehle des Systems als Stream (für `Subscription::run`).
pub fn commands() -> impl Stream<Item = Command> {
    match channel().1.lock().unwrap().take() {
        Some(rx) => rx.left_stream(),
        None => stream::pending().right_stream(),
    }
}

pub struct Controls {
    os: OsMediaControls,
    track: Option<i64>,
    playback: Playback,
    position: Duration,
    at: Instant,
}

impl Controls {
    /// `window`: unter Windows das Fensterhandle (HWND), sonst `()`.
    pub fn new(window: platform::Window) -> Option<Self> {
        match Self::try_new(window) {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("Medientasten nicht verfügbar: {e}");
                None
            }
        }
    }

    fn try_new(window: platform::Window) -> Result<Self, String> {
        let err = |e: &dyn std::fmt::Debug| format!("{e:?}");
        let tx = channel().0.clone();
        let mut os = OsMediaControls::new(platform::config(window)).map_err(|e| err(&e))?;
        os.attach(move |event| {
            if let Some(cmd) = map_event(event) {
                let _ = tx.unbounded_send(cmd);
            }
        })
        .map_err(|e| err(&e))?;
        os.set_permissions(platform::permissions())
            .map_err(|e| err(&e))?;
        Ok(Self {
            os,
            track: None,
            playback: Playback::Stopped,
            position: Duration::ZERO,
            at: Instant::now(),
        })
    }

    /// Gleicht Titel und Status mit dem System ab. Der Titel (`make`) wird nur beim
    /// Wechsel gebaut; die Position nur, wenn sich der Status ändert oder gespult wurde
    /// (das System läuft zwischendurch selbst weiter).
    pub fn sync(
        &mut self,
        id: Option<i64>,
        make: impl FnOnce() -> NowPlaying,
        playback: Playback,
        position: Duration,
    ) {
        let track_changed = id != self.track;
        if track_changed {
            self.track = id;
            let (metadata, cover) = match id {
                Some(_) => {
                    let np = make();
                    let metadata = MediaMetadata {
                        title: Some(np.title),
                        artist: Some(np.artist),
                        album_title: Some(np.album),
                        duration: Some(np.duration),
                        ..MediaMetadata::default()
                    };
                    (metadata, np.cover.and_then(|c| platform::make_cover(&c)))
                }
                None => (MediaMetadata::default(), None),
            };
            if let Err(e) = self.os.set_metadata(metadata) {
                log::debug!("Medientasten: Metadaten: {e:?}");
            }
            // `set_cover(None)` ist nur unter MPRIS erlaubt (macOS: NSException durch `nil` im
            // Dictionary -> abort; Windows: `todo!()`). Dort ersetzt `set_metadata` die
            // Now-Playing-Info samt altem Bild.
            if cover.is_some() || platform::CAN_CLEAR_COVER {
                if let Err(e) = self.os.set_cover(cover) {
                    log::debug!("Medientasten: Cover: {e:?}");
                }
            }
        }

        let expected = match self.playback {
            Playback::Playing => self.position + self.at.elapsed(),
            _ => self.position,
        };
        if track_changed || playback != self.playback || expected.abs_diff(position) > DRIFT {
            let progress = Some(MediaPosition(position));
            let state = match playback {
                Playback::Stopped => MediaPlayback::Stopped,
                Playback::Paused => MediaPlayback::Paused { progress },
                Playback::Playing => MediaPlayback::Playing { progress },
            };
            if let Err(e) = self.os.set_playback(state) {
                log::debug!("Medientasten: Status: {e:?}");
            }
            self.playback = playback;
            self.position = position;
            self.at = Instant::now();
        }
    }
}

fn map_event(event: MediaControlEvent) -> Option<Command> {
    Some(match event {
        MediaControlEvent::Play => Command::Play,
        MediaControlEvent::Pause => Command::Pause,
        MediaControlEvent::Toggle => Command::Toggle,
        MediaControlEvent::Next => Command::Next,
        MediaControlEvent::Previous => Command::Previous,
        MediaControlEvent::Stop => Command::Stop,
        MediaControlEvent::SetPosition(MediaPosition(pos)) => Command::SeekTo(pos),
        MediaControlEvent::Seek(dir) => Command::SeekBy {
            forward: dir == SeekDirection::Forward,
            by: SEEK_STEP,
        },
        MediaControlEvent::SeekBy(dir, by) => Command::SeekBy {
            forward: dir == SeekDirection::Forward,
            by,
        },
        _ => return None,
    })
}

/// Fensterhandle (HWND), das SMTC unter Windows braucht.
#[cfg(windows)]
pub fn window_handle(window: &dyn iced::Window) -> Option<isize> {
    use iced::window::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use souvlaki::platform::apple::{AppleCover, ApplePermissions};

    pub type Window = ();
    pub const CAN_CLEAR_COVER: bool = false;

    pub fn config(_: Window) {}

    pub fn permissions() -> ApplePermissions {
        ApplePermissions {
            toggle_play_pause: true,
            play: true,
            pause: true,
            stop: true,
            previous_track: true,
            next_track: true,
            change_repeat_mode: false,
            change_shuffle_mode: false,
            change_playback_rate: false,
            seek_backward: false,
            seek_forward: false,
            skip_backward: false,
            skip_forward: false,
            change_playback_position: true,
        }
    }

    pub fn make_cover(bytes: &[u8]) -> Option<AppleCover> {
        Some(AppleCover::Bytes(bytes.to_vec()))
    }
}

#[cfg(windows)]
mod platform {
    use souvlaki::platform::windows::{WindowsConfig, WindowsCover, WindowsPermissions};

    /// HWND
    pub type Window = isize;
    pub const CAN_CLEAR_COVER: bool = false;

    pub fn config(hwnd: Window) -> WindowsConfig {
        WindowsConfig {
            hwnd: hwnd as *mut std::ffi::c_void,
        }
    }

    pub fn permissions() -> WindowsPermissions {
        WindowsPermissions {
            is_channel_down_enabled: false,
            is_channel_up_enabled: false,
            is_enabled: true,
            is_fast_forward_enabled: false,
            is_next_enabled: true,
            is_pause_enabled: true,
            is_play_enabled: true,
            is_previous_enabled: true,
            is_record_enabled: false,
            is_rewind_enabled: false,
            is_stop_enabled: true,
        }
    }

    pub fn make_cover(bytes: &[u8]) -> Option<WindowsCover> {
        Some(WindowsCover::Bytes(bytes.to_vec()))
    }
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
mod platform {
    use souvlaki::platform::mpris::{MprisConfig, MprisCover, MprisPermissions};

    pub type Window = ();
    pub const CAN_CLEAR_COVER: bool = true;

    /// Größere Bilder gehen nicht als Base64-Data-URL über D-Bus.
    const MAX_COVER_BYTES: usize = 1_500_000;

    pub fn config(_: Window) -> MprisConfig {
        MprisConfig {
            dbus_name: "fya".to_owned(),
            identity: "FYA".to_owned(),
            // linux/fya.desktop
            desktop_entry: "fya".to_owned(),
        }
    }

    pub fn permissions() -> MprisPermissions {
        MprisPermissions {
            can_go_next: true,
            can_go_previous: true,
            can_play: true,
            can_pause: true,
            can_seek: true,
            can_control: true,
            ..MprisPermissions::none()
        }
    }

    pub fn make_cover(bytes: &[u8]) -> Option<MprisCover> {
        if bytes.len() > MAX_COVER_BYTES {
            return None;
        }
        MprisCover::from_bytes(bytes).ok()
    }
}

#[cfg(not(any(
    target_os = "macos",
    windows,
    all(unix, not(any(target_os = "ios", target_os = "android")))
)))]
compile_error!("Feature `controls` wird auf diesem System nicht unterstützt");
