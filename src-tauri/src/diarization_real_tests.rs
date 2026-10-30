//! Real, native sherpa-onnx speaker-diarization verification. Not part of the
//! default suite: `#[ignore]`d and env-gated so CI and normal `cargo test`
//! runs never depend on locally-imported, licensed model files or private
//! recording audio. Exercises the exact native configuration `run_worker`
//! (see `diarization_engine.rs`) uses -- `create_diarizer` and
//! `load_wav_16k_mono` -- directly, bypassing the subprocess/database/managed-
//! model-directory plumbing so it can run against a plain directory of copied
//! `.wav` files.
//!
//! Run explicitly, e.g. against local scratch copies of real recordings:
//!   DIARIZATION_REAL_MODELS=1 \
//!   DIARIZATION_REAL_SEGMENTATION_MODEL="<path to pyannote segmentation model.onnx>" \
//!   DIARIZATION_REAL_EMBEDDING_MODEL="<path to nemo_en_titanet_small.onnx>" \
//!   DIARIZATION_REAL_RECORDINGS_DIR="<dir of subfolders, each containing one or more track-*.wav files>" \
//!     cargo test --release --manifest-path src-tauri/Cargo.toml --locked -- --ignored real_sherpa_diarizes
//!
//! On Windows, this test first self-heals a real DLL-loading gap: sherpa-onnx's
//! shared DLLs land beside the compiled *application* executable at
//! `target/<profile>/`, but `cargo test` binaries live one level down at
//! `target/<profile>/deps/`, which is not on the default search path -- so
//! without this fixup the loader can silently pick up an unrelated
//! system-installed `onnxruntime.dll` instead (see
//! `ensure_sherpa_dlls_beside_test_binary` below). Run `cargo build --release`
//! (or `--release` on this test command) first so those DLLs exist to copy.
//!
//! Optional: `DIARIZATION_REAL_THRESHOLD` (default 0.5, matching
//! `create_diarizer`'s clustering threshold) to sweep a different value while
//! tuning against real audio without editing source.
use crate::diarization_engine::create_diarizer;
use crate::transcription_engine::load_wav_16k_mono;
use std::{fs, path::PathBuf, time::Instant};

const MAX_SOURCE_SECONDS: u64 = 30 * 60;

/// Windows-only fixup for a real DLL-loading bug this test surfaced: sherpa-onnx-sys's
/// build script copies its shared DLLs (sherpa-onnx-c-api.dll, onnxruntime.dll, ...)
/// beside the compiled *application* executable at `target/<profile>/`, which is
/// exactly right for the shipped app (its own exe lives there, and Windows always
/// searches the executable's own directory first, before even `System32`). A
/// `cargo test` binary, though, lives one level down at `target/<profile>/deps/`,
/// which is not on the default search path -- so the loader falls through to
/// `System32`, and on a machine that happens to have an unrelated, older
/// `onnxruntime.dll` there (for example, one installed by another
/// application), the wrong one loads and native calls crash with an ONNX Runtime API
/// version mismatch (`STATUS_ACCESS_VIOLATION`) instead of a clean error. Copying
/// the correct DLLs beside the test binary itself -- which Windows always checks
/// first -- fixes it without touching global `PATH` or the vendored build script.
#[cfg(windows)]
fn ensure_sherpa_dlls_beside_test_binary() {
    let exe = std::env::current_exe().expect("current_exe must resolve");
    let exe_dir = exe.parent().expect("exe has a parent directory");
    // exe_dir is target/<profile>/deps; the DLLs sherpa-onnx-sys's build script
    // placed beside the application binary live one level up.
    let profile_dir = exe_dir.parent().expect("deps/ has a parent directory");
    let Ok(entries) = fs::read_dir(profile_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("dll") {
            continue;
        }
        let dest = exe_dir.join(path.file_name().unwrap());
        if !dest.exists() {
            if let Err(error) = fs::copy(&path, &dest) {
                println!(
                    "warning: could not copy {} beside the test binary: {error}",
                    path.display()
                );
            }
        }
    }
}
#[cfg(not(windows))]
fn ensure_sherpa_dlls_beside_test_binary() {}

#[test]
#[ignore]
fn real_sherpa_diarizes_copied_recordings() {
    ensure_sherpa_dlls_beside_test_binary();
    let Ok(_) = std::env::var("DIARIZATION_REAL_MODELS") else {
        eprintln!(
            "skipping: set DIARIZATION_REAL_MODELS=1, DIARIZATION_REAL_SEGMENTATION_MODEL, \
             DIARIZATION_REAL_EMBEDDING_MODEL, and DIARIZATION_REAL_RECORDINGS_DIR to run this \
             against real sherpa-onnx models and recordings"
        );
        return;
    };
    let segmentation_model =
        PathBuf::from(std::env::var("DIARIZATION_REAL_SEGMENTATION_MODEL").expect(
            "set DIARIZATION_REAL_SEGMENTATION_MODEL to a pyannote segmentation model.onnx",
        ));
    let embedding_model = PathBuf::from(
        std::env::var("DIARIZATION_REAL_EMBEDDING_MODEL")
            .expect("set DIARIZATION_REAL_EMBEDDING_MODEL to nemo_en_titanet_small.onnx"),
    );
    let dir = std::env::var("DIARIZATION_REAL_RECORDINGS_DIR").expect(
        "set DIARIZATION_REAL_RECORDINGS_DIR to a directory of subfolders, each containing track-*.wav files",
    );
    assert!(segmentation_model.is_file(), "segmentation model not found");
    assert!(embedding_model.is_file(), "embedding model not found");

    let mut entries: Vec<_> = fs::read_dir(&dir)
        .expect("DIARIZATION_REAL_RECORDINGS_DIR must exist")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    assert!(
        !entries.is_empty(),
        "no recording subfolders found under {dir}"
    );

    println!("\n=== Real sherpa-onnx diarization verification ===");
    let mut any_crashed = false;
    for entry in entries {
        let folder = entry.path();
        let name = folder.file_name().unwrap().to_string_lossy().to_string();
        let mut tracks: Vec<_> = fs::read_dir(&folder)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|x| x.to_str()) == Some("wav")
                    && p.file_stem()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| s.starts_with("track-"))
            })
            .collect();
        tracks.sort();
        if tracks.is_empty() {
            println!("--- {name}: no track-*.wav files found, skipping ---");
            continue;
        }
        for track_path in tracks {
            let track_name = track_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let decode_started = Instant::now();
            let samples = match load_wav_16k_mono(&track_path, MAX_SOURCE_SECONDS) {
                Ok(samples) => samples,
                Err(error) => {
                    println!("--- {name}/{track_name}: FAILED to decode: {error} ---");
                    any_crashed = true;
                    continue;
                }
            };
            let audio_seconds = samples.len() as f64 / 16_000.0;
            if samples.is_empty() {
                println!("--- {name}/{track_name}: empty track, skipping ---");
                continue;
            }
            let diarizer = create_diarizer(&segmentation_model, &embedding_model)
                .expect("real models must initialize a diarizer");
            assert_eq!(
                diarizer.sample_rate(),
                16_000,
                "pyannote segmentation model must run at 16 kHz"
            );
            let infer_started = Instant::now();
            let result = diarizer.process(&samples);
            let elapsed = infer_started.elapsed();
            match result {
                Some(result) => {
                    let segments = result.sort_by_start_time();
                    let speakers: std::collections::BTreeSet<i32> =
                        segments.iter().map(|s| s.speaker).collect();
                    let real_time_factor = elapsed.as_secs_f64() / audio_seconds.max(0.001);
                    println!(
                        "--- {name}/{track_name} ---\naudio: {audio_seconds:.1}s | decode: {:.2}s | infer: {:.2}s ({:.3}x real time) | speakers: {} | segments: {}",
                        decode_started.elapsed().as_secs_f64(),
                        elapsed.as_secs_f64(),
                        real_time_factor,
                        speakers.len(),
                        segments.len(),
                    );
                    for segment in segments.iter().take(20) {
                        println!(
                            "    [{:>7.2}-{:>7.2}] speaker {} (confidence {:.2})",
                            segment.start, segment.end, segment.speaker, segment.confidence
                        );
                    }
                    if segments.len() > 20 {
                        println!("    ... and {} more segments", segments.len() - 20);
                    }
                    assert!(
                        segments
                            .iter()
                            .all(|s| s.start.is_finite() && s.end.is_finite() && s.end >= s.start),
                        "{name}/{track_name}: native result contained a malformed segment"
                    );
                }
                None => {
                    println!("--- {name}/{track_name}: native processing returned no result (crash-equivalent failure) ---");
                    any_crashed = true;
                }
            }
        }
    }
    assert!(
        !any_crashed,
        "at least one track failed to decode or process; see output above"
    );
}
