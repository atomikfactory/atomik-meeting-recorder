//! Local summarization via a local Ollama server. Mirrors the shape of
//! `transcription_engine`/`diarization_engine`'s engine traits so the worker in
//! `summary.rs` can be driven by a fake in tests without any real HTTP calls.
use crate::{
    ollama_client::{self, chunk_schema, summary_schema, OllamaClient},
    recording::Recording,
    summary_types::{OwnerContext, SummaryOutput},
    transcription_types::Transcript,
};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug)]
pub struct SummaryRequest {
    pub recording: Recording,
    pub transcript: Transcript,
    pub ollama_url: String,
    pub model: String,
    /// The local user's display name (see `store::Settings::local_speaker_name`),
    /// used to label their own microphone-track lines in the prompt. Falls back
    /// to "You" at the call site when `transcript.local_speaker_name` is unset.
    pub local_speaker_name: String,
    /// Completed local speaker-identification labels (see
    /// `diarization::read_completed_speaker_labels`), parallel to
    /// `transcript.segments`. `None` when diarization has not completed for
    /// this recording -- remote lines then render as "Remote".
    pub remote_speaker_labels: Option<Vec<Option<u32>>>,
    /// Per-recording display names the user has set for this meeting's
    /// speaker labels (see `speaker_names.rs`), so the summary the model
    /// writes uses "Alice"/"Bob" rather than "Speaker 1"/"Speaker 2" once the
    /// user has named them. Regenerating (not auto-regenerating) a summary
    /// is the only way this takes effect -- see `summary.rs::regenerate`.
    pub names: crate::transcription::SpeakerNames,
}

pub trait SummaryEngine: Send + Sync {
    fn summarize(
        &self,
        request: &SummaryRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn FnMut(u8),
    ) -> Result<SummaryOutput, String>;
    fn list_models(&self, ollama_url: &str) -> Result<Vec<String>, String>;
    fn test_connection(&self, ollama_url: &str) -> Result<(), String>;
}

/// 4096..32768, rounded up to the next power of two above the estimated need,
/// leaving headroom for the prompt/schema/response beyond the raw transcript.
fn choose_num_ctx(text_len: usize) -> usize {
    let estimated_tokens = text_len / 3 + 2000;
    let mut ctx = 4096usize;
    while ctx < estimated_tokens && ctx < 32_768 {
        ctx *= 2;
    }
    ctx.min(32_768)
}

fn language_hint(transcript: &Transcript) -> Option<String> {
    let mut languages = transcript.languages_used.clone();
    languages.retain(|l| !l.is_empty());
    languages.dedup();
    (languages.len() == 1).then(|| languages.remove(0))
}

/// A short factual preamble the model receives before the transcript itself,
/// so it can ground timestamps/duration without inferring them from segment
/// text (which is unreliable for a meeting with silence or overlapping speech).
fn meeting_header(recording: &Recording) -> String {
    let minutes = recording.duration_ms / 60_000;
    let sources = recording
        .tracks
        .iter()
        .map(|t| t.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("Meeting duration: about {minutes} minute(s). Audio sources: {sources}.\n\n")
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err("Summarizing cancelled; no result was applied.".into())
    } else {
        Ok(())
    }
}

fn parse_summary(raw: &str, owner_context: &OwnerContext<'_>) -> Result<SummaryOutput, String> {
    let mut output: SummaryOutput = serde_json::from_str(raw)
        .map_err(|e| format!("Ollama returned a summary that did not match the expected shape: {e}"))?;
    if !output.is_meaningful() {
        return Err("Ollama returned an empty summary.".into());
    }
    // Belt-and-suspenders against the prompt instruction being ignored: an
    // owner that is not a real speaker label or a name literally present in
    // the transcript (e.g. "Remote", "Participants") is dropped rather than
    // surfaced -- see `summary_types::OwnerContext`.
    output.sanitize_owners(owner_context);
    Ok(output)
}

#[derive(Default)]
pub struct OllamaSummaryEngine;
impl OllamaSummaryEngine {
    pub fn new() -> Self {
        Self
    }
}
impl SummaryEngine for OllamaSummaryEngine {
    fn summarize(
        &self,
        request: &SummaryRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn FnMut(u8),
    ) -> Result<SummaryOutput, String> {
        check_cancelled(cancelled)?;
        let client = OllamaClient::new(&request.ollama_url)?;
        let language = language_hint(&request.transcript);
        let header = meeting_header(&request.recording);
        // Keyed by segment identity, not position: `plan_chunks` below copies
        // (possibly overlapping) subsets of `request.transcript.segments`, so a
        // positional index into the labels would not line up once chunked.
        let remote_map: Option<std::collections::HashMap<ollama_client::SegmentKey, u32>> =
            request.remote_speaker_labels.as_ref().map(|labels| {
                request
                    .transcript
                    .segments
                    .iter()
                    .zip(labels.iter())
                    .filter_map(|(segment, label)| {
                        label.map(|n| (ollama_client::segment_key(segment), n))
                    })
                    .collect()
            });
        let remote_map = remote_map.as_ref();
        let full_text = format!(
            "{header}{}",
            ollama_client::format_segments(
                &request.transcript.segments,
                &request.recording,
                &request.local_speaker_name,
                remote_map,
                Some(&request.names),
            )
        );
        let num_ctx = choose_num_ctx(full_text.len());
        let chunks = ollama_client::plan_chunks(&request.transcript.segments, num_ctx);
        let device_names: Vec<String> = request.recording.tracks.iter().map(|t| t.name.clone()).collect();
        let owner_context = OwnerContext::new(&request.local_speaker_name, &device_names, &full_text);
        progress(0);
        if chunks.len() <= 1 {
            let system = ollama_client::prompts(language.as_deref(), false);
            let raw = client.chat(&request.model, &system, &full_text, summary_schema(), num_ctx)?;
            check_cancelled(cancelled)?;
            progress(100);
            return parse_summary(&raw, &owner_context);
        }
        // Map: one narrative-notes call per chunk.
        let mut notes = Vec::with_capacity(chunks.len());
        let chunk_system = ollama_client::prompts(language.as_deref(), false);
        for (index, chunk) in chunks.iter().enumerate() {
            check_cancelled(cancelled)?;
            let text = format!(
                "{header}{}",
                ollama_client::format_segments(
                    chunk,
                    &request.recording,
                    &request.local_speaker_name,
                    remote_map,
                    Some(&request.names),
                )
            );
            let raw = client.chat(&request.model, &chunk_system, &text, chunk_schema(), num_ctx)?;
            let parsed: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| format!("Ollama returned malformed chunk notes: {e}"))?;
            let text = parsed["notes"].as_str().unwrap_or_default().to_string();
            notes.push(format!("--- Segment {} of {} ---\n{}", index + 1, chunks.len(), text));
            // Map phase fills the first 80%; the final merge call finishes it.
            progress((((index + 1) * 80) / chunks.len()) as u8);
        }
        // Reduce: merge every chunk's notes into the final structured schema.
        check_cancelled(cancelled)?;
        let merge_system = ollama_client::prompts(language.as_deref(), true);
        let merged_input = notes.join("\n\n");
        let merge_ctx = choose_num_ctx(merged_input.len());
        let raw = client.chat(&request.model, &merge_system, &merged_input, summary_schema(), merge_ctx)?;
        check_cancelled(cancelled)?;
        progress(100);
        parse_summary(&raw, &owner_context)
    }
    fn list_models(&self, ollama_url: &str) -> Result<Vec<String>, String> {
        OllamaClient::new(ollama_url)?.list_models()
    }
    fn test_connection(&self, ollama_url: &str) -> Result<(), String> {
        OllamaClient::new(ollama_url)?.test_connection()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn num_ctx_grows_with_transcript_length_and_caps_at_32k() {
        assert_eq!(choose_num_ctx(100), 4096);
        assert_eq!(choose_num_ctx(20_000), 16_384);
        assert!(choose_num_ctx(1_000_000) <= 32_768);
        assert_eq!(choose_num_ctx(1_000_000), 32_768);
    }
    #[test]
    fn language_hint_only_fires_for_a_single_consistent_language() {
        let mut t = Transcript {
            recording_id: "r".into(),
            model_key: "base".into(),
            model_hash: "h".into(),
            segments: vec![],
            warnings: vec![],
            languages_used: vec!["en".into()],
            local_speaker_name: Some("You".into()),
        };
        assert_eq!(language_hint(&t), Some("en".into()));
        t.languages_used = vec!["en".into(), "fr".into()];
        assert_eq!(language_hint(&t), None);
        t.languages_used = vec![];
        assert_eq!(language_hint(&t), None);
    }
    fn no_owner_context() -> OwnerContext<'static> {
        OwnerContext::new("You", &[], "")
    }
    #[test]
    fn parse_summary_rejects_empty_content() {
        let empty = serde_json::to_string(&SummaryOutput::default()).unwrap();
        assert!(parse_summary(&empty, &no_owner_context()).is_err());
        let filled = SummaryOutput {
            title: "Weekly sync".into(),
            summary: "Discussed the roadmap.".into(),
            key_points: vec!["Roadmap reviewed".into()],
            ..Default::default()
        };
        assert!(parse_summary(&serde_json::to_string(&filled).unwrap(), &no_owner_context()).is_ok());
    }
    #[test]
    fn parse_summary_rejects_malformed_json() {
        assert!(parse_summary("not json", &no_owner_context()).is_err());
    }
    #[test]
    fn parse_summary_drops_a_remote_owner() {
        let filled = SummaryOutput {
            title: "Sync".into(),
            summary: "Discussed the plan.".into(),
            key_points: vec!["Roadmap reviewed".into()],
            action_items: vec![crate::summary_types::ActionItem {
                task: "Confirm the spreadsheet details".into(),
                owner: Some("Remote".into()),
                due: None,
            }],
            ..Default::default()
        };
        let output = parse_summary(&serde_json::to_string(&filled).unwrap(), &no_owner_context()).unwrap();
        assert_eq!(output.action_items[0].owner, None);
    }
}
