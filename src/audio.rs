//! Audiowiedergabe über rodio. MP3/AAC laufen über symphonia (in rodio),
//! Opus wird über ogg + libopus dekodiert und als rodio-Source eingespeist.

use anyhow::{Result, anyhow, bail};
use rodio::cpal::traits::{DeviceTrait, HostTrait};
use rodio::source::SeekError;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use rust_i18n::t;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Ergebnis-Zugriff der BPM-Analyse des laufenden Titels. Ohne das Feature `bpm` gibt es keine
/// Analyse: der Platzhalter wird nie erzeugt.
#[cfg(feature = "bpm")]
type Analysis = crate::bpm::Analysis;
#[cfg(not(feature = "bpm"))]
#[allow(dead_code)]
struct Analysis;
#[cfg(not(feature = "bpm"))]
impl Analysis {
    fn latest_bpm(&self) -> Option<f32> {
        None
    }
}

/// Anzeigename eines Geräts.
fn device_name(d: &rodio::cpal::Device) -> Option<String> {
    d.description().ok().map(|d| d.name().to_string())
}

/// Audio-Hosts in der Reihenfolge, in der sie benutzt werden: zuerst der Standard-Host der
/// Plattform, danach die übrigen verfügbaren. Unter Linux ist der Standard-Host PulseAudio
/// (bzw. PipeWire über dessen Pulse-Schnittstelle), sobald ein solcher Server läuft; sonst ALSA.
fn hosts() -> Vec<rodio::cpal::Host> {
    let default = rodio::cpal::default_host();
    let default_id = default.id();
    let mut hosts = vec![default];
    for id in rodio::cpal::available_hosts() {
        if id != default_id {
            if let Ok(host) = rodio::cpal::host_from_id(id) {
                hosts.push(host);
            }
        }
    }
    hosts
}

/// Namen aller Ausgabegeräte (ohne Duplikate) des ersten Hosts, der welche hat – unter
/// Linux also die Geräte, die auch ein Desktop-Mixer kennt (HDMI, Bluetooth-Lautsprecher …)
/// statt roher ALSA-Einträge. Treiberabfrage – nicht im UI-Thread aufrufen.
pub fn output_devices() -> Vec<String> {
    for host in hosts() {
        let mut names: Vec<String> = Vec::new();
        if let Ok(devices) = host.output_devices() {
            for name in devices.filter_map(|d| device_name(&d)) {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        if !names.is_empty() {
            return names;
        }
    }
    Vec::new()
}

/// Öffnet einen Ausgabestrom auf `device`. Erst mit der Standardkonfiguration des Geräts
/// (inkl. rodios Ausweichen auf andere Konfigurationen); scheitert schon das Ermitteln der
/// Standardkonfiguration (z. B. ALSA-„default“ auf dem Raspberry Pi: „The requested audio
/// device is not available“), werden alle vom Gerät gemeldeten Konfigurationen einzeln
/// probiert.
fn open_on_device<E>(device: &rodio::cpal::Device, on_error: E) -> Result<MixerDeviceSink>
where
    E: FnMut(rodio::cpal::Error) + Send + Clone + 'static,
{
    let name = device_name(device).unwrap_or_else(|| "?".into());
    let first_err = match DeviceSinkBuilder::from_device(device.clone()) {
        Ok(builder) => match builder
            .with_error_callback(on_error.clone())
            .open_sink_or_fallback()
        {
            Ok(sink) => return Ok(sink),
            Err(e) => anyhow!("{e}"),
        },
        Err(e) => anyhow!("{e}"),
    };
    log::debug!("Ausgabegerät „{name}“: Standardkonfiguration nicht nutzbar ({first_err:#})");

    // Alle gemeldeten Konfigurationen einzeln probieren
    if let Ok(configs) = rodio::stream::supported_output_configs(device) {
        for config in configs {
            let opened = DeviceSinkBuilder::default()
                .with_device(device.clone())
                .with_supported_config(&config)
                .with_error_callback(on_error.clone())
                .open_stream();
            match opened {
                Ok(sink) => {
                    log::debug!("Ausgabegerät „{name}“: läuft mit {config:?}");
                    return Ok(sink);
                }
                Err(e) => log::debug!("Ausgabegerät „{name}“: {config:?} abgelehnt ({e})"),
            }
        }
    }
    Err(first_err.context(t!("audio.open_failed", name = name)))
}

/// Systemstandard eines Hosts; ist er nicht nutzbar, werden – wie in rodios
/// `open_default_sink` – die übrigen Geräte des Hosts der Reihe nach probiert.
fn open_default_on_host<E>(
    host: &rodio::cpal::Host,
    on_error: &E,
) -> Result<(MixerDeviceSink, Option<String>)>
where
    E: FnMut(rodio::cpal::Error) + Send + Clone + 'static,
{
    let default = host.default_output_device();
    let first_err = match &default {
        Some(device) => match open_on_device(device, on_error.clone()) {
            Ok(sink) => return Ok((sink, device_name(device))),
            Err(e) => e,
        },
        None => anyhow!(t!("audio.no_default")),
    };
    log::warn!(
        "{}: Systemstandard nicht nutzbar ({first_err:#}); probiere andere Ausgabegeräte",
        host.id().name()
    );

    // Ausweichen: übrige Geräte (ohne „null“-Treiber) der Reihe nach
    let default_name = default.as_ref().and_then(device_name);
    if let Ok(devices) = host.output_devices() {
        for device in devices {
            let usable = device
                .description()
                .map(|d| d.driver().is_none_or(|drv| drv != "null"))
                .unwrap_or(false);
            let name = device_name(&device);
            if !usable || name == default_name {
                continue;
            }
            match open_on_device(&device, on_error.clone()) {
                Ok(sink) => {
                    log::warn!(
                        "Ausgabe läuft auf „{}“ statt auf dem Systemstandard",
                        name.as_deref().unwrap_or("?")
                    );
                    return Ok((sink, name));
                }
                Err(e) => log::debug!("Ausgabegerät {name:?} nicht nutzbar ({e:#})"),
            }
        }
    }
    Err(first_err)
}

/// Öffnet einen Ausgabestrom: `Some(name)` = dieses Gerät, `None` = Systemstandard.
/// Probiert die Hosts der Reihe nach (siehe [`hosts`]): Läuft der Standard-Host nicht, fällt
/// der Systemstandard auf den nächsten zurück. Ein ausdrücklich gewähltes Gerät wird nur
/// genau so geöffnet, ohne Ausweichen auf andere Geräte.
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

    // Ausdrücklich gewähltes Gerät: genau dieses (im ersten Host, der es kennt)
    if let Some(name) = name {
        for host in hosts() {
            let Ok(devices) = host.output_devices() else {
                continue;
            };
            if let Some(device) = devices
                .into_iter()
                .find(|d| device_name(d).as_deref() == Some(name))
            {
                let sink = open_on_device(&device, on_error)?;
                return Ok((sink, device_name(&device)));
            }
        }
        bail!(t!("audio.not_found", name = name));
    }

    // Systemstandard: Host für Host
    let mut first_err = None;
    for host in hosts() {
        match open_default_on_host(&host, &on_error) {
            Ok(opened) => {
                log::debug!("Audio-Host: {}", host.id().name());
                return Ok(opened);
            }
            Err(e) => {
                log::warn!("Audio-Host {} nicht nutzbar: {e:#}", host.id().name());
                first_err.get_or_insert(e);
            }
        }
    }
    Err(first_err
        .unwrap_or_else(|| anyhow!(t!("audio.none_found")))
        .context(t!("audio.none_usable")))
}

/// Geöffneter Ausgabestrom samt Player.
struct Output {
    stream: MixerDeviceSink,
    sink: Player,
}

pub struct Audio {
    /// None, wenn kein Ausgabegerät geöffnet werden konnte: Die App startet trotzdem
    /// und man kann im Player ein Gerät wählen.
    out: Option<Output>,
    volume: f32,
    /// true, solange ein Titel geladen ist (spielend oder pausiert)
    active: bool,
    /// Name des Geräts, auf dem der Strom läuft
    device: Option<String>,
    /// wird vom Fehler-Callback gesetzt, wenn das Gerät verschwindet
    lost: Arc<AtomicBool>,
    /// laufender Titel (für den Neustart beim Gerätewechsel)
    current: Option<PathBuf>,
    /// BPM der gespielten Titel analysieren (Einstellung) + Analyse des laufenden Titels
    analyze_bpm: bool,
    analysis: Option<Analysis>,
    /// Fehlertext, wenn beim Start kein Ausgabegerät nutzbar war (einmal abholbar)
    startup_error: Option<String>,
}

impl Audio {
    /// Öffnet beim Start den Systemstandard (bei Problemen ein anderes Gerät, siehe
    /// [`open_stream`]). Danach wird nur noch auf ausdrücklich gewählte Geräte gewechselt
    /// (siehe [`Audio::set_device`]). Findet sich gar kein Gerät, läuft die App ohne Ton
    /// weiter (statt zu beenden): [`Audio::take_startup_error`] nennt den Grund.
    pub fn new() -> Self {
        let lost = Arc::new(AtomicBool::new(false));
        let (out, device, startup_error) = match open_stream(None, &lost) {
            Ok((stream, device)) => {
                let sink = Player::connect_new(stream.mixer());
                (Some(Output { stream, sink }), device, None)
            }
            Err(e) => {
                log::error!("Kein Ausgabegerät nutzbar: {e:#}");
                (
                    None,
                    None,
                    Some(t!("audio.none_usable_err", e = format!("{e:#}")).to_string()),
                )
            }
        };
        Self {
            out,
            volume: 0.8,
            active: false,
            device,
            lost,
            current: None,
            analyze_bpm: false,
            analysis: None,
            startup_error,
        }
    }

    /// Grund, warum beim Start kein Ausgabegerät geöffnet werden konnte (einmalig).
    pub fn take_startup_error(&mut self) -> Option<String> {
        self.startup_error.take()
    }

    fn sink(&self) -> Option<&Player> {
        self.out.as_ref().map(|o| &o.sink)
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
        // Läuft schon auf diesem Gerät: nichts tun (ALSA-Geräte lassen sich nicht doppelt öffnen)
        if let Some(name) = name {
            if self.out.is_some()
                && self.device.as_deref() == Some(name)
                && !self.lost.load(Ordering::Relaxed)
            {
                return Ok(());
            }
        }
        let resume = match (&self.current, self.active, self.sink()) {
            (Some(path), true, Some(sink)) => {
                Some((path.clone(), sink.get_pos(), sink.is_paused()))
            }
            _ => None,
        };
        let lost = Arc::new(AtomicBool::new(false));
        let (stream, device) = open_stream(name, &lost)?; // scheitert -> nichts verändert

        let sink = Player::connect_new(stream.mixer());
        sink.set_volume(self.volume);
        // alter Strom und Sink werden hier ersetzt (stoppt die alte Wiedergabe)
        self.out = Some(Output { stream, sink });
        self.device = device;
        self.lost = lost;
        self.startup_error = None;

        if let Some((path, pos, paused)) = resume {
            if let Err(e) = self.play(&path) {
                self.active = false;
                return Err(e);
            }
            if let Some(sink) = self.sink() {
                let _ = sink.try_seek(pos);
                if paused {
                    sink.pause();
                }
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
            if let Some(sink) = self.sink() {
                sink.pause();
            }
        }
        self.set_device(None)
    }

    pub fn play(&mut self, path: &Path) -> Result<()> {
        let Some(out) = &self.out else {
            bail!(t!("audio.unavailable"));
        };
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        // Neuen Sink anlegen; der alte wird beim Ersetzen gestoppt.
        let sink = Player::connect_new(out.stream.mixer());
        sink.set_volume(self.volume);
        // Analyse des vorigen Titels beenden (der Thread endet, sobald sein Tap weg ist)
        self.analysis = None;
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Opus (eigener Decoder). Eine .ogg/.oga kann Vorbis, FLAC oder Opus enthalten; symphonia
        // kann Opus nicht decodieren, daher die Weiche am Inhalt. Alles andere erkennt der
        // Standard-Decoder selbst (Feature `ogg`: Vorbis, FLAC; Speex wird nie unterstützt).
        let is_opus = ext == "opus"
            || (cfg!(feature = "ogg")
                && matches!(ext.as_str(), "ogg" | "oga")
                && is_ogg_opus(path));
        if is_opus {
            self.append_source(&sink, OpusSource::open(path)?, label);
        } else {
            self.append_source(&sink, Decoder::try_from(File::open(path)?)?, label);
        }
        if let Some(out) = &mut self.out {
            out.sink = sink;
        }
        self.active = true;
        self.current = Some(path.to_path_buf());
        Ok(())
    }

    /// Ohne das Feature `bpm`: Quelle unverändert anhängen.
    #[cfg(not(feature = "bpm"))]
    fn append_source<S>(&mut self, sink: &Player, source: S, _label: String)
    where
        S: Source + Send + 'static,
    {
        sink.append(source);
    }

    /// Hängt die Quelle an; ist die BPM-Analyse an, läuft sie über einen Tap mit.
    #[cfg(feature = "bpm")]
    fn append_source<S>(&mut self, sink: &Player, source: S, label: String)
    where
        S: Source + Send + 'static,
    {
        let analysis = self
            .analyze_bpm
            .then(|| crate::bpm::start(label, source.sample_rate().get()))
            .flatten();
        match analysis {
            Some((tx, analysis)) => {
                sink.append(crate::bpm::Tap::new(source, tx));
                self.analysis = Some(analysis);
            }
            None => sink.append(source),
        }
    }

    /// BPM-Analyse der gespielten Titel ein-/ausschalten (gilt ab dem nächsten Titel).
    pub fn set_analyze_bpm(&mut self, on: bool) {
        self.analyze_bpm = on;
        if !on {
            self.analysis = None;
        }
    }

    /// Zuletzt erkannte BPM des laufenden Titels (None: aus oder noch nichts erkannt).
    /// (Anzeige im Player)
    pub fn current_bpm(&self) -> Option<f32> {
        self.analysis.as_ref().and_then(|a| a.latest_bpm())
    }

    pub fn toggle_pause(&mut self) {
        if !self.active {
            return;
        }
        if let Some(sink) = self.sink() {
            if sink.is_paused() {
                sink.play();
            } else {
                sink.pause();
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(sink) = self.sink() {
            sink.stop();
        }
        self.active = false;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn is_playing(&self) -> bool {
        self.active && self.sink().is_some_and(|s| !s.is_paused())
    }

    /// true, wenn der aktuelle Titel zu Ende gespielt wurde (einmalig).
    pub fn take_finished(&mut self) -> bool {
        if self.active && self.sink().is_some_and(|s| s.empty()) {
            self.active = false;
            true
        } else {
            false
        }
    }

    pub fn position(&self) -> Duration {
        match (self.active, self.sink()) {
            (true, Some(sink)) => sink.get_pos(),
            _ => Duration::ZERO,
        }
    }

    pub fn seek(&mut self, pos: Duration) -> Result<()> {
        let (true, Some(sink)) = (self.active, self.sink()) else {
            return Ok(());
        };
        sink.try_seek(pos)
            .map_err(|e| anyhow::anyhow!(t!("audio.seek_failed", e = format!("{e:?}"))))
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        if let Some(sink) = self.sink() {
            sink.set_volume(self.volume);
        }
    }
}

/// Enthält die Ogg-Datei einen Opus-Stream? (Der Kopf `OpusHead` steht im ersten Paket,
/// also in den ersten Bytes der ersten Seite.)
fn is_ogg_opus(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 128];
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let n = file.read(&mut head).unwrap_or(0);
    head[..n].starts_with(b"OggS") && head[..n].windows(8).any(|w| w == b"OpusHead")
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
            bail!(t!("audio.invalid_opus", path = path.display()));
        }
        let channels = head.data[9] as u16;
        let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]) as usize;
        let _tags = reader.read_packet_expected()?; // OpusTags

        if !matches!(channels, 1 | 2) {
            bail!(t!("audio.opus_channels", channels = channels));
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
        .map_err(|e| anyhow::anyhow!(t!("audio.opus_decoder", e = e)))
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

    /// Player auf dem Systemstandard; None (Test überspringen), wenn kein Gerät da ist (CI).
    fn audio_or_skip() -> Option<Audio> {
        let audio = Audio::new();
        if audio.device().is_none() {
            eprintln!("kein Ausgabegerät – übersprungen");
            return None;
        }
        Some(audio)
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
        let Some(mut audio) = audio_or_skip() else {
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
        let Some(mut audio) = audio_or_skip() else {
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
        let Some(mut audio) = audio_or_skip() else {
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
        let Some(mut audio) = audio_or_skip() else {
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

    /// Ohne Ausgabegerät (z. B. Raspberry Pi, wo der Systemstandard nicht öffnet) darf die
    /// App nicht abstürzen: alle Funktionen sind harmlos, `play` meldet einen klaren Fehler,
    /// und das Wählen eines Geräts stellt die Wiedergabe wieder her.
    #[test]
    fn ohne_ausgabegeraet_laeuft_die_app_weiter() {
        let mut audio = Audio {
            out: None,
            volume: 0.8,
            active: false,
            device: None,
            lost: Arc::new(AtomicBool::new(false)),
            current: None,
            analyze_bpm: false,
            analysis: None,
            startup_error: Some("Kein Ausgabegerät nutzbar: Test".into()),
        };
        assert_eq!(
            audio.take_startup_error().as_deref(),
            Some("Kein Ausgabegerät nutzbar: Test")
        );
        assert_eq!(audio.take_startup_error(), None, "einmalig");

        rust_i18n::set_locale("de"); // die Meldung wird übersetzt
        let err = audio.play(Path::new("/gibt/es/nicht.mp3")).unwrap_err();
        assert!(err.to_string().contains("Kein Ausgabegerät"), "{err}");
        audio.toggle_pause();
        audio.stop();
        audio.set_volume(0.3);
        assert!(!audio.is_active() && !audio.is_playing() && !audio.take_finished());
        assert_eq!(audio.position(), Duration::ZERO);
        audio.seek(Duration::from_secs(1)).unwrap();
        assert!((audio.volume() - 0.3).abs() < 1e-6);

        // Gerät wählen (Systemstandard) stellt den Betrieb her, sofern ein Gerät existiert
        if audio.set_device(None).is_ok() {
            assert!(audio.device().is_some());
            assert!(audio.out.is_some());
            assert!((audio.volume() - 0.3).abs() < 1e-6);
        }
    }

    /// Der erste Host ist der Standard-Host der Plattform; es gibt mindestens einen.
    #[test]
    fn hosts_standard_host_zuerst() {
        let hosts = hosts();
        assert!(!hosts.is_empty());
        assert_eq!(hosts[0].id(), rodio::cpal::default_host().id());
        let ids: Vec<_> = hosts.iter().map(|h| h.id()).collect();
        let mut unique = ids.clone();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "Hosts doppelt: {ids:?}");
    }

    /// Dasselbe Gerät noch einmal zu wählen ändert nichts (und scheitert nicht an exklusiv
    /// geöffneten ALSA-Geräten).
    #[test]
    fn gleiches_geraet_waehlen_ist_ein_noop() {
        let Some(mut audio) = audio_or_skip() else {
            return;
        };
        let name = audio.device().unwrap().to_string();
        audio.set_device(Some(&name)).unwrap();
        assert_eq!(audio.device(), Some(name.as_str()));
        // nach „Gerät verschwunden“ wird dagegen wirklich neu geöffnet
        audio.lost.store(true, Ordering::Relaxed);
        let _ = audio.set_device(Some(&name));
        assert!(
            !audio.take_device_lost(),
            "Flag gehört zum alten Strom und ist weg"
        );
    }

    /// Ogg-/FLAC-Dateien erzeugen (ffmpeg): (Dateiname, ffmpeg-Argumente).
    #[cfg(feature = "ogg")]
    fn ogg_samples(dir: &Path, ffmpeg: &Path) -> Vec<PathBuf> {
        let cases: [(&str, &[&str]); 4] = [
            (
                "vorbis.ogg",
                &["-c:a", "vorbis", "-strict", "-2", "-f", "ogg"],
            ),
            ("flac_in_ogg.oga", &["-c:a", "flac", "-f", "ogg"]),
            ("native.flac", &["-c:a", "flac"]),
            ("opus_in.ogg", &["-c:a", "libopus", "-f", "ogg"]), // .ogg mit Opus-Inhalt
        ];
        cases
            .iter()
            .map(|(name, args)| {
                let file = dir.join(name);
                let ok = std::process::Command::new(ffmpeg)
                    .args([
                        "-nostdin",
                        "-loglevel",
                        "error",
                        "-f",
                        "lavfi",
                        "-i",
                        "anullsrc=r=48000:cl=stereo",
                        "-t",
                        "6",
                    ])
                    .args(*args)
                    .arg(&file)
                    .status()
                    .unwrap()
                    .success();
                assert!(ok, "{name}: ffmpeg");
                file
            })
            .collect()
    }

    /// Feature `ogg`: Vorbis, FLAC in Ogg, natives FLAC und Opus in einer .ogg lassen sich
    /// abspielen und spulen (Opus über die Weiche zum eigenen Decoder).
    #[cfg(feature = "ogg")]
    #[test]
    fn ogg_vorbis_flac_und_opus_weiche() {
        let Some(ffmpeg) = crate::mixtape::find_ffmpeg() else {
            eprintln!("ffmpeg nicht installiert – übersprungen");
            return;
        };
        let Some(mut audio) = audio_or_skip() else {
            return;
        };
        let dir = temp("ogg");
        for file in ogg_samples(&dir, &ffmpeg) {
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            assert_eq!(
                is_ogg_opus(&file),
                name.starts_with("opus_in"),
                "{name}: Opus-Erkennung"
            );
            audio
                .play(&file)
                .unwrap_or_else(|e| panic!("{name}: play: {e:#}"));
            assert!(audio.is_playing(), "{name}");
            audio
                .seek(Duration::from_secs(3))
                .unwrap_or_else(|e| panic!("{name}: seek: {e:#}"));
            let pos = audio.position().as_secs_f32();
            assert!(
                (2.0..4.5).contains(&pos),
                "{name}: Position {pos} s nach Spulen auf 3 s"
            );
            audio.stop();
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Ohne das Feature `ogg` kennt die App weder Ogg noch FLAC (und `is_ogg_opus` ist harmlos).
    #[cfg(not(feature = "ogg"))]
    #[test]
    fn ohne_feature_kein_ogg() {
        for ext in ["ogg", "oga", "flac"] {
            assert!(!crate::db::AUDIO_EXTENSIONS.contains(&ext), "{ext}");
        }
        assert!(!is_ogg_opus(Path::new("/gibt/es/nicht.ogg")));
    }

    /// Ohne das Feature `bpm` bleibt die Wiedergabe unverändert: Einschalten der Analyse ist
    /// ein Nichts-Tun, es gibt nie einen BPM-Wert.
    #[cfg(not(feature = "bpm"))]
    #[test]
    fn ohne_feature_bpm_keine_analyse() {
        let dir = temp("nobpm");
        let Some(file) = silent_file(&dir, "mp3", "libmp3lame", 44100, 3) else {
            eprintln!("ffmpeg nicht installiert – übersprungen");
            return;
        };
        let Some(mut audio) = audio_or_skip() else {
            return;
        };
        audio.set_volume(0.0);
        audio.set_analyze_bpm(true);
        audio.play(&file).unwrap();
        assert!(audio.is_playing());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(audio.current_bpm(), None);
        audio.stop();
        let _ = std::fs::remove_dir_all(dir);
    }
}
