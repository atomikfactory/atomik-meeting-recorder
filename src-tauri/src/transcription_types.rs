use crate::recording::Recording;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A caller-supplied local model and a completed local recording. `recordings_root`
/// is mandatory so database metadata cannot redirect the engine to arbitrary files.
#[derive(Clone, Debug)]
pub struct TranscriptionRequest {
    pub recording: Recording,
    pub recordings_root: PathBuf,
    /// Extra known library roots (the currently configured recordings folder
    /// and any it replaced) so a recording created under a custom folder can
    /// still be located; see `recording_paths::resolved_recording_directory`.
    pub extra_library_roots: Vec<PathBuf>,
    pub model_path: PathBuf,
    pub model_key: String,
    pub model_hash: String,
    /// `None` requests automatic per-track language detection; `Some(code)` pins
    /// every chunk of every track to that lowercase ISO 639-1 code.
    pub language: Option<String>,
    /// Whether to run acoustic echo cancellation (see `echo_cancel.rs`) on
    /// input (microphone) tracks against summed output (loopback) tracks
    /// before transcription, when the recording has both. A no-op when the
    /// recording lacks either track direction, regardless of this flag.
    pub echo_cancellation: bool,
    /// Whether to run the text-level echo-duplicate cleanup (see
    /// `echo_dedup.rs`) after transcription. Independent of
    /// `echo_cancellation`: a safety net for whatever signal-level AEC
    /// leaves behind, or does not get to run at all.
    pub dedupe_echo_segments: bool,
    /// The local user's display name at the time of transcription (see
    /// `store::Settings::local_speaker_name`), baked into the resulting
    /// `Transcript` so its Markdown rendering stays a pure, checkpoint-safe
    /// function of `(Recording, Transcript)` even if the setting later
    /// changes -- see `Transcript::local_speaker_name`.
    pub local_speaker_name: String,
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
    /// The local user's display name in effect when this transcript was
    /// produced (see `store::Settings::local_speaker_name`), used to render
    /// input-track (microphone) segments in Markdown and the summary prompt.
    /// `None` for transcripts written before this field existed, or if
    /// somehow unset; renders fall back to "You". Renaming yourself in
    /// Settings only affects transcripts produced after the change --
    /// rewriting an already-checkpointed Markdown file retroactively would
    /// defeat its own tamper-evidence check.
    #[serde(default)]
    pub local_speaker_name: Option<String>,
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
