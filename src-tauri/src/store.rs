use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    pub auto_record: bool,
    pub start_debounce_seconds: u16,
    pub end_grace_seconds: u16,
    pub whisper_model: WhisperModel,
    #[serde(default = "enabled_by_default")]
    pub diarization_enabled: bool,
    #[serde(default)]
    pub audio_sources: Vec<AudioSourceSelection>,
    /// "auto" (per-track detection, once per track) or a lowercase ISO 639-1 code
    /// such as "en". Missing/legacy payloads default to "auto".
    #[serde(default = "default_language")]
    pub transcription_language: String,
    /// Acoustic echo cancellation (see `echo_cancel.rs`) for recordings that
    /// have both an input (microphone) and an output (loopback) track --
    /// the common case where the user has speakers rather than headphones,
    /// so remote participants' audio leaks into the mic and is otherwise
    /// transcribed twice. A no-op for recordings missing either track
    /// direction, so this only needs one setting rather than a per-track
    /// toggle. Missing/legacy payloads default to enabled.
    #[serde(default = "enabled_by_default")]
    pub echo_cancellation: bool,
    /// Text-level duplicate cleanup (see `echo_dedup.rs`): after
    /// transcription, drops a microphone-track segment that closely matches
    /// an overlapping loopback-track segment. A safety net independent of
    /// `echo_cancellation` -- it also helps when AEC is off, or was skipped
    /// for a track -- so it is its own setting rather than folded into that
    /// one. Missing/legacy payloads default to enabled.
    #[serde(default = "enabled_by_default")]
    pub dedupe_echo_segments: bool,
    /// Local meeting summaries via a local Ollama server (see `summary.rs`).
    /// Missing/legacy payloads default to enabled; it is harmless when Ollama is
    /// absent -- jobs simply wait with a clear message. Missing/legacy payloads
    /// default to enabled.
    #[serde(default = "enabled_by_default")]
    pub summary_enabled: bool,
    /// Loopback-only (see `ollama_client::validate_loopback_url`) so summaries
    /// can never leave this device, matching the app's "no cloud requests"
    /// guarantee. Missing/legacy payloads default to the standard local port.
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,
    /// An Ollama model tag, e.g. "qwen3:8b". Missing/legacy payloads default to
    /// the recommended local model.
    #[serde(default = "default_summary_model")]
    pub summary_model: String,
    /// Lossless FLAC compression of a finished recording's WAV tracks once no
    /// pipeline stage still needs the raw audio (see `audio_compress.rs`).
    /// Missing/legacy payloads default to enabled -- it only ever runs after
    /// a verified bit-exact round trip, so it is safe to default on.
    #[serde(default = "enabled_by_default")]
    pub compress_audio: bool,
    /// Presentation label for the local user's own (microphone/input-track)
    /// speech in transcript Markdown and the summary prompt. Never affects
    /// `transcript.json`'s `source_name`/`source_id` fields, which remain the
    /// raw device labels for backward compatibility -- this is applied only
    /// where transcripts and summaries are rendered for a human. Missing/legacy
    /// payloads default to "You". Trimmed and length-limited on save.
    #[serde(default = "default_local_speaker_name")]
    pub local_speaker_name: String,
    /// Where meeting folders (audio tracks, transcript/summary files) are
    /// stored. `None` means the default location inside the app data
    /// directory. Databases and imported models always stay in the app data
    /// directory regardless of this setting -- see `docs/recording.md`.
    /// Only set through the dedicated `set_recordings_folder`/
    /// `reset_recordings_folder` commands, which perform the real
    /// existence/writability validation; `save_settings` accepts this field
    /// unchanged (like `audio_sources`) so it cannot be used as a bypass.
    #[serde(default)]
    pub recordings_folder: Option<String>,
    /// Folders `recordings_folder` previously pointed to, most recent first,
    /// kept so recordings that were never explicitly moved (see
    /// `move_recordings_to_configured_folder`) can still be found after the
    /// folder changes again. Capped and de-duplicated by
    /// `record_previous_library_root`. Not directly user-editable.
    #[serde(default)]
    pub legacy_library_roots: Vec<String>,
}
/// Longest accepted `legacy_library_roots` list -- generous for anyone who
/// changes folders occasionally, bounded so the settings payload cannot grow
/// without limit.
pub const MAX_LEGACY_LIBRARY_ROOTS: usize = 10;
fn enabled_by_default() -> bool {
    true
}
fn default_language() -> String {
    "auto".into()
}
fn default_local_speaker_name() -> String {
    "You".into()
}
/// Longest accepted `local_speaker_name`, generous for a person's display name
/// while keeping Markdown/JSON output and the summary prompt bounded.
pub const MAX_LOCAL_SPEAKER_NAME_LEN: usize = 64;
fn default_ollama_url() -> String {
    "http://127.0.0.1:11434".into()
}
fn default_summary_model() -> String {
    "qwen3:8b".into()
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioSourceSelection {
    pub id: String,
    pub name: String,
    pub direction: AudioSourceDirection,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioSourceDirection {
    Input,
    Output,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WhisperModel {
    Tiny,
    Base,
    Small,
    Medium,
    Large,
    /// large-v3-turbo: a pruned 4-decoder-layer variant of large-v3. Recommended
    /// (quantized, e.g. ggml-large-v3-turbo-q5_0.bin) over `base` for accuracy.
    Turbo,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_record: true,
            start_debounce_seconds: 10,
            end_grace_seconds: 30,
            // New installs default to `base`, not the recommended `turbo`: `base` is
            // small and fast to import, and we do not want a first run to silently
            // block on a ~550 MB model the user has not chosen to import yet.
            whisper_model: WhisperModel::Base,
            diarization_enabled: true,
            audio_sources: vec![],
            transcription_language: default_language(),
            echo_cancellation: enabled_by_default(),
            dedupe_echo_segments: enabled_by_default(),
            summary_enabled: enabled_by_default(),
            ollama_url: default_ollama_url(),
            summary_model: default_summary_model(),
            compress_audio: enabled_by_default(),
            local_speaker_name: default_local_speaker_name(),
            recordings_folder: None,
            legacy_library_roots: Vec::new(),
        }
    }
}
impl Settings {
    /// Trims the local speaker name and enforces its length limit. Called by
    /// callers that accept user-edited settings before `validate`/`save`, so a
    /// stray leading/trailing space from a text field does not stick around.
    pub fn normalize(mut self) -> Self {
        self.local_speaker_name = self.local_speaker_name.trim().to_string();
        self
    }
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.start_debounce_seconds > 60 {
            return Err(StoreError::Validation(
                "Start debounce must be between 0 and 60 seconds.".into(),
            ));
        }
        if !(5..=300).contains(&self.end_grace_seconds) {
            return Err(StoreError::Validation(
                "End grace must be between 5 and 300 seconds.".into(),
            ));
        }
        if !is_valid_language(&self.transcription_language) {
            return Err(StoreError::Validation(
                "Transcription language must be \"auto\" or a lowercase ISO 639-1 code.".into(),
            ));
        }
        crate::ollama_client::validate_loopback_url(&self.ollama_url)
            .map_err(StoreError::Validation)?;
        if self.summary_model.trim().is_empty() {
            return Err(StoreError::Validation(
                "Summary model must not be empty.".into(),
            ));
        }
        if self.local_speaker_name.trim().is_empty()
            || self.local_speaker_name.len() > MAX_LOCAL_SPEAKER_NAME_LEN
            || self.local_speaker_name.chars().any(|c| c.is_control())
        {
            return Err(StoreError::Validation(format!(
                "Your name must be 1-{MAX_LOCAL_SPEAKER_NAME_LEN} plain-text characters."
            )));
        }
        // Only a shape check: `recordings_folder` must be an absolute path if
        // set at all. The real existence/writability/symlink/install-dir
        // checks run once, up front, in the dedicated
        // `set_recordings_folder` command -- not on every settings load,
        // since the folder (e.g. a removable drive) may legitimately become
        // unavailable later without that being an error in stored settings.
        if let Some(folder) = &self.recordings_folder {
            if !Path::new(folder).is_absolute() {
                return Err(StoreError::Validation(
                    "Recordings folder must be an absolute path.".into(),
                ));
            }
        }
        if self.legacy_library_roots.len() > MAX_LEGACY_LIBRARY_ROOTS {
            return Err(StoreError::Validation(
                "Too many previous recordings folders are recorded.".into(),
            ));
        }
        Ok(())
    }
}
/// "auto", or 2-3 lowercase ASCII letters (a superset of the languages the UI
/// currently offers, so a future addition does not need a store-level change).
fn is_valid_language(value: &str) -> bool {
    value == "auto"
        || ((2..=3).contains(&value.len())
            && value.bytes().all(|b| b.is_ascii_lowercase()))
}
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("Settings storage failed: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("Settings storage failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid saved settings: {0}")]
    Invalid(String),
    #[error("Invalid settings: {0}")]
    Validation(String),
}
pub struct SettingsStore {
    conn: Connection,
}
pub struct SettingsStorage {
    path: PathBuf,
    store: Option<SettingsStore>,
}
impl SettingsStorage {
    pub fn new(path: PathBuf) -> Self {
        Self { path, store: None }
    }
    fn open_if_needed(&mut self) -> Result<&SettingsStore, StoreError> {
        if self.store.is_none() {
            self.store = Some(SettingsStore::open(&self.path)?);
        }
        Ok(self.store.as_ref().expect("store was initialized"))
    }
    pub fn load(&mut self) -> Result<Settings, StoreError> {
        self.open_if_needed()?.get()
    }
    pub fn save(&mut self, settings: Settings) -> Result<Settings, StoreError> {
        self.open_if_needed()?.get()?;
        self.open_if_needed()?.save(settings)
    }
}
impl SettingsStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        if let Some(p) = path.as_ref().parent() {
            fs::create_dir_all(p)?
        };
        let conn = Connection::open(path)?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL);")?;
        let store = Self { conn };
        if store.count()? == 0 {
            store.save(Settings::default())?;
        }
        Ok(store)
    }
    fn count(&self) -> Result<u8, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM settings", [], |r| r.get(0))?)
    }
    pub fn get(&self) -> Result<Settings, StoreError> {
        let raw: String =
            self.conn
                .query_row("SELECT payload FROM settings WHERE id=1", [], |r| r.get(0))?;
        let s: Settings =
            serde_json::from_str(&raw).map_err(|e| StoreError::Invalid(e.to_string()))?;
        s.validate()?;
        Ok(s)
    }
    pub fn save(&self, s: Settings) -> Result<Settings, StoreError> {
        s.validate()?;
        let json = serde_json::to_string(&s).map_err(|e| StoreError::Invalid(e.to_string()))?;
        self.conn.execute("INSERT INTO settings(id,payload) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",params![json])?;
        Ok(s)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn path(n: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("meeting-recorder-{n}-{}", std::process::id()));
        let _ = fs::remove_file(&p);
        p
    }
    #[test]
    fn defaults_persist() {
        let p = path("default.db");
        let s = SettingsStore::open(&p).unwrap();
        assert!(s.get().unwrap().auto_record);
        drop(s);
        assert!(SettingsStore::open(p).unwrap().get().unwrap().auto_record)
    }
    #[test]
    fn modified_settings_persist_after_reopen() {
        let p = path("modified.db");
        let store = SettingsStore::open(&p).unwrap();
        let expected = Settings {
            auto_record: false,
            start_debounce_seconds: 22,
            end_grace_seconds: 95,
            whisper_model: WhisperModel::Small,
            diarization_enabled: false,
            audio_sources: vec![],
            transcription_language: "fr".into(),
            echo_cancellation: false,
            dedupe_echo_segments: false,
            summary_enabled: false,
            ollama_url: "http://localhost:11434".into(),
            summary_model: "qwen2.5:7b-instruct".into(),
            compress_audio: false,
            local_speaker_name: "Ben".into(),
            recordings_folder: Some(
                std::env::temp_dir().join("recordings").display().to_string(),
            ),
            legacy_library_roots: vec![std::env::temp_dir()
                .join("old-recordings")
                .display()
                .to_string()],
        };
        store.save(expected.clone()).unwrap();
        drop(store);
        assert_eq!(SettingsStore::open(p).unwrap().get().unwrap(), expected);
    }
    #[test]
    fn invalid_update_does_not_replace_saved_value() {
        let p = path("invalid.db");
        let s = SettingsStore::open(p).unwrap();
        let good = s.get().unwrap();
        let bad = Settings {
            start_debounce_seconds: 61,
            ..good.clone()
        };
        assert!(s.save(bad).is_err());
        assert_eq!(s.get().unwrap(), good)
    }
    #[test]
    fn invalid_end_grace_does_not_replace_saved_value() {
        let p = path("grace.db");
        let store = SettingsStore::open(p).unwrap();
        let good = store.get().unwrap();
        assert!(store
            .save(Settings {
                end_grace_seconds: 4,
                ..good.clone()
            })
            .is_err());
        assert!(store
            .save(Settings {
                end_grace_seconds: 301,
                ..good.clone()
            })
            .is_err());
        assert_eq!(store.get().unwrap(), good);
    }
    #[test]
    fn corrupt_payload_is_reported() {
        let p = path("corrupt.db");
        let s = SettingsStore::open(p).unwrap();
        s.conn
            .execute("UPDATE settings SET payload='not-json' WHERE id=1", [])
            .unwrap();
        assert!(matches!(s.get(), Err(StoreError::Invalid(_))))
    }
    #[test]
    fn invalid_database_bytes_remain_unchanged() {
        let p = path("bad-bytes.db");
        let bytes = b"not a sqlite database";
        fs::write(&p, bytes).unwrap();
        assert!(SettingsStorage::new(p.clone()).load().is_err());
        assert_eq!(fs::read(p).unwrap(), bytes);
    }
    #[test]
    fn failed_path_retries_after_directory_is_removed() {
        let p = path("retry.db");
        fs::create_dir(&p).unwrap();
        let mut storage = SettingsStorage::new(p.clone());
        assert!(storage.load().is_err());
        fs::remove_dir(&p).unwrap();
        assert!(storage.load().is_ok());
    }
    #[test]
    fn legacy_json_migrates_with_empty_audio_sources() {
        let parsed: Settings = serde_json::from_str(r#"{"auto_record":true,"start_debounce_seconds":10,"end_grace_seconds":30,"whisper_model":"base"}"#).unwrap();
        assert!(parsed.audio_sources.is_empty());
        assert!(parsed.diarization_enabled);
        assert_eq!(parsed.transcription_language, "auto");
    }
    #[test]
    fn turbo_model_key_round_trips() {
        let parsed: Settings = serde_json::from_str(r#"{"auto_record":true,"start_debounce_seconds":10,"end_grace_seconds":30,"whisper_model":"turbo","transcription_language":"fr"}"#).unwrap();
        assert_eq!(parsed.whisper_model, WhisperModel::Turbo);
        assert_eq!(parsed.transcription_language, "fr");
    }
    #[test]
    fn legacy_json_migrates_with_summary_defaults() {
        let parsed: Settings = serde_json::from_str(r#"{"auto_record":true,"start_debounce_seconds":10,"end_grace_seconds":30,"whisper_model":"base"}"#).unwrap();
        assert!(parsed.summary_enabled);
        assert_eq!(parsed.ollama_url, "http://127.0.0.1:11434");
        assert_eq!(parsed.summary_model, "qwen3:8b");
    }
    #[test]
    fn non_loopback_ollama_url_does_not_replace_saved_value() {
        let p = path("ollama-url.db");
        let store = SettingsStore::open(p).unwrap();
        let good = store.get().unwrap();
        assert!(store
            .save(Settings {
                ollama_url: "http://example.com:11434".into(),
                ..good.clone()
            })
            .is_err());
        assert!(store
            .save(Settings {
                ollama_url: "http://192.168.1.5:11434".into(),
                ..good.clone()
            })
            .is_err());
        assert!(store
            .save(Settings {
                ollama_url: "http://localhost:11434".into(),
                ..good.clone()
            })
            .is_ok());
        assert_eq!(store.get().unwrap().ollama_url, "http://localhost:11434");
    }
    #[test]
    fn empty_summary_model_does_not_replace_saved_value() {
        let p = path("summary-model.db");
        let store = SettingsStore::open(p).unwrap();
        let good = store.get().unwrap();
        assert!(store
            .save(Settings {
                summary_model: "".into(),
                ..good.clone()
            })
            .is_err());
        assert_eq!(store.get().unwrap(), good);
    }
    #[test]
    fn invalid_language_does_not_replace_saved_value() {
        let p = path("language.db");
        let store = SettingsStore::open(p).unwrap();
        let good = store.get().unwrap();
        assert!(store
            .save(Settings {
                transcription_language: "English".into(),
                ..good.clone()
            })
            .is_err());
        assert!(store
            .save(Settings {
                transcription_language: "".into(),
                ..good.clone()
            })
            .is_err());
        assert_eq!(store.get().unwrap(), good);
    }
    #[test]
    fn legacy_payload_without_local_speaker_name_defaults_to_you() {
        // Simulates a settings row saved before this field existed.
        let p = path("legacy-speaker-name.db");
        let store = SettingsStore::open(&p).unwrap();
        store
            .conn
            .execute(
                "UPDATE settings SET payload = '{\"auto_record\":true,\"start_debounce_seconds\":10,\"end_grace_seconds\":30,\"whisper_model\":\"base\",\"diarization_enabled\":true,\"audio_sources\":[],\"transcription_language\":\"auto\",\"echo_cancellation\":true,\"dedupe_echo_segments\":true,\"summary_enabled\":true,\"ollama_url\":\"http://127.0.0.1:11434\",\"summary_model\":\"qwen3:8b\",\"compress_audio\":true}' WHERE id=1",
                [],
            )
            .unwrap();
        assert_eq!(store.get().unwrap().local_speaker_name, "You");
    }
    #[test]
    fn blank_or_oversized_local_speaker_name_does_not_replace_saved_value() {
        let p = path("speaker-name.db");
        let store = SettingsStore::open(p).unwrap();
        let good = store.get().unwrap();
        assert!(store
            .save(Settings {
                local_speaker_name: "   ".into(),
                ..good.clone()
            })
            .is_err());
        assert!(store
            .save(Settings {
                local_speaker_name: "x".repeat(MAX_LOCAL_SPEAKER_NAME_LEN + 1),
                ..good.clone()
            })
            .is_err());
        assert_eq!(store.get().unwrap(), good);
    }
    #[test]
    fn normalize_trims_local_speaker_name() {
        let settings = Settings {
            local_speaker_name: "  Ben  ".into(),
            ..Settings::default()
        }
        .normalize();
        assert_eq!(settings.local_speaker_name, "Ben");
    }
    #[test]
    fn relative_recordings_folder_does_not_replace_saved_value() {
        let p = path("recordings-folder.db");
        let store = SettingsStore::open(p).unwrap();
        let good = store.get().unwrap();
        assert!(store
            .save(Settings {
                recordings_folder: Some("relative/folder".into()),
                ..good.clone()
            })
            .is_err());
        assert_eq!(store.get().unwrap(), good);
        let absolute = std::env::temp_dir().join("recordings").display().to_string();
        assert!(store
            .save(Settings {
                recordings_folder: Some(absolute.clone()),
                ..good.clone()
            })
            .is_ok());
        assert_eq!(
            store.get().unwrap().recordings_folder,
            Some(absolute)
        );
    }
    #[test]
    fn too_many_legacy_library_roots_does_not_replace_saved_value() {
        let p = path("legacy-roots.db");
        let store = SettingsStore::open(p).unwrap();
        let good = store.get().unwrap();
        let too_many = (0..=MAX_LEGACY_LIBRARY_ROOTS)
            .map(|i| format!("root-{i}"))
            .collect();
        assert!(store
            .save(Settings {
                legacy_library_roots: too_many,
                ..good.clone()
            })
            .is_err());
        assert_eq!(store.get().unwrap(), good);
        let exactly_max = (0..MAX_LEGACY_LIBRARY_ROOTS)
            .map(|i| format!("root-{i}"))
            .collect();
        assert!(store
            .save(Settings {
                legacy_library_roots: exactly_max,
                ..good
            })
            .is_ok());
    }
    #[test]
    fn recordings_folder_defaults_to_none_and_is_none_by_default() {
        assert_eq!(Settings::default().recordings_folder, None);
        assert!(Settings::default().legacy_library_roots.is_empty());
        let parsed: Settings = serde_json::from_str(
            r#"{"auto_record":true,"start_debounce_seconds":10,"end_grace_seconds":30,"whisper_model":"base"}"#,
        )
        .unwrap();
        assert_eq!(parsed.recordings_folder, None);
        assert!(parsed.legacy_library_roots.is_empty());
    }
}
