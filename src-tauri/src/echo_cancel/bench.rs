//! Manual, `#[ignore]`d speed benchmark for `cancel_echo` alone, run against
//! real recordings on disk rather than synthetic fixtures. Not part of the
//! normal test suite (no recordings are committed to this repo): point
//! `AEC_BENCH_MIC` / `AEC_BENCH_REF` at a mic/loopback WAV pair (any channel
//! count, 16-bit PCM; `cancel_echo` itself is sample-rate-agnostic, but
//! production always calls it with 16 kHz buffers from
//! `transcription_engine::decode_track_16k`, so a 16 kHz pair benchmarks the
//! actual production path most closely) and run
//!   cargo test --release --manifest-path src-tauri/Cargo.toml \
//!       echo_cancel::bench -- --ignored --nocapture
//! Reports wall time and speed as a multiple of real time (audio duration /
//! processing time) on stderr. This module never reads or writes anything
//! outside the paths named by those two env vars.
use super::cancel_echo;
use hound::WavReader;
use std::time::Instant;

/// Decodes a PCM16 WAV at any channel count/sample rate to mono `f32`
/// (channel-averaged), returning `(samples, sample_rate)`. A minimal,
/// self-contained decoder so this benchmark does not depend on
/// `transcription_engine`'s private chunked reader.
fn load_mono(path: &str) -> (Vec<f32>, u32) {
    let mut reader = WavReader::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
    let samples: Vec<i16> = reader
        .samples::<i16>()
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| panic!("decode {path}: {e}"));
    let mono: Vec<f32> = samples
        .chunks(channels)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|&s| i32::from(s)).sum();
            (sum as f32 / channels as f32) / i16::MAX as f32
        })
        .collect();
    (mono, spec.sample_rate)
}

#[test]
#[ignore = "needs real recordings on disk; set AEC_BENCH_MIC / AEC_BENCH_REF"]
fn cancel_echo_speed_on_a_real_recording() {
    let (Ok(mic_path), Ok(ref_path)) = (
        std::env::var("AEC_BENCH_MIC"),
        std::env::var("AEC_BENCH_REF"),
    ) else {
        eprintln!("skipping: set AEC_BENCH_MIC and AEC_BENCH_REF to a mic/loopback WAV pair");
        return;
    };
    let (mic, sample_rate) = load_mono(&mic_path);
    let (reference, ref_rate) = load_mono(&ref_path);
    assert_eq!(sample_rate, ref_rate, "benchmark expects matching native sample rates");
    let real_time_secs = mic.len() as f64 / f64::from(sample_rate);

    let start = Instant::now();
    let (_output, stats) = cancel_echo(&mic, &reference, sample_rate);
    let elapsed = start.elapsed();

    let elapsed_secs = elapsed.as_secs_f64();
    let speed = real_time_secs / elapsed_secs.max(1e-9);
    eprintln!(
        "cancel_echo: {real_time_secs:.2}s of audio in {elapsed_secs:.3}s ({speed:.1}x real time); \
         applied={} delay={:?}ms erle={:?}dB",
        stats.applied, stats.estimated_delay_ms, stats.erle_db
    );
    // Measured ~40x real time on a 3950X release build against the 5
    // real recordings this AEC was developed against (see
    // docs/transcription.md); 30x leaves comfortable margin for machine
    // variance while still catching a real regression (e.g. an accidental
    // return to the pre-fix ~20x, see `delay::REACQUIRE_STREAK` and
    // `filter::BLOCK_MS`).
    assert!(
        speed >= 30.0,
        "AEC speed regressed: {speed:.1}x real time (expected ~40x)"
    );
}
