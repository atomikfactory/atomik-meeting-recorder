use crate::{
    capture_meter::{CaptureMeter, SourceLevel},
    meeting_title::best_effort_title,
    recording_paths::create_recording_directory,
    recording_store::RecordingStore,
    recording_wav::{spawn_writer, AudioPacket, WavWriterHandle},
    store::{AudioSourceDirection, AudioSourceSelection},
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub source_id: String,
    pub name: String,
    pub direction: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub file_path: String,
    pub frames_written: u64,
    pub dropped_samples: u64,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recording {
    pub recording_id: String,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub duration_ms: u64,
    pub status: String,
    pub tracks: Vec<Track>,
    pub error: Option<String>,
    #[serde(default)]
    pub meeting_title: Option<String>,
    #[serde(default)]
    pub storage_path: Option<String>,
    #[serde(default)]
    pub auto_transcribe: bool,
    pub transcription_status: String,
    pub appflowy_status: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct RecordingState {
    pub status: String,
    pub active: Option<Recording>,
    pub recent: Vec<Recording>,
    pub error: Option<String>,
    pub levels: Vec<SourceLevel>,
}
impl Default for RecordingState {
    fn default() -> Self {
        Self {
            status: "idle".into(),
            active: None,
            recent: vec![],
            error: None,
            levels: vec![],
        }
    }
}
type SharedState = Arc<Mutex<RecordingState>>;
type Reply = mpsc::Sender<Result<(), String>>;
enum Command {
    Start(Vec<AudioSourceSelection>, Reply),
    Stop(Reply),
    Refresh(Reply),
    Shutdown(Reply),
}

/// Native stream handles stay on their owner thread. Only commands and snapshots cross threads.
#[derive(Clone)]
pub struct RecordingService {
    tx: mpsc::Sender<Command>,
    state: SharedState,
}
impl RecordingService {
    pub fn new(dir: PathBuf) -> Self {
        Self::with_backend_and_title(dir, || Box::new(NativeCapture), best_effort_title)
    }
    #[cfg(test)]
    fn with_backend(
        dir: PathBuf,
        factory: impl FnOnce() -> Box<dyn AudioCapture> + Send + 'static,
    ) -> Self {
        Self::with_backend_and_title(dir, factory, || None)
    }
    fn with_backend_and_title(
        dir: PathBuf,
        factory: impl FnOnce() -> Box<dyn AudioCapture> + Send + 'static,
        title_provider: fn() -> Option<String>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(RecordingState {
            status: "processing".into(),
            ..Default::default()
        }));
        let shared = state.clone();
        if let Err(error) = thread::Builder::new()
            .name("recording-coordinator".into())
            .spawn(move || run(dir, factory(), rx, shared, title_provider))
        {
            set_error(
                &state,
                format!("Could not start recording service: {error}"),
            );
        }
        Self { tx, state }
    }
    pub fn state(&self) -> RecordingState {
        self.state
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| RecordingState {
                status: "failed".into(),
                error: Some("Recording state lock failed".into()),
                ..Default::default()
            })
    }
    pub fn start(&self, sources: Vec<AudioSourceSelection>) -> Result<(), String> {
        self.call(|r| Command::Start(sources, r))
    }
    pub fn stop(&self) -> Result<(), String> {
        self.call(Command::Stop)
    }
    pub fn shutdown(&self) -> Result<(), String> {
        self.call(Command::Shutdown)
    }
    pub fn refresh_history(&self) -> Result<(), String> {
        self.call(Command::Refresh)
    }
    fn call(&self, command: impl FnOnce(Reply) -> Command) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(command(tx))
            .map_err(|_| "Recording service is unavailable".to_string())?;
        // A timeout here could report failure and leave a late capture running.
        rx.recv()
            .map_err(|_| "Recording coordinator stopped unexpectedly".to_string())?
    }
}
#[derive(Clone, Copy)]
pub(crate) struct CaptureFormat {
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
}
trait AudioCapture {
    fn resolve(&self, source: &AudioSourceSelection) -> Result<Box<dyn CaptureDevice>, String>;
}
/// `pub(crate)`: implemented outside this module by the Linux PulseAudio/
/// PipeWire monitor backend (`linux_pulse.rs`), which feeds the same
/// downstream pipeline (WAV writer, capture meter, frame timeline) as the
/// CPAL-backed `NativeDevice`/`cpal::Stream` implementations below.
pub(crate) trait CaptureDevice {
    fn format(&self) -> CaptureFormat;
    fn build(&self, sink: CaptureSink) -> Result<Box<dyn CaptureStream>, String>;
}
pub(crate) trait CaptureStream {
    fn play(&self) -> Result<(), String>;
}
pub(crate) struct CaptureSink {
    pub(crate) sender: mpsc::SyncSender<AudioPacket>,
    pub(crate) errors: mpsc::SyncSender<String>,
    pub(crate) dropped: Arc<AtomicU64>,
    pub(crate) gaps: Arc<AtomicU64>,
    pub(crate) timestamp_adjustments: Arc<AtomicU64>,
    pub(crate) last_buffer_ms: Arc<AtomicU64>,
    pub(crate) meter: Arc<CaptureMeter>,
    pub(crate) source_name: String,
    pub(crate) started: Instant,
    /// Flips to true once the frame timeline has observed a packet that
    /// needed no correction, i.e. capture has caught up with real time.
    /// `route_stream_error` uses it to treat driver-reported discontinuities
    /// from the start-up window the same way the timeline itself does.
    pub(crate) settled: Arc<AtomicBool>,
}
struct Active {
    rec: Recording,
    started: Instant,
    streams: Vec<Box<dyn CaptureStream>>,
    writers: Vec<WavWriterHandle>,
    drops: Vec<Arc<AtomicU64>>,
    gaps: Vec<Arc<AtomicU64>>,
    timestamp_adjustments: Vec<Arc<AtomicU64>>,
    last_buffers: Vec<Arc<AtomicU64>>,
    meters: Vec<Arc<CaptureMeter>>,
    errors: mpsc::Receiver<String>,
}
struct Engine {
    dir: PathBuf,
    store: RecordingStore,
    backend: Box<dyn AudioCapture>,
    active: Option<Active>,
    pending_save: Option<Recording>,
    state: SharedState,
    title_provider: fn() -> Option<String>,
}
const MAX_WAV_DATA: u64 = 0xf000_0000;
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn set_error(state: &SharedState, error: String) {
    if let Ok(mut s) = state.lock() {
        s.status = "failed".into();
        s.error = Some(error);
    }
}
fn run(
    dir: PathBuf,
    backend: Box<dyn AudioCapture>,
    rx: mpsc::Receiver<Command>,
    state: SharedState,
    title_provider: fn() -> Option<String>,
) {
    let store = RecordingStore::open(&dir).and_then(|store| {
        store.recover()?;
        Ok(store)
    });
    let store = match store {
        Ok(store) => store,
        Err(error) => {
            set_error(&state, error.clone());
            // Even a storage initialization failure must allow Quit.
            while let Ok(command) = rx.recv() {
                match command {
                    Command::Shutdown(reply) => {
                        let _ = reply.send(Ok(()));
                        break;
                    }
                    Command::Start(_, reply) | Command::Stop(reply) | Command::Refresh(reply) => {
                        let _ = reply.send(Err(error.clone()));
                    }
                }
            }
            return;
        }
    };
    match store.recent() {
        Ok(recent) => {
            if let Ok(mut s) = state.lock() {
                s.recent = recent;
                s.status = "idle".into();
            }
        }
        Err(error) => set_error(&state, error),
    }
    let mut engine = Engine {
        dir,
        store,
        backend,
        active: None,
        pending_save: None,
        state,
        title_provider,
    };
    loop {
        engine.monitor();
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Command::Start(sources, reply)) => {
                let result = engine.start(sources);
                let started = result.is_ok();
                if reply.send(result).is_err() && started {
                    let _ = engine.stop(Some("Start requester disconnected".into()));
                }
            }
            Ok(Command::Stop(reply)) => {
                let _ = reply.send(engine.stop(None));
            }
            Ok(Command::Refresh(reply)) => {
                let _ = reply.send(engine.refresh_history());
            }
            Ok(Command::Shutdown(reply)) => {
                let result = if engine.active.is_some() {
                    engine.stop(None)
                } else {
                    engine.retry_save()
                };
                let done = result.is_ok();
                let _ = reply.send(result);
                if done {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = engine.stop(None);
                break;
            }
        }
    }
}
impl Engine {
    fn refresh_history(&mut self) -> Result<(), String> {
        let recent = self.store.recent()?;
        if let Ok(mut state) = self.state.lock() {
            state.recent = recent;
        }
        Ok(())
    }
    fn start(&mut self, sources: Vec<AudioSourceSelection>) -> Result<(), String> {
        if self.active.is_some() {
            return Err("A recording is already in progress.".into());
        }
        self.retry_save()?;
        let result = self.prepare(sources);
        if let Err(error) = &result {
            set_error(&self.state, error.clone());
        }
        result
    }
    fn prepare(&mut self, sources: Vec<AudioSourceSelection>) -> Result<(), String> {
        if sources.is_empty() {
            return Err("Choose and save at least one audio source before recording.".into());
        }
        let mut ids = HashSet::new();
        if sources.iter().any(|s| !ids.insert(&s.id)) {
            return Err("Each audio source may only be recorded once.".into());
        }
        let devices = sources
            .iter()
            .map(|s| self.backend.resolve(s))
            .collect::<Result<Vec<_>, _>>()?;
        let id = Uuid::new_v4().to_string();
        let started = Instant::now();
        let started_at = now();
        let mut layout = Recording {
            recording_id: id.clone(),
            started_at,
            ended_at: None,
            duration_ms: 0,
            status: "recording".into(),
            tracks: vec![],
            error: None,
            meeting_title: (self.title_provider)(),
            storage_path: None,
            auto_transcribe: true,
            transcription_status: "not_started".into(),
            appflowy_status: "not_started".into(),
        };
        let folder = create_recording_directory(&self.dir, &mut layout)?;
        let tracks = sources
            .iter()
            .zip(&devices)
            .enumerate()
            .map(|(i, (s, d))| {
                let format = d.format();
                Track {
                    source_id: s.id.clone(),
                    name: s.name.clone(),
                    direction: match s.direction {
                        AudioSourceDirection::Input => "input",
                        AudioSourceDirection::Output => "output",
                    }
                    .into(),
                    sample_rate: format.sample_rate,
                    channels: format.channels,
                    file_path: folder
                        .join(format!("track-{i:02}.wav"))
                        .display()
                        .to_string(),
                    frames_written: 0,
                    dropped_samples: 0,
                    error: None,
                }
            })
            .collect();
        let rec = Recording {
            recording_id: id,
            started_at,
            ended_at: None,
            duration_ms: 0,
            status: "recording".into(),
            tracks,
            error: None,
            meeting_title: layout.meeting_title,
            storage_path: layout.storage_path,
            auto_transcribe: true,
            transcription_status: "not_started".into(),
            appflowy_status: "not_started".into(),
        };
        // Durable intent precedes native stream construction and playback.
        self.store.begin(&rec)?;
        let (errors, error_rx) = mpsc::sync_channel(16);
        let mut active = Active {
            rec,
            started,
            streams: vec![],
            writers: vec![],
            drops: vec![],
            gaps: vec![],
            timestamp_adjustments: vec![],
            last_buffers: vec![],
            meters: vec![],
            errors: error_rx,
        };
        self.publish_active(&active, "processing");
        let setup = (|| -> Result<(), String> {
            for (i, device) in devices.iter().enumerate() {
                let track = &active.rec.tracks[i];
                let writer = spawn_writer(
                    std::path::Path::new(&track.file_path),
                    track.sample_rate,
                    track.channels,
                    MAX_WAV_DATA,
                    64,
                )
                .map_err(|e| format!("{}: {e}", track.name))?;
                let dropped = Arc::new(AtomicU64::new(0));
                let gaps = Arc::new(AtomicU64::new(0));
                let timestamp_adjustments = Arc::new(AtomicU64::new(0));
                let last_buffer_ms = Arc::new(AtomicU64::new(0));
                let meter = Arc::new(CaptureMeter::default());
                let settled = Arc::new(AtomicBool::new(false));
                let sink = CaptureSink {
                    sender: writer.sender.clone(),
                    errors: errors.clone(),
                    dropped: dropped.clone(),
                    gaps: gaps.clone(),
                    timestamp_adjustments: timestamp_adjustments.clone(),
                    last_buffer_ms: last_buffer_ms.clone(),
                    meter: meter.clone(),
                    source_name: track.name.clone(),
                    started,
                    settled,
                };
                active.writers.push(writer);
                active.drops.push(dropped);
                active.gaps.push(gaps);
                active.timestamp_adjustments.push(timestamp_adjustments);
                active.last_buffers.push(last_buffer_ms);
                active.meters.push(meter);
                active.streams.push(
                    device
                        .build(sink)
                        .map_err(|e| format!("{}: {e}", track.name))?,
                );
            }
            for stream in &active.streams {
                stream.play()?;
            }
            Ok(())
        })();
        drop(errors);
        if let Err(error) = setup {
            let save = self.finalize(active, Some(error.clone()));
            return Err(match save {
                Ok(()) => error,
                Err(save_error) => format!("{error}; {save_error}"),
            });
        }
        self.publish_active(&active, "recording");
        self.active = Some(active);
        Ok(())
    }
    fn publish_active(&self, active: &Active, status: &str) {
        if let Ok(mut s) = self.state.lock() {
            s.status = status.into();
            s.active = Some(active.rec.clone());
            s.error = None;
            s.levels = active
                .meters
                .iter()
                .zip(&active.rec.tracks)
                .map(|(meter, track)| {
                    meter.snapshot(
                        track.source_id.clone(),
                        active.started.elapsed().as_millis() as u64,
                    )
                })
                .collect();
        }
    }
    fn stop(&mut self, error: Option<String>) -> Result<(), String> {
        match self.active.take() {
            Some(active) => self.finalize(active, error),
            None if self.pending_save.is_some() => self.retry_save(),
            None => Err("No recording is in progress.".into()),
        }
    }
    fn finalize(&mut self, mut active: Active, initial: Option<String>) -> Result<(), String> {
        self.publish_active(&active, "processing");
        active.streams.clear();
        // Streams release callback sender clones; each retained sender is then
        // dropped so its writer drains ALL queued packets before join returns.
        let mut errors: Vec<String> = initial.into_iter().collect();
        errors.extend(active.errors.try_iter());
        for (i, writer) in active.writers.drain(..).enumerate() {
            drop(writer.sender);
            let track = &mut active.rec.tracks[i];
            track.dropped_samples = active.drops[i].load(Ordering::Relaxed);
            match writer.join.join() {
                Ok(result) => {
                    track.frames_written = result.frames_written;
                    track.error = result.error;
                }
                Err(_) => track.error = Some("WAV writer stopped unexpectedly".into()),
            }
        }
        for track in &mut active.rec.tracks {
            if track.frames_written == 0 && track.error.is_none() {
                track.error = Some("No audio frames received from this source".into());
            }
            if let Some(error) = &track.error {
                errors.push(format!("{}: {error}", track.name));
            }
            if track.dropped_samples > 0 {
                errors.push(format!(
                    "{}: {} samples dropped",
                    track.name, track.dropped_samples
                ));
            }
        }
        active.rec.duration_ms = active.started.elapsed().as_millis() as u64;
        active.rec.ended_at = Some(active.rec.started_at.saturating_add(active.rec.duration_ms));
        let warnings: Vec<String> = active.gaps.iter().enumerate().filter_map(|(i, gaps)| {
            let count = gaps.load(Ordering::Relaxed);
            (count > 0).then(|| format!("{}: {count} device-reported audio discontinuities; audio retained but may contain gaps", active.rec.tracks[i].name))
        }).collect();
        let timestamp_warnings: Vec<String> = active
            .timestamp_adjustments
            .iter()
            .enumerate()
            .filter_map(|(i, adjustments)| {
                let count = adjustments.load(Ordering::Relaxed);
                (count > 0).then(|| format!(
                    "{}: {count} capture timestamp adjustments; audio retained and timing may contain gaps",
                    active.rec.tracks[i].name
                ))
            })
            .collect();
        active.rec.status = if !errors.is_empty() {
            "failed"
        } else if !warnings.is_empty() || !timestamp_warnings.is_empty() {
            "interrupted"
        } else {
            "recorded"
        }
        .into();
        for (i, gaps) in active.gaps.iter().enumerate() {
            let count = gaps.load(Ordering::Relaxed);
            if count > 0 && active.rec.tracks[i].error.is_none() {
                active.rec.tracks[i].error =
                    Some(format!("{count} device-reported discontinuities"));
            }
        }
        errors.extend(warnings);
        errors.extend(timestamp_warnings);
        active.rec.error = (!errors.is_empty()).then(|| errors.join("; "));
        let result = self.store.finish(&active.rec);
        self.pending_save = result.as_ref().err().map(|_| active.rec.clone());
        self.publish_finished(active.rec, result.as_ref().err().cloned());
        result
    }
    fn publish_finished(&self, record: Recording, persistence_error: Option<String>) {
        if let Ok(mut s) = self.state.lock() {
            s.active = None;
            s.error = persistence_error.clone().or_else(|| record.error.clone());
            s.levels.clear();
            s.status = if persistence_error.is_some() || record.status == "failed" {
                "failed"
            } else {
                "idle"
            }
            .into();
            let mut visible = record;
            if let Some(error) = persistence_error {
                visible.status = "failed".into();
                visible.error = Some(format!("Audio retained; metadata save pending: {error}"));
            }
            s.recent.retain(|r| r.recording_id != visible.recording_id);
            s.recent.insert(0, visible);
            s.recent.truncate(25);
        }
    }
    fn retry_save(&mut self) -> Result<(), String> {
        if let Some(record) = self.pending_save.as_ref() {
            self.store.finish(record)?;
            let record = self.pending_save.take().unwrap();
            self.publish_finished(record, None);
        }
        Ok(())
    }
    fn monitor(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let mut error = active.errors.try_recv().ok();
        for (i, writer) in active.writers.iter().enumerate() {
            let progress = writer.health.snapshot();
            active.rec.tracks[i].frames_written = progress.frames_written;
            active.rec.tracks[i].dropped_samples = active.drops[i].load(Ordering::Relaxed);
            error = error.or(progress.error);
            if active.rec.tracks[i].dropped_samples > 0 {
                error
                    .get_or_insert_with(|| "Capture queue overflow; partial audio retained".into());
            }
            // Quiet loopback outputs can legitimately deliver no callbacks.
            if active.rec.tracks[i].direction == "input"
                && active.started.elapsed().as_millis() as u64
                    > active.last_buffers[i]
                        .load(Ordering::Relaxed)
                        .saturating_add(10_000)
            {
                error.get_or_insert_with(|| {
                    "Microphone stopped delivering audio; check device or sleep/wake state".into()
                });
            }
        }
        active.rec.duration_ms = active.started.elapsed().as_millis() as u64;
        active.rec.error = (active.gaps.iter().any(|g| g.load(Ordering::Relaxed) > 0)
            || active
                .timestamp_adjustments
                .iter()
                .any(|count| count.load(Ordering::Relaxed) > 0))
            .then(|| "Capture timing was adjusted. Capture continues; saved audio will be marked as potentially containing gaps.".into());
        if let Some(error) = error {
            let _ = self.stop(Some(error));
        } else if let Ok(mut s) = self.state.lock() {
            s.active = Some(active.rec.clone());
            s.error = active.rec.error.clone();
            s.levels = active
                .meters
                .iter()
                .zip(&active.rec.tracks)
                .map(|(meter, track)| {
                    meter.snapshot(
                        track.source_id.clone(),
                        active.started.elapsed().as_millis() as u64,
                    )
                })
                .collect();
        }
    }
}
struct NativeCapture;
struct NativeDevice {
    device: cpal::Device,
    config: cpal::SupportedStreamConfig,
}
impl AudioCapture for NativeCapture {
    fn resolve(&self, source: &AudioSourceSelection) -> Result<Box<dyn CaptureDevice>, String> {
        // A Linux system-output source is a PulseAudio/PipeWire monitor, not a
        // CPAL device at all: its id never carries the CPAL `:input`/`:output`
        // suffix stripped below. Recognize and dispatch it before that parse.
        #[cfg(target_os = "linux")]
        if matches!(source.direction, AudioSourceDirection::Output) {
            if let Some(sink_name) = crate::system_output::parse_pulse_monitor_id(&source.id) {
                return crate::linux_pulse::PulseMonitorDevice::resolve(sink_name)
                    .map(|device| Box::new(device) as Box<dyn CaptureDevice>);
            }
        }
        let suffix = match source.direction {
            AudioSourceDirection::Input => ":input",
            AudioSourceDirection::Output => ":output",
        };
        let wanted = source
            .id
            .strip_suffix(suffix)
            .ok_or("Invalid source direction identifier")?;
        if matches!(source.direction, AudioSourceDirection::Output) && !cfg!(target_os = "windows")
        {
            return Err(
                "Native system output capture is currently supported only on Windows and Linux (via PulseAudio/PipeWire monitors)"
                    .into(),
            );
        }
        for device in cpal::default_host().devices().map_err(|e| e.to_string())? {
            if device.id().map(|id| id.to_string()).ok().as_deref() != Some(wanted) {
                continue;
            }
            let config = match source.direction {
                AudioSourceDirection::Input => device.default_input_config(),
                AudioSourceDirection::Output => device.default_output_config(),
            }
            .map_err(|e| format!("{}: {e}", source.name))?;
            if !matches!(
                config.sample_format(),
                cpal::SampleFormat::F32
                    | cpal::SampleFormat::F64
                    | cpal::SampleFormat::I16
                    | cpal::SampleFormat::U16
                    | cpal::SampleFormat::I32
            ) {
                return Err(format!(
                    "{} uses unsupported sample format {}",
                    source.name,
                    config.sample_format()
                ));
            }
            return Ok(Box::new(NativeDevice { device, config }));
        }
        Err(format!(
            "Selected audio source '{}' is no longer available",
            source.name
        ))
    }
}
impl CaptureStream for cpal::Stream {
    fn play(&self) -> Result<(), String> {
        StreamTrait::play(self).map_err(|e| e.to_string())
    }
}
impl CaptureDevice for NativeDevice {
    fn format(&self) -> CaptureFormat {
        CaptureFormat {
            sample_rate: self.config.sample_rate(),
            channels: self.config.channels(),
        }
    }
    fn build(&self, sink: CaptureSink) -> Result<Box<dyn CaptureStream>, String> {
        let rate = self.config.sample_rate();
        let channels = self.config.channels();
        let errors = sink.errors.clone();
        let gaps = sink.gaps.clone();
        let source_name = sink.source_name.clone();
        let error_settled = sink.settled.clone();
        let error = move |error: cpal::Error| {
            route_stream_error(error, &source_name, &errors, &gaps, &error_settled);
        };
        macro_rules! build {
            ($ty:ty,$convert:expr) => {{
                let mut clock = PacketClock::default();
                self.device.build_input_stream(
                    self.config.clone().into(),
                    move |data: &[$ty], info: &cpal::InputCallbackInfo| {
                        if data.is_empty() {
                            return;
                        }
                        sink.last_buffer_ms
                            .store(sink.started.elapsed().as_millis() as u64, Ordering::Relaxed);
                        if data.len() > 262_144 {
                            sink.dropped.fetch_add(data.len() as u64, Ordering::Relaxed);
                            let _ = sink.errors.try_send(format!(
                                "{}: audio callback exceeded bounded packet size",
                                sink.source_name
                            ));
                            return;
                        }
                        if channels == 0 || data.len() % usize::from(channels) != 0 {
                            let _ = sink.errors.try_send(format!(
                                "{}: audio callback had an invalid channel layout",
                                sink.source_name
                            ));
                            return;
                        }
                        let offset = match clock.offset(
                            info,
                            sink.started,
                            rate,
                            data.len() as u64 / u64::from(channels),
                            &sink.timestamp_adjustments,
                        ) {
                            Ok(offset) => offset,
                            Err(error) => {
                                let _ = sink
                                    .errors
                                    .try_send(format!("{}: {error}", sink.source_name));
                                return;
                            }
                        };
                        if clock.is_settled() {
                            sink.settled.store(true, Ordering::Relaxed);
                        }
                        let samples: Vec<f32> = data.iter().map($convert).collect();
                        sink.meter
                            .observe(&samples, sink.started.elapsed().as_millis() as u64);
                        if sink
                            .sender
                            .try_send(AudioPacket {
                                samples,
                                offset_frames: offset,
                            })
                            .is_err()
                        {
                            sink.dropped.fetch_add(data.len() as u64, Ordering::Relaxed);
                        }
                    },
                    error,
                    Some(Duration::from_secs(5)),
                )
            }};
        }
        let stream = match self.config.sample_format() {
            cpal::SampleFormat::F32 => build!(f32, |v: &f32| *v),
            cpal::SampleFormat::F64 => build!(f64, |v: &f64| *v as f32),
            cpal::SampleFormat::I16 => build!(i16, |v: &i16| f32::from(*v) / 32768.0),
            cpal::SampleFormat::U16 => build!(u16, |v: &u16| (f32::from(*v) - 32768.0) / 32768.0),
            cpal::SampleFormat::I32 => build!(i32, |v: &i32| *v as f32 / 2147483648.0),
            _ => return Err("Unsupported sample format".into()),
        }
        .map_err(|e| e.to_string())?;
        Ok(Box::new(stream))
    }
}
fn route_stream_error(
    error: cpal::Error,
    source_name: &str,
    errors: &mpsc::SyncSender<String>,
    gaps: &AtomicU64,
    settled: &AtomicBool,
) {
    // CPAL's WASAPI backend continues delivering packets after a discontinuity.
    // Keep recording but never label the resulting audio gap-free.
    if error.kind() == cpal::ErrorKind::Xrun {
        // CPAL already ignores AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY on the
        // very first WASAPI buffer (its device_position is still 0 then), but
        // real devices routinely flag the buffer right after that one too --
        // the boundary between device pre-roll and steady delivery, not lost
        // audio. Until the frame timeline has seen a packet land on schedule
        // (`settled`), treat a reported discontinuity as start-up noise.
        if settled.load(Ordering::Relaxed) {
            gaps.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        let _ = errors.try_send(format!("{source_name}: audio stream error: {error}"));
    }
}
#[derive(Default)]
struct PacketClock {
    first: Option<cpal::StreamInstant>,
    first_frame: u64,
    timeline: FrameTimeline,
}
impl PacketClock {
    fn is_settled(&self) -> bool {
        self.timeline.is_settled()
    }
    fn offset(
        &mut self,
        info: &cpal::InputCallbackInfo,
        started: Instant,
        rate: u32,
        frames: u64,
        timestamp_adjustments: &AtomicU64,
    ) -> Result<u64, String> {
        let stamp = info.timestamp();
        let first = match self.first {
            Some(first) => first,
            None => {
                // This is necessarily the very first callback (`self.first`
                // is only ever `None` here). There is no prior anchor to be
                // consistent with yet, so a callback/capture ordering that
                // cannot be subtracted is a units quirk of this one packet,
                // not a lost interval: fall back silently instead of
                // recording a spurious start-up adjustment.
                let latency = stamp
                    .callback
                    .checked_duration_since(stamp.capture)
                    .unwrap_or(Duration::ZERO);
                self.first_frame = (started.elapsed().saturating_sub(latency).as_secs_f64()
                    * f64::from(rate))
                .round() as u64;
                self.first = Some(stamp.capture);
                stamp.capture
            }
        };
        let measured = match stamp.capture.checked_duration_since(first) {
            Some(elapsed) => Some(
                self.first_frame
                    .saturating_add((elapsed.as_secs_f64() * f64::from(rate)).round() as u64),
            ),
            None => {
                // A driver timestamp can reset before the initial anchor. Do
                // not leave this stream permanently timestamp-less: anchor the
                // new epoch to the already-emitted frame cursor. Only counted
                // once the timeline has already caught up with real time --
                // during start-up this is the same buffering artifact as the
                // very first packet, just observed one callback later.
                if self.timeline.is_settled() {
                    timestamp_adjustments.fetch_add(1, Ordering::Relaxed);
                }
                self.first = Some(stamp.capture);
                self.first_frame = self.timeline.next;
                self.timeline.reanchor_measurement_epoch();
                Some(self.first_frame)
            }
        };
        let session_frames = (started.elapsed().as_secs_f64() * f64::from(rate)).round() as u64;
        self.timeline.offset(
            measured,
            rate,
            frames,
            session_frames,
            timestamp_adjustments,
        )
    }
}

/// Bounds the start-up grace below: if capture never lands on schedule within
/// this many callbacks, corrections stop being treated as free start-up noise
/// even though the timeline has technically never "settled". This keeps a
/// persistently broken stream from silently going unreported forever.
const SETTLE_GRACE_PACKETS: u32 = 5;

/// The emitted packet order is the durable clock. CPAL timestamps are used to
/// retain substantive forward gaps, but callback wake-up jitter or a device
/// timestamp reset must never create a WAV overlap or discard captured audio.
/// `pub(crate)`: reused as-is by the Linux PulseAudio/PipeWire monitor
/// capture loop (`linux_pulse.rs`) so system-output tracks get the same
/// start-up grace and gap/adjustment accounting as CPAL-backed tracks,
/// instead of a second bespoke implementation of the same policy.
#[derive(Default)]
pub(crate) struct FrameTimeline {
    next: u64,
    last_measured: Option<u64>,
    rebase_frames: u64,
    /// True once a packet has landed inside the jitter budget on its own,
    /// i.e. capture has caught up with real time. Before that, every stream
    /// starts behind frame 0 by its own device buffering/start-up latency;
    /// that gap has nothing prior to be discontinuous with, so it is not
    /// counted until the timeline demonstrates it has stabilized.
    settled: bool,
    packets: u32,
}
impl FrameTimeline {
    pub(crate) fn is_settled(&self) -> bool {
        self.settled
    }

    fn reanchor_measurement_epoch(&mut self) {
        self.last_measured = None;
        self.rebase_frames = 0;
    }

    pub(crate) fn offset(
        &mut self,
        measured: Option<u64>,
        rate: u32,
        frames: u64,
        session_frames: u64,
        discontinuities: &AtomicU64,
    ) -> Result<u64, String> {
        if rate == 0 || frames == 0 {
            return Err("Capture callback had an invalid frame count.".into());
        }
        let tolerance = u64::from(rate / 100).max(2); // 10 ms: timestamp jitter, not audio loss.
        self.packets = self.packets.saturating_add(1);
        // While unsettled, a correction is presumed to be device/driver
        // buffering catching up, not a genuine loss; report it once capture
        // has proven it can land on schedule, or once the grace window has
        // run out without that ever happening.
        let counts = self.settled || self.packets > SETTLE_GRACE_PACKETS;
        let mut offset = self.next;
        if let Some(measured) = measured {
            let reset = self
                .last_measured
                .is_some_and(|previous| measured < previous);
            if reset {
                // Preserve buffered audio on a device/QPC reset. Re-anchor the
                // measurement, retaining the monotonically emitted frame clock.
                if counts {
                    discontinuities.fetch_add(1, Ordering::Relaxed);
                }
                self.rebase_frames = self.next.saturating_sub(measured);
            }
            let rebased = measured.saturating_add(self.rebase_frames);
            if rebased > self.next.saturating_add(tolerance) {
                // A long quiet input/loopback interval is legitimate when it
                // matches elapsed session time. Reject only timestamp leaps
                // implausibly ahead of the active recording clock; WAV writing
                // separately caps total retained data.
                if rebased > session_frames.saturating_add(u64::from(rate).saturating_mul(30)) {
                    return Err(
                        "Capture timestamp jump exceeded the 30 second safety limit.".into(),
                    );
                }
                offset = rebased;
                if counts {
                    discontinuities.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                self.settled = true;
            }
            self.last_measured = Some(measured);
        } else if counts {
            discontinuities.fetch_add(1, Ordering::Relaxed);
        }
        self.next = offset
            .checked_add(frames)
            .ok_or_else(|| "Capture frame timeline overflowed.".to_string())?;
        Ok(offset)
    }
}
#[cfg(test)]
#[path = "recording_tests.rs"]
mod tests;
