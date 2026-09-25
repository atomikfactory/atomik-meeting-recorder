use crate::recording::Recording;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// All paths are revalidated by the isolated worker before any native model API
/// is called. Model files must be managed copies under `recordings_root/models`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiarizationRequest {
    pub recording: Recording,
    pub recordings_root: PathBuf,
    /// Extra known library roots (the currently configured recordings folder
    /// and any it replaced) so a recording created under a custom folder can
    /// still be located; see `recording_paths::resolved_recording_directory`.
    #[serde(default)]
    pub extra_library_roots: Vec<PathBuf>,
    pub segmentation_model: PathBuf,
    pub embedding_model: PathBuf,
    pub segmentation_hash: String,
    pub embedding_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DiarizationOutput {
    pub recording_id: String,
    pub turns: Vec<SpeakerTurn>,
    pub warnings: Vec<String>,
}

/// `speaker` is a clustering index local to this one recorded source. It is
/// not a person identity and must not be compared across sources.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SpeakerTurn {
    pub source_id: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: u32,
    pub confidence: Option<f32>,
}
