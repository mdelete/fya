//! Zugriff auf den Musikordner: Ordner-Dialog und (macOS) Security-Scoped
//! Bookmarks für die App Sandbox.
//!
//! Unter der Sandbox gilt die Freigabe, die der User im Ordner-Dialog erteilt,
//! nur bis zum Ende des Prozesses. Damit der Ordner beim nächsten Start ohne
//! erneute Auswahl lesbar ist, wird direkt nach der Auswahl ein
//! Security-Scoped Bookmark erzeugt (-> Datenbank, Tabelle `settings`) und beim
//! Start wieder aufgelöst. Ohne Sandbox (Linux, `cargo run`) genügt der Pfad.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

/// Hält den Zugriff auf den Musikordner, solange das Objekt lebt.
pub struct FolderAccess {
    path: PathBuf,
    #[cfg(target_os = "macos")]
    _scoped: Option<macos::ScopedUrl>,
}

/// Ergebnis von [`FolderAccess::restore`].
pub struct Restored {
    pub access: FolderAccess,
    /// Neues Bookmark, falls das gespeicherte veraltet war (muss neu gespeichert werden).
    pub refreshed_bookmark: Option<Vec<u8>>,
}

impl FolderAccess {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Direkt nach dem Ordner-Dialog: Zugriff sichern und ein Bookmark erzeugen.
    #[cfg(target_os = "macos")]
    pub fn from_picked(path: PathBuf) -> (Self, Option<Vec<u8>>) {
        let scoped = macos::start_for_path(&path);
        let bookmark = macos::create_bookmark(scoped.url());
        (
            Self {
                path,
                _scoped: Some(scoped),
            },
            bookmark,
        )
    }

    #[cfg(not(target_os = "macos"))]
    pub fn from_picked(path: PathBuf) -> (Self, Option<Vec<u8>>) {
        (Self { path }, None)
    }

    /// Beim Start: Zugriff auf den gespeicherten Ordner wiederherstellen.
    pub fn restore(stored_path: &Path, bookmark: Option<&[u8]>) -> Result<Restored> {
        #[cfg(target_os = "macos")]
        {
            if let Some(bytes) = bookmark {
                if let Some((scoped, path, stale)) = macos::resolve(bytes) {
                    let refreshed_bookmark = if stale {
                        macos::create_bookmark(scoped.url())
                    } else {
                        None
                    };
                    return Ok(Restored {
                        access: Self {
                            path,
                            _scoped: Some(scoped),
                        },
                        refreshed_bookmark,
                    });
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = bookmark;

        // Kein (brauchbares) Bookmark: normaler Pfad (Linux, Build ohne Sandbox).
        std::fs::read_dir(stored_path)
            .map_err(|e| anyhow!("{} ist nicht lesbar ({e})", stored_path.display()))?;
        Ok(Restored {
            access: Self {
                path: stored_path.to_path_buf(),
                #[cfg(target_os = "macos")]
                _scoped: None,
            },
            refreshed_bookmark: None,
        })
    }
}

/// Nativer Ordner-Dialog (macOS: NSOpenPanel, Linux: XDG Desktop Portal).
#[cfg(not(target_os = "macos"))]
pub async fn pick_folder(start: Option<PathBuf>) -> Option<PathBuf> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Musikordner wählen");
    if let Some(dir) = start {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder().await.map(|h| h.path().to_path_buf())
}

/// macOS: eigener NSOpenPanel, der versteckte Ordner (z. B. `~/Library`) zeigt
/// (AppKit zeigt dann auch Punkt-Dateien/-Ordner).
#[cfg(target_os = "macos")]
pub async fn pick_folder(start: Option<PathBuf>) -> Option<PathBuf> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    dispatch2::DispatchQueue::main().exec_async(move || {
        let _ = tx.send(macos::pick_folder(start.as_deref()));
    });
    rx.await.ok().flatten()
}

/// Steckt hinter diesem Fehler ein verweigerter Dateizugriff?
pub fn is_permission_denied(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::PermissionDenied)
    })
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::rc::Retained;
    use objc2::runtime::Bool;
    use objc2_foundation::{
        NSData, NSString, NSURL, NSURLBookmarkCreationOptions, NSURLBookmarkResolutionOptions,
    };
    use std::path::{Path, PathBuf};

    /// Security-scoped URL; beendet den Zugriff beim Drop.
    pub struct ScopedUrl {
        url: Retained<NSURL>,
        started: bool,
    }

    impl ScopedUrl {
        pub fn url(&self) -> &NSURL {
            &self.url
        }
    }

    impl Drop for ScopedUrl {
        fn drop(&mut self) {
            if self.started {
                unsafe { self.url.stopAccessingSecurityScopedResource() };
            }
        }
    }

    /// URL für einen frisch gewählten Pfad. Der Ordner-Dialog hat dem Prozess
    /// bereits Zugriff gegeben; `start…` ist hier nur zur Sicherheit.
    pub fn start_for_path(path: &Path) -> ScopedUrl {
        let ns_path = NSString::from_str(&path.to_string_lossy());
        let url = NSURL::fileURLWithPath(&ns_path);
        let started = unsafe { url.startAccessingSecurityScopedResource() };
        ScopedUrl { url, started }
    }

    pub fn create_bookmark(url: &NSURL) -> Option<Vec<u8>> {
        let data = url
            .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
                NSURLBookmarkCreationOptions::WithSecurityScope,
                None,
                None,
            )
            .ok()?;
        Some(data.to_vec())
    }

    /// Bookmark auflösen und den Zugriff starten.
    /// Rückgabe: (URL-Handle, aktueller Pfad, Bookmark veraltet?)
    pub fn resolve(bookmark: &[u8]) -> Option<(ScopedUrl, PathBuf, bool)> {
        let data = NSData::with_bytes(bookmark);
        let mut stale = Bool::NO;
        let url = unsafe {
            NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
                &data,
                NSURLBookmarkResolutionOptions::WithSecurityScope,
                None,
                &mut stale,
            )
        }
        .ok()?;
        let started = unsafe { url.startAccessingSecurityScopedResource() };
        if !started {
            return None;
        }
        let path = PathBuf::from(url.path()?.to_string());
        Some((ScopedUrl { url, started }, path, stale.as_bool()))
    }

    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSModalResponseOK, NSOpenPanel};

    /// Muss auf dem Main-Thread laufen.
    pub fn pick_folder(start: Option<&Path>) -> Option<PathBuf> {
        let mtm = MainThreadMarker::new()?;
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(false);
        panel.setCanChooseDirectories(true);
        panel.setAllowsMultipleSelection(false);
        panel.setShowsHiddenFiles(true);
        panel.setMessage(Some(&NSString::from_str("Musikordner wählen")));
        if let Some(dir) = start {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&dir.to_string_lossy()));
            panel.setDirectoryURL(Some(&url));
        }
        let response = panel.runModal();
        if response != NSModalResponseOK {
            return None;
        }
        let url = panel.URL()?;
        Some(PathBuf::from(url.path()?.to_string()))
    }
}

/// Zeigt die Datei im Dateimanager: macOS = Finder mit markierter Datei
/// (NSWorkspace, funktioniert auch in der Sandbox), sonst der enthaltende
/// Ordner über `xdg-open`.
pub fn reveal_in_file_manager(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSArray, NSString, NSURL};
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let urls = NSArray::from_retained_slice(&[url]);
        objc2_app_kit::NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&urls);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let dir = path.parent().unwrap_or(path);
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}
