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
}
