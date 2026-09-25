//! Local CPU transcription. Audio never leaves this process.

use crate::{
    echo_cancel, echo_dedup,
    recording::Track,
    recording_paths::resolved_recording_directory,
    transcription_types::{ModelInfo, Transcript, TranscriptSegment, TranscriptionRequest},
};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use rubato::{audioadapter_buffers::direct::InterleavedSlice, Fft, FixedSync, Resampler};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};
use whisper_rs::{
    get_lang_str, FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters,
};

/// Whether this build was compiled with GPU (CUDA) support. Runtime GPU use
/// still depends on a working NVIDIA driver/runtime and falls back to CPU if
/// context creation fails.
const GPU_BUILD: bool = cfg!(feature = "cuda");

const BACKEND_CPU: u8 = 1;
const BACKEND_CUDA: u8 = 2;

fn backend_label_for(code: u8) -> &'static str {
    if code == BACKEND_CUDA {
        "cuda"
    } else {
        "cpu (AVX2)"
    }
}

const TARGET_RATE: u32 = 16_000;
const MAX_CHUNK_SECONDS: u32 = 60;
const MAX_MODEL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_WAV_SAMPLE_RATE: u32 = 192_000;
const MAX_WAV_CHANNELS: u16 = 8;

// Energy-based speech-region pre-pass (see `detect_speech_regions`), applied
// independently within each `MAX_CHUNK_SECONDS` window so the existing bounded
// per-chunk memory footprint is unchanged. It is a coarse gate, not a real VAD
// model: it exists to skip long silent stretches (which both waste compute and
// are a common source of Whisper hallucinations like repeated "Thank you.")
// and to break each chunk into shorter, more focused decode windows.
const VAD_FRAME_MS: u32 = 20;
const VAD_HANGOVER_MS: u32 = 300;
const VAD_MERGE_GAP_MS: u32 = 1_000;
const VAD_MIN_REGION_MS: u32 = 250;
/// Frames at or below this percentile of the chunk's own RMS distribution set
/// the adaptive noise floor; speech must exceed it by `VAD_THRESHOLD_DB`.
const VAD_FLOOR_PERCENTILE: f32 = 0.2;
const VAD_THRESHOLD_DB: f32 = 12.0;
/// A floor below which we never treat audio as speech even if everything in
/// the chunk is that quiet (near-silent chunk: adaptive floor alone would
/// otherwise chase the noise up to the threshold).
const VAD_ABSOLUTE_FLOOR_DB: f32 = -60.0;
const VAD_MAX_WINDOW_SECONDS: u32 = 30;
/// Segments this confident that a decoded window contained no speech are
/// dropped instead of surfaced as (often hallucinated) text.
const NO_SPEECH_DROP_THRESHOLD: f32 = 0.85;

pub trait TranscriptionEngine: Send + Sync {
    fn transcribe(
        &self,
        request: &TranscriptionRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn FnMut(u8),
    ) -> Result<Transcript, String>;

    /// The backend actually used by the most recent (or, if none has run yet,
    /// the backend a new request would attempt first). Human-readable, shown
    /// next to the model status in the UI; not a stable identifier.
    fn backend_label(&self) -> String {
        backend_label_for(BACKEND_CPU).to_string()
    }
}

/// The Whisper model context is created once per request (not once per audio
/// chunk) and remains local to the calling worker. `backend` records which
/// device the most recent request actually used so the UI can show it.
pub struct LocalWhisperEngine {
    backend: AtomicU8,
}

impl Default for LocalWhisperEngine {
    fn default() -> Self {
        Self {
            backend: AtomicU8::new(BACKEND_CPU),
        }
    }
}

impl LocalWhisperEngine {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TranscriptionEngine for LocalWhisperEngine {
    fn transcribe(
        &self,
        request: &TranscriptionRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn FnMut(u8),
    ) -> Result<Transcript, String> {
        check_cancelled(cancelled)?;
        let model = inspect_model(&request.model_path)?;
        if model.key != request.model_key || model.sha256 != request.model_hash {
            return Err(
                "Selected model changed after validation; choose it again before transcribing."
                    .into(),
            );
        }
        let tracks = validated_tracks(request)?;
        let mut warnings = Vec::new();
        // Acoustic echo cancellation runs on already-decoded, in-memory PCM
        // before anything below touches the recorder's own WAV files, and
        // only when the recording actually has both an input (microphone)
        // and an output (loopback) track -- the common two-track meeting
        // recorder setup where the user has speakers, not headphones, so
        // remote audio leaks into the mic and would otherwise be
        // transcribed twice. `_aec_temp_files` exists only to keep its
        // temporary WAVs alive (and clean them up on drop) until decoding
        // below is done with them.
        let (tracks, _aec_temp_files) = if request.echo_cancellation {
            apply_echo_cancellation(&tracks, &mut warnings)?
        } else {
            (tracks, Vec::new())
        };
        // Redirect library diagnostics away from stdout/stderr when no application
        // logging backend is configured. This is process-global but idempotent.
        whisper_rs::install_logging_hooks();
        let context = self.create_context(&request.model_path)?;
        // Paired 1:1 with `segments` (see the sort below, which keeps both in
        // lockstep) so text-level dedup can tell a microphone segment from a
        // loopback one without adding a `direction` field to the serialized
        // `TranscriptSegment` shape.
        let mut segments = Vec::new();
        let mut directions: Vec<&'static str> = Vec::new();
        let mut languages_used = Vec::new();
        let total = tracks.len().max(1) as u32;
        progress(0);
        for (track_index, (track, path)) in tracks.iter().enumerate() {
            check_cancelled(cancelled)?;
            let start_progress = ((track_index as u32 * 100) / total) as u8;
            let end_progress = (((track_index + 1) as u32 * 100) / total) as u8;
            let before = segments.len();
            let (chunk_count, detected_language) = transcribe_track(
                &context,
                track,
                path,
                request.language.as_deref(),
                cancelled,
                &mut segments,
                &mut |track_percent| {
                    let span = u16::from(end_progress.saturating_sub(start_progress));
                    progress(
                        start_progress
                            .saturating_add((span * u16::from(track_percent) / 100) as u8),
                    );
                },
            )?;
            let direction: &'static str = if track.direction == "output" { "output" } else { "input" };
            directions.extend(std::iter::repeat_n(direction, segments.len() - before));
            if chunk_count > 1 {
                warnings.push(format!(
                    "{} was processed in {chunk_count} local chunks; words near a chunk boundary may be split or repeated.",
                    track.name
                ));
            }
            if let Some(language) = detected_language {
                if !languages_used.contains(&language) {
                    languages_used.push(language);
                }
            }
            progress(end_progress);
        }
        // Sort `segments` and `directions` together (as one indirect key) so
        // they stay paired index-for-index.
        let mut order: Vec<usize> = (0..segments.len()).collect();
        order.sort_by_key(|&i| (segments[i].start_ms, segments[i].end_ms, segments[i].source_id.clone()));
        let sorted_segments: Vec<TranscriptSegment> =
            order.iter().map(|&i| segments[i].clone()).collect();
        let sorted_directions: Vec<&'static str> = order.iter().map(|&i| directions[i]).collect();
        let mut segments = sorted_segments;
        if request.dedupe_echo_segments {
            let (deduped, removed) =
                echo_dedup::dedupe_echo_segments(segments, &sorted_directions);
            segments = deduped;
            if removed > 0 {
                warnings.push(format!(
                    "Removed {removed} duplicate segment{} likely caused by audio appearing on both the microphone and loopback tracks.",
                    if removed == 1 { "" } else { "s" }
                ));
            }
        }
        if segments
            .windows(2)
            .any(|pair| pair[0].end_ms > pair[1].start_ms)
        {
            warnings.push("Tracks may overlap or duplicate speech. Source labels identify recorded inputs, not speakers; diarization is not available yet.".into());
        }
        languages_used.sort();
        Ok(Transcript {
            recording_id: request.recording.recording_id.clone(),
            model_key: request.model_key.clone(),
            model_hash: request.model_hash.clone(),
            segments,
            languages_used,
            warnings,
        })
    }

    fn backend_label(&self) -> String {
        backend_label_for(self.backend.load(Ordering::Acquire)).to_string()
    }
}

impl LocalWhisperEngine {
    /// Creates the Whisper context, preferring GPU when this build was
    /// compiled with the `cuda` feature. If GPU context creation fails at
    /// runtime (missing driver/runtime, unsupported device, etc.) this falls
    /// back to CPU instead of failing the whole transcription job.
    fn create_context(&self, model_path: &Path) -> Result<WhisperContext, String> {
        if GPU_BUILD {
            let mut gpu_params = WhisperContextParameters::default();
            gpu_params.use_gpu(true);
            gpu_params.flash_attn(true);
            match WhisperContext::new_with_params(model_path, gpu_params) {
                Ok(context) => {
                    self.backend.store(BACKEND_CUDA, Ordering::Release);
                    return Ok(context);
                }
                Err(error) => {
                    eprintln!(
                        "GPU (CUDA) Whisper context unavailable, falling back to CPU: {error}"
                    );
                }
            }
        }
        let mut cpu_params = WhisperContextParameters::default();
        cpu_params.use_gpu(false);
        let context = WhisperContext::new_with_params(model_path, cpu_params)
            .map_err(|error| format!("Could not load local Whisper model: {error}"))?;
        self.backend.store(BACKEND_CPU, Ordering::Release);
        Ok(context)
    }
}

fn transcribe_track(
    context: &WhisperContext,
    track: &Track,
    path: &Path,
    language: Option<&str>,
    cancelled: &AtomicBool,
    segments: &mut Vec<TranscriptSegment>,
    progress: &mut dyn FnMut(u8),
) -> Result<(u32, Option<String>), String> {
    let header = validate_wav(path)?;
    if header.channels == 0 || header.sample_rate == 0 {
        return Err("WAV has an invalid channel count or sample rate.".into());
    }
    let mut reader =
        WavReader::open(path).map_err(|error| format!("Could not read saved WAV: {error}"))?;
    let spec = reader.spec();
    if spec.channels != header.channels
        || spec.sample_rate != header.sample_rate
        || spec.sample_format != SampleFormat::Int
        || spec.bits_per_sample != 16
    {
        return Err(
            "Only the recorder's PCM16 WAV tracks are supported for local transcription.".into(),
        );
    }
    let chunk_frames = (u64::from(spec.sample_rate) * u64::from(MAX_CHUNK_SECONDS)).max(1) as usize;
    let total_frames = header.data_bytes / (u64::from(spec.channels) * 2);
    let total_chunks = total_frames.div_ceil(chunk_frames as u64).max(1);
    let mut samples = reader.samples::<i16>();
    let mut offset_frames = 0u64;
    let mut chunks = 0u32;
    // Auto mode ("auto"/`language: None`) detects once from the first decoded
    // speech window and pins every later window of this track to it, instead
    // of letting each 60-second chunk re-detect (and potentially flip
    // languages mid-meeting on a quiet or code-switched chunk).
    let mut detected_language: Option<String> = None;
    loop {
        check_cancelled(cancelled)?;
        let mono = read_mono_chunk(&mut samples, spec.channels, chunk_frames)?;
        if mono.is_empty() {
            break;
        }
        let input_frames = mono.len() as u64;
        let offset_ms = offset_frames.saturating_mul(1000) / u64::from(spec.sample_rate);
        offset_frames = offset_frames.saturating_add(input_frames);
        chunks += 1;
        // Exact digital silence cannot contain speech. We intentionally do not use
        // a broad RMS threshold that could discard quiet participants.
        if is_digital_silence(&mono) {
            progress(((u64::from(chunks) * 100) / total_chunks) as u8);
            continue;
        }
        let pcm = resample_16k(mono, spec.sample_rate)?;
        if pcm.is_empty() {
            progress(((u64::from(chunks) * 100) / total_chunks) as u8);
            continue;
        }
        // A silence pre-pass over this chunk: fully silent/quiet chunks (long
        // stretches of dead air on one track while the other speaks) are
        // skipped entirely rather than decoded, which both saves time and
        // avoids a common source of hallucinated text ("Thank you.", repeated
        // lines) on near-silent input.
        let regions = detect_speech_regions(&pcm, TARGET_RATE);
        for (window_start, window_end) in windows_for_regions(&regions, TARGET_RATE) {
            check_cancelled(cancelled)?;
            let window_offset_ms = offset_ms.saturating_add(
                (window_start as u64).saturating_mul(1000) / u64::from(TARGET_RATE),
            );
            let effective_language = language.or(detected_language.as_deref());
            let newly_detected = decode_window(
                context,
                &pcm[window_start..window_end],
                window_offset_ms,
                effective_language,
                cancelled,
                track,
                segments,
            )?;
            if detected_language.is_none() {
                detected_language = newly_detected;
            }
        }
        progress(((u64::from(chunks) * 100) / total_chunks) as u8);
    }
    Ok((chunks, detected_language))
}

/// Decodes one bounded 16 kHz mono PCM window and appends any accepted
/// segments to `segments`, with timestamps mapped back onto the original
/// track timeline via `base_offset_ms` (this window's start time). Returns
/// the language whisper.cpp auto-detected for this window, if `language` was
/// `None` (i.e. detection was requested rather than pinned).
#[allow(clippy::too_many_arguments)]
fn decode_window(
    context: &WhisperContext,
    pcm: &[f32],
    base_offset_ms: u64,
    language: Option<&str>,
    cancelled: &AtomicBool,
    track: &Track,
    segments: &mut Vec<TranscriptSegment>,
) -> Result<Option<String>, String> {
    check_cancelled(cancelled)?;
    let mut state = context
        .create_state()
        .map_err(|error| format!("Could not create Whisper decoder state: {error}"))?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(cpu_threads());
    params.set_language(language);
    params.set_translate(false); // verbatim source language, never translation
    params.set_no_context(true); // no cross-track or cross-chunk invented context
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    // Anti-hallucination: never let the decoder emit a run of blanks or
    // non-speech tokens as if it were transcript text, retry at a slightly
    // higher temperature instead of forcing a low-confidence greedy guess,
    // and ask whisper.cpp to score each segment's no-speech probability so we
    // can drop the ones it is not confident actually contain speech.
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    params.set_temperature(0.0);
    params.set_temperature_inc(0.2);
    params.set_no_speech_thold(0.6);
    let abort_requested = Arc::new(AtomicBool::new(false));
    let abort_callback = Arc::clone(&abort_requested);
    let abort_callback: Box<dyn FnMut() -> bool> =
        Box::new(move || abort_callback.load(Ordering::Acquire));
    params.set_abort_callback_safe::<_, Box<dyn FnMut() -> bool>>(Some(abort_callback));
    let watcher_done = Arc::new(AtomicBool::new(false));
    let watcher_done_for_thread = Arc::clone(&watcher_done);
    let full_result = std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            while !watcher_done_for_thread.load(Ordering::Acquire) {
                if cancelled.load(Ordering::Acquire) {
                    abort_requested.store(true, Ordering::Release);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let result = state.full(params, pcm);
        watcher_done.store(true, Ordering::Release);
        let _ = watcher.join();
        result
    });
    check_cancelled(cancelled)?;
    full_result.map_err(|error| format!("Local Whisper decoding failed: {error}"))?;
    check_cancelled(cancelled)?;
    for segment in state.as_iter() {
        if segment.no_speech_probability() >= NO_SPEECH_DROP_THRESHOLD {
            continue;
        }
        let text = segment
            .to_str_lossy()
            .map_err(|error| format!("Could not read Whisper text: {error}"))?
            .trim()
            .to_owned();
        if text.is_empty() {
            continue;
        }
        let start_ms = base_offset_ms
            .saturating_add((segment.start_timestamp().max(0) as u64).saturating_mul(10));
        let end_ms = base_offset_ms
            .saturating_add((segment.end_timestamp().max(0) as u64).saturating_mul(10));
        segments.push(TranscriptSegment {
            start_ms,
            end_ms: end_ms.max(start_ms),
            text,
            source_id: track.source_id.clone(),
            source_name: track.name.clone(),
        });
    }
    if language.is_some() {
        return Ok(None);
    }
    let lang_id = state.full_lang_id_from_state();
    Ok(if lang_id >= 0 {
        get_lang_str(lang_id).map(str::to_owned)
    } else {
        None
    })
}

fn to_dbfs(rms: f32) -> f32 {
    20.0 * rms.max(1e-9).log10()
}

/// A lightweight, energy-based speech-region detector: not a real voice
/// activity detection model (whisper.cpp's built-in VAD needs a separate
/// Silero GGML model file we do not bundle or import), just a coarse gate
/// against long silent stretches. Frames are `VAD_FRAME_MS` wide; a frame
/// counts as speech when its RMS is more than `VAD_THRESHOLD_DB` above an
/// adaptive noise floor (the `VAD_FLOOR_PERCENTILE` quietest frames in this
/// same input), never below `VAD_ABSOLUTE_FLOOR_DB`. Raw speech runs under
/// `VAD_MIN_REGION_MS` are dropped as noise spikes *before* padding (padding
/// them first would make almost any spike look long enough to keep);
/// surviving runs are then padded by `VAD_HANGOVER_MS` on each side, and
/// gaps between padded regions under `VAD_MERGE_GAP_MS` are merged. A fully
/// silent (or fully sub-floor) input returns no regions.
fn detect_speech_regions(samples: &[f32], sample_rate: u32) -> Vec<(usize, usize)> {
    if samples.is_empty() || sample_rate == 0 {
        return Vec::new();
    }
    let frame_len = ((u64::from(sample_rate) * u64::from(VAD_FRAME_MS)) / 1000).max(1) as usize;
    let frame_rms: Vec<f32> = samples
        .chunks(frame_len)
        .map(|frame| {
            let sum_sq: f64 = frame.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
            ((sum_sq / frame.len() as f64).sqrt()) as f32
        })
        .collect();
    let mut sorted = frame_rms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let floor_index = (((sorted.len().max(1) - 1) as f32) * VAD_FLOOR_PERCENTILE) as usize;
    let noise_floor_db = to_dbfs(sorted[floor_index.min(sorted.len() - 1)]);
    let threshold_db = (noise_floor_db + VAD_THRESHOLD_DB).max(VAD_ABSOLUTE_FLOOR_DB);
    let is_speech: Vec<bool> = frame_rms
        .iter()
        .map(|rms| to_dbfs(*rms) > threshold_db)
        .collect();

    let pad_frames = (u64::from(VAD_HANGOVER_MS) / u64::from(VAD_FRAME_MS)).max(1) as usize;
    let merge_gap_frames = (u64::from(VAD_MERGE_GAP_MS) / u64::from(VAD_FRAME_MS)).max(1) as usize;
    let min_region_frames = (u64::from(VAD_MIN_REGION_MS) / u64::from(VAD_FRAME_MS)).max(1) as usize;

    // Raw (unpadded) contiguous speech runs first, so a short noise spike can
    // be dropped by its true duration rather than by a duration inflated by
    // hangover padding (which would otherwise make almost any spike look long
    // enough to keep).
    let mut raw_runs: Vec<(usize, usize)> = Vec::new();
    let mut index = 0;
    while index < is_speech.len() {
        if is_speech[index] {
            let start = index;
            let mut end = index;
            while end < is_speech.len() && is_speech[end] {
                end += 1;
            }
            raw_runs.push((start, end));
            index = end;
        } else {
            index += 1;
        }
    }

    let padded: Vec<(usize, usize)> = raw_runs
        .into_iter()
        .filter(|(start, end)| end.saturating_sub(*start) >= min_region_frames)
        .map(|(start, end)| {
            (
                start.saturating_sub(pad_frames),
                (end + pad_frames).min(is_speech.len()),
            )
        })
        .collect();

    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in padded {
        if let Some(last) = merged.last_mut() {
            if start <= last.1.saturating_add(merge_gap_frames) {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }

    merged
        .into_iter()
        .map(|(start, end)| (start * frame_len, (end * frame_len).min(samples.len())))
        .collect()
}

/// Splits speech regions into decode windows no longer than
/// `VAD_MAX_WINDOW_SECONDS`, preserving exact sample-index boundaries so
/// callers can map decoded timestamps back onto the original timeline.
fn windows_for_regions(regions: &[(usize, usize)], sample_rate: u32) -> Vec<(usize, usize)> {
    let max_window = ((sample_rate as usize).saturating_mul(VAD_MAX_WINDOW_SECONDS as usize)).max(1);
    let mut windows = Vec::new();
    for &(start, end) in regions {
        let mut window_start = start;
        while window_start < end {
            let window_end = (window_start + max_window).min(end);
            windows.push((window_start, window_end));
            window_start = window_end;
        }
    }
    windows
}

/// Whisper's decode work parallelizes well up to roughly a physical-core
/// count; beyond that, hyperthreads/SMT siblings add diminishing (sometimes
/// negative) returns. `available_parallelism` reports logical cores, so this
/// is capped well above the old hardcoded 4 but still bounded to avoid
/// oversubscribing very large machines.
fn cpu_threads() -> i32 {
    std::thread::available_parallelism()
        .map(|value| value.get().clamp(1, 16) as i32)
        .unwrap_or(4)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err("Transcription cancelled.".into())
    } else {
        Ok(())
    }
}

fn read_mono_chunk(
    samples: &mut hound::WavSamples<'_, BufReader<File>, i16>,
    channels: u16,
    max_frames: usize,
) -> Result<Vec<f32>, String> {
    let channels = usize::from(channels);
    let mut mono = Vec::with_capacity(max_frames);
    for _ in 0..max_frames {
        let mut sum = 0i64;
        for channel in 0..channels {
            match samples.next() {
                Some(Ok(sample)) => sum += i64::from(sample),
                Some(Err(error)) => return Err(format!("Invalid PCM sample: {error}")),
                None if channel == 0 => return Ok(mono),
                None => return Err("WAV ended in the middle of an audio frame.".into()),
            }
        }
        mono.push((sum as f32 / channels as f32) / 32768.0);
    }
    Ok(mono)
}

/// FFT resampling uses Rubato's anti-aliasing filter; it is deliberately not a
/// naive decimator. A 60-second chunk bounds its input/output allocations.
fn resample_16k(samples: Vec<f32>, sample_rate: u32) -> Result<Vec<f32>, String> {
    resample_to(samples, sample_rate, TARGET_RATE)
}

/// As `resample_16k`, but to an arbitrary target rate (used to bring
/// loopback/reference tracks to a microphone track's own native rate for
/// echo cancellation, ahead of and independent from the fixed 16 kHz Whisper
/// target).
fn resample_to(samples: Vec<f32>, from_rate: u32, to_rate: u32) -> Result<Vec<f32>, String> {
    if from_rate == to_rate {
        return Ok(samples);
    }
    let input_frames = samples.len();
    let input = InterleavedSlice::new(&samples, 1, input_frames)
        .map_err(|error| format!("Could not adapt mono audio for resampling: {error}"))?;
    let mut resampler = Fft::<f32>::new(from_rate as usize, to_rate as usize, 1024, 1, FixedSync::Both)
        .map_err(|error| format!("Could not configure anti-aliasing resampler: {error}"))?;
    let output = resampler
        .process_all(&input, input_frames, None)
        .map_err(|error| format!("Could not resample WAV audio: {error}"))?;
    Ok(output.take_data())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WavHeader {
    channels: u16,
    sample_rate: u32,
    data_bytes: u64,
}

fn is_digital_silence(samples: &[f32]) -> bool {
    samples.iter().all(|sample| *sample == 0.0)
}

fn validate_wav(path: &Path) -> Result<WavHeader, String> {
    let mut file =
        File::open(path).map_err(|error| format!("Could not open saved WAV: {error}"))?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    let mut head = [0u8; 12];
    file.read_exact(&mut head)
        .map_err(|_| "WAV is shorter than its RIFF header.".to_string())?;
    if &head[..4] != b"RIFF" || &head[8..] != b"WAVE" {
        return Err("Track is not a RIFF/WAVE file.".into());
    }
    let mut fmt = None;
    let mut data = None;
    loop {
        let mut chunk = [0u8; 8];
        if file.read_exact(&mut chunk).is_err() {
            break;
        }
        let bytes = u64::from(u32::from_le_bytes(chunk[4..8].try_into().unwrap()));
        let position = file.stream_position().map_err(|error| error.to_string())?;
        if position.saturating_add(bytes) > length {
            return Err("WAV data chunk exceeds file length.".into());
        }
        if &chunk[..4] == b"fmt " {
            if bytes < 16 {
                return Err("WAV format chunk is incomplete.".into());
            }
            let mut value = [0u8; 16];
            file.read_exact(&mut value)
                .map_err(|error| error.to_string())?;
            let format = u16::from_le_bytes(value[0..2].try_into().unwrap());
            let channels = u16::from_le_bytes(value[2..4].try_into().unwrap());
            let sample_rate = u32::from_le_bytes(value[4..8].try_into().unwrap());
            let bits = u16::from_le_bytes(value[14..16].try_into().unwrap());
            if format != 1
                || channels == 0
                || channels > MAX_WAV_CHANNELS
                || sample_rate == 0
                || sample_rate > MAX_WAV_SAMPLE_RATE
                || bits != 16
            {
                return Err("WAV must be PCM16 with a safe channel count and sample rate.".into());
            }
            fmt = Some(WavHeader {
                channels,
                sample_rate,
                data_bytes: 0,
            });
        } else if &chunk[..4] == b"data" {
            data = Some(bytes);
        }
        file.seek(SeekFrom::Start(
            position.saturating_add(bytes).saturating_add(bytes & 1),
        ))
        .map_err(|error| error.to_string())?;
    }
    let mut fmt = fmt.ok_or_else(|| "WAV has no format chunk.".to_string())?;
    let data_bytes = data.ok_or_else(|| "WAV has no data chunk.".to_string())?;
    let bytes_per_frame = u64::from(fmt.channels).saturating_mul(2);
    if bytes_per_frame == 0 || data_bytes % bytes_per_frame != 0 {
        return Err("WAV data does not contain complete PCM16 frames.".into());
    }
    fmt.data_bytes = data_bytes;
    Ok(fmt)
}

pub(crate) fn validated_recording_tracks(
    recording: &crate::recording::Recording,
    recordings_root: &Path,
) -> Result<Vec<(Track, PathBuf)>, String> {
    if uuid::Uuid::parse_str(&recording.recording_id).is_err() {
        return Err("Recording identity is invalid.".into());
    }
    let directory = resolved_recording_directory(recordings_root, recording)
        .map_err(|_| "Recording directory is unavailable or invalid.".to_string())?;
    if recording.tracks.is_empty() {
        return Err("Recording has no audio tracks to transcribe.".into());
    }
    recording
        .tracks
        .iter()
        .map(|track| {
            let metadata = fs::symlink_metadata(&track.file_path)
                .map_err(|_| "A recording track is unavailable.".to_string())?;
            if metadata.file_type().is_symlink() {
                return Err("A recording track must not be a symbolic link.".into());
            }
            let path = fs::canonicalize(&track.file_path)
                .map_err(|_| "A recording track is unavailable.".to_string())?;
            if !path.is_file() || path.parent() != Some(directory.as_path()) {
                return Err("A recording track is outside its recording directory.".into());
            }
            Ok((track.clone(), path))
        })
        .collect()
}

fn validated_tracks(request: &TranscriptionRequest) -> Result<Vec<(Track, PathBuf)>, String> {
    validated_recording_tracks(&request.recording, &request.recordings_root)
}

/// Reads a recorder-owned PCM16 WAV in bounded pieces and returns 16 kHz mono
/// samples. Callers must first establish that `path` is a managed track path.
pub(crate) fn load_wav_16k_mono(
    path: &Path,
    max_duration_seconds: u64,
) -> Result<Vec<f32>, String> {
    let header = validate_wav(path)?;
    let max_frames = max_duration_seconds
        .checked_mul(u64::from(header.sample_rate))
        .ok_or_else(|| "WAV duration limit overflowed.".to_string())?;
    let frames = header.data_bytes / (u64::from(header.channels) * 2);
    if frames > max_frames {
        return Err(format!(
            "Recorded source exceeds the {} minute diarization limit.",
            max_duration_seconds / 60
        ));
    }
    let output_capacity =
        (frames.saturating_mul(u64::from(TARGET_RATE)) / u64::from(header.sample_rate)) as usize;
    let output_limit = usize::try_from(
        max_duration_seconds
            .checked_mul(u64::from(TARGET_RATE))
            .ok_or_else(|| "WAV duration limit overflowed.".to_string())?,
    )
    .map_err(|_| "WAV duration limit is too large for this platform.".to_string())?;
    let mut output = Vec::with_capacity(output_capacity);
    let mut reader =
        WavReader::open(path).map_err(|error| format!("Could not read saved WAV: {error}"))?;
    let spec = reader.spec();
    if spec.channels != header.channels
        || spec.sample_rate != header.sample_rate
        || spec.sample_format != SampleFormat::Int
        || spec.bits_per_sample != 16
    {
        return Err(
            "Only the recorder's PCM16 WAV tracks are supported for local processing.".into(),
        );
    }
    let chunk_frames = (u64::from(spec.sample_rate) * u64::from(MAX_CHUNK_SECONDS)).max(1) as usize;
    let mut samples = reader.samples::<i16>();
    loop {
        let mono = read_mono_chunk(&mut samples, spec.channels, chunk_frames)?;
        if mono.is_empty() {
            break;
        }
        let resampled = resample_16k(mono, spec.sample_rate)?;
        if output.len().saturating_add(resampled.len()) > output_limit {
            return Err(format!(
                "Recorded source exceeds the {} minute diarization limit.",
                max_duration_seconds / 60
            ));
        }
        output.extend(resampled);
    }
    Ok(output)
}

/// Upper bound on a track's duration for echo cancellation: `cancel_echo`
/// holds full-length, native-rate `f32` buffers for both the microphone and
/// the summed reference in memory (see docs/transcription.md), unlike the
/// bounded 60-second chunking the rest of this file uses. Recordings longer
/// than this skip echo cancellation for that track (with a warning) rather
/// than risk an unbounded allocation on an unusually long capture.
const AEC_MAX_DURATION_SECONDS: u64 = 6 * 60 * 60;

/// As `load_wav_16k_mono`, but returns mono samples at the WAV's own native
/// sample rate (no resampling) alongside that rate. Used for echo
/// cancellation, which needs to align a microphone track against a loopback
/// track at whatever rate they were actually recorded at.
fn load_wav_native_mono(path: &Path, max_duration_seconds: u64) -> Result<(Vec<f32>, u32), String> {
    let header = validate_wav(path)?;
    let max_frames = max_duration_seconds
        .checked_mul(u64::from(header.sample_rate))
        .ok_or_else(|| "WAV duration limit overflowed.".to_string())?;
    let frames = header.data_bytes / (u64::from(header.channels) * 2);
    if frames > max_frames {
        return Err(format!(
            "Recorded source exceeds the {} minute echo-cancellation limit.",
            max_duration_seconds / 60
        ));
    }
    let mut reader =
        WavReader::open(path).map_err(|error| format!("Could not read saved WAV: {error}"))?;
    let spec = reader.spec();
    if spec.channels != header.channels
        || spec.sample_rate != header.sample_rate
        || spec.sample_format != SampleFormat::Int
        || spec.bits_per_sample != 16
    {
        return Err(
            "Only the recorder's PCM16 WAV tracks are supported for local processing.".into(),
        );
    }
    let chunk_frames = (u64::from(spec.sample_rate) * u64::from(MAX_CHUNK_SECONDS)).max(1) as usize;
    let mut samples = reader.samples::<i16>();
    let mut output = Vec::with_capacity(frames as usize);
    loop {
        let mono = read_mono_chunk(&mut samples, spec.channels, chunk_frames)?;
        if mono.is_empty() {
            break;
        }
        output.extend(mono);
    }
    Ok((output, spec.sample_rate))
}

/// Writes a mono `f32` buffer as a temporary PCM16 WAV, for feeding an
/// echo-cancelled buffer back through the existing (file-based) per-track
/// decode path unchanged. Removed automatically by the returned guard.
fn write_temp_pcm16_wav(samples: &[f32], sample_rate: u32) -> Result<TempWavGuard, String> {
    let path = std::env::temp_dir().join(format!("meeting-recorder-aec-{}.wav", uuid::Uuid::new_v4()));
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let result = (|| {
        let mut writer =
            WavWriter::create(&path, spec).map_err(|error| format!("Could not open temporary AEC WAV: {error}"))?;
        for &sample in samples {
            let clamped = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            writer
                .write_sample(clamped)
                .map_err(|error| format!("Could not write temporary AEC WAV: {error}"))?;
        }
        writer
            .finalize()
            .map_err(|error| format!("Could not finalize temporary AEC WAV: {error}"))
    })();
    match result {
        Ok(()) => Ok(TempWavGuard(path)),
        Err(error) => {
            let _ = fs::remove_file(&path);
            Err(error)
        }
    }
}

/// Deletes its temporary file when dropped. A best-effort cleanup: a failure
/// to remove an OS temp file is not surfaced as a transcription error.
struct TempWavGuard(PathBuf);
impl Drop for TempWavGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
impl TempWavGuard {
    fn path(&self) -> &Path {
        &self.0
    }
}

/// Runs acoustic echo cancellation (`echo_cancel::cancel_echo`) on every
/// input (microphone) track against the sum of every output (loopback)
/// track, when the recording has at least one of each -- the two-track
/// meeting-recorder setup where the local speakers leak remote audio back
/// into the mic. Tracks are otherwise returned unchanged (same paths), so a
/// recording with only one track direction, or where cancellation is
/// skipped for a specific track (too long, decode failure, or a linear
/// no-op such as a silent reference), costs nothing beyond the direction
/// check. Returns the (possibly-substituted) track list and the temporary
/// WAV guards backing any substituted input track, which must outlive the
/// caller's use of those paths.
/// A track list paired with the temporary WAV guards backing any
/// AEC-substituted input tracks within it.
type TracksWithGuards = (Vec<(Track, PathBuf)>, Vec<TempWavGuard>);

fn apply_echo_cancellation(
    tracks: &[(Track, PathBuf)],
    warnings: &mut Vec<String>,
) -> Result<TracksWithGuards, String> {
    let output_tracks: Vec<&(Track, PathBuf)> =
        tracks.iter().filter(|(track, _)| track.direction == "output").collect();
    if output_tracks.is_empty() || !tracks.iter().any(|(track, _)| track.direction == "input") {
        return Ok((tracks.to_vec(), Vec::new()));
    }
    let mut result = Vec::with_capacity(tracks.len());
    let mut guards = Vec::new();
    for (track, path) in tracks {
        if track.direction != "input" {
            result.push((track.clone(), path.clone()));
            continue;
        }
        match echo_cancel_one_track(track, path, &output_tracks) {
            Ok(Some((cleaned_track, guard))) => {
                let path = guard.path().to_owned();
                guards.push(guard);
                result.push((cleaned_track, path));
            }
            Ok(None) => result.push((track.clone(), path.clone())),
            Err(error) => {
                warnings.push(format!(
                    "Echo cancellation for {} was skipped: {error}",
                    track.name
                ));
                result.push((track.clone(), path.clone()));
            }
        }
    }
    if !guards.is_empty() {
        let reference_names: Vec<&str> =
            output_tracks.iter().map(|(track, _)| track.name.as_str()).collect();
        warnings.push(format!(
            "Acoustic echo cancellation was applied to reduce audio from {} appearing twice in this transcript.",
            reference_names.join(", ")
        ));
    }
    Ok((result, guards))
}

/// Cancels echo for one input track against the summed, rate-matched output
/// tracks. `Ok(None)` means cancellation legitimately did nothing (e.g. a
/// silent reference) and the original track/path should be used unchanged.
fn echo_cancel_one_track(
    track: &Track,
    path: &Path,
    output_tracks: &[&(Track, PathBuf)],
) -> Result<Option<(Track, TempWavGuard)>, String> {
    let (mic, sample_rate) = load_wav_native_mono(path, AEC_MAX_DURATION_SECONDS)?;
    let mut reference = vec![0.0f32; mic.len()];
    for (_output_track, output_path) in output_tracks {
        let (samples, output_rate) = load_wav_native_mono(output_path, AEC_MAX_DURATION_SECONDS)?;
        let resampled = resample_to(samples, output_rate, sample_rate)?;
        for (slot, sample) in reference.iter_mut().zip(resampled.iter()) {
            *slot += *sample;
        }
    }
    let (cleaned, stats) = echo_cancel::cancel_echo(&mic, &reference, sample_rate);
    if !stats.applied {
        return Ok(None);
    }
    let guard = write_temp_pcm16_wav(&cleaned, sample_rate)?;
    Ok(Some((track.clone(), guard)))
}

pub fn hash_model(path: &Path) -> Result<String, String> {
    let mut file = BufReader::new(
        File::open(path).map_err(|error| format!("Could not open local model: {error}"))?,
    );
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("Could not hash local model: {error}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Validates legacy Whisper GGML metadata without loading model weights.
/// This is a fast import-time check; loading the decoder remains the final
/// validity check before transcription.
pub fn inspect_model(path: &Path) -> Result<ModelInfo, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("Could not inspect local model: {error}"))?;
    if !metadata.is_file() || metadata.len() < 48 || metadata.len() > MAX_MODEL_BYTES {
        return Err("Model is missing, too small, or exceeds the local size limit.".into());
    }
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut header = [0u8; 48];
    file.read_exact(&mut header)
        .map_err(|_| "Model header is incomplete.".to_string())?;
    if u32::from_le_bytes(header[..4].try_into().unwrap()) != 0x6767_6d6c {
        return Err("Model is not a supported Whisper GGML file.".into());
    }
    // Dimensions only; `ftype` (word 11, bytes 44..48) records the quantization
    // scheme (f16, q5_0, q8_0, ...) and is deliberately not inspected here, so
    // quantized GGML files are accepted on the same footing as f16/f32 ones.
    // `n_mels` (word 10, bytes 40..44) also does not affect the size key: 80 vs
    // 128 mel bins is an encoder input detail, not a model-size identifier.
    let state = i32::from_le_bytes(header[28..32].try_into().unwrap());
    let audio_layers = i32::from_le_bytes(header[20..24].try_into().unwrap());
    let text_layers = i32::from_le_bytes(header[36..40].try_into().unwrap());
    let key = match (state, audio_layers, text_layers) {
        (384, 4, 4) => "tiny",
        (512, 6, 6) => "base",
        (768, 12, 12) => "small",
        (1024, 24, 24) => "medium",
        (1280, 32, 32) => "large",
        // large-v3-turbo: large-v3's encoder with a pruned 4-layer decoder.
        // Recommended over tiny/base/small for accuracy; use a quantized
        // build (e.g. ggml-large-v3-turbo-q5_0.bin, ~550 MB) to keep it CPU-friendly.
        (1280, 32, 4) => "turbo",
        // distil-large-v3 prunes the decoder to 2 layers instead of 4. It is a
        // different distillation with different accuracy/robustness tradeoffs
        // than turbo (in particular, no built-in translation support and a
        // decoder trained on English-heavy data); rather than silently treating
        // it as turbo, reject it with a specific, actionable message.
        (1280, 32, 2) => return Err(
            "This model looks like distil-large-v3 (2-layer decoder), which is not supported. Use large-v3-turbo (recommended) or large instead."
                .into(),
        ),
        _ => return Err(
            "Whisper GGML metadata does not match a supported tiny/base/small/medium/large/turbo model."
                .into(),
        ),
    };
    Ok(ModelInfo {
        key: key.into(),
        sha256: hash_model(path)?,
        bytes: metadata.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("meeting-recorder-{label}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pcm16_wav(samples: &[i16], channels: u16, sample_rate: u32) -> Vec<u8> {
        let data_bytes = (samples.len() * 2) as u32;
        let mut wav = Vec::with_capacity(44 + data_bytes as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + data_bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
        wav.extend_from_slice(&(channels * 2).to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    #[test]
    fn mono_resample_keeps_audible_output() {
        let source = (0..48_000)
            .map(|n| if n % 100 == 0 { 0.5 } else { 0.0 })
            .collect();
        let output = resample_16k(source, 48_000).unwrap();
        assert!(!output.is_empty());
        assert!((15_900..=16_100).contains(&output.len()));
    }

    /// A synthetic 16 kHz tone: `speech_at` (in seconds, half-open ranges) are
    /// loud (0.2 amplitude), everything else is quiet background noise
    /// (0.0005 amplitude, well above exact digital silence but well below the
    /// speech level) so the adaptive floor/threshold logic is exercised.
    fn synthetic_track(total_seconds: u32, speech_at: &[(u32, u32)]) -> Vec<f32> {
        let sample_rate = TARGET_RATE;
        let total_samples = (total_seconds * sample_rate) as usize;
        (0..total_samples)
            .map(|index| {
                let t = index as f32 / sample_rate as f32;
                let is_speech = speech_at
                    .iter()
                    .any(|(start, end)| t >= *start as f32 && t < *end as f32);
                let amplitude = if is_speech { 0.2 } else { 0.0005 };
                // A simple tone rather than pure DC so RMS reflects a signal.
                amplitude * (index as f32 * 0.4).sin()
            })
            .collect()
    }

    #[test]
    fn fully_silent_track_yields_no_speech_regions() {
        let samples = vec![0.0f32; TARGET_RATE as usize * 5];
        assert!(detect_speech_regions(&samples, TARGET_RATE).is_empty());
    }

    #[test]
    fn quiet_background_noise_yields_no_speech_regions() {
        let samples = synthetic_track(5, &[]);
        assert!(detect_speech_regions(&samples, TARGET_RATE).is_empty());
    }

    #[test]
    fn a_speech_burst_is_detected_and_padded() {
        let samples = synthetic_track(10, &[(3, 5)]);
        let regions = detect_speech_regions(&samples, TARGET_RATE);
        assert_eq!(regions.len(), 1);
        let (start, end) = regions[0];
        // Padding extends the raw [3s, 5s) burst outward by ~VAD_HANGOVER_MS.
        assert!(start < 3 * TARGET_RATE as usize);
        assert!(end > 5 * TARGET_RATE as usize);
        assert!(start > (3 * TARGET_RATE as usize).saturating_sub(TARGET_RATE as usize / 2));
        assert!(end < 5 * TARGET_RATE as usize + TARGET_RATE as usize / 2);
    }

    #[test]
    fn short_gap_between_bursts_is_merged_into_one_region() {
        // A 500ms gap is under VAD_MERGE_GAP_MS (1000ms), so this should merge
        // into a single contiguous region rather than two. Built manually
        // (rather than via `synthetic_track`, which only takes whole seconds)
        // to get an exact half-second gap.
        let sample_rate = TARGET_RATE;
        let total_samples = (10 * sample_rate) as usize;
        let samples: Vec<f32> = (0..total_samples)
            .map(|index| {
                let t = index as f32 / sample_rate as f32;
                let is_speech = (2.0..4.0).contains(&t) || (4.5..6.0).contains(&t);
                let amplitude = if is_speech { 0.2 } else { 0.0005 };
                amplitude * (index as f32 * 0.4).sin()
            })
            .collect();
        let regions = detect_speech_regions(&samples, sample_rate);
        assert_eq!(regions.len(), 1, "regions: {regions:?}");
    }

    #[test]
    fn long_gap_between_bursts_stays_split() {
        let samples = synthetic_track(20, &[(2, 4), (12, 14)]);
        let regions = detect_speech_regions(&samples, TARGET_RATE);
        assert_eq!(regions.len(), 2, "regions: {regions:?}");
    }

    #[test]
    fn a_very_short_blip_below_the_minimum_region_length_is_dropped() {
        // 50ms is under VAD_MIN_REGION_MS (250ms).
        let sample_rate = TARGET_RATE;
        let total_samples = (5 * sample_rate) as usize;
        let samples: Vec<f32> = (0..total_samples)
            .map(|index| {
                let t = index as f32 / sample_rate as f32;
                let is_speech = (2.0..2.05).contains(&t);
                let amplitude = if is_speech { 0.2 } else { 0.0005 };
                amplitude * (index as f32 * 0.4).sin()
            })
            .collect();
        assert!(detect_speech_regions(&samples, sample_rate).is_empty());
    }

    #[test]
    fn region_windowing_maps_sample_offsets_back_to_the_original_timeline() {
        // A region spanning [1s, 4s) at 16kHz split into <=30s windows should
        // remain a single window (it is well under the cap), with exact
        // sample-index boundaries preserved for timestamp remapping.
        let sample_rate = TARGET_RATE;
        let region = (sample_rate as usize, 4 * sample_rate as usize);
        let windows = windows_for_regions(&[region], sample_rate);
        assert_eq!(windows, vec![region]);
    }

    #[test]
    fn region_windowing_splits_regions_longer_than_the_cap() {
        let sample_rate = TARGET_RATE;
        // A 70-second region must split into ceil(70/30) = 3 windows, each
        // capped at 30 seconds, with contiguous, non-overlapping boundaries
        // that reconstruct the original region exactly.
        let region = (0usize, 70 * sample_rate as usize);
        let windows = windows_for_regions(&[region], sample_rate);
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0], (0, 30 * sample_rate as usize));
        assert_eq!(
            windows[1],
            (30 * sample_rate as usize, 60 * sample_rate as usize)
        );
        assert_eq!(
            windows[2],
            (60 * sample_rate as usize, 70 * sample_rate as usize)
        );
        for pair in windows.windows(2) {
            assert_eq!(pair[0].1, pair[1].0, "windows must be contiguous");
        }
    }

    #[test]
    fn only_exact_zero_pcm_is_skipped_as_silence() {
        assert!(is_digital_silence(&[0.0, -0.0]));
        assert!(!is_digital_silence(&[0.0, 1.0 / 32768.0]));
    }

    fn track(direction: &str, name: &str, path: &Path, sample_rate: u32) -> Track {
        Track {
            source_id: name.to_lowercase(),
            name: name.into(),
            direction: direction.into(),
            sample_rate,
            channels: 1,
            file_path: path.to_string_lossy().into(),
            frames_written: (sample_rate as u64) * 3,
            dropped_samples: 0,
            error: None,
        }
    }

    fn tone_i16(seconds: u32, sample_rate: u32, amplitude: i16) -> Vec<i16> {
        (0..seconds * sample_rate)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (amplitude as f32 * (2.0 * std::f32::consts::PI * 300.0 * t).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn echo_cancellation_substitutes_the_input_track_when_both_directions_are_present() {
        let dir = temp_dir("aec");
        let sample_rate = 16_000;
        let reference_samples = tone_i16(3, sample_rate, 8_000);
        // The "mic" is just the reference attenuated and delayed a little,
        // simulating loudspeaker leakage picked up by the microphone.
        let delay = 8usize;
        let mic_samples: Vec<i16> = (0..reference_samples.len())
            .map(|i| {
                if i >= delay {
                    (reference_samples[i - delay] as f32 * 0.6) as i16
                } else {
                    0
                }
            })
            .collect();
        let input_path = dir.join("mic.wav");
        let output_path = dir.join("loopback.wav");
        fs::write(&input_path, pcm16_wav(&mic_samples, 1, sample_rate)).unwrap();
        fs::write(&output_path, pcm16_wav(&reference_samples, 1, sample_rate)).unwrap();
        let tracks = vec![
            (track("input", "Microphone", &input_path, sample_rate), input_path.clone()),
            (track("output", "Speakers", &output_path, sample_rate), output_path.clone()),
        ];
        let mut warnings = Vec::new();
        let (result, guards) = apply_echo_cancellation(&tracks, &mut warnings).unwrap();
        assert_eq!(result.len(), 2);
        let cleaned_input = &result.iter().find(|(t, _)| t.direction == "input").unwrap().1;
        assert_ne!(cleaned_input, &input_path, "input track should be substituted with a temp file");
        assert!(!guards.is_empty());
        assert!(warnings.iter().any(|w| w.contains("echo cancellation")));
        // The substituted output track is untouched.
        let output_result = &result.iter().find(|(t, _)| t.direction == "output").unwrap().1;
        assert_eq!(output_result, &output_path);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn echo_cancellation_is_a_no_op_without_both_track_directions() {
        let dir = temp_dir("aec-noop");
        let sample_rate = 16_000;
        let input_path = dir.join("mic.wav");
        fs::write(&input_path, pcm16_wav(&tone_i16(1, sample_rate, 4_000), 1, sample_rate)).unwrap();
        let tracks = vec![(track("input", "Microphone", &input_path, sample_rate), input_path.clone())];
        let mut warnings = Vec::new();
        let (result, guards) = apply_echo_cancellation(&tracks, &mut warnings).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1, tracks[0].1);
        assert!(guards.is_empty());
        assert!(warnings.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn echo_cancellation_leaves_the_track_unchanged_when_the_reference_is_silent() {
        let dir = temp_dir("aec-silent");
        let sample_rate = 16_000;
        let input_path = dir.join("mic.wav");
        let output_path = dir.join("loopback.wav");
        fs::write(&input_path, pcm16_wav(&tone_i16(1, sample_rate, 4_000), 1, sample_rate)).unwrap();
        fs::write(&output_path, pcm16_wav(&vec![0i16; sample_rate as usize], 1, sample_rate)).unwrap();
        let tracks = vec![
            (track("input", "Microphone", &input_path, sample_rate), input_path.clone()),
            (track("output", "Speakers", &output_path, sample_rate), output_path.clone()),
        ];
        let mut warnings = Vec::new();
        let (result, guards) = apply_echo_cancellation(&tracks, &mut warnings).unwrap();
        let input_result = &result.iter().find(|(t, _)| t.direction == "input").unwrap().1;
        assert_eq!(input_result, &input_path, "a silent reference must leave the mic track untouched");
        assert!(guards.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn wav_validation_rejects_incomplete_pcm_frame() {
        let dir = temp_dir("wav");
        let path = dir.join("track.wav");
        let mut wav = pcm16_wav(&[1], 2, 48_000);
        wav[40..44].copy_from_slice(&2u32.to_le_bytes());
        fs::write(&path, wav).unwrap();
        assert!(validate_wav(&path).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validated_tracks_rejects_a_path_outside_its_recording_directory() {
        let root = temp_dir("paths");
        let recording_id = uuid::Uuid::new_v4().to_string();
        let recording_dir = root.join(&recording_id);
        fs::create_dir_all(&recording_dir).unwrap();
        let outside = root.join("outside.wav");
        fs::write(&outside, pcm16_wav(&[0], 1, 16_000)).unwrap();
        let request = TranscriptionRequest {
            recording: crate::recording::Recording {
                recording_id,
                started_at: 0,
                ended_at: Some(0),
                duration_ms: 0,
                status: "complete".into(),
                tracks: vec![crate::recording::Track {
                    source_id: "source".into(),
                    name: "Source".into(),
                    direction: "input".into(),
                    sample_rate: 16_000,
                    channels: 1,
                    file_path: outside.to_string_lossy().into(),
                    frames_written: 1,
                    dropped_samples: 0,
                    error: None,
                }],
                error: None,
                meeting_title: None,
                storage_path: None,
                auto_transcribe: false,
                transcription_status: "idle".into(),
                appflowy_status: "idle".into(),
            },
            recordings_root: root.clone(),
            model_path: root.join("model.bin"),
            model_key: "tiny".into(),
            model_hash: "hash".into(),
            language: None,
            echo_cancellation: false,
            dedupe_echo_segments: false,
        };
        assert!(validated_tracks(&request).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn model_header_requires_known_dimensions() {
        let dir = temp_dir("model-header");
        let path = dir.join("bad.bin");
        fs::write(&path, b"not a model").unwrap();
        assert!(inspect_model(&path).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn model_header_accepts_serialized_ggml_magic() {
        let dir = temp_dir("model-valid-header");
        let path = dir.join("tiny.bin");
        let mut header = [0u8; 48];
        header[0..4].copy_from_slice(&0x6767_6d6cu32.to_le_bytes());
        header[20..24].copy_from_slice(&4i32.to_le_bytes());
        header[28..32].copy_from_slice(&384i32.to_le_bytes());
        header[36..40].copy_from_slice(&4i32.to_le_bytes());
        fs::write(&path, header).unwrap();
        let model = inspect_model(&path).unwrap();
        assert_eq!(model.key, "tiny");
        assert_eq!(model.bytes, 48);
        let _ = fs::remove_dir_all(dir);
    }

    fn ggml_header(audio_state: i32, audio_layers: i32, text_layers: i32, ftype: i32) -> [u8; 48] {
        let mut header = [0u8; 48];
        header[0..4].copy_from_slice(&0x6767_6d6cu32.to_le_bytes());
        header[20..24].copy_from_slice(&audio_layers.to_le_bytes());
        header[28..32].copy_from_slice(&audio_state.to_le_bytes());
        header[36..40].copy_from_slice(&text_layers.to_le_bytes());
        header[44..48].copy_from_slice(&ftype.to_le_bytes());
        header
    }

    #[test]
    fn large_v3_turbo_dimensions_map_to_turbo() {
        let dir = temp_dir("model-turbo");
        let path = dir.join("turbo.bin");
        fs::write(&path, ggml_header(1280, 32, 4, 1)).unwrap();
        assert_eq!(inspect_model(&path).unwrap().key, "turbo");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn distil_large_v3_dimensions_are_rejected_with_a_specific_message() {
        let dir = temp_dir("model-distil");
        let path = dir.join("distil.bin");
        fs::write(&path, ggml_header(1280, 32, 2, 1)).unwrap();
        let error = inspect_model(&path).unwrap_err();
        assert!(error.contains("distil-large-v3"), "unexpected error: {error}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn quantized_ftype_does_not_change_the_model_key() {
        let dir = temp_dir("model-quantized");
        // ftype values differ across whisper.cpp quantization schemes (f16=1,
        // q5_0=9, q8_0=7, ...); the size key must not depend on that byte.
        for ftype in [0, 1, 7, 9] {
            let path = dir.join(format!("base-ftype-{ftype}.bin"));
            fs::write(&path, ggml_header(512, 6, 6, ftype)).unwrap();
            assert_eq!(inspect_model(&path).unwrap().key, "base");
        }
        let _ = fs::remove_dir_all(dir);
    }

    /// Opt-in integration coverage only. It reads exactly the model and public
    /// fixture explicitly named by the caller; no discovery or download occurs.
    #[test]
    #[ignore = "set WHISPER_TEST_MODEL and WHISPER_TEST_AUDIO to explicit public fixture paths"]
    fn transcribes_explicit_public_fixture() {
        let model_path =
            std::env::var("WHISPER_TEST_MODEL").expect("Explicitly select a local test model");
        let audio_path =
            std::env::var("WHISPER_TEST_AUDIO").expect("Explicitly select public speech audio");
        let info = inspect_model(Path::new(&model_path)).expect("explicit model must be valid");
        let root = temp_dir("public-whisper-fixture");
        let recording_id = uuid::Uuid::new_v4().to_string();
        let recording_dir = root.join(&recording_id);
        fs::create_dir_all(&recording_dir).unwrap();
        let track_path = recording_dir.join("public.wav");
        fs::copy(audio_path, &track_path).expect("explicit public audio must be readable");
        let request = TranscriptionRequest {
            recording: crate::recording::Recording {
                recording_id,
                started_at: 0,
                ended_at: Some(0),
                duration_ms: 0,
                status: "complete".into(),
                tracks: vec![crate::recording::Track {
                    source_id: "public_fixture".into(),
                    name: "Public fixture".into(),
                    direction: "input".into(),
                    sample_rate: 16_000,
                    channels: 1,
                    file_path: track_path.to_string_lossy().into(),
                    frames_written: 0,
                    dropped_samples: 0,
                    error: None,
                }],
                error: None,
                meeting_title: None,
                storage_path: None,
                auto_transcribe: false,
                transcription_status: "idle".into(),
                appflowy_status: "idle".into(),
            },
            recordings_root: root.clone(),
            model_path: PathBuf::from(model_path),
            model_key: info.key.clone(),
            model_hash: info.sha256.clone(),
            language: None,
            echo_cancellation: false,
            dedupe_echo_segments: false,
        };
        let cancelled = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let transcript = LocalWhisperEngine::new()
            .transcribe(&request, &cancelled, &mut |_| {})
            .expect("explicit public fixture must transcribe locally");
        assert_eq!(transcript.recording_id, request.recording.recording_id);
        assert!(
            !transcript.segments.is_empty(),
            "Public speech fixture must yield text"
        );
        assert!(transcript
            .segments
            .iter()
            .all(|segment| segment.source_id == "public_fixture"));
        println!(
            "Public fixture: {} segments in {:?}; text: {}",
            transcript.segments.len(),
            started.elapsed(),
            transcript
                .segments
                .iter()
                .map(|segment| segment.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let _ = fs::remove_dir_all(root);
    }
}
