//! Local CPU transcription. Audio never leaves this process.

use crate::{
    recording::Track,
    recording_paths::resolved_recording_directory,
    transcription_types::{ModelInfo, Transcript, TranscriptSegment, TranscriptionRequest},
};
use hound::{SampleFormat, WavReader};
use rubato::{audioadapter_buffers::direct::InterleavedSlice, Fft, FixedSync, Resampler};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const TARGET_RATE: u32 = 16_000;
const MAX_CHUNK_SECONDS: u32 = 60;
const MAX_MODEL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_WAV_SAMPLE_RATE: u32 = 192_000;
const MAX_WAV_CHANNELS: u16 = 8;

pub trait TranscriptionEngine: Send + Sync {
    fn transcribe(
        &self,
        request: &TranscriptionRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn FnMut(u8),
    ) -> Result<Transcript, String>;
}

/// Stateless CPU implementation. The Whisper model context is created once per
/// request (not once per audio chunk) and remains local to the calling worker.
#[derive(Default)]
pub struct LocalWhisperEngine;

impl LocalWhisperEngine {
    pub fn new() -> Self {
        Self
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
        // Redirect library diagnostics away from stdout/stderr when no application
        // logging backend is configured. This is process-global but idempotent.
        whisper_rs::install_logging_hooks();
        let mut context_params = WhisperContextParameters::default();
        context_params.use_gpu(false);
        let context = WhisperContext::new_with_params(&request.model_path, context_params)
            .map_err(|error| format!("Could not load local Whisper model: {error}"))?;
        let mut segments = Vec::new();
        let mut warnings = Vec::new();
        let total = tracks.len().max(1) as u32;
        progress(0);
        for (track_index, (track, path)) in tracks.iter().enumerate() {
            check_cancelled(cancelled)?;
            let start_progress = ((track_index as u32 * 100) / total) as u8;
            let end_progress = (((track_index + 1) as u32 * 100) / total) as u8;
            let chunk_count = transcribe_track(
                &context,
                track,
                path,
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
            if chunk_count > 1 {
                warnings.push(format!(
                    "{} was processed in {chunk_count} local chunks; words near a chunk boundary may be split or repeated.",
                    track.name
                ));
            }
            progress(end_progress);
        }
        segments
            .sort_by_key(|segment| (segment.start_ms, segment.end_ms, segment.source_id.clone()));
        if segments
            .windows(2)
            .any(|pair| pair[0].end_ms > pair[1].start_ms)
        {
            warnings.push("Tracks may overlap or duplicate speech. Source labels identify recorded inputs, not speakers; diarization is not available yet.".into());
        }
        Ok(Transcript {
            recording_id: request.recording.recording_id.clone(),
            model_key: request.model_key.clone(),
            model_hash: request.model_hash.clone(),
            segments,
            warnings,
        })
    }
}

fn transcribe_track(
    context: &WhisperContext,
    track: &Track,
    path: &Path,
    cancelled: &AtomicBool,
    segments: &mut Vec<TranscriptSegment>,
    progress: &mut dyn FnMut(u8),
) -> Result<u32, String> {
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
        check_cancelled(cancelled)?;
        let mut state = context
            .create_state()
            .map_err(|error| format!("Could not create Whisper decoder state: {error}"))?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(cpu_threads());
        params.set_language(None); // automatic language identification
        params.set_translate(false); // verbatim source language, never translation
        params.set_no_context(true); // no cross-track or cross-chunk invented context
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
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
            let result = state.full(params, &pcm);
            watcher_done.store(true, Ordering::Release);
            let _ = watcher.join();
            result
        });
        check_cancelled(cancelled)?;
        full_result.map_err(|error| format!("Local Whisper decoding failed: {error}"))?;
        check_cancelled(cancelled)?;
        for segment in state.as_iter() {
            let text = segment
                .to_str_lossy()
                .map_err(|error| format!("Could not read Whisper text: {error}"))?
                .trim()
                .to_owned();
            if text.is_empty() {
                continue;
            }
            let start_ms = offset_ms
                .saturating_add((segment.start_timestamp().max(0) as u64).saturating_mul(10));
            let end_ms = offset_ms
                .saturating_add((segment.end_timestamp().max(0) as u64).saturating_mul(10));
            segments.push(TranscriptSegment {
                start_ms,
                end_ms: end_ms.max(start_ms),
                text,
                source_id: track.source_id.clone(),
                source_name: track.name.clone(),
            });
        }
        progress(((u64::from(chunks) * 100) / total_chunks) as u8);
    }
    Ok(chunks)
}

fn cpu_threads() -> i32 {
    std::thread::available_parallelism()
        .map(|value| value.get().min(4) as i32)
        .unwrap_or(1)
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
    if sample_rate == TARGET_RATE {
        return Ok(samples);
    }
    let input_frames = samples.len();
    let input = InterleavedSlice::new(&samples, 1, input_frames)
        .map_err(|error| format!("Could not adapt mono audio for resampling: {error}"))?;
    let mut resampler = Fft::<f32>::new(
        sample_rate as usize,
        TARGET_RATE as usize,
        1024,
        1,
        FixedSync::Both,
    )
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
    let state = i32::from_le_bytes(header[28..32].try_into().unwrap());
    let audio_layers = i32::from_le_bytes(header[20..24].try_into().unwrap());
    let text_layers = i32::from_le_bytes(header[36..40].try_into().unwrap());
    let key = match (state, audio_layers, text_layers) {
        (384, 4, 4) => "tiny",
        (512, 6, 6) => "base",
        (768, 12, 12) => "small",
        (1024, 24, 24) => "medium",
        (1280, 32, 32) => "large",
        _ => return Err(
            "Whisper GGML metadata does not match a supported tiny/base/small/medium/large model."
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

    #[test]
    fn only_exact_zero_pcm_is_skipped_as_silence() {
        assert!(is_digital_silence(&[0.0, -0.0]));
        assert!(!is_digital_silence(&[0.0, 1.0 / 32768.0]));
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
