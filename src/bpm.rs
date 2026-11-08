//! BPM-Erkennung der gespielten Titel mit `bpm-analyzer` (lgepatcht, aktuell verwendete Version
//! unter `https://github.com/mdelete/bpm-analyzer`). Die Samples, die rodio gerade abspielt,
//! werden über einen Tap abgegriffen und an die Analyse geschickt; das Ergebnis wird im Player
//! angezeigt (letzter Wert) und auf Debug-Ebene geloggt.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bpm_analyzer::{AnalyzerConfig, Sender, begin_with_receiver, bounded};
use rodio::source::SeekError;
use rodio::{ChannelCount, SampleRate, Source};

/// Größe der Warteschlange zwischen Wiedergabe und Analyse (Stereo-Frames, ≈ 1,5 s).
const QUEUE_FRAMES: usize = 65_536;

/// Zugriff auf das Ergebnis der laufenden Analyse eines Titels. Der Sender für den Tap
/// liegt bewusst nicht hier, sondern nur im [`Tap`]: Wird der Tap weggeworfen
/// (Titelende, Wechsel, Stopp), endet die Analyse von selbst.
pub struct Analysis {
    latest: Arc<AtomicU32>,
}

impl Analysis {
    /// Zuletzt erkannte BPM (None, solange noch nichts erkannt wurde).
    pub fn latest_bpm(&self) -> Option<f32> {
        let bits = self.latest.load(Ordering::Relaxed);
        (bits != 0).then(|| f32::from_bits(bits))
    }
}

/// Startet die Analyse für einen Titel (`label` nur fürs Log). Rückgabe: Sender für den
/// [`Tap`] + Ergebnis-Zugriff; `None`, wenn die Analyse für diese Samplerate nicht
/// möglich ist.
pub fn start(label: String, sample_rate: u32) -> Option<(Sender<(f32, f32)>, Analysis)> {
    let (tx, rx) = bounded(QUEUE_FRAMES);
    let results = match begin_with_receiver(AnalyzerConfig::electronic(), sample_rate, rx) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("BPM-Analyse für „{label}“ nicht möglich: {e}");
            return None;
        }
    };

    let latest = Arc::new(AtomicU32::new(0));
    let shared = latest.clone();
    std::thread::spawn(move || {
        let mut logged: Option<f32> = None;
        let mut last_log = Instant::now() - Duration::from_secs(60);
        for detection in results.iter() {
            let Some(bpm) = detection.bpm() else {
                continue;
            };
            shared.store(bpm.to_bits(), Ordering::Relaxed);
            // nicht bei jeder Messung loggen: nur bei Änderung um ≥ 0,5 BPM, höchstens alle 2 s
            let changed = logged.is_none_or(|b| (b - bpm).abs() >= 0.5);
            if changed && last_log.elapsed() >= Duration::from_secs(2) {
                log::debug!("BPM {bpm:.1} – {label}");
                logged = Some(bpm);
                last_log = Instant::now();
            }
        }
    });
    Some((tx, Analysis { latest }))
}

/// Reicht die Samples einer rodio-Source unverändert durch und schickt davon (als
/// Stereo-Frames, Mono wird verdoppelt) eine Kopie an die Analyse. `try_send` blockiert
/// nie: ist die Warteschlange voll, geht ein Frame verloren statt die Wiedergabe zu stören.
pub struct Tap<S> {
    inner: S,
    tx: Sender<(f32, f32)>,
    channels: usize,
    index: usize,
    frame: [f32; 2],
}

impl<S: Source> Tap<S> {
    pub fn new(inner: S, tx: Sender<(f32, f32)>) -> Self {
        Self {
            inner,
            tx,
            channels: 0,
            index: 0,
            frame: [0.0; 2],
        }
    }
}

impl<S: Source> Iterator for Tap<S> {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index == 0 {
            self.channels = usize::from(self.inner.channels().get());
        }
        let sample = self.inner.next()?;
        if self.index < 2 {
            self.frame[self.index] = sample;
        }
        self.index += 1;
        if self.index >= self.channels {
            let [l, r] = self.frame;
            let r = if self.channels > 1 { r } else { l };
            let _ = self.tx.try_send((l, r));
            self.index = 0;
        }
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for Tap<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }
    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }
    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.index = 0; // angefangenes Frame verwerfen
        self.inner.try_seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;
    use std::num::NonZero;

    /// Klick-Spur: pro Schlag ein 30 ms langer, exponentiell abklingender Rauschstoß.
    fn click_track(bpm: f32, rate: u32, secs: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        let period = (60.0 / bpm * rate as f32) as usize;
        let mut seed = 12345u32;
        (0..n)
            .map(|i| {
                let t = (i % period) as f32 / rate as f32;
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                0.9 * noise * (-80.0 * t).exp()
            })
            .collect()
    }

    /// Die (gepatchte) Analyse erkennt das Tempo einer eingespeisten Klick-Spur
    /// und beendet ihren Thread, sobald der Sender weg ist.
    #[test]
    fn erkennt_tempo_eingespeister_samples() {
        for bpm in [120.0f32, 130.0, 150.0] {
            let (tx, rx) = bounded(1 << 16);
            let results = begin_with_receiver(AnalyzerConfig::electronic(), 44_100, rx).unwrap();
            let feeder = std::thread::spawn(move || {
                for s in click_track(bpm, 44_100, 20.0) {
                    // Echtzeit-Zufuhr würde ewig dauern: warten, wenn die Queue voll ist
                    let mut frame = (s, s);
                    while let Err(e) = tx.try_send(frame) {
                        frame = e.into_inner();
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            });
            feeder.join().unwrap();

            // Ergebnisse einsammeln; der Kanal schließt sich, wenn der Analyse-Thread endet
            let detections: Vec<f32> = results.iter().filter_map(|d| d.bpm()).collect();
            assert!(!detections.is_empty(), "{bpm}: keine Erkennung");
            let last = *detections.last().unwrap();
            let ok = detections
                .iter()
                .rev()
                .take(3)
                .any(|d| (d - bpm).abs() <= 4.0);
            assert!(
                ok,
                "{bpm} BPM erwartet, zuletzt erkannt: {:?}",
                &detections[detections.len().saturating_sub(5)..]
            );
            eprintln!("{bpm} BPM -> zuletzt {last:.1}");
        }
    }

    /// Der Tap reicht die Samples unverändert durch und liefert Stereo-Frames (Mono wird
    /// verdoppelt, bei mehr als zwei Kanälen zählen die ersten beiden).
    #[test]
    fn tap_reicht_durch_und_liefert_frames() {
        let ch = |n: u16| NonZero::new(n).unwrap();
        let rate = NonZero::new(44_100).unwrap();
        for (channels, samples, frames) in [
            (
                1u16,
                vec![0.1f32, 0.2, 0.3],
                vec![(0.1, 0.1), (0.2, 0.2), (0.3, 0.3)],
            ),
            (2, vec![0.1, 0.2, 0.3, 0.4], vec![(0.1, 0.2), (0.3, 0.4)]),
            (
                3,
                vec![0.1, 0.2, 0.9, 0.3, 0.4, 0.8],
                vec![(0.1, 0.2), (0.3, 0.4)],
            ),
        ] {
            let (tx, rx) = bounded(16);
            let tap = Tap::new(SamplesBuffer::new(ch(channels), rate, samples.clone()), tx);
            let out: Vec<f32> = tap.collect(); // Tap wird dabei verbraucht -> Sender weg
            assert_eq!(out, samples, "{channels} Kanäle: Samples verändert");
            let got: Vec<(f32, f32)> = rx.try_iter().collect();
            assert_eq!(got, frames, "{channels} Kanäle");
        }
    }

    /// Wird der Tap (Sender) weggeworfen, enden Analyse- und Ergebnis-Thread von selbst:
    /// kein Thread bleibt pro gespieltem Titel zurück.
    #[test]
    fn threads_enden_wenn_tap_wegfaellt() {
        let (tx, analysis) = start("test".into(), 44_100).unwrap();
        // der Ergebnis-Thread hält einen Klon des Ergebnis-Speichers
        assert_eq!(Arc::strong_count(&analysis.latest), 2);
        for s in click_track(120.0, 44_100, 5.0) {
            let mut frame = (s, s);
            while let Err(e) = tx.try_send(frame) {
                frame = e.into_inner();
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        drop(tx); // wie beim Titelwechsel/Stopp
        let start = Instant::now();
        while Arc::strong_count(&analysis.latest) > 1 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "Threads laufen weiter"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Nicht unterstützte Samplerate: Analyse startet nicht (kein Absturz, kein Thread).
    #[test]
    fn unbekannte_samplerate_wird_abgelehnt() {
        assert!(start("test".into(), 12_345).is_none());
    }

    /// Ende-zu-Ende über den Player (stumm, ~10 s Echtzeit): `cargo test -- --ignored`.
    #[test]
    #[ignore = "dauert ca. 10 s (spielt stumm in Echtzeit ab)"]
    fn player_analysiert_gespielten_titel() {
        let _ = env_logger::builder().is_test(true).try_init(); // RUST_LOG=debug zeigt die BPM-Zeile
        let Some(ffmpeg) = crate::mixtape::find_ffmpeg() else {
            eprintln!("ffmpeg nicht installiert – übersprungen");
            return;
        };
        let mut audio = crate::audio::Audio::new();
        if audio.device().is_none() {
            eprintln!("kein Ausgabegerät – übersprungen");
            return;
        }
        let dir = std::env::temp_dir().join(format!("fya-bpm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("klick.mp3");
        let ok = std::process::Command::new(ffmpeg)
            .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("aevalsrc=0.9*random(0)*exp(-80*mod(t\\,60/130)):s=44100:d=14")
            .args(["-c:a", "libmp3lame"])
            .arg(&file)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ffmpeg");

        audio.set_volume(0.0); // stumm; der Tap sitzt vor der Lautstärke
        audio.set_analyze_bpm(true);
        audio.play(&file).unwrap();
        let start = Instant::now();
        let mut bpm = None;
        while start.elapsed() < Duration::from_secs(13) {
            std::thread::sleep(Duration::from_millis(250));
            bpm = audio.current_bpm().or(bpm);
            if bpm.is_some_and(|b| (b - 130.0).abs() <= 4.0) {
                break;
            }
        }
        eprintln!("erkannt: {bpm:?} nach {:.1?}", start.elapsed());
        assert!(
            bpm.is_some_and(|b| (b - 130.0).abs() <= 4.0),
            "130 BPM erwartet, erkannt: {bpm:?}"
        );
        audio.stop();
        let _ = std::fs::remove_dir_all(dir);
    }
}
