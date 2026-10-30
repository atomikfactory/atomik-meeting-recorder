use crate::recording::Recording;
use chrono::{Local, TimeZone};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

pub(crate) fn library_directory(root: &Path) -> Result<PathBuf, String> {
    if fs::symlink_metadata(root)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("recordings root must not be a symbolic link".into());
    }
    fs::create_dir_all(root).map_err(|e| format!("create recordings root: {e}"))?;
    let root = fs::canonicalize(root).map_err(|e| format!("canonicalize recordings root: {e}"))?;
    let path = root.join("meetings");
    if fs::symlink_metadata(&path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("recordings library must not be a symbolic link".into());
    }
    fs::create_dir_all(&path).map_err(|e| format!("create recordings library: {e}"))?;
    let path =
        fs::canonicalize(path).map_err(|e| format!("canonicalize recordings library: {e}"))?;
    if path.parent() != Some(root.as_path()) || !path.is_dir() {
        return Err("recordings library is outside its owned location".into());
    }
    Ok(path)
}

pub(crate) fn create_recording_directory(
    root: &Path,
    recording: &mut Recording,
) -> Result<PathBuf, String> {
    valid_uuid(&recording.recording_id)?;
    let library = library_directory(root)?;
    let timestamp = Local
        .timestamp_millis_opt(recording.started_at as i64)
        .single()
        .ok_or("recording start time is invalid")?
        .format("%Y-%m-%d_%H-%M-%S")
        .to_string();
    let basename = format!(
        "{timestamp}_{}--{}",
        safe_slug(recording.meeting_title.as_deref().unwrap_or("Meeting")),
        recording.recording_id
    );
    let path = library.join(&basename);
    fs::create_dir(&path).map_err(|e| format!("create recording folder: {e}"))?;
    let path = fs::canonicalize(path).map_err(|e| format!("canonicalize recording folder: {e}"))?;
    if path.parent() != Some(library.as_path()) {
        return Err("recording folder escaped its library".into());
    }
    recording.storage_path = Some(format!("meetings/{basename}"));
    // Record which root this recording actually lives under so it can still be
    // found later even if the configured library folder subsequently changes.
    // `library`'s parent is the (canonical) root the caller asked us to create
    // under -- capture that, not `root` itself, since `library` is already the
    // canonicalized, symlink-checked path.
    recording.library_root = library
        .parent()
        .map(|p| p.display().to_string())
        .or_else(|| Some(root.display().to_string()));
    Ok(path)
}

/// Resolves a recording's on-disk folder against `default_root` (the location
/// used before this feature existed, and still the implicit home for any
/// recording whose `library_root` is `None`) plus `extra_roots` -- typically
/// the currently configured library folder and any folder it replaced. A
/// recording created under a custom folder records that folder's canonical
/// path in `recording.library_root`; that value is only trusted when it
/// matches one of these known roots, so a tampered or stale value can never
/// point resolution somewhere unexpected.
pub(crate) fn resolved_recording_directory(
    default_root: &Path,
    extra_roots: &[PathBuf],
    recording: &Recording,
) -> Result<PathBuf, String> {
    let canonical_default =
        fs::canonicalize(default_root).map_err(|e| format!("canonicalize recordings root: {e}"))?;
    let root = match recording.library_root.as_deref() {
        None => canonical_default.clone(),
        Some(saved) => {
            let saved_canonical = fs::canonicalize(saved)
                .map_err(|e| format!("recording's storage location is unavailable: {e}"))?;
            let known = std::iter::once(&canonical_default).chain(extra_roots.iter());
            let mut matched = None;
            for candidate in known {
                if let Ok(candidate_canonical) = fs::canonicalize(candidate) {
                    if candidate_canonical == saved_canonical {
                        matched = Some(candidate_canonical);
                        break;
                    }
                }
            }
            matched.ok_or_else(|| {
                "recording's storage location is not a recognized recordings folder".to_string()
            })?
        }
    };
    let (parent, path) = match recording.storage_path.as_deref() {
        None => {
            if !valid_legacy_id(&recording.recording_id) {
                return Err("recording id is invalid".into());
            }
            (root.clone(), root.join(&recording.recording_id))
        }
        Some(relative) => {
            valid_uuid(&recording.recording_id)?;
            let rel = Path::new(relative);
            let mut parts = rel.components();
            if rel.is_absolute()
                || parts.next() != Some(Component::Normal(std::ffi::OsStr::new("meetings")))
                || parts.next().is_none()
                || parts.next().is_some()
            {
                return Err("recording storage path is invalid".into());
            }
            let base = rel
                .file_name()
                .and_then(|x| x.to_str())
                .ok_or("recording storage path is invalid")?;
            if !valid_basename(base, &recording.recording_id) {
                return Err("recording storage path is invalid".into());
            }
            let parent = root.join("meetings");
            (parent.clone(), parent.join(base))
        }
    };
    if fs::symlink_metadata(&path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("recording directory must not be a symbolic link".into());
    }
    if fs::symlink_metadata(&parent)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("recordings library must not be a symbolic link".into());
    }
    let expected_parent = if recording.storage_path.is_some() {
        root.join("meetings")
    } else {
        root.clone()
    };
    let candidate = path;
    let path =
        fs::canonicalize(&candidate).map_err(|e| format!("recording directory missing: {e}"))?;
    let parent =
        fs::canonicalize(parent).map_err(|e| format!("canonicalize recording parent: {e}"))?;
    if parent != expected_parent
        || path != candidate
        || !path.is_dir()
        || path.parent() != Some(parent.as_path())
    {
        return Err("recording directory is outside its owned location".into());
    }
    Ok(path)
}

pub(crate) fn safe_slug(title: &str) -> String {
    let slug: String = title
        .chars()
        .filter_map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => Some(c),
            ' ' | '-' | '_' => Some('_'),
            _ => None,
        })
        .collect();
    let slug = slug.trim_matches('_');
    if slug.is_empty() {
        "Meeting".into()
    } else {
        slug.chars().take(80).collect()
    }
}
fn valid_uuid(id: &str) -> Result<(), String> {
    let uuid = uuid::Uuid::parse_str(id).map_err(|_| "recording id is invalid".to_string())?;
    if uuid.to_string() != id {
        return Err("recording id is invalid".into());
    }
    Ok(())
}
fn valid_legacy_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn valid_basename(name: &str, id: &str) -> bool {
    name.ends_with(&format!("--{id}"))
        && name.len() <= 160
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{Recording, Track};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "meeting-recorder-path-{label}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
    fn recording(id: String) -> Recording {
        Recording {
            recording_id: id,
            started_at: 1_704_067_200_000,
            ended_at: None,
            duration_ms: 0,
            status: "recording".into(),
            tracks: vec![Track {
                source_id: "s".into(),
                name: "n".into(),
                direction: "input".into(),
                sample_rate: 16_000,
                channels: 1,
                file_path: String::new(),
                frames_written: 0,
                dropped_samples: 0,
                error: None,
            }],
            error: None,
            meeting_title: Some("A / B: <C>".into()),
            storage_path: None,
            library_root: None,
            auto_transcribe: false,
            transcription_status: "idle".into(),
            appflowy_status: "idle".into(),
        }
    }

    #[test]
    fn slug_is_safe() {
        assert_eq!(safe_slug("A / B: <C>"), "A__B_C");
        assert_eq!(safe_slug("***"), "Meeting");
    }

    #[test]
    fn new_layout_round_trips_with_relative_storage_path() {
        let root = root("new");
        let mut record = recording(uuid::Uuid::new_v4().to_string());
        let directory = create_recording_directory(&root, &mut record).unwrap();
        assert!(record
            .storage_path
            .as_deref()
            .unwrap()
            .starts_with("meetings/"));
        assert!(directory
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("A__B_C--"));
        assert_eq!(
            resolved_recording_directory(&root, &[], &record).unwrap(),
            directory
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_folder_resolves_without_move() {
        let root = root("legacy");
        let mut record = recording("legacy_one".into());
        record.meeting_title = None;
        record.storage_path = None;
        let legacy = root.join("legacy_one");
        fs::create_dir(&legacy).unwrap();
        assert_eq!(
            resolved_recording_directory(&root, &[], &record).unwrap(),
            legacy.canonicalize().unwrap()
        );
        assert!(!root.join("meetings").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hostile_storage_paths_are_rejected() {
        let root = root("hostile");
        let id = uuid::Uuid::new_v4().to_string();
        let mut record = recording(id.clone());
        for path in [
            "../outside",
            "C:/outside",
            "meetings/okay--wrong",
            "meetings/okay--00000000-0000-0000-0000-000000000000",
        ] {
            record.storage_path = Some(path.into());
            assert!(
                resolved_recording_directory(&root, &[], &record).is_err(),
                "{path}"
            );
        }
        record.recording_id = id.to_ascii_uppercase();
        record.storage_path = Some(format!("meetings/x--{}", record.recording_id));
        assert!(resolved_recording_directory(&root, &[], &record).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_recording_created_under_a_custom_root_resolves_via_extra_roots() {
        let default_root = root("default-a");
        let custom_root = root("custom-a");
        let mut record = recording(uuid::Uuid::new_v4().to_string());
        let directory = create_recording_directory(&custom_root, &mut record).unwrap();
        assert!(record.library_root.is_some());
        // The default root alone does not know about it...
        assert!(resolved_recording_directory(&default_root, &[], &record).is_err());
        // ...but listing the custom root as a known extra root resolves it.
        assert_eq!(
            resolved_recording_directory(
                &default_root,
                std::slice::from_ref(&custom_root),
                &record
            )
            .unwrap(),
            directory
        );
        let _ = fs::remove_dir_all(default_root);
        let _ = fs::remove_dir_all(custom_root);
    }

    #[test]
    fn an_untrusted_library_root_is_rejected_even_if_the_path_is_real() {
        let default_root = root("default-b");
        let untracked_root = root("untracked-b");
        let mut record = recording(uuid::Uuid::new_v4().to_string());
        create_recording_directory(&untracked_root, &mut record).unwrap();
        // `untracked_root` is a real directory containing a real, validly
        // named recording folder -- but it was never registered as the
        // default or an extra (configured/legacy) root, so it must not be
        // trusted merely because the recording's own JSON claims it.
        assert!(resolved_recording_directory(&default_root, &[], &record).is_err());
        let _ = fs::remove_dir_all(default_root);
        let _ = fs::remove_dir_all(untracked_root);
    }

    #[test]
    fn a_legacy_recording_with_no_library_root_always_uses_the_default_root() {
        let default_root = root("default-c");
        let mut record = recording("legacy_two".into());
        record.meeting_title = None;
        record.storage_path = None;
        record.library_root = None;
        let legacy = default_root.join("legacy_two");
        fs::create_dir(&legacy).unwrap();
        // Even with unrelated extra roots supplied, a `None` library_root
        // always means the default root -- extra roots are irrelevant here.
        let unrelated = root("unrelated-c");
        assert_eq!(
            resolved_recording_directory(&default_root, std::slice::from_ref(&unrelated), &record)
                .unwrap(),
            legacy.canonicalize().unwrap()
        );
        let _ = fs::remove_dir_all(default_root);
        let _ = fs::remove_dir_all(unrelated);
    }
}
