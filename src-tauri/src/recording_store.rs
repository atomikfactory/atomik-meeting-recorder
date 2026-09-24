//! Durable recording intent and crash recovery.
//!
//! This module deliberately trusts neither paths stored in SQLite nor WAV headers: both
//! can have been left half-written when the process (or machine) stopped unexpectedly.

use crate::{recording::Recording, recording_paths::resolved_recording_directory};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

pub type StoreResult<T> = Result<T, String>;

/// A small, bounded SQLite index.  WAV data remains in recording-owned folders; the
/// database contains only serialized metadata and is never used to discover files.
pub struct RecordingStore {
    root: PathBuf,
    connection: Mutex<Connection>,
}

impl RecordingStore {
    pub fn open(root: &Path) -> StoreResult<Self> {
        fs::create_dir_all(root).map_err(|e| format!("create recording directory: {e}"))?;
        let root = root
            .canonicalize()
            .map_err(|e| format!("canonicalize recording directory: {e}"))?;
        let db = root.join("recordings.sqlite3");
        if fs::symlink_metadata(&db)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err("recording database must not be a symlink".into());
        }
        let connection =
            Connection::open(&db).map_err(|e| format!("open recording database: {e}"))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 CREATE TABLE IF NOT EXISTS recordings (
                   recording_id TEXT PRIMARY KEY NOT NULL,
                   started_at INTEGER NOT NULL,
                   status TEXT NOT NULL,
                   payload TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS recording_tombstones (
                   recording_id TEXT PRIMARY KEY NOT NULL,
                   deleted_at INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS recordings_recent ON recordings(started_at DESC);",
            )
            .map_err(|e| format!("initialize recording database: {e}"))?;
        Ok(Self {
            root,
            connection: Mutex::new(connection),
        })
    }

    /// Atomically writes the entire metadata document.  In particular, a database error
    /// is returned to the caller rather than being converted into a false success.
    pub fn save(&self, recording: &Recording) -> StoreResult<()> {
        if !valid_id(&recording.recording_id) {
            return Err("recording id is not a safe directory name".into());
        }
        let payload = serde_json::to_string(recording)
            .map_err(|e| format!("serialize recording metadata: {e}"))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "recording database lock poisoned")?;
        let tx = connection
            .transaction()
            .map_err(|e| format!("begin recording save: {e}"))?;
        let deleted: Option<i64> = tx
            .query_row(
                "SELECT deleted_at FROM recording_tombstones WHERE recording_id=?1",
                params![recording.recording_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("check recording tombstone: {e}"))?;
        if deleted.is_some() {
            return Err("Recording was deleted and cannot be modified.".into());
        }
        tx.execute(
            "INSERT INTO recordings(recording_id, started_at, status, payload)
             VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(recording_id) DO UPDATE SET
                 started_at=excluded.started_at, status=excluded.status, payload=excluded.payload",
            params![
                recording.recording_id,
                recording.started_at as i64,
                recording.status,
                payload
            ],
        )
        .map_err(|e| format!("save recording metadata: {e}"))?;
        tx.commit()
            .map_err(|e| format!("commit recording save: {e}"))?;
        Ok(())
    }

    /// Semantic aliases make the recording lifecycle explicit at call sites.  Both use
    /// the same atomic upsert so retrying either operation is safe.
    pub fn begin(&self, recording: &Recording) -> StoreResult<()> {
        self.save(recording)
    }
    pub fn finish(&self, recording: &Recording) -> StoreResult<()> {
        self.save(recording)
    }

    pub fn recent(&self) -> StoreResult<Vec<Recording>> {
        self.query("SELECT recording_id, started_at, status, payload FROM recordings WHERE NOT EXISTS (SELECT 1 FROM recording_tombstones t WHERE t.recording_id=recordings.recording_id) ORDER BY started_at DESC LIMIT 25")
    }

    pub fn unfinished(&self) -> StoreResult<Vec<Recording>> {
        self.query("SELECT recording_id, started_at, status, payload FROM recordings WHERE status IN ('recording', 'processing') AND NOT EXISTS (SELECT 1 FROM recording_tombstones t WHERE t.recording_id=recordings.recording_id) ORDER BY started_at DESC")
    }

    /// Repairs each unfinished recording independently.  A corrupt record does not
    /// prevent another, healthy record from being recovered.  The first successful pass
    /// changes its status away from unfinished, so a second pass is a no-op.
    pub fn recover_unfinished(&self) -> StoreResult<Vec<Recording>> {
        let unfinished = self.unfinished_rows()?;
        let mut recovered = Vec::with_capacity(unfinished.len());
        for (id, payload) in unfinished {
            // A single malformed JSON document must not prevent repair of another
            // recording.  It has no safely editable Recording payload, so retire its
            // index row from the unfinished queue without overwriting its bytes.
            let mut recording: Recording = match serde_json::from_str(&payload) {
                Ok(recording) => recording,
                Err(_) => {
                    self.mark_malformed_failed(&id)?;
                    continue;
                }
            };
            if recording.recording_id != id {
                self.mark_malformed_failed(&id)?;
                continue;
            }
            let mut failures = Vec::new();
            if recording.tracks.is_empty() {
                failures.push("recording has no tracks".into());
            }
            let prior_error = recording.error.clone();
            let recording_dir = match resolved_recording_directory(&self.root, &recording) {
                Ok(dir) => dir,
                Err(e) => {
                    failures.push(e);
                    PathBuf::new()
                }
            };

            if failures.is_empty() {
                for track in &mut recording.tracks {
                    match repair_track(&recording_dir, track) {
                        Ok((frames, duration_ms)) => {
                            track.frames_written = frames;
                            // A capture's useful duration is its actual audio duration,
                            // never the time elapsed while the application was down.
                            recording.duration_ms = recording.duration_ms.max(duration_ms);
                        }
                        Err(e) => {
                            track.error = Some(format!("recovery: {e}"));
                            failures.push(format!("{}: {e}", track.name));
                        }
                    }
                }
            }

            if failures.is_empty() {
                if let Some(error) = prior_error {
                    recording.status = "failed".into();
                    recording.error = Some(error);
                } else {
                    recording.status = "interrupted".into();
                }
            } else {
                if let Some(error) = prior_error {
                    failures.push(format!("prior capture error: {error}"));
                }
                recording.status = "failed".into();
                recording.error = Some(format!("recovery failed: {}", failures.join("; ")));
            }
            // Do not manufacture a wall-clock duration after restart.  end time is
            // merely the audio-derived logical endpoint when that is representable.
            recording.ended_at = Some(recording.started_at.saturating_add(recording.duration_ms));
            self.save(&recording)?;
            recovered.push(recording);
        }
        Ok(recovered)
    }

    pub fn recover(&self) -> StoreResult<Vec<Recording>> {
        self.recover_unfinished()
    }

    fn query(&self, sql: &str) -> StoreResult<Vec<Recording>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "recording database lock poisoned")?;
        let mut statement = connection
            .prepare(sql)
            .map_err(|e| format!("query recordings: {e}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| format!("query recording rows: {e}"))?;
        let mut recordings = Vec::new();
        for row in rows {
            let (id, started_at, _status, payload) =
                row.map_err(|e| format!("read recording row: {e}"))?;
            match serde_json::from_str::<Recording>(&payload) {
                Ok(recording) if recording.recording_id == id => recordings.push(recording),
                _ => recordings.push(Recording {
                    recording_id: id,
                    started_at: started_at.max(0) as u64,
                    ended_at: None,
                    duration_ms: 0,
                    status: "failed".into(),
                    tracks: Vec::new(),
                    error: Some(
                        "Stored recording metadata is corrupted and was not modified.".into(),
                    ),
                    meeting_title: None,
                    storage_path: None,
                    auto_transcribe: false,
                    transcription_status: "not_started".into(),
                    appflowy_status: "not_started".into(),
                }),
            }
        }
        Ok(recordings)
    }

    /// Unbounded finalized history for durable background workers. This is
    /// intentionally separate from the UI's recent-25 query.
    pub fn finalized_eligible(&self) -> StoreResult<Vec<Recording>> {
        self.query("SELECT recording_id, started_at, status, payload FROM recordings WHERE status IN ('recorded','interrupted') AND NOT EXISTS (SELECT 1 FROM recording_tombstones t WHERE t.recording_id=recordings.recording_id) ORDER BY started_at ASC")
    }
    pub fn get_by_id(&self, id: &str) -> StoreResult<Option<Recording>> {
        if !valid_id(id) {
            return Err("recording id is not a safe directory name".into());
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| "recording database lock poisoned")?;
        let row: Option<(String, i64, String, String)> = connection.query_row(
            "SELECT recording_id, started_at, status, payload FROM recordings WHERE recording_id=?1 AND NOT EXISTS (SELECT 1 FROM recording_tombstones t WHERE t.recording_id=recordings.recording_id)", params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).optional().map_err(|e| format!("query recording: {e}"))?;
        match row {
            Some((stored, started, status, payload)) => {
                match serde_json::from_str::<Recording>(&payload) {
                    Ok(recording)
                        if recording.recording_id == stored
                            && recording.status == status
                            && started >= 0
                            && recording.started_at == started as u64 =>
                    {
                        Ok(Some(recording))
                    }
                    _ => Ok(Some(Recording {
                        recording_id: stored,
                        started_at: started.max(0) as u64,
                        ended_at: None,
                        duration_ms: 0,
                        status: "failed".into(),
                        tracks: vec![],
                        error: Some(
                            "Stored recording metadata is corrupted and was not modified.".into(),
                        ),
                        meeting_title: None,
                        storage_path: None,
                        auto_transcribe: false,
                        transcription_status: "not_started".into(),
                        appflowy_status: "not_started".into(),
                    })),
                }
            }
            None => Ok(None),
        }
    }

    /// Changes only presentation metadata. Folder names and media checksums remain stable.
    pub fn rename(&self, id: &str, title: String) -> StoreResult<Recording> {
        if !valid_id(id) || title.trim().is_empty() || title.chars().count() > 160 {
            return Err("Recording title must contain at most 160 characters.".into());
        }
        if title.chars().any(char::is_control) {
            return Err("Recording title contains an unsupported control character.".into());
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "recording database lock poisoned")?;
        let tx = connection
            .transaction()
            .map_err(|e| format!("begin recording rename: {e}"))?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT status,payload FROM recordings WHERE recording_id=?1 AND NOT EXISTS (SELECT 1 FROM recording_tombstones t WHERE t.recording_id=recordings.recording_id)",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| format!("query recording for rename: {e}"))?;
        let (status, payload) = row.ok_or("Recording metadata is unavailable or was deleted.")?;
        if matches!(status.as_str(), "recording" | "processing") {
            return Err("Stop the active recording before renaming it.".into());
        }
        let mut recording: Recording = serde_json::from_str(&payload)
            .map_err(|_| "Recording metadata is corrupt and cannot be renamed.")?;
        if recording.recording_id != id || recording.status != status {
            return Err("Recording metadata changed while it was being renamed.".into());
        }
        recording.meeting_title = Some(title.trim().to_owned());
        let payload = serde_json::to_string(&recording)
            .map_err(|e| format!("serialize renamed recording: {e}"))?;
        let changed = tx
            .execute(
                "UPDATE recordings SET payload=?2 WHERE recording_id=?1 AND status=?3 AND NOT EXISTS (SELECT 1 FROM recording_tombstones t WHERE t.recording_id=recordings.recording_id)",
                params![id, payload, status],
            )
            .map_err(|e| format!("save renamed recording: {e}"))?;
        if changed != 1 {
            return Err("Recording changed while it was being renamed.".into());
        }
        tx.commit()
            .map_err(|e| format!("commit recording rename: {e}"))?;
        Ok(recording)
    }

    /// Hides a finalized recording while preserving its directory and metadata for recovery.
    pub fn tombstone(&self, id: &str, deleted_at: u64) -> StoreResult<()> {
        if !valid_id(id) {
            return Err("recording id is not a safe directory name".into());
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| "recording database lock poisoned")?;
        let status: Option<String> = connection
            .query_row(
                "SELECT status FROM recordings WHERE recording_id=?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("query recording: {e}"))?;
        let status = status.ok_or("Recording metadata is unavailable.")?;
        if matches!(status.as_str(), "recording" | "processing") {
            return Err("Stop the active recording before deleting it.".into());
        }
        connection
            .execute(
                "INSERT INTO recording_tombstones(recording_id,deleted_at) VALUES(?1,?2) ON CONFLICT(recording_id) DO NOTHING",
                params![id, i64::try_from(deleted_at).map_err(|_| "invalid deletion time")?],
            )
            .map_err(|e| format!("tombstone recording: {e}"))?;
        Ok(())
    }

    fn unfinished_rows(&self) -> StoreResult<Vec<(String, String)>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "recording database lock poisoned")?;
        let mut statement = connection.prepare("SELECT recording_id, payload FROM recordings WHERE status IN ('recording', 'processing') AND NOT EXISTS (SELECT 1 FROM recording_tombstones t WHERE t.recording_id=recordings.recording_id) ORDER BY started_at DESC").map_err(|e| format!("query unfinished recordings: {e}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| format!("query unfinished rows: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read unfinished row: {e}"))
    }

    fn mark_malformed_failed(&self, id: &str) -> StoreResult<()> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "recording database lock poisoned")?;
        let tx = connection
            .transaction()
            .map_err(|e| format!("begin malformed recording update: {e}"))?;
        tx.execute(
            "UPDATE recordings SET status='failed' WHERE recording_id=?1",
            params![id],
        )
        .map_err(|e| format!("retire malformed recording: {e}"))?;
        tx.commit()
            .map_err(|e| format!("commit malformed recording update: {e}"))
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn repair_track(
    recording_dir: &Path,
    track: &mut crate::recording::Track,
) -> StoreResult<(u64, u64)> {
    if track.channels == 0 || track.sample_rate == 0 {
        return Err("invalid PCM format".into());
    }
    let path = Path::new(&track.file_path);
    // Stored file paths must resolve to a direct, non-symlink child of the one recording
    // directory selected from the recording id.  This makes a tampered DB harmless.
    let file_name = path.file_name().ok_or("track path has no filename")?;
    if !path.is_absolute() {
        return Err("track path must be absolute".into());
    }
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_none_or(|ext| !ext.eq_ignore_ascii_case("wav"))
    {
        return Err("track path is not a WAV file".into());
    }
    let safe_path = recording_dir.join(file_name);
    // It may be absolute (the normal application case) or relative, but must identify
    // exactly this child after resolution.  This also rejects `..` and outside paths.
    let stored_canonical = path
        .canonicalize()
        .map_err(|e| format!("WAV missing: {e}"))?;
    let safe_canonical = safe_path
        .canonicalize()
        .map_err(|e| format!("WAV missing: {e}"))?;
    if stored_canonical != safe_canonical {
        return Err("track path escapes recording directory".into());
    }
    let metadata = fs::symlink_metadata(&safe_path).map_err(|e| format!("WAV missing: {e}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("WAV is not a regular file".into());
    }
    let block_align = track
        .channels
        .checked_mul(2)
        .ok_or("invalid PCM block alignment")?;
    let bytes_per_frame = u64::from(block_align);
    let len = metadata.len();
    if len <= 44 {
        return Err("WAV contains no audio".into());
    }
    let audio_bytes = len - 44;
    let repaired_bytes = audio_bytes - (audio_bytes % bytes_per_frame);
    if repaired_bytes == 0 {
        return Err("WAV contains no complete PCM frame".into());
    }
    if repaired_bytes > u32::MAX as u64 - 36 {
        return Err("WAV is too large for RIFF PCM".into());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&safe_path)
        .map_err(|e| format!("open WAV: {e}"))?;
    let mut existing_header = [0u8; 44];
    file.read_exact(&mut existing_header)
        .map_err(|e| format!("read WAV header: {e}"))?;
    validate_or_placeholder_header(
        &existing_header,
        track.sample_rate,
        track.channels,
        block_align,
    )?;
    if repaired_bytes != audio_bytes {
        file.set_len(44 + repaired_bytes)
            .map_err(|e| format!("truncate incomplete PCM frame: {e}"))?;
    }
    write_pcm16_header(
        &mut file,
        track.sample_rate,
        track.channels,
        block_align,
        repaired_bytes,
    )?;
    file.sync_all()
        .map_err(|e| format!("sync repaired WAV: {e}"))?;
    let frames = repaired_bytes / bytes_per_frame;
    Ok((
        frames,
        frames.saturating_mul(1000) / u64::from(track.sample_rate),
    ))
}

fn validate_or_placeholder_header(
    header: &[u8; 44],
    sample_rate: u32,
    channels: u16,
    block_align: u16,
) -> StoreResult<()> {
    // The writer reserves a zeroed 44-byte header before its first audio write.  That
    // exact placeholder is recoverable; any other malformed header is left untouched.
    if header.iter().all(|byte| *byte == 0) {
        return Ok(());
    }
    let read_u16 = |at| u16::from_le_bytes([header[at], header[at + 1]]);
    let read_u32 =
        |at| u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]);
    if &header[0..4] != b"RIFF"
        || &header[8..12] != b"WAVE"
        || &header[12..16] != b"fmt "
        || read_u32(16) != 16
        || read_u16(20) != 1
        || read_u16(22) != channels
        || read_u32(24) != sample_rate
        || read_u16(32) != block_align
        || read_u16(34) != 16
        || &header[36..40] != b"data"
    {
        return Err("WAV header is not PCM16 for this track".into());
    }
    Ok(())
}

fn write_pcm16_header(
    file: &mut File,
    sample_rate: u32,
    channels: u16,
    block_align: u16,
    data_bytes: u64,
) -> StoreResult<()> {
    let byte_rate = sample_rate
        .checked_mul(u32::from(channels))
        .and_then(|v| v.checked_mul(2))
        .ok_or("invalid WAV byte rate")?;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    let riff_size = u32::try_from(36u64 + data_bytes).map_err(|_| "WAV is too large")?;
    header.extend_from_slice(&riff_size.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&channels.to_le_bytes());
    header.extend_from_slice(&sample_rate.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("seek WAV header: {e}"))?;
    file.write_all(&header)
        .map_err(|e| format!("write WAV header: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::Track;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "meeting-recorder-store-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn recording(id: &str, file_path: String) -> Recording {
        Recording {
            recording_id: id.into(),
            started_at: 1_000,
            ended_at: None,
            duration_ms: 0,
            status: "recording".into(),
            tracks: vec![Track {
                source_id: "source".into(),
                name: "mic".into(),
                direction: "input".into(),
                sample_rate: 1_000,
                channels: 1,
                file_path,
                frames_written: 0,
                dropped_samples: 0,
                error: None,
            }],
            error: None,
            meeting_title: None,
            storage_path: None,
            auto_transcribe: false,
            transcription_status: "not_started".into(),
            appflowy_status: "not_started".into(),
        }
    }

    #[test]
    fn records_survive_reopen_and_duplicate_save_is_one_row() {
        let root = temp_root("reopen");
        let rec = recording("one", root.join("one/track.wav").display().to_string());
        {
            let store = RecordingStore::open(&root).unwrap();
            store.save(&rec).unwrap();
            store.save(&rec).unwrap();
            assert_eq!(store.recent().unwrap().len(), 1);
        }
        let store = RecordingStore::open(&root).unwrap();
        assert_eq!(store.recent().unwrap()[0].recording_id, "one");
        drop(store);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn recovery_repairs_stale_header_and_partial_pcm_tail_once() {
        let root = temp_root("repair");
        let dir = root.join("one");
        fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("track.wav");
        // 44-byte stale placeholder header plus two PCM frames and one dangling byte.
        fs::write(&wav, [vec![0u8; 44], vec![1, 0, 2, 0, 99]].concat()).unwrap();
        let store = RecordingStore::open(&root).unwrap();
        store
            .begin(&recording("one", wav.display().to_string()))
            .unwrap();
        let first = store.recover().unwrap();
        assert_eq!(first[0].status, "interrupted");
        assert_eq!(first[0].tracks[0].frames_written, 2);
        assert_eq!(first[0].duration_ms, 2);
        let repaired = fs::read(&wav).unwrap();
        assert_eq!(repaired.len(), 48);
        assert_eq!(&repaired[..4], b"RIFF");
        assert_eq!(&repaired[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(repaired[40..44].try_into().unwrap()), 4);
        assert!(store.recover().unwrap().is_empty());
        assert_eq!(fs::read(&wav).unwrap(), repaired);
        drop(store);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_or_empty_wav_becomes_failed() {
        let root = temp_root("empty");
        let dir = root.join("one");
        fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("track.wav");
        fs::write(&wav, vec![0u8; 44]).unwrap();
        let store = RecordingStore::open(&root).unwrap();
        store
            .begin(&recording("one", wav.display().to_string()))
            .unwrap();
        let recovered = store.recover().unwrap();
        assert_eq!(recovered[0].status, "failed");
        assert!(recovered[0].tracks[0]
            .error
            .as_deref()
            .unwrap()
            .contains("no audio"));
        drop(store);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn outside_path_is_never_touched() {
        let root = temp_root("outside");
        fs::create_dir_all(root.join("one")).unwrap();
        let outside = root
            .parent()
            .unwrap()
            .join(format!("outside-{}.wav", std::process::id()));
        fs::write(&outside, [vec![0u8; 44], vec![7, 0]].concat()).unwrap();
        let original = fs::read(&outside).unwrap();
        let store = RecordingStore::open(&root).unwrap();
        store
            .begin(&recording("one", outside.display().to_string()))
            .unwrap();
        assert_eq!(store.recover().unwrap()[0].status, "failed");
        assert_eq!(fs::read(&outside).unwrap(), original);
        drop(store);
        fs::remove_file(&outside).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prior_capture_error_still_repairs_audio() {
        let root = temp_root("prior-error");
        let dir = root.join("one");
        fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("track.wav");
        fs::write(&wav, [vec![0u8; 44], vec![1, 0]].concat()).unwrap();
        let store = RecordingStore::open(&root).unwrap();
        let mut rec = recording("one", wav.display().to_string());
        rec.error = Some("device disconnected".into());
        store.save(&rec).unwrap();
        let result = store.recover().unwrap();
        assert_eq!(result[0].status, "failed");
        assert_eq!(result[0].tracks[0].frames_written, 1);
        assert_eq!(&fs::read(&wav).unwrap()[..4], b"RIFF");
        drop(store);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn zero_track_recording_fails() {
        let root = temp_root("zero-tracks");
        fs::create_dir_all(root.join("one")).unwrap();
        let store = RecordingStore::open(&root).unwrap();
        let mut rec = recording("one", String::new());
        rec.tracks.clear();
        store.save(&rec).unwrap();
        assert_eq!(store.recover().unwrap()[0].status, "failed");
        drop(store);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn row_key_payload_mismatch_never_edits_payload_target() {
        let root = temp_root("mismatch");
        let target_dir = root.join("two");
        fs::create_dir_all(&target_dir).unwrap();
        let wav = target_dir.join("track.wav");
        fs::write(&wav, [vec![0u8; 44], vec![1, 0, 2, 0, 3]].concat()).unwrap();
        let original = fs::read(&wav).unwrap();
        let store = RecordingStore::open(&root).unwrap();
        let rec = recording("two", wav.display().to_string());
        store.save(&rec).unwrap();
        drop(store);
        let db = Connection::open(root.join("recordings.sqlite3")).unwrap();
        db.execute("UPDATE recordings SET recording_id='one'", [])
            .unwrap();
        drop(db);
        let store = RecordingStore::open(&root).unwrap();
        assert!(store.recover().unwrap().is_empty());
        assert_eq!(fs::read(&wav).unwrap(), original);
        drop(store);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn malformed_row_does_not_block_healthy_recovery_and_is_visible() {
        let root = temp_root("malformed");
        let dir = root.join("one");
        fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("track.wav");
        fs::write(&wav, [vec![0u8; 44], vec![1, 0]].concat()).unwrap();
        let store = RecordingStore::open(&root).unwrap();
        store
            .save(&recording("one", wav.display().to_string()))
            .unwrap();
        drop(store);
        let db = Connection::open(root.join("recordings.sqlite3")).unwrap();
        db.execute("INSERT INTO recordings(recording_id,started_at,status,payload) VALUES('bad',2,'recording','not json')", []).unwrap();
        drop(db);
        let store = RecordingStore::open(&root).unwrap();
        assert_eq!(store.recover().unwrap().len(), 1);
        assert!(store
            .recent()
            .unwrap()
            .iter()
            .any(|r| r.recording_id == "bad" && r.status == "failed"));
        drop(store);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn failed_sql_save_leaves_previous_payload_intact() {
        let root = temp_root("query-only");
        let store = RecordingStore::open(&root).unwrap();
        let rec = recording("one", "C:\\unused.wav".into());
        store.save(&rec).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
        let mut changed = rec.clone();
        changed.status = "failed".into();
        assert!(store.save(&changed).is_err());
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=OFF")
            .unwrap();
        assert_eq!(store.recent().unwrap()[0].status, "recording");
        drop(store);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn invalid_database_bytes_are_not_changed_by_failed_open() {
        let root = temp_root("bad-db");
        fs::create_dir_all(&root).unwrap();
        let db = root.join("recordings.sqlite3");
        let bytes = b"not a sqlite database";
        fs::write(&db, bytes).unwrap();
        assert!(RecordingStore::open(&root).is_err());
        assert_eq!(fs::read(&db).unwrap(), bytes);
        fs::remove_dir_all(&root).unwrap();
    }
}
