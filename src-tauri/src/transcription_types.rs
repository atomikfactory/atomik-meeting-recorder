use crate::recording::Recording;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A caller-supplied local model and a completed local recording. `recordings_root`
/// is mandatory so database metadata cannot redirect the engine to arbitrary files.
#[derive(Clone, Debug)]
pub struct TranscriptionRequest {
    pub recording: Recording,
    pub recordings_root: PathBuf,
    pub model_path: PathBuf,
    pub model_key: String,
    pub model_hash: String,
    /// `None` requests automatic per-track language detection; `Some(code)` pins
    /// every chunk of every track to that lowercase ISO 639-1 code.
    pub language: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Transcript {
    pub recording_id: String,
    pub model_key: String,
    pub model_hash: String,
    pub segments: Vec<TranscriptSegment>,
    pub warnings: Vec<String>,
    /// The language(s) actually used to decode this transcript: the requested
    /// code, or codes auto-detected per track. Absent in transcripts written
    /// before this field existed; `validate_transcript`/checkpoint recovery do
    /// not depend on it.
    #[serde(default)]
    pub languages_used: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    /// An audio-track label, never a claimed speaker identity.
    pub source_id: String,
    pub source_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub key: String,
    pub sha256: String,
    pub bytes: u64,
}
