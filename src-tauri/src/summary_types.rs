use serde::{Deserialize, Serialize};

/// An action item extracted from the meeting. `owner`/`due` are `None` when the
/// transcript does not name one; the model is instructed never to invent either.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ActionItem {
    pub task: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub due: Option<String>,
}

/// The structured local summary of one meeting, produced by a local Ollama
/// model. Never leaves this device; the request that produced it only ever
/// reaches the loopback-only Ollama server (see `ollama_client::validate_loopback_url`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct SummaryOutput {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub key_points: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub action_items: Vec<ActionItem>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    /// BCP-47-ish language tag/name the model reports having written in, e.g. "en" or "fr".
    #[serde(default)]
    pub language: String,
}

impl SummaryOutput {
    /// A job whose model returned effectively nothing is not a usable summary; treat
    /// it the same as a hard failure rather than writing an empty checkpoint.
    pub fn is_meaningful(&self) -> bool {
        !self.summary.trim().is_empty()
            && (!self.key_points.is_empty()
                || !self.decisions.is_empty()
                || !self.action_items.is_empty()
                || !self.open_questions.is_empty())
    }
    /// Post-hoc guard against the model naming an owner that is not a real
    /// speaker (see `OwnerContext::is_valid`): normalizes every action item's
    /// `owner` in place, dropping (setting to `None`) anything that is not
    /// one of the transcript's own speaker labels ("You"/"Speaker N") or a
    /// person's name that literally appears in the transcript text. Applied
    /// regardless of what the prompt asked for, so a model that ignores the
    /// instruction (e.g. writes "Remote" or "Participants") can never surface
    /// a non-person owner in a saved summary.
    pub fn sanitize_owners(&mut self, context: &OwnerContext<'_>) {
        for item in &mut self.action_items {
            item.owner = context.normalize(item.owner.as_deref());
        }
    }
}

/// Generic group/device words that are never a valid action-item owner even
/// though they may appear verbatim in the transcript (e.g. as a speaker
/// label or a spoken word) -- see `SummaryOutput::sanitize_owners`. Compared
/// case-insensitively against the *entire* trimmed owner string, not as a
/// substring, so a real name that happens to contain one of these words
/// (unlikely, but not impossible) is not penalized.
const DISALLOWED_OWNER_LABELS: &[&str] = &[
    "remote",
    "participants",
    "participant",
    "everyone",
    "the team",
    "team",
    "attendees",
    "unknown",
    "unknown speaker",
    "unidentified",
    "unidentified speaker",
];

/// Everything `SummaryOutput::sanitize_owners` needs to decide whether a
/// model-proposed `owner` string is trustworthy: the local user's display
/// name (so "You" and their chosen name both count as the same valid
/// label), the meeting's own audio source/device names (never valid owners,
/// even if the model echoes one back), and the full rendered transcript text
/// a literal person's name must appear in.
pub struct OwnerContext<'a> {
    local_speaker_name: &'a str,
    device_names_lower: Vec<String>,
    transcript_lower: String,
}

impl<'a> OwnerContext<'a> {
    pub fn new(
        local_speaker_name: &'a str,
        device_names: &[String],
        transcript_text: &str,
    ) -> Self {
        Self {
            local_speaker_name,
            device_names_lower: device_names.iter().map(|n| n.to_lowercase()).collect(),
            transcript_lower: transcript_text.to_lowercase(),
        }
    }
    /// Whether `owner` (as returned by the model, pre-trim) is an acceptable
    /// action-item owner: an exact "You"/local-name/"Speaker N" label, or a
    /// name that is literally present in the transcript -- and, either way,
    /// never a disallowed group word or one of this recording's own device
    /// names.
    pub fn is_valid(&self, owner: &str) -> bool {
        let trimmed = owner.trim();
        if trimmed.is_empty() {
            return false;
        }
        let lower = trimmed.to_lowercase();
        if DISALLOWED_OWNER_LABELS.contains(&lower.as_str()) {
            return false;
        }
        if self.device_names_lower.contains(&lower) {
            return false;
        }
        if lower == "you" || lower == self.local_speaker_name.trim().to_lowercase() {
            return true;
        }
        if let Some(rest) = lower.strip_prefix("speaker") {
            let rest = rest.trim();
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
        self.transcript_lower.contains(&lower)
    }
    /// Normalizes a raw model `owner` value: `None`/empty/invalid all collapse
    /// to `None` (never a person), otherwise the trimmed owner is kept as-is.
    pub fn normalize(&self, owner: Option<&str>) -> Option<String> {
        let trimmed = owner.map(str::trim).filter(|s| !s.is_empty())?;
        self.is_valid(trimmed).then(|| trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(local: &'a str, transcript: &str) -> OwnerContext<'a> {
        OwnerContext::new(
            local,
            &["Zoom Audio".into(), "BlackHole 2ch".into()],
            transcript,
        )
    }

    #[test]
    fn remote_is_never_a_valid_owner() {
        let c = ctx("You", "Speaker 1: Confirm the details of the spreadsheet.");
        assert_eq!(c.normalize(Some("Remote")), None);
        assert_eq!(c.normalize(Some("remote")), None);
    }

    #[test]
    fn generic_group_words_are_rejected() {
        let c = ctx("You", "some transcript text");
        for word in ["Participants", "Everyone", "The team", "Unknown"] {
            assert_eq!(
                c.normalize(Some(word)),
                None,
                "expected {word} to be rejected"
            );
        }
    }

    #[test]
    fn device_names_are_rejected_even_if_spoken() {
        let c = ctx("You", "Someone said Zoom Audio out loud.");
        assert_eq!(c.normalize(Some("Zoom Audio")), None);
    }

    #[test]
    fn exact_labels_are_valid_without_appearing_in_the_transcript_text() {
        let c = ctx("Ben", "no labels mentioned here");
        assert_eq!(c.normalize(Some("You")), Some("You".into()));
        assert_eq!(c.normalize(Some("Ben")), Some("Ben".into()));
        assert_eq!(c.normalize(Some("Speaker 2")), Some("Speaker 2".into()));
    }

    #[test]
    fn a_name_present_in_the_transcript_is_kept() {
        let c = ctx("You", "Speaker 1: I'll ask Priya to send the deck.");
        assert_eq!(c.normalize(Some("Priya")), Some("Priya".into()));
    }

    #[test]
    fn a_name_absent_from_the_transcript_is_dropped() {
        let c = ctx("You", "Speaker 1: I'll send the deck.");
        assert_eq!(c.normalize(Some("Priya")), None);
    }

    #[test]
    fn none_and_empty_owner_stay_none() {
        let c = ctx("You", "text");
        assert_eq!(c.normalize(None), None);
        assert_eq!(c.normalize(Some("")), None);
        assert_eq!(c.normalize(Some("   ")), None);
    }

    #[test]
    fn sanitize_owners_rewrites_every_action_item_in_place() {
        let mut output = SummaryOutput {
            title: "t".into(),
            summary: "s".into(),
            action_items: vec![
                ActionItem {
                    task: "a".into(),
                    owner: Some("Remote".into()),
                    due: None,
                },
                ActionItem {
                    task: "b".into(),
                    owner: Some("Speaker 1".into()),
                    due: None,
                },
                ActionItem {
                    task: "c".into(),
                    owner: Some("Priya".into()),
                    due: None,
                },
            ],
            ..Default::default()
        };
        let context = ctx("You", "Speaker 1: I'll ask Priya to help.");
        output.sanitize_owners(&context);
        assert_eq!(output.action_items[0].owner, None);
        assert_eq!(output.action_items[1].owner, Some("Speaker 1".into()));
        assert_eq!(output.action_items[2].owner, Some("Priya".into()));
    }
}
