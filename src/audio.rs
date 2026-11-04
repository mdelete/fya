//! Audiowiedergabe über rodio. MP3/AAC laufen über symphonia (in rodio),
//! Opus wird über ogg + libopus dekodiert und als rodio-Source eingespeist.

use anyhow::{Result, bail};
use rodio::source::SeekError;
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::Duration;

pub struct Audio {
    stream: OutputStream,
    sink: Sink,
    volume: f32,
    /// true, solange ein Titel geladen ist (spielend oder pausiert)
    active: bool,
}

impl Audio {
    pub fn new() -> Result<Self> {
        let stream = OutputStreamBuilder::open_default_stream()?;
        let sink = Sink::connect_new(stream.mixer());
        Ok(Self {
            stream,
            sink,
            volume: 0.8,
            active: false,
        })
    }

    pub fn play(&mut self, path: &Path) -> Result<()> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        // Neuen Sink anlegen; der alte wird beim Ersetzen gestoppt.
        let sink = Sink::connect_new(self.stream.mixer());
        sink.set_volume(self.volume);
        match ext.as_str() {
            "opus" => sink.append(OpusSource::open(path)?),
            _ => sink.append(Decoder::try_from(File::open(path)?)?),
        }
        self.sink = sink;
        self.active = true;
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
    fn channels(&self) -> u16 {
        self.channels
    }
    fn sample_rate(&self) -> u32 {
        48_000
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
