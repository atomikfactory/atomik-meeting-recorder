//! Voice profiles: "remember this voice for future meetings".
//!
//! A profile is a named speaker-embedding centroid, tied to the exact
//! embedding model that produced it (`model_hash`) since embeddings from
//! different models are not comparable. Stored in the same app-state SQLite
//! database family as `speaker_names.rs`, for the same reasons (crash-safe
//! atomic writes, queryable across recordings, independent of the
//! recordings folder).
//!
//! This module is deliberately split into:
//! - pure math (`cosine_similarity`, `centroid`, `weighted_running_mean`,
//!   `match_clusters`) with no I/O, so it is exhaustively unit tested;
//! - an `EmbeddingExtractor` trait so the same pure logic can be driven by a
//!   fake extractor in tests and by the real sherpa-onnx model in
//!   production/the ignored real-data verification test.
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const MAX_NAME_LEN: usize = 60;
/// Turns shorter than this are too short for a reliable embedding and are
/// skipped when building a centroid (docs/diarization.md: "skip turns < ~1s").
pub const MIN_TURN_SECONDS: f64 = 1.0;
/// A speaker must have at least this much usable (>= `MIN_TURN_SECONDS`)
/// speech in the meeting before a voice profile is created or matched
/// against, else the resulting embedding is too noisy to trust.
pub const MIN_TOTAL_SPEECH_SECONDS: f64 = 5.0;
/// Cosine similarity a best match must reach to be accepted. See
/// docs/diarization.md "Named speakers and voice profiles" for the measured
/// basis of this default; tune with real data via the ignored
/// `diarization_real_tests.rs` harness.
pub const DEFAULT_MATCH_THRESHOLD: f32 = 0.6;
/// The best match must also beat the second-best candidate profile by at
/// least this much cosine similarity, so two similar-sounding voices are not
/// silently confused.
pub const DEFAULT_MATCH_MARGIN: f32 = 0.08;

/// Real embedding extractor wrapping sherpa-onnx's standalone
/// `SpeakerEmbeddingExtractor` -- the same model file diarization uses (see
/// `diarization_engine.rs`'s `create_diarizer`), fed the whole segment's
/// waveform in one shot via a fresh `OnlineStream` per call. Not part of the
/// diarization worker's own clustering: this extracts one embedding per
/// caller-chosen span of audio (a diarized turn), for building/matching
/// voice profiles, which is a separate concern from segmentation+clustering.
pub struct SherpaEmbeddingExtractor {
    extractor: sherpa_onnx::SpeakerEmbeddingExtractor,
    model_hash: String,
}
impl SherpaEmbeddingExtractor {
    pub fn create(model_path: &Path, model_hash: &str) -> Result<Self, String> {
        let config = sherpa_onnx::SpeakerEmbeddingExtractorConfig {
            model: Some(model_path.to_string_lossy().into_owned()),
            num_threads: 2,
            debug: false,
            provider: Some("cpu".into()),
        };
        let extractor = sherpa_onnx::SpeakerEmbeddingExtractor::create(&config)
            .ok_or_else(|| "Could not initialize the local voice embedding model.".to_string())?;
        Ok(Self {
            extractor,
            model_hash: model_hash.to_string(),
        })
    }
}
impl EmbeddingExtractor for SherpaEmbeddingExtractor {
    fn model_hash(&self) -> String {
        self.model_hash.clone()
    }
    fn extract(&self, samples: &[f32]) -> Result<Vec<f32>, String> {
        let stream = self
            .extractor
            .create_stream()
            .ok_or("Could not create a voice embedding stream.")?;
        stream.accept_waveform(16_000, samples);
        stream.input_finished();
        if !self.extractor.is_ready(&stream) {
            return Err("Not enough audio to compute a voice embedding.".into());
        }
        self.extractor
            .compute(&stream)
            .ok_or_else(|| "Could not compute a voice embedding.".to_string())
    }
}

pub trait EmbeddingExtractor: Send + Sync {
    /// Identifies the exact model producing embeddings from this extractor;
    /// profiles store the hash of the model that created them and are never
    /// matched against a different one (see `VoiceProfile::model_hash`).
    fn model_hash(&self) -> String;
    /// Computes one embedding vector from 16 kHz mono PCM samples. `samples`
    /// covers exactly one speaker's contiguous or concatenated turns; error
    /// only on a genuine extraction failure, never on "too little audio"
    /// (callers filter that before calling, per `MIN_TURN_SECONDS`).
    fn extract(&self, samples: &[f32]) -> Result<Vec<f32>, String>;
}

/// One named speaker whose embedding centroid is remembered across meetings.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceProfile {
    pub id: String,
    pub name: String,
    pub embedding: Vec<f32>,
    pub model_hash: String,
    pub sample_seconds: f64,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct VoiceProfileStore {
    conn: Connection,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn validate_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty()
        || trimmed.chars().count() > MAX_NAME_LEN
        || trimmed.chars().any(|c| c.is_control())
    {
        return Err(format!(
            "Voice profile names must be 1-{MAX_NAME_LEN} plain-text characters."
        ));
    }
    Ok(trimmed)
}

fn encode(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}
fn decode(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

impl VoiceProfileStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err("voice profiles database must not be a symlink".into());
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA busy_timeout=5000;\
             CREATE TABLE IF NOT EXISTS voice_profiles(\
                id TEXT PRIMARY KEY,\
                name TEXT NOT NULL,\
                embedding BLOB NOT NULL,\
                model_hash TEXT NOT NULL,\
                sample_seconds REAL NOT NULL,\
                created_at INTEGER NOT NULL,\
                updated_at INTEGER NOT NULL\
             );",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self { conn })
    }

    pub fn create(
        &self,
        name: &str,
        embedding: &[f32],
        model_hash: &str,
        sample_seconds: f64,
    ) -> Result<VoiceProfile, String> {
        let name = validate_name(name)?;
        if embedding.is_empty() || !embedding.iter().all(|v| v.is_finite()) {
            return Err("Invalid voice embedding.".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let t = now();
        self.conn
            .execute(
                "INSERT INTO voice_profiles(id,name,embedding,model_hash,sample_seconds,created_at,updated_at) \
                 VALUES(?1,?2,?3,?4,?5,?6,?6)",
                params![id, name, encode(embedding), model_hash, sample_seconds, t],
            )
            .map_err(|e| e.to_string())?;
        Ok(VoiceProfile {
            id,
            name,
            embedding: embedding.to_vec(),
            model_hash: model_hash.to_string(),
            sample_seconds,
            created_at: t,
            updated_at: t,
        })
    }

    /// Updates an existing profile with a new observation using a weighted
    /// running mean (weight = each observation's total usable speech
    /// seconds), per docs/diarization.md: "Naming the same person again in
    /// another meeting updates the profile (running weighted mean)."
    /// Refuses silently-wrong updates: the new embedding must come from the
    /// same model that created the profile (see `model_hash`).
    pub fn update_running_mean(
        &self,
        id: &str,
        new_embedding: &[f32],
        new_model_hash: &str,
        new_seconds: f64,
    ) -> Result<VoiceProfile, String> {
        let existing = self.get(id)?.ok_or("Unknown voice profile.")?;
        if existing.model_hash != new_model_hash {
            return Err(
                "This voice profile was built with a different embedding model and is incompatible."
                    .into(),
            );
        }
        let merged = weighted_running_mean(
            &existing.embedding,
            existing.sample_seconds,
            new_embedding,
            new_seconds,
        );
        let total_seconds = existing.sample_seconds + new_seconds;
        let t = now();
        self.conn
            .execute(
                "UPDATE voice_profiles SET embedding=?2, sample_seconds=?3, updated_at=?4 WHERE id=?1",
                params![id, encode(&merged), total_seconds, t],
            )
            .map_err(|e| e.to_string())?;
        Ok(VoiceProfile {
            embedding: merged,
            sample_seconds: total_seconds,
            updated_at: t,
            ..existing
        })
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        let name = validate_name(name)?;
        let n = self
            .conn
            .execute(
                "UPDATE voice_profiles SET name=?2, updated_at=?3 WHERE id=?1",
                params![id, name, now()],
            )
            .map_err(|e| e.to_string())?;
        if n == 1 {
            Ok(())
        } else {
            Err("Unknown voice profile.".into())
        }
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM voice_profiles WHERE id=?1", params![id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn delete_all(&self) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM voice_profiles", [])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<VoiceProfile>, String> {
        self.conn
            .query_row(
                "SELECT id,name,embedding,model_hash,sample_seconds,created_at,updated_at FROM voice_profiles WHERE id=?1",
                params![id],
                Self::row,
            )
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn list(&self) -> Result<Vec<VoiceProfile>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id,name,embedding,model_hash,sample_seconds,created_at,updated_at FROM voice_profiles ORDER BY name",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], Self::row)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(rows)
    }

    fn row(r: &rusqlite::Row) -> rusqlite::Result<VoiceProfile> {
        let embedding: Vec<u8> = r.get(2)?;
        Ok(VoiceProfile {
            id: r.get(0)?,
            name: r.get(1)?,
            embedding: decode(&embedding),
            model_hash: r.get(3)?,
            sample_seconds: r.get(4)?,
            created_at: r.get(5)?,
            updated_at: r.get(6)?,
        })
    }
}

pub fn store_path(root: &Path) -> PathBuf {
    root.join("voice_profiles.sqlite3")
}

// ---------------------------------------------------------------------
// Pure math
// ---------------------------------------------------------------------

fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Weighted mean of `vectors` (each with a non-negative `weights[i]`),
/// L2-normalized. Returns `None` if `vectors` is empty, weights don't match
/// its length, or every weight is zero.
pub fn centroid(vectors: &[Vec<f32>], weights: &[f64]) -> Option<Vec<f32>> {
    if vectors.is_empty() || vectors.len() != weights.len() {
        return None;
    }
    let dim = vectors[0].len();
    if dim == 0 || vectors.iter().any(|v| v.len() != dim) {
        return None;
    }
    let total_weight: f64 = weights.iter().sum();
    if total_weight <= 0.0 {
        return None;
    }
    let mut mean = vec![0f32; dim];
    for (vector, weight) in vectors.iter().zip(weights) {
        let w = (*weight / total_weight) as f32;
        for (m, x) in mean.iter_mut().zip(vector) {
            *m += x * w;
        }
    }
    l2_normalize(&mut mean);
    Some(mean)
}

/// Merges an existing profile embedding (weight = its accumulated
/// `sample_seconds`) with a new observation (weight = its own seconds) into
/// one weighted-mean, L2-normalized vector.
pub fn weighted_running_mean(
    old: &[f32],
    old_weight: f64,
    new: &[f32],
    new_weight: f64,
) -> Vec<f32> {
    centroid(&[old.to_vec(), new.to_vec()], &[old_weight, new_weight])
        .unwrap_or_else(|| old.to_vec())
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        (dot / (na * nb)).clamp(-1.0, 1.0)
    }
}

/// One detected speaker cluster in a meeting, identified by its diarization
/// speaker number, with its computed centroid embedding.
#[derive(Clone, Debug)]
pub struct ClusterCentroid {
    pub speaker: u32,
    pub embedding: Vec<f32>,
}

/// A candidate profile match for one cluster, kept for reporting (e.g. the
/// real-data verification harness's "best, second-best" table).
#[derive(Clone, Debug, PartialEq)]
pub struct MatchCandidate {
    pub speaker: u32,
    pub profile_id: String,
    pub profile_name: String,
    pub best_score: f32,
    pub second_best_score: f32,
}

/// Greedy one-to-one matching of detected speaker clusters against known
/// voice profiles: each cluster's best-scoring profile is accepted only if
/// `best_score >= threshold` AND `best_score - second_best_score >= margin`;
/// a profile already assigned to a (higher-scoring) cluster is never assigned
/// again. Unmatched clusters are simply absent from the result -- callers
/// leave them "Speaker N". `profiles` should already be filtered to the
/// current embedding model's hash by the caller (a profile from a different
/// model is never a valid candidate at all, not merely a low-scoring one).
pub fn match_clusters(
    clusters: &[ClusterCentroid],
    profiles: &[VoiceProfile],
    threshold: f32,
    margin: f32,
) -> Vec<MatchCandidate> {
    if profiles.is_empty() {
        return Vec::new();
    }
    let mut used_profiles: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut used_clusters: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut accepted = Vec::new();
    // Re-derive per remaining cluster/profile pool until no more accepted
    // matches can be found -- a simple, small-N-friendly greedy fixed point
    // rather than a full Hungarian assignment (meetings have a handful of
    // speakers and a handful of profiles). Each round scores every
    // still-unassigned cluster against every still-unassigned profile (via
    // `candidate_scores`) and accepts only the single strongest qualifying
    // match, so a profile that is the top choice for two clusters goes to
    // whichever one it matches best; the loser is rescored next round
    // against whatever profiles remain.
    loop {
        let remaining_clusters: Vec<ClusterCentroid> = clusters
            .iter()
            .filter(|c| !used_clusters.contains(&c.speaker))
            .cloned()
            .collect();
        let remaining_profiles: Vec<VoiceProfile> = profiles
            .iter()
            .filter(|p| !used_profiles.contains(&p.id))
            .cloned()
            .collect();
        let best = candidate_scores(&remaining_clusters, &remaining_profiles)
            .into_iter()
            .filter(|c| c.best_score >= threshold && c.best_score - c.second_best_score >= margin)
            .max_by(|a, b| {
                a.best_score
                    .partial_cmp(&b.best_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        match best {
            Some(candidate) => {
                used_clusters.insert(candidate.speaker);
                used_profiles.insert(candidate.profile_id.clone());
                accepted.push(candidate);
            }
            None => break,
        }
    }
    accepted.sort_by_key(|c| c.speaker);
    accepted
}

/// All match candidates considered (including rejected ones), for reporting
/// -- e.g. the real-data verification harness's similarity table. Does not
/// enforce one-to-one assignment; use `match_clusters` for the accepted set.
pub fn candidate_scores(
    clusters: &[ClusterCentroid],
    profiles: &[VoiceProfile],
) -> Vec<MatchCandidate> {
    if profiles.is_empty() {
        return Vec::new();
    }
    clusters
        .iter()
        .filter_map(|cluster| {
            let mut scored: Vec<(f32, &VoiceProfile)> = profiles
                .iter()
                .map(|p| (cosine_similarity(&cluster.embedding, &p.embedding), p))
                .collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let (best_score, best_profile) = *scored.first()?;
            let second_best_score = scored.get(1).map(|(s, _)| *s).unwrap_or(f32::NEG_INFINITY);
            Some(MatchCandidate {
                speaker: cluster.speaker,
                profile_id: best_profile.id.clone(),
                profile_name: best_profile.name.clone(),
                best_score,
                second_best_score,
            })
        })
        .collect()
}

/// One usable (>= `MIN_TURN_SECONDS`) turn's worth of 16 kHz mono samples for
/// a single detected speaker, used to build that speaker's centroid.
pub struct SpeakerSamples {
    pub speaker: u32,
    pub seconds: f64,
    pub samples: Vec<f32>,
}

/// Computes one speaker's embedding centroid from its turns' audio, per
/// docs/diarization.md: skip turns under `MIN_TURN_SECONDS`, require at
/// least `MIN_TOTAL_SPEECH_SECONDS` total usable speech, else explain why.
pub fn speaker_centroid(
    extractor: &dyn EmbeddingExtractor,
    turns: &[SpeakerSamples],
    target_speaker: u32,
) -> Result<(Vec<f32>, f64), String> {
    let usable: Vec<&SpeakerSamples> = turns
        .iter()
        .filter(|t| t.speaker == target_speaker && t.seconds >= MIN_TURN_SECONDS)
        .collect();
    let total_seconds: f64 = usable.iter().map(|t| t.seconds).sum();
    if total_seconds < MIN_TOTAL_SPEECH_SECONDS {
        return Err(format!(
            "Not enough speech to remember this voice ({total_seconds:.1}s of at least {MIN_TOTAL_SPEECH_SECONDS:.0}s needed)."
        ));
    }
    let mut embeddings = Vec::with_capacity(usable.len());
    let mut weights = Vec::with_capacity(usable.len());
    for turn in usable {
        embeddings.push(extractor.extract(&turn.samples)?);
        weights.push(turn.seconds);
    }
    let merged = centroid(&embeddings, &weights).ok_or("Could not compute a voice embedding.")?;
    Ok((merged, total_seconds))
}

/// As `speaker_centroid`, but for every speaker present in `turns` that has
/// enough usable speech; speakers below `MIN_TOTAL_SPEECH_SECONDS` are simply
/// omitted (used for the recognition pass over a whole meeting, where a
/// speaker with too little speech should stay unmatched rather than fail the
/// whole recognition run).
pub fn all_speaker_centroids(
    extractor: &dyn EmbeddingExtractor,
    turns: &[SpeakerSamples],
) -> Vec<ClusterCentroid> {
    let mut speakers: Vec<u32> = turns.iter().map(|t| t.speaker).collect();
    speakers.sort_unstable();
    speakers.dedup();
    speakers
        .into_iter()
        .filter_map(|speaker| {
            speaker_centroid(extractor, turns, speaker)
                .ok()
                .map(|(embedding, _)| ClusterCentroid { speaker, embedding })
        })
        .collect()
}

/// Loads 16 kHz mono samples for each usable (>= `MIN_TURN_SECONDS`)
/// diarization turn, from the recording's own tracks -- one WAV decode per
/// source track, reused across every turn on that track. Errors only on a
/// genuine I/O/track problem; a turn with no matching track is skipped
/// rather than failing the whole batch, since `turns` may in principle
/// reference a track that has since been removed from `recording.tracks`.
pub fn load_speaker_samples(
    recording: &crate::recording::Recording,
    recordings_root: &Path,
    extra_roots: &[PathBuf],
    turns: &[crate::diarization_types::SpeakerTurn],
) -> Result<Vec<SpeakerSamples>, String> {
    use crate::transcription_engine::{load_wav_16k_mono, validated_recording_tracks};
    const MAX_SOURCE_SECONDS: u64 = 30 * 60;
    let tracks = validated_recording_tracks(recording, recordings_root, extra_roots)?;
    let mut by_source: std::collections::HashMap<String, Vec<f32>> =
        std::collections::HashMap::new();
    let mut out = Vec::new();
    for turn in turns {
        let seconds = turn.end_ms.saturating_sub(turn.start_ms) as f64 / 1000.0;
        if seconds < MIN_TURN_SECONDS {
            continue;
        }
        if !by_source.contains_key(&turn.source_id) {
            let Some((_, path)) = tracks.iter().find(|(t, _)| t.source_id == turn.source_id) else {
                continue;
            };
            by_source.insert(
                turn.source_id.clone(),
                load_wav_16k_mono(path, MAX_SOURCE_SECONDS)?,
            );
        }
        let samples = &by_source[&turn.source_id];
        let start = ((turn.start_ms as f64 / 1000.0) * 16_000.0) as usize;
        let end = (((turn.end_ms as f64 / 1000.0) * 16_000.0) as usize).min(samples.len());
        if start >= end {
            continue;
        }
        out.push(SpeakerSamples {
            speaker: turn.speaker,
            seconds,
            samples: samples[start..end].to_vec(),
        });
    }
    Ok(out)
}

/// Shared by the automatic post-diarization recognition pass
/// (`diarization::tick`) and the explicit "Re-identify speakers" command
/// (`reidentify_speakers` in `lib.rs`): matches this recording's detected
/// speaker clusters against saved voice profiles compatible with
/// `extractor`'s model, and assigns a name to any confident, unambiguous
/// match (see `match_clusters`), skipping any label the user has already
/// named themselves (an existing *non*-auto-identified name is never
/// touched). Returns exactly the assignments it applied, as
/// `(speaker, profile_name, best_score)`, for reporting to the caller.
pub fn recognize_speakers(
    root: &Path,
    extra_roots: &[PathBuf],
    recording: &crate::recording::Recording,
    turns: &[crate::diarization_types::SpeakerTurn],
    extractor: &dyn EmbeddingExtractor,
) -> Result<Vec<(u32, String, f32)>, String> {
    let model_hash = extractor.model_hash();
    let turn_samples = load_speaker_samples(recording, root, extra_roots, turns)?;
    let clusters = all_speaker_centroids(extractor, &turn_samples);
    let profile_store = VoiceProfileStore::open(store_path(root))?;
    let profiles: Vec<_> = profile_store
        .list()?
        .into_iter()
        .filter(|p| p.model_hash == model_hash)
        .collect();
    let matches = match_clusters(
        &clusters,
        &profiles,
        DEFAULT_MATCH_THRESHOLD,
        DEFAULT_MATCH_MARGIN,
    );
    let names =
        crate::speaker_names::SpeakerNameStore::open(crate::speaker_names::store_path(root))?;
    let existing = names.list(&recording.recording_id)?;
    let mut applied = Vec::new();
    for candidate in matches {
        let label = format!("Speaker {}", candidate.speaker);
        let user_set = existing
            .iter()
            .any(|n| n.label == label && !n.auto_identified);
        if user_set {
            continue;
        }
        names.set(
            &recording.recording_id,
            &label,
            &candidate.profile_name,
            true,
        )?;
        applied.push((
            candidate.speaker,
            candidate.profile_name,
            candidate.best_score,
        ));
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeExtractor {
        hash: String,
        /// Maps a sample slice's first element (used as a cheap fingerprint
        /// in tests) to a fixed embedding, so tests can control exactly what
        /// each turn "sounds like" without real audio.
        fixed: Vec<f32>,
    }
    impl EmbeddingExtractor for FakeExtractor {
        fn model_hash(&self) -> String {
            self.hash.clone()
        }
        fn extract(&self, samples: &[f32]) -> Result<Vec<f32>, String> {
            if samples.is_empty() {
                return Err("empty".into());
            }
            // Each sample's single value *is* its embedding contribution,
            // scaled -- deterministic and trivially inspectable in tests.
            Ok(self.fixed.iter().map(|f| f * samples[0]).collect())
        }
    }

    fn profile(
        store: &VoiceProfileStore,
        name: &str,
        embedding: Vec<f32>,
        hash: &str,
        seconds: f64,
    ) -> VoiceProfile {
        store.create(name, &embedding, hash, seconds).unwrap()
    }

    #[test]
    fn fake_extractor_reports_its_configured_model_hash() {
        let extractor = FakeExtractor {
            hash: "hash-a".into(),
            fixed: vec![1.0],
        };
        assert_eq!(extractor.model_hash(), "hash-a");
    }

    #[test]
    fn centroid_is_weighted_mean_normalized() {
        let v = centroid(&[vec![1.0, 0.0], vec![0.0, 1.0]], &[1.0, 1.0]).unwrap();
        // Equal weights on orthogonal unit vectors -> 45 degrees.
        assert!((v[0] - v[1]).abs() < 1e-6);
        let norm = (v[0] * v[0] + v[1] * v[1]).sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[test]
    fn centroid_rejects_mismatched_or_empty_input() {
        assert!(centroid(&[], &[]).is_none());
        assert!(centroid(&[vec![1.0]], &[1.0, 2.0]).is_none());
        assert!(centroid(&[vec![1.0], vec![1.0, 2.0]], &[1.0, 1.0]).is_none());
        assert!(centroid(&[vec![1.0]], &[0.0]).is_none());
    }

    #[test]
    fn cosine_similarity_identical_and_orthogonal_and_opposite() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn weighted_running_mean_biases_toward_the_heavier_observation() {
        let merged = weighted_running_mean(&[1.0, 0.0], 90.0, &[0.0, 1.0], 10.0);
        // Mostly the old (heavier) direction.
        assert!(merged[0] > merged[1]);
    }

    #[test]
    fn running_mean_update_rejects_a_different_model_hash() {
        let store = VoiceProfileStore::open(temp_db()).unwrap();
        let p = profile(&store, "Alice", vec![1.0, 0.0], "hash-a", 10.0);
        let result = store.update_running_mean(&p.id, &[0.0, 1.0], "hash-b", 10.0);
        assert!(result.is_err());
    }

    #[test]
    fn running_mean_update_merges_and_accumulates_seconds() {
        let store = VoiceProfileStore::open(temp_db()).unwrap();
        let p = profile(&store, "Alice", vec![1.0, 0.0], "hash-a", 10.0);
        let updated = store
            .update_running_mean(&p.id, &[1.0, 0.0], "hash-a", 5.0)
            .unwrap();
        assert!((updated.sample_seconds - 15.0).abs() < 1e-9);
        assert!((updated.embedding[0] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn match_clusters_accepts_a_clear_best_match_above_threshold_and_margin() {
        let store = VoiceProfileStore::open(temp_db()).unwrap();
        let alice = profile(&store, "Alice", vec![1.0, 0.0], "h", 20.0);
        let bob = profile(&store, "Bob", vec![0.0, 1.0], "h", 20.0);
        let clusters = vec![
            ClusterCentroid {
                speaker: 1,
                embedding: vec![1.0, 0.0],
            },
            ClusterCentroid {
                speaker: 2,
                embedding: vec![0.0, 1.0],
            },
        ];
        let matches = match_clusters(
            &clusters,
            &[alice.clone(), bob.clone()],
            DEFAULT_MATCH_THRESHOLD,
            DEFAULT_MATCH_MARGIN,
        );
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].profile_id, alice.id);
        assert_eq!(matches[1].profile_id, bob.id);
    }

    #[test]
    fn match_clusters_rejects_a_match_below_threshold() {
        let store = VoiceProfileStore::open(temp_db()).unwrap();
        let alice = profile(&store, "Alice", vec![1.0, 0.0], "h", 20.0);
        // Cluster is nearly orthogonal to Alice -- similarity well under 0.6.
        let clusters = vec![ClusterCentroid {
            speaker: 1,
            embedding: vec![0.1, 0.99],
        }];
        let matches = match_clusters(
            &clusters,
            &[alice],
            DEFAULT_MATCH_THRESHOLD,
            DEFAULT_MATCH_MARGIN,
        );
        assert!(matches.is_empty());
    }

    #[test]
    fn match_clusters_rejects_an_ambiguous_match_within_margin_of_the_runner_up() {
        let store = VoiceProfileStore::open(temp_db()).unwrap();
        let alice = profile(&store, "Alice", vec![1.0, 0.0], "h", 20.0);
        // A near-duplicate profile whose score will be within margin of Alice's.
        let alice_twin = profile(&store, "AliceTwin", vec![0.99, 0.05], "h", 20.0);
        let clusters = vec![ClusterCentroid {
            speaker: 1,
            embedding: vec![1.0, 0.0],
        }];
        let matches = match_clusters(
            &clusters,
            &[alice, alice_twin],
            DEFAULT_MATCH_THRESHOLD,
            DEFAULT_MATCH_MARGIN,
        );
        assert!(matches.is_empty());
    }

    #[test]
    fn match_clusters_never_assigns_the_same_profile_to_two_clusters() {
        let store = VoiceProfileStore::open(temp_db()).unwrap();
        let alice = profile(&store, "Alice", vec![1.0, 0.0], "h", 20.0);
        // Both clusters are closest to Alice; the weaker one must fall
        // through unmatched (there's no second profile to reassign it to).
        let clusters = vec![
            ClusterCentroid {
                speaker: 1,
                embedding: vec![1.0, 0.0],
            },
            ClusterCentroid {
                speaker: 2,
                embedding: vec![0.95, 0.05],
            },
        ];
        let matches = match_clusters(
            &clusters,
            std::slice::from_ref(&alice),
            DEFAULT_MATCH_THRESHOLD,
            DEFAULT_MATCH_MARGIN,
        );
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].speaker, 1);
        assert_eq!(matches[0].profile_id, alice.id);
    }

    #[test]
    fn speaker_centroid_refuses_too_little_speech() {
        let extractor = FakeExtractor {
            hash: "h".into(),
            fixed: vec![1.0, 0.0],
        };
        let turns = vec![SpeakerSamples {
            speaker: 1,
            seconds: 2.0,
            samples: vec![1.0, 1.0],
        }];
        let result = speaker_centroid(&extractor, &turns, 1);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Not enough speech"));
    }

    #[test]
    fn speaker_centroid_skips_short_turns_and_uses_the_rest() {
        let extractor = FakeExtractor {
            hash: "h".into(),
            fixed: vec![1.0, 0.0],
        };
        let turns = vec![
            SpeakerSamples {
                speaker: 1,
                seconds: 0.5,
                samples: vec![99.0],
            }, // skipped: too short
            SpeakerSamples {
                speaker: 1,
                seconds: 3.0,
                samples: vec![1.0],
            },
            SpeakerSamples {
                speaker: 1,
                seconds: 3.0,
                samples: vec![1.0],
            },
        ];
        let (_embedding, total) = speaker_centroid(&extractor, &turns, 1).unwrap();
        assert!((total - 6.0).abs() < 1e-9);
    }

    #[test]
    fn speaker_centroid_ignores_other_speakers_turns() {
        let extractor = FakeExtractor {
            hash: "h".into(),
            fixed: vec![1.0, 0.0],
        };
        let turns = vec![
            SpeakerSamples {
                speaker: 2,
                seconds: 30.0,
                samples: vec![1.0],
            },
            SpeakerSamples {
                speaker: 1,
                seconds: 3.0,
                samples: vec![1.0],
            },
            SpeakerSamples {
                speaker: 1,
                seconds: 3.0,
                samples: vec![1.0],
            },
        ];
        let (_embedding, total) = speaker_centroid(&extractor, &turns, 1).unwrap();
        assert!((total - 6.0).abs() < 1e-9);
    }

    #[test]
    fn all_speaker_centroids_omits_speakers_with_too_little_speech() {
        let extractor = FakeExtractor {
            hash: "h".into(),
            fixed: vec![1.0, 0.0],
        };
        let turns = vec![
            SpeakerSamples {
                speaker: 1,
                seconds: 6.0,
                samples: vec![1.0],
            },
            SpeakerSamples {
                speaker: 2,
                seconds: 2.0,
                samples: vec![1.0],
            },
        ];
        let clusters = all_speaker_centroids(&extractor, &turns);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].speaker, 1);
    }

    #[test]
    fn profile_crud_create_rename_delete() {
        let store = VoiceProfileStore::open(temp_db()).unwrap();
        let p = profile(&store, "Alice", vec![1.0, 0.0], "h", 10.0);
        store.rename(&p.id, "Alicia").unwrap();
        assert_eq!(store.get(&p.id).unwrap().unwrap().name, "Alicia");
        store.delete(&p.id).unwrap();
        assert!(store.get(&p.id).unwrap().is_none());
    }

    #[test]
    fn delete_all_clears_every_profile() {
        let store = VoiceProfileStore::open(temp_db()).unwrap();
        profile(&store, "Alice", vec![1.0, 0.0], "h", 10.0);
        profile(&store, "Bob", vec![0.0, 1.0], "h", 10.0);
        store.delete_all().unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn profiles_persist_after_reopen() {
        let path = temp_db();
        {
            let store = VoiceProfileStore::open(&path).unwrap();
            profile(&store, "Alice", vec![1.0, 0.5, -0.3], "h", 12.5);
        }
        let reopened = VoiceProfileStore::open(&path).unwrap();
        let list = reopened.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Alice");
        assert!((list[0].embedding[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn rejects_blank_or_oversized_names() {
        let store = VoiceProfileStore::open(temp_db()).unwrap();
        assert!(store.create("   ", &[1.0], "h", 10.0).is_err());
        assert!(store
            .create(&"x".repeat(MAX_NAME_LEN + 1), &[1.0], "h", 10.0)
            .is_err());
    }

    fn temp_db() -> PathBuf {
        std::env::temp_dir().join(format!("voice-profiles-{}.db", uuid::Uuid::new_v4()))
    }
}
