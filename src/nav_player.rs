//! Navidrome playback engine: HTTP download -> symphonia decode -> 44.1 kHz stereo -> ALSA sink.
//! It runs on its own thread; the receiver talks to it through `Handle` and `Event`.
use crate::navidrome::{Client, Track};
use librespot::playback::{
    SAMPLE_RATE,
    audio_backend::Sink,
    convert::Converter,
    decoder::AudioPacket,
    dither::{TriangularDitherer, mk_ditherer},
};
use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    time::Duration,
};
use symphonia::core::{
    audio::{SampleBuffer, SignalSpec},
    codecs::{CODEC_TYPE_NULL, DecoderOptions},
    errors::Error as DecodeFailure,
    formats::FormatOptions,
    io::{MediaSource, MediaSourceStream},
    meta::MetadataOptions,
    probe::Hint,
};
use tokio::{runtime::Handle as Runtime, sync::mpsc::UnboundedSender};

const PREBUFFER: u64 = 192 * 1024;
const MAX_DOWNLOAD: u64 = 160 * 1024 * 1024;
const CHUNK_SAMPLES: usize = 8192;
const RESTART_AFTER_MS: u32 = 3000;
const MAX_FAILURES: u32 = 3;
const MAX_BAD_PACKETS: u32 = 50;

#[derive(Debug)]
pub enum Event {
    Started(Track),
    Paused,
    Resumed,
    Stopped,
    Failed(String),
}
enum Cmd {
    Configure(Option<Arc<Client>>),
    Play(Vec<Track>),
    Pause,
    Resume,
    Next,
    Previous,
    Stop,
}
#[derive(Clone)]
pub struct Handle {
    tx: Sender<Cmd>,
    position: Arc<AtomicU32>,
}
impl Handle {
    pub fn configure(&self, client: Option<Arc<Client>>) {
        let _ = self.tx.send(Cmd::Configure(client));
    }
    pub fn play(&self, queue: Vec<Track>) {
        let _ = self.tx.send(Cmd::Play(queue));
    }
    pub fn pause(&self) {
        let _ = self.tx.send(Cmd::Pause);
    }
    pub fn resume(&self) {
        let _ = self.tx.send(Cmd::Resume);
    }
    pub fn next(&self) {
        let _ = self.tx.send(Cmd::Next);
    }
    pub fn previous(&self) {
        let _ = self.tx.send(Cmd::Previous);
    }
    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }
    pub fn position_ms(&self) -> u32 {
        self.position.load(Ordering::Relaxed)
    }
}

pub fn spawn(
    make_sink: impl FnOnce() -> Box<dyn Sink> + Send + 'static,
    runtime: Runtime,
    events: UnboundedSender<Event>,
    tmp_dir: PathBuf,
) -> io::Result<Handle> {
    let (tx, rx) = mpsc::channel();
    let position = Arc::new(AtomicU32::new(0));
    let handle = Handle {
        tx,
        position: position.clone(),
    };
    std::thread::Builder::new()
        .name("spotiya-navidrome".into())
        .stack_size(1024 * 1024)
        .spawn(move || {
            let _ = fs::remove_dir_all(&tmp_dir);
            let mut engine = Engine {
                client: None,
                runtime,
                events,
                sink: make_sink(),
                converter: Converter::new(Some(mk_ditherer::<TriangularDitherer>)),
                tmp_dir,
                position,
                queue: Vec::new(),
                index: 0,
                sink_open: false,
                prefetch: None,
            };
            engine.run(&rx);
        })?;
    Ok(handle)
}

enum Outcome {
    Ended,
    Next,
    Previous,
    Restart,
    Replace(Vec<Track>),
    Stop,
    Failed(String),
}

struct Engine {
    client: Option<Arc<Client>>,
    runtime: Runtime,
    events: UnboundedSender<Event>,
    sink: Box<dyn Sink>,
    converter: Converter,
    tmp_dir: PathBuf,
    position: Arc<AtomicU32>,
    queue: Vec<Track>,
    index: usize,
    sink_open: bool,
    prefetch: Option<(usize, Download)>,
}
impl Engine {
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
    fn run(&mut self, rx: &Receiver<Cmd>) {
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Configure(client) => self.client = client,
                Cmd::Play(queue) => {
                    self.queue = queue;
                    self.index = 0;
                    self.play_queue(rx);
                }
                Cmd::Resume if !self.queue.is_empty() => self.play_queue(rx),
                _ => {}
            }
        }
        self.close_sink();
    }
    fn open_sink(&mut self) -> Result<(), String> {
        let mut last = String::new();
        for attempt in 0..4 {
            if self.sink_open {
                return Ok(());
            }
            if attempt > 0 {
                // Another player may still be releasing the ALSA device.
                std::thread::sleep(Duration::from_millis(300));
            }
            match self.sink.start() {
                Ok(()) => self.sink_open = true,
                Err(e) => last = e.to_string(),
            }
        }
        if self.sink_open {
            Ok(())
        } else {
            Err(format!("Audio device unavailable: {last}"))
        }
    }
    fn close_sink(&mut self) {
        if self.sink_open {
            let _ = self.sink.stop();
            self.sink_open = false;
        }
    }
    fn play_queue(&mut self, rx: &Receiver<Cmd>) {
        let mut failures = 0;
        while let Some(track) = self.queue.get(self.index).cloned() {
            match self.play_track(rx, &track) {
                Outcome::Ended | Outcome::Next => {
                    failures = 0;
                    self.index += 1;
                }
                Outcome::Previous => {
                    failures = 0;
                    self.index = self.index.saturating_sub(1);
                }
                Outcome::Restart => failures = 0,
                Outcome::Replace(queue) => {
                    failures = 0;
                    self.prefetch = None;
                    self.queue = queue;
                    self.index = 0;
                }
                Outcome::Stop => {
                    self.prefetch = None;
                    self.close_sink();
                    self.emit(Event::Stopped);
                    return;
                }
                Outcome::Failed(message) => {
                    self.emit(Event::Failed(message));
                    failures += 1;
                    if failures >= MAX_FAILURES {
                        break;
                    }
                    self.index += 1;
                }
            }
        }
        if self.index >= self.queue.len() {
            self.index = 0;
        }
        self.prefetch = None;
        self.close_sink();
        self.emit(Event::Stopped);
    }
    /// Handles queued commands without blocking; an outcome ends the current track.
    fn interrupt(&mut self, rx: &Receiver<Cmd>) -> Option<Outcome> {
        loop {
            let cmd = match rx.try_recv() {
                Ok(cmd) => cmd,
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => return Some(Outcome::Stop),
            };
            if let Some(outcome) = self.command(cmd, rx) {
                return Some(outcome);
            }
        }
    }
    fn command(&mut self, cmd: Cmd, rx: &Receiver<Cmd>) -> Option<Outcome> {
        match cmd {
            Cmd::Pause => self.pause(rx),
            Cmd::Resume => None,
            Cmd::Next => Some(Outcome::Next),
            Cmd::Previous => Some(
                if self.position.load(Ordering::Relaxed) > RESTART_AFTER_MS {
                    Outcome::Restart
                } else {
                    Outcome::Previous
                },
            ),
            Cmd::Stop => Some(Outcome::Stop),
            Cmd::Play(queue) => Some(Outcome::Replace(queue)),
            Cmd::Configure(client) => {
                self.client = client;
                None
            }
        }
    }
    fn pause(&mut self, rx: &Receiver<Cmd>) -> Option<Outcome> {
        self.close_sink();
        self.emit(Event::Paused);
        loop {
            match rx.recv() {
                Ok(Cmd::Resume) => {
                    self.emit(Event::Resumed);
                    return None;
                }
                Ok(Cmd::Pause) => {}
                Ok(Cmd::Configure(client)) => self.client = client,
                Ok(Cmd::Next) => return Some(Outcome::Next),
                Ok(Cmd::Previous) => return Some(Outcome::Previous),
                Ok(Cmd::Play(queue)) => return Some(Outcome::Replace(queue)),
                Ok(Cmd::Stop) | Err(_) => return Some(Outcome::Stop),
            }
        }
    }
    fn download(&self, client: &Client, track: &Track) -> Result<Download, String> {
        Download::start(
            &self.runtime,
            client.stream(&track.id),
            &self.tmp_dir,
            client.config().format == "raw",
        )
    }
    fn play_track(&mut self, rx: &Receiver<Cmd>, track: &Track) -> Outcome {
        let Some(client) = self.client.clone() else {
            return Outcome::Failed("Navidrome is not configured".into());
        };
        self.position.store(0, Ordering::Relaxed);
        let mut played_ms = 0;
        let outcome = self.stream_track(rx, &client, track, &mut played_ms);
        let counts = track.duration_ms >= 30_000
            && played_ms >= (track.duration_ms / 2).min(240_000)
            && !matches!(outcome, Outcome::Failed(_));
        if client.config().scrobble && counts {
            let (client, id) = (client.clone(), track.id.clone());
            self.runtime
                .spawn(async move { client.scrobble(&id, true).await });
        }
        outcome
    }
    fn stream_track(
        &mut self,
        rx: &Receiver<Cmd>,
        client: &Arc<Client>,
        track: &Track,
        played_ms: &mut u32,
    ) -> Outcome {
        let download = match self.prefetch.take() {
            Some((index, download)) if index == self.index => download,
            _ => match self.download(client, track) {
                Ok(download) => download,
                Err(e) => return Outcome::Failed(e),
            },
        };
        if let Err(outcome) = self.prebuffer(rx, &download) {
            return outcome;
        }
        let mut decoder_state = match open_decoder(&download) {
            Ok(state) => state,
            Err(e) => return Outcome::Failed(e),
        };
        if let Some(next) = self.queue.get(self.index + 1) {
            self.prefetch = self
                .download(client, next)
                .ok()
                .map(|d| (self.index + 1, d));
        }
        self.emit(Event::Started(track.clone()));
        if client.config().scrobble {
            let (client, id) = (client.clone(), track.id.clone());
            self.runtime
                .spawn(async move { client.scrobble(&id, false).await });
        }
        let mut out = Vec::with_capacity(CHUNK_SAMPLES * 2);
        let mut frames_out = 0u64;
        let mut bad_packets = 0;
        loop {
            if let Some(outcome) = self.interrupt(rx) {
                return outcome;
            }
            let decoded = match decoder_state.next(&mut out) {
                Ok(Some(())) => {
                    bad_packets = 0;
                    true
                }
                Ok(None) => break,
                Err(DecodeFailure::DecodeError(_)) if bad_packets < MAX_BAD_PACKETS => {
                    bad_packets += 1;
                    false
                }
                Err(e) => return Outcome::Failed(format!("Playback error: {e}")),
            };
            if decoded && out.len() >= CHUNK_SAMPLES {
                frames_out += (out.len() / 2) as u64;
                if let Err(e) = self.write(&mut out) {
                    return Outcome::Failed(e);
                }
                *played_ms = self.publish_position(frames_out);
            }
        }
        if !out.is_empty() {
            frames_out += (out.len() / 2) as u64;
            if let Err(e) = self.write(&mut out) {
                return Outcome::Failed(e);
            }
            *played_ms = self.publish_position(frames_out);
        }
        // A natural end counts as fully played even if the server's duration is rounded.
        *played_ms = (*played_ms).max(track.duration_ms);
        Outcome::Ended
    }
    fn publish_position(&self, frames: u64) -> u32 {
        let ms = (frames * 1000 / u64::from(SAMPLE_RATE)).min(u64::from(u32::MAX)) as u32;
        self.position.store(ms, Ordering::Relaxed);
        ms
    }
    fn write(&mut self, out: &mut Vec<f64>) -> Result<(), String> {
        self.open_sink()?;
        let samples = std::mem::replace(out, Vec::with_capacity(CHUNK_SAMPLES * 2));
        self.sink
            .write(AudioPacket::Samples(samples), &mut self.converter)
            .map_err(|e| e.to_string())
    }
    /// Waits for headers and the first bytes, staying responsive to commands.
    fn prebuffer(&mut self, rx: &Receiver<Cmd>, download: &Download) -> Result<(), Outcome> {
        loop {
            {
                let state = download.shared.lock();
                if let Some(error) = &state.error {
                    return Err(Outcome::Failed(error.clone()));
                }
                if state.done
                    || (state.headers
                        && state.written >= PREBUFFER.min(state.total.unwrap_or(PREBUFFER)))
                {
                    return Ok(());
                }
                let _ = download
                    .shared
                    .ready
                    .wait_timeout(state, Duration::from_millis(100));
            }
            if let Some(outcome) = self.interrupt(rx) {
                return Err(outcome);
            }
        }
    }
}

struct Decoder {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    samples: Option<SampleBuffer<f32>>,
    spec: Option<SignalSpec>,
    resampler: Resampler,
    input: Vec<[f32; 2]>,
}
fn open_decoder(download: &Download) -> Result<Decoder, String> {
    let source = GrowingFile::open(download)?;
    let mut hint = Hint::new();
    if let Some(kind) = download.shared.lock().content_type.clone() {
        hint.mime_type(&kind);
    }
    let stream = MediaSourceStream::new(Box::new(source), Default::default());
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Unsupported audio: {e}"))?;
    let format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("No audio track")?;
    let track_id = track.id;
    let decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Unsupported codec: {e}"))?;
    Ok(Decoder {
        format,
        decoder,
        track_id,
        samples: None,
        spec: None,
        resampler: Resampler::new(SAMPLE_RATE),
        input: Vec::new(),
    })
}
impl Decoder {
    /// Decodes one packet and appends 44.1 kHz stereo samples to `out`; `None` is end of stream.
    fn next(&mut self, out: &mut Vec<f64>) -> Result<Option<()>, DecodeFailure> {
        let packet = loop {
            match self.format.next_packet() {
                Ok(packet) if packet.track_id() == self.track_id => break packet,
                Ok(_) => {}
                Err(DecodeFailure::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
        };
        let buffer = self.decoder.decode(&packet)?;
        let spec = *buffer.spec();
        let channels = spec.channels.count();
        if channels == 0 {
            return Ok(Some(()));
        }
        if self.spec != Some(spec) {
            self.resampler = Resampler::new(spec.rate);
            self.spec = Some(spec);
            self.samples = None;
        }
        let needed = buffer.capacity() * channels;
        if self.samples.as_ref().is_none_or(|s| s.capacity() < needed) {
            self.samples = Some(SampleBuffer::new(buffer.capacity() as u64, spec));
        }
        let samples = self.samples.as_mut().expect("sample buffer allocated");
        samples.copy_interleaved_ref(buffer);
        self.input.clear();
        self.input
            .extend(samples.samples().chunks_exact(channels).map(|f| {
                let right = if channels > 1 { f[1] } else { f[0] };
                [f[0], right]
            }));
        self.resampler.process(&self.input, out);
        Ok(Some(()))
    }
}

/// Linear-interpolation resampler to the sink's fixed 44.1 kHz rate.
struct Resampler {
    step: f64,
    position: f64,
    pending: Vec<[f32; 2]>,
}
impl Resampler {
    fn new(rate: u32) -> Self {
        Self {
            step: f64::from(rate.max(1)) / f64::from(SAMPLE_RATE),
            position: 0.0,
            pending: Vec::new(),
        }
    }
    fn process(&mut self, input: &[[f32; 2]], out: &mut Vec<f64>) {
        let clean = |s: f32| {
            if s.is_finite() {
                f64::from(s).clamp(-1.0, 1.0)
            } else {
                0.0
            }
        };
        if (self.step - 1.0).abs() < f64::EPSILON {
            out.extend(input.iter().flatten().map(|s| clean(*s)));
            return;
        }
        self.pending.extend_from_slice(input);
        loop {
            let index = self.position as usize;
            let (Some(a), Some(b)) = (self.pending.get(index), self.pending.get(index + 1)) else {
                break;
            };
            let frac = (self.position - index as f64) as f32;
            for channel in 0..2 {
                out.push(clean(a[channel] + (b[channel] - a[channel]) * frac));
            }
            self.position += self.step;
        }
        let consumed = (self.position as usize).min(self.pending.len().saturating_sub(1));
        self.pending.drain(..consumed);
        self.position -= consumed as f64;
    }
}

#[derive(Default)]
struct State {
    written: u64,
    total: Option<u64>,
    done: bool,
    headers: bool,
    error: Option<String>,
    content_type: Option<String>,
}
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    cancel: AtomicBool,
    exact_length: bool,
}
impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
/// A track being downloaded into a temporary file; dropping it cancels and deletes the file.
struct Download {
    shared: Arc<Shared>,
    path: PathBuf,
}
impl Download {
    fn start(
        runtime: &Runtime,
        request: reqwest::RequestBuilder,
        dir: &Path,
        exact_length: bool,
    ) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("{}.part", uuid::Uuid::new_v4().simple()));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("Cannot create temporary file: {e}"))?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
            cancel: AtomicBool::new(false),
            exact_length,
        });
        runtime.spawn(download(request, file, shared.clone()));
        Ok(Self { shared, path })
    }
}
impl Drop for Download {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Release);
        self.shared.ready.notify_all();
        let _ = fs::remove_file(&self.path);
    }
}
async fn download(request: reqwest::RequestBuilder, mut file: File, shared: Arc<Shared>) {
    let result: Result<(), String> = async {
        let mut response = request
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if !response.status().is_success() {
            return Err(format!("Navidrome HTTP {}", response.status().as_u16()));
        }
        let kind = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            });
        if kind
            .as_deref()
            .is_some_and(|k| k.starts_with("text/") || k.contains("json") || k.contains("xml"))
        {
            return Err("Navidrome did not return audio".into());
        }
        {
            let mut state = shared.lock();
            state.total = response.content_length();
            state.content_type = kind.filter(|k| !k.is_empty());
            state.headers = true;
        }
        shared.ready.notify_all();
        while !shared.cancel.load(Ordering::Acquire) {
            let chunk = tokio::time::timeout(Duration::from_secs(30), response.chunk())
                .await
                .map_err(|_| "Navidrome stream stalled".to_owned())?
                .map_err(|e| e.without_url().to_string())?;
            let Some(chunk) = chunk else { break };
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            let mut state = shared.lock();
            state.written += chunk.len() as u64;
            if state.written > MAX_DOWNLOAD {
                return Err("Track is too large".into());
            }
            drop(state);
            shared.ready.notify_all();
        }
        Ok(())
    }
    .await;
    let mut state = shared.lock();
    state.headers = true;
    state.done = true;
    state.error = result.err();
    drop(state);
    shared.ready.notify_all();
}

/// Reads the temporary file as it grows, waiting for the downloader when it is ahead of it.
struct GrowingFile {
    file: File,
    shared: Arc<Shared>,
    position: u64,
}
impl GrowingFile {
    fn open(download: &Download) -> Result<Self, String> {
        Ok(Self {
            file: File::open(&download.path).map_err(|e| e.to_string())?,
            shared: download.shared.clone(),
            position: 0,
        })
    }
}
impl Read for GrowingFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = self.shared.lock();
        loop {
            if self.shared.cancel.load(Ordering::Acquire) {
                return Err(io::Error::other("cancelled"));
            }
            if self.position < state.written {
                let available = (state.written - self.position).min(buf.len() as u64) as usize;
                drop(state);
                let n = self.file.read_at(&mut buf[..available], self.position)?;
                self.position += n as u64;
                return Ok(n);
            }
            if state.done {
                return match &state.error {
                    Some(e) => Err(io::Error::other(e.clone())),
                    None => Ok(0),
                };
            }
            state = self
                .shared
                .ready
                .wait_timeout(state, Duration::from_millis(200))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}
impl Seek for GrowingFile {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(d) => i128::from(self.position) + i128::from(d),
            SeekFrom::End(d) => {
                let total = self.byte_len().ok_or(io::ErrorKind::Unsupported)?;
                i128::from(total) + i128::from(d)
            }
        };
        self.position = u64::try_from(target).map_err(|_| io::ErrorKind::InvalidInput)?;
        Ok(self.position)
    }
}
impl MediaSource for GrowingFile {
    fn is_seekable(&self) -> bool {
        self.byte_len().is_some()
    }
    // A transcoded stream has no trustworthy length, so it is read strictly forward.
    fn byte_len(&self) -> Option<u64> {
        if self.shared.exact_length {
            self.shared.lock().total
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navidrome::{Config, mock};
    use std::sync::Mutex as StdMutex;

    fn wav(rate: u32, channels: u16, seconds: u32) -> Vec<u8> {
        let frames = rate * seconds;
        let data_len = frames * u32::from(channels) * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        out.extend_from_slice(&(channels * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for n in 0..frames {
            let sample =
                ((n as f32 / rate as f32 * 440.0 * std::f32::consts::TAU).sin() * 12000.0) as i16;
            for _ in 0..channels {
                out.extend_from_slice(&sample.to_le_bytes());
            }
        }
        out
    }
    #[test]
    fn resampler_converts_rates_without_clicks_and_keeps_state_across_packets() {
        let ramp: Vec<[f32; 2]> = (0..4800).map(|n| [n as f32 / 4800.0; 2]).collect();
        let mut whole = Vec::new();
        Resampler::new(48000).process(&ramp, &mut whole);
        let frames = whole.len() / 2;
        assert!((4410..=4412).contains(&frames), "{frames}");
        assert!(whole.windows(2).all(|w| w[1] >= w[0] - 1e-9));
        let mut split = Vec::new();
        let mut resampler = Resampler::new(48000);
        for part in ramp.chunks(333) {
            resampler.process(part, &mut split);
        }
        assert_eq!(split.len(), whole.len());
        assert!(split.iter().zip(&whole).all(|(a, b)| (a - b).abs() < 1e-6));
        let mut doubled = Vec::new();
        Resampler::new(22050).process(&ramp[..2205], &mut doubled);
        assert!((4408..=4412).contains(&(doubled.len() / 2)));
        let mut same = vec![];
        Resampler::new(44100).process(&[[2.0, f32::NAN]], &mut same);
        assert_eq!(same, [1.0, 0.0]);
    }
    #[test]
    fn growing_file_waits_for_the_downloader_and_reports_errors_and_cancel() {
        let dir = std::env::temp_dir().join(format!("spotiya-gf-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.part");
        let mut writer = File::create(&path).unwrap();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
            cancel: AtomicBool::new(false),
            exact_length: true,
        });
        shared.lock().total = Some(10);
        let download = Download {
            shared: shared.clone(),
            path: path.clone(),
        };
        let mut reader = GrowingFile::open(&download).unwrap();
        assert!(reader.is_seekable());
        assert_eq!(reader.seek(SeekFrom::End(-2)).unwrap(), 8);
        reader.seek(SeekFrom::Start(0)).unwrap();
        let feeder = {
            let shared = shared.clone();
            std::thread::spawn(move || {
                for part in [&b"hello"[..], &b"world"[..]] {
                    std::thread::sleep(Duration::from_millis(50));
                    writer.write_all(part).unwrap();
                    shared.lock().written += part.len() as u64;
                    shared.ready.notify_all();
                }
                shared.lock().done = true;
                shared.ready.notify_all();
            })
        };
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        feeder.join().unwrap();
        assert_eq!(text, "helloworld");
        shared.lock().error = Some("boom".into());
        reader.seek(SeekFrom::Start(10)).unwrap();
        assert_eq!(reader.read(&mut [0; 4]).unwrap_err().to_string(), "boom");
        drop(download);
        assert!(!path.exists());
        assert!(reader.read(&mut [0; 4]).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    const FIXTURES: [(&str, &[u8]); 5] = [
        ("mp3", include_bytes!("../testdata/tone.mp3")),
        ("mono", include_bytes!("../testdata/tone-mono-22k.mp3")),
        ("flac", include_bytes!("../testdata/tone.flac")),
        ("ogg", include_bytes!("../testdata/tone.ogg")),
        ("m4a", include_bytes!("../testdata/tone.m4a")),
    ];
    fn fixture_route(target: &str) -> (&'static str, Vec<u8>) {
        let name = target
            .split("id=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .unwrap_or("");
        let kind = if name.ends_with("mp3") || name == "mono" {
            "audio/mpeg"
        } else {
            "application/octet-stream"
        };
        match FIXTURES.iter().find(|(n, _)| *n == name) {
            Some((_, bytes)) => (kind, bytes.to_vec()),
            None => ("application/json", b"{}".to_vec()),
        }
    }
    /// Every container/codec we promise must decode to about one second of 44.1 kHz stereo.
    /// Transcoded (`mp3`) streams have no usable length and are read strictly forward.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn decodes_every_supported_format_seekable_and_forward_only() {
        let url = mock::serve(fixture_route).await;
        let tmp = std::env::temp_dir().join(format!("spotiya-fx-{}", uuid::Uuid::new_v4()));
        for (name, _) in FIXTURES {
            for exact in [true, false] {
                if !exact && !matches!(name, "mp3" | "mono") {
                    continue;
                }
                let mut config = Config::new("https://music.example", "alice", "pw").unwrap();
                config.url = url.clone();
                config.format = if exact { "raw" } else { "mp3" }.into();
                let client = Client::new(config).unwrap();
                let (runtime, dir) = (Runtime::current(), tmp.clone());
                let (frames, peak) = tokio::task::spawn_blocking(move || {
                    let download =
                        Download::start(&runtime, client.stream(name), &dir, exact).unwrap();
                    let mut decoder = open_decoder(&download)?;
                    let (mut samples, mut peak) = (0usize, 0f64);
                    let mut out = Vec::new();
                    while decoder.next(&mut out).map_err(|e| e.to_string())?.is_some() {
                        samples += out.len();
                        peak = out.iter().fold(peak, |p, s| p.max(s.abs()));
                        out.clear();
                    }
                    Ok::<_, String>((samples / 2, peak))
                })
                .await
                .unwrap()
                .unwrap_or_else(|e| panic!("{name} exact={exact}: {e}"));
                assert!(
                    (43_000..=49_000).contains(&frames),
                    "{name} exact={exact}: {frames} frames"
                );
                // ffmpeg's sine source peaks at about -21 dBFS (0.088).
                assert!(
                    (0.07..=0.12).contains(&peak),
                    "{name} exact={exact}: peak {peak}"
                );
            }
        }
        let _ = fs::remove_dir_all(tmp);
    }

    struct Capture {
        samples: Arc<StdMutex<Vec<f64>>>,
        log: Arc<StdMutex<Vec<&'static str>>>,
    }
    impl Sink for Capture {
        fn start(&mut self) -> librespot::playback::audio_backend::SinkResult<()> {
            self.log.lock().unwrap().push("start");
            Ok(())
        }
        fn stop(&mut self) -> librespot::playback::audio_backend::SinkResult<()> {
            self.log.lock().unwrap().push("stop");
            Ok(())
        }
        fn write(
            &mut self,
            packet: AudioPacket,
            _: &mut Converter,
        ) -> librespot::playback::audio_backend::SinkResult<()> {
            // Real sinks block until the device drains; pace the fake one the same way.
            std::thread::sleep(Duration::from_millis(40));
            if let AudioPacket::Samples(s) = packet {
                self.samples.lock().unwrap().extend(s);
            }
            Ok(())
        }
    }
    fn route(target: &str) -> (&'static str, Vec<u8>) {
        match target.split('?').next().unwrap() {
            "/rest/stream" if target.contains("id=wide") => ("audio/wav", wav(44100, 2, 2)),
            "/rest/stream" if target.contains("id=mono") => ("audio/x-wav", wav(22050, 1, 1)),
            _ => (
                "application/json",
                br#"{"subsonic-response":{"status":"failed","error":{"code":70,"message":"Not found"}}}"#.to_vec(),
            ),
        }
    }
    fn track(id: &str) -> Track {
        Track {
            id: id.into(),
            title: id.into(),
            duration_ms: 1000,
            ..Default::default()
        }
    }
    async fn next(events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Event {
        tokio::time::timeout(Duration::from_secs(20), events.recv())
            .await
            .expect("engine event")
            .expect("engine alive")
    }
    #[tokio::test]
    async fn plays_a_queue_with_resampling_pause_failure_skip_and_stop() {
        let url = mock::serve(route).await;
        let mut config = Config::new("https://music.example", "alice", "pw").unwrap();
        config.url = url;
        config.format = "raw".into();
        config.scrobble = false;
        let samples = Arc::new(StdMutex::new(Vec::new()));
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let tmp = std::env::temp_dir().join(format!("spotiya-eng-{}", uuid::Uuid::new_v4()));
        let sink = Capture {
            samples: samples.clone(),
            log: log.clone(),
        };
        let handle = spawn(
            move || Box::new(sink),
            Runtime::current(),
            events_tx,
            tmp.clone(),
        )
        .unwrap();
        handle.configure(Some(Arc::new(Client::new(config).unwrap())));
        handle.play(vec![track("missing"), track("mono"), track("wide")]);
        assert!(
            matches!(next(&mut events).await, Event::Failed(m) if m == "Not found" || m.contains("not return audio"))
        );
        assert!(matches!(next(&mut events).await, Event::Started(t) if t.id == "mono"));
        handle.pause();
        assert!(matches!(next(&mut events).await, Event::Paused));
        handle.resume();
        assert!(matches!(next(&mut events).await, Event::Resumed));
        assert!(matches!(next(&mut events).await, Event::Started(t) if t.id == "wide"));
        assert!(matches!(next(&mut events).await, Event::Stopped));
        // 1 s of 22.05 kHz mono and 2 s of 44.1 kHz stereo, both delivered as 44.1 kHz stereo.
        let frames = samples.lock().unwrap().len() / 2;
        assert!(
            (44100 * 3 - 8..=44100 * 3 + 8).contains(&frames),
            "{frames}"
        );
        assert!(samples.lock().unwrap().iter().all(|s| s.abs() <= 1.0));
        assert_eq!(log.lock().unwrap().last(), Some(&"stop"));
        // The queue is kept: resume replays the first track after the end of the queue.
        handle.resume();
        assert!(matches!(next(&mut events).await, Event::Failed(_)));
        handle.stop();
        loop {
            if matches!(next(&mut events).await, Event::Stopped) {
                break;
            }
        }
        drop(handle);
        std::thread::sleep(Duration::from_millis(200));
        let leftovers = fs::read_dir(&tmp).map(|d| d.count()).unwrap_or(0);
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(tmp);
    }
}
