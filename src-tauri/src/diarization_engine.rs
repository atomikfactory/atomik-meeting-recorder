//! Local-only speaker diarization in an owned helper process.
//!
//! sherpa-onnx processes a complete waveform without a cancellation hook. The
//! application therefore runs native inference only in a same-executable child:
//! cancellation, timeout, or a parent crash cannot leave work attached to the
//! Tauri process.

use crate::{
    diarization_types::{DiarizationOutput, DiarizationRequest, SpeakerTurn},
    recording_paths::resolved_recording_directory,
    transcription_engine::{hash_model, load_wav_16k_mono, validated_recording_tracks},
};
use serde::Serialize;
use sherpa_onnx::{
    FastClusteringConfig, OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
    OfflineSpeakerSegmentationModelConfig, OfflineSpeakerSegmentationPyannoteModelConfig,
    SpeakerEmbeddingExtractorConfig,
};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const MAX_SOURCE_SECONDS: u64 = 30 * 60;
const CHILD_TIMEOUT: Duration = Duration::from_secs(35 * 60);
const MAX_RESULT_BYTES: u64 = 2 * 1024 * 1024;
const CHILD_EXIT_PARENT_GONE: i32 = 73;

pub trait DiarizationEngine: Send + Sync {
    fn diarize(
        &self,
        request: &DiarizationRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn FnMut(u8),
    ) -> Result<DiarizationOutput, String>;
}

/// Stateless parent-side engine. Native inference runs only in the worker.
#[derive(Default)]
pub struct LocalDiarizationEngine;

impl LocalDiarizationEngine {
    pub fn new() -> Self {
        Self
    }
}

impl DiarizationEngine for LocalDiarizationEngine {
    fn diarize(
        &self,
        request: &DiarizationRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn FnMut(u8),
    ) -> Result<DiarizationOutput, String> {
        check_cancelled(cancelled)?;
        validate_request(request)?;
        let recording_dir = recording_directory(request)?;
        let nonce = Uuid::new_v4();
        let request_path = recording_dir.join(format!(".diarization-{nonce}.request.json"));
        let result_path = recording_dir.join(format!(".diarization-{nonce}.result.json"));
        write_json(&request_path, request)?;

        let mut child = match spawn_worker(&request_path, &result_path) {
            Ok(child) => child,
            Err(error) => {
                cleanup(&request_path, &result_path);
                return Err(error);
            }
        };
        // Keep stdin open until this child exits. Its monitor exits it if this
        // parent crashes and the pipe reaches EOF.
        let _parent_liveness_stdin = child.stdin.take();
        progress(0);
        let started = Instant::now();
        let status = loop {
            if let Err(error) = check_cancelled(cancelled) {
                terminate_child(&mut child);
                cleanup(&request_path, &result_path);
                return Err(error);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Err(error) => {
                    terminate_child(&mut child);
                    cleanup(&request_path, &result_path);
                    return Err(format!("Could not poll diarization worker: {error}"));
                }
                Ok(None) if started.elapsed() >= CHILD_TIMEOUT => {
                    terminate_child(&mut child);
                    cleanup(&request_path, &result_path);
                    return Err("Diarization timed out; no result was applied.".into());
                }
                Ok(None) => thread::sleep(Duration::from_millis(100)),
            }
        };
        if !status.success() {
            cleanup(&request_path, &result_path);
            return Err("Local diarization worker failed; no result was applied.".into());
        }
        let output = read_output(&result_path, request);
        cleanup(&request_path, &result_path);
        let output = output?;
        progress(100);
        Ok(output)
    }
}

fn spawn_worker(request_path: &Path, result_path: &Path) -> Result<Child, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not locate diarization worker executable: {error}"))?;
    let mut command = Command::new(executable);
    command
        .arg("--diarization-worker")
        .arg(request_path)
        .arg(result_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
        .spawn()
        .map_err(|error| format!("Could not start local diarization worker: {error}"))
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn cleanup(request_path: &Path, result_path: &Path) {
    let _ = fs::remove_file(request_path);
    let _ = fs::remove_file(result_path);
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err("Diarization cancelled; no result was applied.".into())
    } else {
        Ok(())
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| format!("Could not prepare diarization request: {error}"))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("Could not create local diarization request: {error}"))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("Could not write local diarization request: {error}"))
}

fn recording_directory(request: &DiarizationRequest) -> Result<PathBuf, String> {
    resolved_recording_directory(&request.recordings_root, &request.recording)
        .map_err(|_| "Recording directory is unavailable or invalid.".to_string())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_managed_model(
    root: &Path,
    path: &Path,
    expected_hash: &str,
) -> Result<PathBuf, String> {
    if !valid_sha256(expected_hash) {
        return Err("Diarization model identity is invalid.".into());
    }
    let model_root_path = root.join("models");
    if fs::symlink_metadata(&model_root_path)
        .map_err(|_| "Managed local model directory is unavailable.".to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("Managed local model directory must not be a symbolic link.".into());
    }
    let model_root = fs::canonicalize(&model_root_path)
        .map_err(|_| "Managed local model directory is unavailable.".to_string())?;
    if model_root.parent() != Some(root) {
        return Err("Managed local model directory escaped the recording root.".into());
    }
    let expected_path = model_root.join(format!("{expected_hash}.onnx"));
    if fs::symlink_metadata(&expected_path)
        .map_err(|_| "Managed diarization model is unavailable.".to_string())?
        .file_type()
        .is_symlink()
        || fs::symlink_metadata(path)
            .map_err(|_| "Selected diarization model is unavailable.".to_string())?
            .file_type()
            .is_symlink()
    {
        return Err("Managed diarization model must not be a symbolic link.".into());
    }
    let expected = fs::canonicalize(&expected_path)
        .map_err(|_| "Managed diarization model is unavailable.".to_string())?;
    let actual = fs::canonicalize(path)
        .map_err(|_| "Selected diarization model is unavailable.".to_string())?;
    if actual != expected || !actual.is_file() || hash_model(&actual)? != expected_hash {
        return Err("Diarization model changed or is outside managed local storage.".into());
    }
    Ok(actual)
}

fn validate_request(request: &DiarizationRequest) -> Result<(PathBuf, PathBuf), String> {
    let root = fs::canonicalize(&request.recordings_root)
        .map_err(|_| "Recording root is unavailable.".to_string())?;
    let _ = validated_recording_tracks(&request.recording, &root)?;
    let segmentation = validate_managed_model(
        &root,
        &request.segmentation_model,
        &request.segmentation_hash,
    )?;
    let embedding =
        validate_managed_model(&root, &request.embedding_model, &request.embedding_hash)?;
    Ok((segmentation, embedding))
}

fn read_output(path: &Path, request: &DiarizationRequest) -> Result<DiarizationOutput, String> {
    let metadata = fs::metadata(path)
        .map_err(|_| "Diarization worker did not return a result.".to_string())?;
    if metadata.len() > MAX_RESULT_BYTES {
        return Err("Diarization worker result exceeded its size limit.".into());
    }
    let bytes =
        fs::read(path).map_err(|_| "Could not read diarization worker result.".to_string())?;
    let output: DiarizationOutput = serde_json::from_slice(&bytes)
        .map_err(|_| "Diarization worker returned an invalid result.".to_string())?;
    validate_output(output, request)
}

fn validate_output(
    output: DiarizationOutput,
    request: &DiarizationRequest,
) -> Result<DiarizationOutput, String> {
    if output.recording_id != request.recording.recording_id || output.warnings.len() > 32 {
        return Err("Diarization worker result did not match its request.".into());
    }
    let source_ids: HashSet<&str> = request
        .recording
        .tracks
        .iter()
        .map(|track| track.source_id.as_str())
        .collect();
    for warning in &output.warnings {
        if warning.len() > 512 {
            return Err("Diarization worker returned an oversized warning.".into());
        }
    }
    for turn in &output.turns {
        if !source_ids.contains(turn.source_id.as_str())
            || turn.end_ms < turn.start_ms
            || turn.end_ms > MAX_SOURCE_SECONDS * 1000
            || turn
                .confidence
                .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err("Diarization worker returned an invalid turn.".into());
        }
    }
    Ok(output)
}

/// Runs in the dedicated subprocess. No caller-visible audio or transcript is
/// written to stdout/stderr; results go only to the requested local JSON file.
pub fn run_worker(request_path: &Path, result_path: &Path) -> Result<(), String> {
    install_parent_liveness_monitor();
    if fs::symlink_metadata(request_path)
        .map_err(|_| "Could not read diarization worker request.".to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("Diarization worker request must not be a symbolic link.".into());
    }
    let request_metadata = fs::metadata(request_path)
        .map_err(|_| "Could not read diarization worker request.".to_string())?;
    if request_metadata.len() > MAX_RESULT_BYTES {
        return Err("Diarization worker request exceeded its size limit.".into());
    }
    let request_bytes = fs::read(request_path)
        .map_err(|_| "Could not read diarization worker request.".to_string())?;
    let request: DiarizationRequest = serde_json::from_slice(&request_bytes)
        .map_err(|_| "Diarization worker request is invalid.".to_string())?;
    let (segmentation_model, embedding_model) = validate_request(&request)?;
    let directory = recording_directory(&request)?;
    let request_parent = request_path
        .parent()
        .and_then(|parent| fs::canonicalize(parent).ok());
    let result_parent = result_path
        .parent()
        .and_then(|parent| fs::canonicalize(parent).ok());
    if request_parent.as_deref() != Some(directory.as_path())
        || result_parent.as_deref() != Some(directory.as_path())
        || result_path.exists()
    {
        return Err("Diarization worker request or result path is invalid.".into());
    }
    let tracks = validated_recording_tracks(&request.recording, &request.recordings_root)?;
    let mut turns = Vec::new();
    for (track, path) in tracks {
        let samples = load_wav_16k_mono(&path, MAX_SOURCE_SECONDS)?;
        if samples.is_empty() {
            continue;
        }
        let diarizer = create_diarizer(&segmentation_model, &embedding_model)?;
        if diarizer.sample_rate() != 16_000 {
            return Err("Diarization model does not use the required 16 kHz sample rate.".into());
        }
        let result = diarizer
            .process(&samples)
            .ok_or_else(|| "Local diarization processing failed.".to_string())?;
        for segment in result.sort_by_start_time() {
            if !segment.start.is_finite()
                || !segment.end.is_finite()
                || segment.start < 0.0
                || segment.end < segment.start
                || segment.speaker < 0
            {
                return Err("Local diarization returned an invalid segment.".into());
            }
            let start_ms = (segment.start as f64 * 1000.0).round() as u64;
            let end_ms = (segment.end as f64 * 1000.0).round() as u64;
            if end_ms > MAX_SOURCE_SECONDS * 1000 {
                return Err("Local diarization returned a segment beyond the source limit.".into());
            }
            turns.push(SpeakerTurn {
                source_id: track.source_id.clone(),
                start_ms,
                end_ms,
                speaker: segment.speaker as u32,
                confidence: segment.confidence.is_finite().then_some(segment.confidence),
            });
        }
    }
    turns.sort_by_key(|turn| {
        (
            turn.start_ms,
            turn.end_ms,
            turn.source_id.clone(),
            turn.speaker,
        )
    });
    write_json(
        result_path,
        &DiarizationOutput {
            recording_id: request.recording.recording_id,
            turns,
            warnings: vec![
                "Speaker numbers are local to each recorded source and are not person identities."
                    .into(),
            ],
        },
    )
}

fn create_diarizer(
    segmentation_model: &Path,
    embedding_model: &Path,
) -> Result<OfflineSpeakerDiarization, String> {
    let config = OfflineSpeakerDiarizationConfig {
        segmentation: OfflineSpeakerSegmentationModelConfig {
            pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                model: Some(segmentation_model.to_string_lossy().into_owned()),
                window_shift_ratio: 0.1,
            },
            num_threads: 2,
            debug: false,
            provider: Some("cpu".into()),
        },
        embedding: SpeakerEmbeddingExtractorConfig {
            model: Some(embedding_model.to_string_lossy().into_owned()),
            num_threads: 2,
            debug: false,
            provider: Some("cpu".into()),
        },
        clustering: FastClusteringConfig {
            num_clusters: -1,
            threshold: 0.5,
            compute_confidence: true,
        },
        ..Default::default()
    };
    OfflineSpeakerDiarization::create(&config)
        .ok_or_else(|| "Could not initialize local diarization models.".into())
}

fn install_parent_liveness_monitor() {
    thread::spawn(|| {
        let mut stdin = std::io::stdin();
        let mut byte = [0u8; 1];
        let _ = stdin.read(&mut byte);
        std::process::exit(CHILD_EXIT_PARENT_GONE);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording() -> crate::recording::Recording {
        crate::recording::Recording {
            recording_id: Uuid::new_v4().to_string(),
            started_at: 0,
            ended_at: Some(0),
            duration_ms: 0,
            status: "complete".into(),
            tracks: vec![crate::recording::Track {
                source_id: "track-a".into(),
                name: "Track A".into(),
                direction: "input".into(),
                sample_rate: 16_000,
                channels: 1,
                file_path: "unused.wav".into(),
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
        }
    }

    #[test]
    fn accepts_only_lowercase_sha256_identities() {
        assert!(valid_sha256(&"a".repeat(64)));
        assert!(!valid_sha256(&"A".repeat(64)));
        assert!(!valid_sha256("bad"));
    }

    #[test]
    fn worker_output_cannot_introduce_an_unknown_source() {
        let recording = recording();
        let request = DiarizationRequest {
            recording: recording.clone(),
            recordings_root: PathBuf::from("unused"),
            segmentation_model: PathBuf::from("segment.onnx"),
            embedding_model: PathBuf::from("embed.onnx"),
            segmentation_hash: "a".repeat(64),
            embedding_hash: "b".repeat(64),
        };
        let output = DiarizationOutput {
            recording_id: recording.recording_id,
            turns: vec![SpeakerTurn {
                source_id: "forged".into(),
                start_ms: 0,
                end_ms: 10,
                speaker: 0,
                confidence: Some(0.5),
            }],
            warnings: vec![],
        };
        assert!(validate_output(output, &request).is_err());
    }

    #[test]
    fn worker_output_rejects_non_finite_confidence() {
        let recording = recording();
        let request = DiarizationRequest {
            recording: recording.clone(),
            recordings_root: PathBuf::from("unused"),
            segmentation_model: PathBuf::from("segment.onnx"),
            embedding_model: PathBuf::from("embed.onnx"),
            segmentation_hash: "a".repeat(64),
            embedding_hash: "b".repeat(64),
        };
        let output = DiarizationOutput {
            recording_id: recording.recording_id,
            turns: vec![SpeakerTurn {
                source_id: "track-a".into(),
                start_ms: 0,
                end_ms: 10,
                speaker: 0,
                confidence: Some(f32::NAN),
            }],
            warnings: vec![],
        };
        assert!(validate_output(output, &request).is_err());
    }

    #[test]
    fn worker_output_rejects_out_of_range_confidence() {
        let recording = recording();
        let request = DiarizationRequest {
            recording: recording.clone(),
            recordings_root: PathBuf::from("unused"),
            segmentation_model: PathBuf::from("segment.onnx"),
            embedding_model: PathBuf::from("embed.onnx"),
            segmentation_hash: "a".repeat(64),
            embedding_hash: "b".repeat(64),
        };
        let output = DiarizationOutput {
            recording_id: recording.recording_id,
            turns: vec![SpeakerTurn {
                source_id: "track-a".into(),
                start_ms: 0,
                end_ms: 10,
                speaker: 0,
                confidence: Some(1.1),
            }],
            warnings: vec![],
        };
        assert!(validate_output(output, &request).is_err());
    }
}
