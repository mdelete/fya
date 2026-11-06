//! Audiowiedergabe über rodio. MP3/AAC laufen über symphonia (in rodio),
//! Opus wird über ogg + libopus dekodiert und als rodio-Source eingespeist.

use anyhow::{Result, anyhow, bail};
use rodio::cpal::traits::{DeviceTrait, HostTrait};
use rodio::source::SeekError;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Anzeigename eines Geräts.
fn device_name(d: &rodio::cpal::Device) -> Option<String> {
    d.description().ok().map(|d| d.name().to_string())
}

/// Namen aller Ausgabegeräte (ohne Duplikate). Treiberabfrage – nicht im UI-Thread
/// aufrufen.
pub fn output_devices() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(devices) = rodio::cpal::default_host().output_devices() {
        for name in devices.filter_map(|d| device_name(&d)) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// Öffnet einen Ausgabestrom: `Some(name)` = dieses Gerät, `None` = Systemstandard.
/// Verschwindet das Gerät später (z. B. Bluetooth getrennt), meldet cpal das über den
/// Fehler-Callback (ereignisgesteuert, ohne Nachfragen); `lost` wird dann gesetzt.
/// Rückgabe: Strom + Name des geöffneten Geräts.
fn open_stream(
    name: Option<&str>,
    lost: &Arc<AtomicBool>,
) -> Result<(MixerDeviceSink, Option<String>)> {
    let flag = lost.clone();
    let on_error = move |e: rodio::cpal::Error| {
        if e.kind() == rodio::cpal::ErrorKind::DeviceNotAvailable {
            flag.store(true, Ordering::Relaxed);
        }
    };
    let host = rodio::cpal::default_host();
    let device = match name {
        Some(name) => host
            .output_devices()?
            .find(|d| device_name(d).as_deref() == Some(name))
            .ok_or_else(|| anyhow!("Ausgabegerät „{name}“ nicht gefunden"))?,
        None => host
            .default_output_device()
            .ok_or_else(|| anyhow!("Kein Ausgabegerät gefunden"))?,
    };
    let device_name = device_name(&device);
    let sink = DeviceSinkBuilder::from_device(device)?
        .with_error_callback(on_error)
        .open_sink_or_fallback()?;
    Ok((sink, device_name))
}

pub struct Audio {
    stream: MixerDeviceSink,
    sink: Player,
    volume: f32,
    /// true, solange ein Titel geladen ist (spielend oder pausiert)
    active: bool,
    /// Name des Geräts, auf dem der Strom läuft
    device: Option<String>,
    /// wird vom Fehler-Callback gesetzt, wenn das Gerät verschwindet
    lost: Arc<AtomicBool>,
    /// laufender Titel (für den Neustart beim Gerätewechsel)
    current: Option<PathBuf>,
}

impl Audio {
    /// Öffnet beim Start den Systemstandard. Danach wird nur noch auf ausdrücklich
    /// gewählte Geräte gewechselt (siehe [`Audio::set_device`]).
    pub fn new() -> Result<Self> {
        let lost = Arc::new(AtomicBool::new(false));
        let (stream, device) = open_stream(None, &lost)?;
        let sink = Player::connect_new(stream.mixer());
        Ok(Self {
            stream,
            sink,
            volume: 0.8,
            active: false,
            device,
            lost,
            current: None,
        })
    }

    /// Name des Geräts, auf dem gerade ausgegeben wird.
    pub fn device(&self) -> Option<&str> {
        self.device.as_deref()
    }

    /// Wechselt das Ausgabegerät (`None` = Systemstandard, nur für die Wiederherstellung
    /// gedacht). Ein laufender Titel wird auf dem neuen Gerät an derselben Position und
    /// im selben Pausenzustand fortgesetzt. Bei einem Fehler bleibt der bisherige
    /// Strom aktiv.
    pub fn set_device(&mut self, name: Option<&str>) -> Result<()> {
        let resume = match (&self.current, self.active) {
            (Some(path), true) => Some((path.clone(), self.sink.get_pos(), self.sink.is_paused())),
            _ => None,
        };
        let lost = Arc::new(AtomicBool::new(false));
        let (stream, device) = open_stream(name, &lost)?; // scheitert -> nichts verändert

        let old_stream = std::mem::replace(&mut self.stream, stream);
        self.sink = Player::connect_new(self.stream.mixer()); // stoppt den alten Sink
        self.sink.set_volume(self.volume);
        drop(old_stream);
        self.device = device;
        self.lost = lost;

        if let Some((path, pos, paused)) = resume {
            if let Err(e) = self.play(&path) {
                self.active = false;
                return Err(e);
            }
            let _ = self.sink.try_seek(pos);
            if paused {
                self.sink.pause();
            }
        }
        Ok(())
    }

    /// Ist das Gerät seit dem letzten Aufruf verschwunden? (einmalig; nur ein
    /// atomares Flag, kein Treiberzugriff)
    pub fn take_device_lost(&self) -> bool {
        self.lost.swap(false, Ordering::Relaxed)
    }

    /// Gerät verschwunden: Wiedergabe pausieren und auf den Systemstandard umstellen
    /// (Position bleibt erhalten, es läuft nichts von allein weiter).
    pub fn recover_from_lost_device(&mut self) -> Result<()> {
        if self.active {
            self.sink.pause();
        }
        self.set_device(None)
    }

    pub fn play(&mut self, path: &Path) -> Result<()> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        // Neuen Sink anlegen; der alte wird beim Ersetzen gestoppt.
        let sink = Player::connect_new(self.stream.mixer());
        sink.set_volume(self.volume);
        match ext.as_str() {
            "opus" => sink.append(OpusSource::open(path)?),
            _ => sink.append(Decoder::try_from(File::open(path)?)?),
        }
        self.sink = sink;
        self.active = true;
        self.current = Some(path.to_path_buf());
        Ok(())
    }

    pub fn toggle_pause(&mut self) {
        if !self.active {
            return;
        }
        if self.sink.is_paused() {
            self.sink.play();
        } else {
            self.sink.pause();
        }
    }

    pub fn stop(&mut self) {
        self.sink.stop();
        self.active = false;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn is_playing(&self) -> bool {
        self.active && !self.sink.is_paused()
    }

    /// true, wenn der aktuelle Titel zu Ende gespielt wurde (einmalig).
    pub fn take_finished(&mut self) -> bool {
        if self.active && self.sink.empty() {
            self.active = false;
            true
        } else {
            false
        }
    }

    pub fn position(&self) -> Duration {
        if self.active {
            self.sink.get_pos()
        } else {
            Duration::ZERO
        }
    }

    pub fn seek(&mut self, pos: Duration) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        self.sink
            .try_seek(pos)
            .map_err(|e| anyhow::anyhow!("Spulen nicht möglich: {e:?}"))
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        self.sink.set_volume(self.volume);
    }
}

// ---------------------------------------------------------------------------
// Opus
// ---------------------------------------------------------------------------

pub struct OpusSource {
    reader: ogg::PacketReader<BufReader<File>>,
    decoder: opus_rs::OpusDecoder,
    channels: u16,
    scratch: Vec<f32>,
    buf: Vec<f32>,
    pos: usize,
    /// Pre-Skip aus dem OpusHead (Samples pro Kanal bei 48 kHz)
    pre_skip: usize,
    /// noch zu überspringende Samples (interleaved)
    skip: usize,
    done: bool,
}

impl OpusSource {
    pub fn open(path: &Path) -> Result<Self> {
        let mut reader = ogg::PacketReader::new(BufReader::new(File::open(path)?));

        let head = reader.read_packet_expected()?;
        if head.data.len() < 19 || &head.data[..8] != b"OpusHead" {
            bail!("keine gültige Opus-Datei: {}", path.display());
        }
        let channels = head.data[9] as u16;
        let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]) as usize;
        let _tags = reader.read_packet_expected()?; // OpusTags

        if !matches!(channels, 1 | 2) {
            bail!("Opus mit {channels} Kanälen wird nicht unterstützt");
        }
        let decoder = new_decoder(channels)?;

        Ok(Self {
            reader,
            decoder,
            channels,
            // max. Framegröße: 120 ms @ 48 kHz = 5760 Samples pro Kanal
            scratch: vec![0.0; 5760 * channels as usize],
            buf: Vec::new(),
            pos: 0,
            pre_skip,
            skip: pre_skip * channels as usize,
            done: false,
        })
    }
}

impl Iterator for OpusSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        loop {
            if self.pos < self.buf.len() {
                let s = self.buf[self.pos];
                self.pos += 1;
                return Some(s);
            }
            if self.done {
                return None;
            }
            match self.reader.read_packet() {
                Ok(Some(packet)) => {
                    // defekte/leere Pakete überspringen statt abzubrechen
                    let ch = self.channels as usize;
                    let Some(frame_total) = packet_frame_size(&packet.data) else {
                        continue;
                    };
                    let out = &mut self.scratch[..frame_total * ch];
                    if let Ok(n) = self.decoder.decode(&packet.data, frame_total, out) {
                        // n = dekodierte Samples pro Kanal
                        let total = (n * ch).min(out.len());
                        self.buf.clear();
                        self.buf.extend_from_slice(&self.scratch[..total]);
                        let s = self.skip.min(total);
                        self.skip -= s;
                        self.pos = s;
                    }
                }
                Ok(None) | Err(_) => self.done = true,
            }
        }
    }
}

impl Source for OpusSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> rodio::ChannelCount {
        rodio::ChannelCount::new(self.channels).unwrap_or(rodio::ChannelCount::MIN)
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        rodio::SampleRate::new(48_000).expect("48000 != 0")
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }

    /// Spulen: auf die Ogg-Seite vor dem Ziel springen (80 ms Preroll für den
    /// Decoder). Die Genauigkeit liegt bei etwa einer Ogg-Seite.
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        const PREROLL: u64 = 3840; // 80 ms @ 48 kHz
        let target = (pos.as_secs_f64() * 48_000.0) as u64;
        let start = target.saturating_sub(PREROLL);

        self.reader
            .seek_absgp(None, start + self.pre_skip as u64)
            .map_err(|_| SeekError::NotSupported {
                underlying_source: "OpusSource",
            })?;
        // frischer Decoder = definierter Zustand nach dem Sprung
        if let Ok(d) = new_decoder(self.channels) {
            self.decoder = d;
        }
        self.buf.clear();
        self.pos = 0;
        self.done = false;
        self.skip = (target - start) as usize * self.channels as usize;
        Ok(())
    }
}

fn new_decoder(channels: u16) -> Result<opus_rs::OpusDecoder> {
    opus_rs::OpusDecoder::new(48_000, channels as usize)
        .map_err(|e| anyhow::anyhow!("Opus-Decoder: {e}"))
}

/// Gesamtlänge eines Opus-Pakets in Samples pro Kanal bei 48 kHz,
/// berechnet aus dem TOC-Byte (RFC 6716, Abschnitt 3.1/3.2).
fn packet_frame_size(data: &[u8]) -> Option<usize> {
    const RATE: usize = 48_000;
    let toc = *data.first()?;
    let config = (toc >> 3) as usize;
    let frame = match config {
        0..=11 => [RATE / 100, RATE / 50, RATE / 25, RATE * 3 / 50][config & 3],
        12..=15 => [RATE / 100, RATE / 50][config & 1],
        _ => [RATE / 400, RATE / 200, RATE / 100, RATE / 50][config & 3],
    };
    let frames = match toc & 0x03 {
        0 => 1,
        1 | 2 => 2,
        _ => (*data.get(1)? & 0x3f) as usize,
    };
    let total = frame * frames;
    // gültig: 1 Frame bis max. 120 ms (5760 Samples)
    (frames > 0 && total <= 5760).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stumme Testdatei via ffmpeg (None, wenn ffmpeg fehlt).
    fn silent_file(dir: &Path, ext: &str, codec: &str, rate: u32, secs: u32) -> Option<PathBuf> {
        let ffmpeg = crate::mixtape::find_ffmpeg()?;
        let file = dir.join(format!("stumm.{ext}"));
        let ok = std::process::Command::new(ffmpeg)
            .args(["-nostdin", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg(format!("anullsrc=r={rate}:cl=stereo"))
            .args(["-t", &secs.to_string(), "-c:a", codec])
            .arg(&file)
            .status()
            .unwrap()
            .success();
        assert!(ok, "{ext}: ffmpeg");
        Some(file)
    }

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fya-audio-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Auflisten darf ohne Audiogerät (CI) nicht abstürzen; Namen sind eindeutig.
    #[test]
    fn geraeteliste_eindeutig() {
        let names = output_devices();
        eprintln!("Geräte: {names:?}");
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }

    /// Start auf dem Systemstandard; danach nur ausdrücklich gewählte Geräte.
    #[test]
    fn start_standard_dann_explizit() {
        let Ok(mut audio) = Audio::new() else {
            eprintln!("kein Ausgabegerät – übersprungen");
            return;
        };
        let start = audio.device().map(str::to_string);
        eprintln!("Start-Gerät: {start:?}");
        let names = output_devices();
        for name in names.iter().take(3) {
            // manche Geräte verweigern das Öffnen – dann bleibt der alte Strom aktiv
            match audio.set_device(Some(name)) {
                Ok(()) => assert_eq!(audio.device(), Some(name.as_str())),
                Err(e) => {
                    eprintln!("{name}: {e}");
                    assert_ne!(audio.device(), Some(name.as_str()));
                }
            }
        }
        let before = audio.device().map(str::to_string);
        assert!(audio.set_device(Some("gibt-es-nicht")).is_err());
        assert_eq!(
            audio.device().map(str::to_string),
            before,
            "Fehler darf nichts verändern"
        );
        assert!(!audio.take_device_lost());
    }

    /// Gerätewechsel mitten im Titel: Position, Pausenzustand und Lautstärke bleiben.
    #[test]
    fn geraetewechsel_setzt_titel_fort() {
        let dir = temp("switch");
        let Some(file) = silent_file(&dir, "mp3", "libmp3lame", 44100, 5) else {
            eprintln!("ffmpeg nicht installiert – übersprungen");
            return;
        };
        let Ok(mut audio) = Audio::new() else {
            eprintln!("kein Ausgabegerät – übersprungen");
            return;
        };
        audio.set_volume(0.5);
        audio.play(&file).unwrap();
        audio.seek(Duration::from_secs(2)).unwrap();
        audio.toggle_pause();
        assert!(!audio.is_playing());

        for name in output_devices().iter().take(2) {
            if audio.set_device(Some(name)).is_err() {
                continue; // Gerät verweigert das Öffnen
            }
            assert!(
                audio.is_active() && !audio.is_playing(),
                "Pause bleibt erhalten"
            );
            let pos = audio.position().as_secs_f32();
            assert!(
                (1.5..3.0).contains(&pos),
                "Position {pos} s nach Wechsel auf {name}"
            );
            assert!((audio.volume() - 0.5).abs() < 1e-6);
        }
        audio.stop();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Gerät verschwindet (Fehler-Callback setzt das Flag): Wiedergabe wird pausiert,
    /// Position bleibt, es läuft auf dem Systemstandard weiter – und das Flag ist danach
    /// zurückgesetzt. Ohne Flag passiert nichts (kein automatisches Umschalten).
    #[test]
    fn verschwundenes_geraet_pausiert_und_stellt_standard_ein() {
        let dir = temp("lost");
        let Some(file) = silent_file(&dir, "mp3", "libmp3lame", 44100, 5) else {
            eprintln!("ffmpeg nicht installiert – übersprungen");
            return;
        };
        let Ok(mut audio) = Audio::new() else {
            eprintln!("kein Ausgabegerät – übersprungen");
            return;
        };
        let default_dev = audio.device().map(str::to_string);
        audio.play(&file).unwrap();
        audio.seek(Duration::from_secs(2)).unwrap();
        assert!(audio.is_playing());
        assert!(!audio.take_device_lost(), "ohne Ereignis kein Alarm");

        audio.lost.store(true, Ordering::Relaxed); // wie der cpal-Fehler-Callback
        assert!(audio.take_device_lost());
        assert!(!audio.take_device_lost(), "einmalig");

        audio.recover_from_lost_device().unwrap();
        assert!(
            audio.is_active() && !audio.is_playing(),
            "pausiert, nicht gestoppt"
        );
        let pos = audio.position().as_secs_f32();
        assert!((1.5..3.0).contains(&pos), "Position {pos} s");
        assert_eq!(
            audio.device().map(str::to_string),
            default_dev,
            "Systemstandard aktiv"
        );
        audio.stop();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Wiedergabe + Spulen für MP3, Opus und AAC.
    #[test]
    fn abspielen_und_spulen_mp3_opus_aac() {
        let Ok(mut audio) = Audio::new() else {
            eprintln!("kein Ausgabegerät – übersprungen");
            return;
        };
        let dir = temp("play");
        for (ext, codec) in [("mp3", "libmp3lame"), ("opus", "libopus"), ("m4a", "aac")] {
            let Some(file) = silent_file(&dir, ext, codec, 48000, 6) else {
                eprintln!("ffmpeg nicht installiert – übersprungen");
                return;
            };
            audio
                .play(&file)
                .unwrap_or_else(|e| panic!("{ext}: play: {e:#}"));
            assert!(audio.is_playing(), "{ext}");
            audio
                .seek(Duration::from_secs(3))
                .unwrap_or_else(|e| panic!("{ext}: seek: {e:#}"));
            let pos = audio.position().as_secs_f32();
            assert!(
                (2.0..4.5).contains(&pos),
                "{ext}: Position {pos} s nach Spulen auf 3 s"
            );
            audio.stop();
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
