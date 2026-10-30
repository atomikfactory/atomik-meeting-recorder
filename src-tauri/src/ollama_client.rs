//! A small blocking client for a local Ollama server. Every request is
//! validated to target loopback only (`localhost`/`127.0.0.1`/`::1`), which is
//! what keeps this feature inside the app's "no cloud requests" guarantee --
//! `summary_enabled` merely queues local work; this module is the only place
//! that ever opens a socket for it.
use crate::recording::Recording;
use crate::transcription::presentation_label;
use crate::transcription_types::TranscriptSegment;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A refused connection (Ollama not running) returns immediately regardless of
/// this value; it only bounds how long a genuinely slow/busy local machine is
/// given before the job moves to "waiting" and is retried with backoff.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Needs to cover a slow CPU-bound local decode of a long transcript; 10
/// minutes matches the brief's stated budget per call.
const READ_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Overlap kept between adjacent chunks so a decision or action item is not
/// severed at a chunk boundary and lost from every summary.
const CHUNK_OVERLAP_SEGMENTS: usize = 4;

/// Rejects anything but loopback so a misconfigured URL can never become a
/// real network request. Accepts an explicit scheme (defaults to http) and an
/// optional port; hostnames are compared case-insensitively.
pub fn validate_loopback_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| "Ollama URL must start with http:// or https://.".to_string())?;
    let host_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(bracket_end) = host_port.strip_prefix('[') {
        bracket_end
            .split(']')
            .next()
            .ok_or_else(|| "Invalid Ollama URL host.".to_string())?
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
        Ok(())
    } else {
        Err("Ollama URL must be a loopback address (localhost, 127.0.0.1, or [::1]) to keep summaries fully local.".into())
    }
}

/// The Ollama JSON Schema (`format`) that pins the model's structured output
/// to the shape `summary_types::SummaryOutput` deserializes.
pub fn summary_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "title": {"type": "string"},
            "summary": {"type": "string"},
            "key_points": {"type": "array", "items": {"type": "string"}},
            "decisions": {"type": "array", "items": {"type": "string"}},
            "action_items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "task": {"type": "string"},
                        "owner": {"type": ["string", "null"]},
                        "due": {"type": ["string", "null"]}
                    },
                    "required": ["task"]
                }
            },
            "open_questions": {"type": "array", "items": {"type": "string"}},
            "language": {"type": "string"}
        },
        "required": ["title", "summary", "key_points", "decisions", "action_items", "open_questions", "language"]
    })
}

/// Chunks of a multi-part meeting only need a plain narrative in return, not
/// the full final schema, so the intermediate schema is a single string field.
pub fn chunk_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "notes": {"type": "string"} },
        "required": ["notes"]
    })
}

fn system_prompt(language_hint: Option<&str>) -> String {
    let language = language_hint
        .filter(|l| !l.is_empty() && *l != "auto")
        .map(|l| format!("Write your entire response in the meeting's language ({l})."))
        .unwrap_or_else(|| "Detect the meeting's dominant language from the transcript and write your entire response in that language.".into());
    format!(
        "You are summarizing a local meeting transcript for the person who recorded it. {language}\n\n\
        Each line is labeled with who said it: \"You\" (or the local user's chosen name) is the person who recorded the meeting, speaking on their own microphone; \"Speaker 1\", \"Speaker 2\", and so on are distinct remote participants identified by voice; \"Remote\" is a remote participant's line that could not be confidently assigned to a specific speaker number. These are not necessarily this person's real name -- only use a real name as `owner` in an action item when that name (or an exact label like \"You\"/\"Speaker 2\") is actually spoken or written in the transcript; otherwise leave `owner` null. Some of these transcripts predate an echo-cancellation feature and may contain near-duplicate lines across the local and remote labels -- one side's speech leaking faintly into the other -- treat closely matching overlapping lines as likely echo of the same speech, not two people saying the same thing.\n\n\
        `owner` must always be either an exact speaker label that appears in the transcript (\"You\", or \"Speaker 1\"/\"Speaker 2\"/etc.) or a specific person's name that is literally spoken or written somewhere in the transcript. \"Remote\" is never a valid `owner` -- it is not a person, only an unresolved line label -- and neither is a generic group word (\"Participants\", \"Everyone\", \"The team\") or an audio device/app name. If the only thing you know is that an unidentified remote participant should do something, leave `owner` null rather than writing \"Remote\".\n\n\
        Do not invent names, owners, or due dates that are not stated or clearly implied in the transcript; leave `owner`/`due` null rather than guessing. Only report action items, decisions, and open questions that are actually present. Keep the summary factual and concise; it will be read by someone who was in the meeting."
    )
}

fn merge_prompt(language_hint: Option<&str>) -> String {
    format!(
        "{}\n\nYou are given partial notes from consecutive segments of one longer meeting, in order. Merge them into a single coherent summary of the whole meeting, removing duplication introduced by overlap between segments.",
        system_prompt(language_hint)
    )
}

/// Identifies one transcript segment independent of its position in whatever
/// (possibly reordered, possibly overlapping) slice is being rendered --
/// map-reduce chunking in `plan_chunks` copies subsets of the original
/// segment list, so a plain index into `remote_speaker_by_segment` would not
/// line up with the original checkpoint's labels.
pub type SegmentKey = (String, u64, u64);
pub fn segment_key(segment: &TranscriptSegment) -> SegmentKey {
    (segment.source_id.clone(), segment.start_ms, segment.end_ms)
}

/// Renders transcript segments as plain lines the model can read, using
/// wall-clock-style timestamps and the same You/Speaker N/Remote labels a
/// human transcript would show (see `transcription::presentation_label`), so
/// the model receives exactly the ambiguity -- and the same identity
/// resolution -- a human reader would.
pub fn format_segments(
    segments: &[TranscriptSegment],
    recording: &Recording,
    local_speaker_name: &str,
    remote_speaker_by_segment: Option<&std::collections::HashMap<SegmentKey, u32>>,
    names: Option<&crate::transcription::SpeakerNames>,
) -> String {
    segments
        .iter()
        .map(|segment| {
            let remote_speaker =
                remote_speaker_by_segment.and_then(|m| m.get(&segment_key(segment)).copied());
            let label = presentation_label(
                recording,
                segment,
                local_speaker_name,
                remote_speaker,
                names,
            );
            format!(
                "[{}] {}: {}",
                timestamp(segment.start_ms),
                label,
                segment.text.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn timestamp(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// A rough, deliberately conservative characters-per-token estimate used only
/// to decide whether map-reduce chunking is needed; Ollama enforces the real
/// context window, this just avoids sending something certain to be truncated.
const CHARS_PER_TOKEN: usize = 4;

/// A rough per-line length estimate for chunk-size budgeting only (see
/// `plan_chunks`): the exact rendered label ("You"/"Speaker N"/"Remote") is
/// not yet known at this stage (it depends on the recording and any
/// diarization result), but its length is close enough to the raw source
/// name for this to stay a safe, conservative estimate either way.
fn rendered_line_len_estimate(segment: &TranscriptSegment) -> usize {
    // "[HH:MM:SS] " + label + ": " + text + newline
    11 + segment.source_name.len() + 2 + segment.text.trim().len() + 1
}

/// Splits segments into chunks whose rendered text fits within `num_ctx` tokens
/// (leaving room for the prompt and the response), each overlapping the
/// previous chunk by a few segments so a point spanning a boundary is not lost
/// from every chunk's summary. Returns a single chunk covering everything when
/// it already fits, so callers do not need a separate "direct" path.
pub fn plan_chunks(segments: &[TranscriptSegment], num_ctx: usize) -> Vec<Vec<TranscriptSegment>> {
    let budget_chars = num_ctx
        .saturating_mul(CHARS_PER_TOKEN)
        .saturating_sub(2000)
        .max(2000);
    if segments.is_empty() {
        return vec![Vec::new()];
    }
    if segments
        .iter()
        .map(rendered_line_len_estimate)
        .sum::<usize>()
        <= budget_chars
    {
        return vec![segments.to_vec()];
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < segments.len() {
        let mut end = start;
        let mut len = 0usize;
        while end < segments.len() {
            let line_len = rendered_line_len_estimate(&segments[end]);
            if end > start && len + line_len > budget_chars {
                break;
            }
            len += line_len;
            end += 1;
        }
        if end == start {
            end = start + 1; // a single oversized segment still makes progress
        }
        chunks.push(segments[start..end].to_vec());
        if end >= segments.len() {
            break;
        }
        start = end.saturating_sub(CHUNK_OVERLAP_SEGMENTS).max(start + 1);
    }
    chunks
}

#[derive(Serialize)]
struct ChatOptions {
    temperature: f32,
    num_ctx: usize,
}
#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: String,
}
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
    format: serde_json::Value,
    options: ChatOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    think: Option<bool>,
}
#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
}
#[derive(Deserialize)]
struct ChatResponse {
    message: ChatResponseMessage,
}
#[derive(Deserialize)]
struct TagsModel {
    name: String,
}
#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagsModel>,
}

pub struct OllamaClient {
    base_url: String,
    agent: ureq::Agent,
}
impl OllamaClient {
    pub fn new(base_url: &str) -> Result<Self, String> {
        validate_loopback_url(base_url)?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(CONNECT_TIMEOUT)
                .timeout_read(READ_TIMEOUT)
                .timeout_write(Duration::from_secs(30))
                .build(),
        })
    }
    pub fn list_models(&self) -> Result<Vec<String>, String> {
        let response = self
            .agent
            .get(&format!("{}/api/tags", self.base_url))
            .call()
            .map_err(describe_transport_error)?;
        let parsed: TagsResponse = response
            .into_json()
            .map_err(|e| format!("Ollama returned an unreadable model list: {e}"))?;
        Ok(parsed.models.into_iter().map(|m| m.name).collect())
    }
    pub fn test_connection(&self) -> Result<(), String> {
        self.list_models().map(|_| ())
    }
    /// Runs one structured chat call. `system`/`user` are plain prompt text;
    /// `schema` pins the JSON shape of the returned `content` string, which the
    /// caller still parses (Ollama does not validate its own model's output).
    pub fn chat(
        &self,
        model: &str,
        system: &str,
        user: &str,
        schema: serde_json::Value,
        num_ctx: usize,
    ) -> Result<String, String> {
        let request = ChatRequest {
            model,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: system.to_string(),
                },
                ChatMessage {
                    role: "user",
                    content: user.to_string(),
                },
            ],
            stream: false,
            format: schema,
            options: ChatOptions {
                temperature: 0.2,
                num_ctx,
            },
            // qwen3's "thinking" mode roughly doubles latency for a summarization
            // task that does not benefit from it; other models ignore this field.
            think: model.starts_with("qwen3").then_some(false),
        };
        let response = self
            .agent
            .post(&format!("{}/api/chat", self.base_url))
            .send_json(&request)
            .map_err(describe_chat_error(model))?;
        let parsed: ChatResponse = response
            .into_json()
            .map_err(|e| format!("Ollama returned an unreadable response: {e}"))?;
        Ok(parsed.message.content)
    }
}
fn describe_transport_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            format!("Ollama returned HTTP {code}: {}", response.status_text())
        }
        ureq::Error::Transport(_) => {
            "Ollama is not reachable at the configured URL. Start Ollama and it will be retried automatically.".into()
        }
    }
}
fn describe_chat_error(model: &str) -> impl Fn(ureq::Error) -> String + '_ {
    move |error| {
        match error {
        ureq::Error::Status(404, _) => format!(
            "Ollama does not have the model '{model}'. Run `ollama pull {model}` and it will be retried automatically."
        ),
        ureq::Error::Status(code, response) => {
            format!("Ollama returned HTTP {code}: {}", response.status_text())
        }
        ureq::Error::Transport(_) => {
            "Ollama is not reachable at the configured URL. Start Ollama and it will be retried automatically.".into()
        }
    }
    }
}

pub(crate) fn prompts(language_hint: Option<&str>, is_merge: bool) -> String {
    if is_merge {
        merge_prompt(language_hint)
    } else {
        system_prompt(language_hint)
    }
}

#[cfg(test)]
#[path = "ollama_http_tests.rs"]
mod http_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn segment(ms: u64, source: &str, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            start_ms: ms,
            end_ms: ms + 1000,
            text: text.into(),
            source_id: source.into(),
            source_name: source.into(),
        }
    }
    #[test]
    fn accepts_only_loopback_hosts() {
        assert!(validate_loopback_url("http://127.0.0.1:11434").is_ok());
        assert!(validate_loopback_url("http://localhost:11434").is_ok());
        assert!(validate_loopback_url("http://LOCALHOST:11434").is_ok());
        assert!(validate_loopback_url("http://[::1]:11434").is_ok());
        assert!(validate_loopback_url("http://example.com").is_err());
        assert!(validate_loopback_url("http://192.168.1.5:11434").is_err());
        assert!(validate_loopback_url("ftp://127.0.0.1").is_err());
        assert!(validate_loopback_url("http://127.0.0.1.evil.com").is_err());
    }
    #[test]
    fn schema_declares_every_output_field_as_required() {
        let schema = summary_schema();
        let required = schema["required"].as_array().unwrap();
        for field in [
            "title",
            "summary",
            "key_points",
            "decisions",
            "action_items",
            "open_questions",
            "language",
        ] {
            assert!(required.iter().any(|v| v == field), "missing {field}");
        }
    }
    #[test]
    fn small_transcript_is_a_single_chunk() {
        let segments = vec![
            segment(0, "Microphone", "hello"),
            segment(1000, "Speakers", "hi"),
        ];
        let chunks = plan_chunks(&segments, 32_768);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 2);
    }
    #[test]
    fn oversized_transcript_is_chunked_with_overlap() {
        let segments: Vec<_> = (0..2000)
            .map(|i| segment(i * 1000, "Microphone", &"word ".repeat(50)))
            .collect();
        let chunks = plan_chunks(&segments, 2048);
        assert!(chunks.len() > 1);
        // Every chunk after the first repeats a tail of the previous chunk.
        for window in chunks.windows(2) {
            let previous_tail =
                &window[0][window[0].len().saturating_sub(CHUNK_OVERLAP_SEGMENTS)..];
            assert_eq!(previous_tail[0].start_ms, window[1][0].start_ms);
        }
        // Chunking must not drop text: every source segment appears somewhere.
        let covered: std::collections::HashSet<_> =
            chunks.iter().flatten().map(|s| s.start_ms).collect();
        assert_eq!(covered.len(), segments.len());
    }
    #[test]
    fn empty_transcript_plans_one_empty_chunk() {
        assert_eq!(
            plan_chunks(&[], 32_768),
            vec![Vec::<TranscriptSegment>::new()]
        );
    }
    #[test]
    fn think_is_set_false_only_for_qwen3_models() {
        // The request is not sent (no server); this only checks serialization shape.
        let request = ChatRequest {
            model: "qwen3:8b",
            messages: vec![],
            stream: false,
            format: serde_json::json!({}),
            options: ChatOptions {
                temperature: 0.2,
                num_ctx: 4096,
            },
            think: "qwen3:8b".starts_with("qwen3").then_some(false),
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["think"], serde_json::json!(false));
        let request2 = ChatRequest {
            model: "gemma4:12b",
            think: "gemma4:12b".starts_with("qwen3").then_some(false),
            ..request
        };
        let value2 = serde_json::to_value(&request2).unwrap();
        assert!(value2.get("think").is_none());
    }
    #[test]
    fn prompt_mentions_speaker_labels_and_forbids_invention() {
        let text = system_prompt(Some("en"));
        assert!(text.contains("\"You\""));
        assert!(text.contains("Speaker 1"));
        assert!(text.contains("not invent"));
        assert!(text.contains("(en)"));
    }
}
