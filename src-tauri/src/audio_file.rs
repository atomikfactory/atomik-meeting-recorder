//! A small shared abstraction over the two on-disk audio track formats this
//! app produces: the recorder's own PCM16 WAV, and the lossless FLAC that
//! `audio_compress.rs` may later replace it with (see that module for the
//! safety argument around the swap). Every reader of a track's audio --
//! transcription, diarization, and the compression stage's own verification
//! pass -- goes through this module rather than assuming a WAV extension, so
//! adding FLAC did not require touching each call site's decode logic beyond
//! a one-line dispatch.
//!
//! Only 16-bit PCM is supported in either format, matching the one format
//! `recording_wav.rs` ever writes; a FLAC track written by `audio_compress.rs`
//! always preserves that bit depth (see its `encode_flac_atomic` caller).

use claxon::FlacReader;
use flacenc::error::Verify;
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    /// Inter-channel sample count, i.e. audio duration in samples. `None`
    /// when a FLAC stream did not record it (rare; our own encoder always
    /// does), in which case callers fall back to decoding to find out.
    pub frames: Option<u64>,
}

pub fn is_flac_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("flac"))
}

/// Reads and validates a FLAC stream's header without decoding samples.
pub fn flac_info(path: &Path) -> Result<AudioInfo, String> {
    let reader =
        FlacReader::open(path).map_err(|error| format!("Could not read saved FLAC: {error}"))?;
    let info = reader.streaminfo();
    if info.channels == 0 || info.sample_rate == 0 {
        return Err("FLAC has an invalid channel count or sample rate.".into());
    }
    if info.bits_per_sample != 16 {
        return Err("Only 16-bit FLAC tracks are supported.".into());
    }
    Ok(AudioInfo {
        sample_rate: info.sample_rate,
        channels: info.channels as u16,
        bits_per_sample: 16,
        frames: info.samples,
    })
}

/// Decodes an entire FLAC track to interleaved 16-bit PCM. Used for the
/// compression stage's round-trip verification and by the (small,
/// diarization-length-limited) callers that already load a whole track.
pub fn read_flac_pcm16(path: &Path) -> Result<(AudioInfo, Vec<i16>), String> {
    let info = flac_info(path)?;
    let mut reader =
        FlacReader::open(path).map_err(|error| format!("Could not read saved FLAC: {error}"))?;
    let capacity = info
        .frames
        .and_then(|frames| frames.checked_mul(u64::from(info.channels)))
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    let mut samples = Vec::with_capacity(capacity);
    for sample in reader.samples() {
        let value = sample.map_err(|error| format!("Invalid FLAC sample: {error}"))?;
        if !(i32::from(i16::MIN)..=i32::from(i16::MAX)).contains(&value) {
            return Err("FLAC sample is outside the 16-bit PCM range.".into());
        }
        samples.push(value as i16);
    }
    Ok((info, samples))
}

/// A cursor over a FLAC track's samples, offering the same chunked
/// mono-averaging shape as `transcription_engine::read_mono_chunk` so its
/// resample loop can be reused unchanged for either format. Backed by a full
/// upfront decode (`read_flac_pcm16`) rather than incremental block reads:
/// claxon's `samples()` iterator owns internal per-block decode state that
/// does not survive being recreated between chunk calls, so re-decoding a
/// fresh iterator per chunk (as this originally tried) silently drops most
/// of a block's samples. A track's decoded PCM is the same size either way
/// this app already keeps in memory once decoded (see `decode_tracks_16k`'s
/// own doc comment), so this is not a new memory-shape regression.
pub struct FlacSampleCursor {
    samples: Vec<i16>,
    position: usize,
}
impl FlacSampleCursor {
    pub fn open(path: &Path) -> Result<(AudioInfo, Self), String> {
        let (info, samples) = read_flac_pcm16(path)?;
        Ok((
            info,
            Self {
                samples,
                position: 0,
            },
        ))
    }
    /// Reads up to `max_frames` inter-channel frames, averaged to mono and
    /// normalized to `[-1.0, 1.0]` exactly like
    /// `transcription_engine::read_mono_chunk`. Returns an empty vector at
    /// end of stream.
    pub fn read_mono_chunk(
        &mut self,
        channels: u16,
        max_frames: usize,
    ) -> Result<Vec<f32>, String> {
        let channels = usize::from(channels).max(1);
        let remaining_frames = (self.samples.len() - self.position) / channels;
        let take_frames = remaining_frames.min(max_frames);
        let mut mono = Vec::with_capacity(take_frames);
        for _ in 0..take_frames {
            let mut sum = 0i64;
            for offset in 0..channels {
                sum += i64::from(self.samples[self.position + offset]);
            }
            mono.push((sum as f32 / channels as f32) / 32768.0);
            self.position += channels;
        }
        Ok(mono)
    }
}

/// Encodes interleaved 16-bit PCM to a brand-new FLAC file at `path` (which
/// must not already exist -- see `audio_compress.rs`, which always encodes
/// to a fresh temp path). The file is `fsync`'d before returning so a crash
/// immediately afterwards leaves durable bytes on disk for the caller's
/// verification decode.
pub fn encode_flac_new(
    path: &Path,
    sample_rate: u32,
    channels: u16,
    samples: &[i16],
) -> Result<(), String> {
    if sample_rate == 0 || channels == 0 {
        return Err("FLAC sample rate and channel count must be non-zero".into());
    }
    if !samples.len().is_multiple_of(usize::from(channels)) {
        return Err("PCM buffer is not aligned to the channel count.".into());
    }
    let interleaved: Vec<i32> = samples.iter().map(|&sample| i32::from(sample)).collect();
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|error| format!("Invalid FLAC encoder configuration: {error:?}"))?;
    let source = flacenc::source::MemSource::from_samples(
        &interleaved,
        usize::from(channels),
        16,
        sample_rate as usize,
    );
    let block_size = config.block_size;
    let stream = flacenc::encode_with_fixed_block_size(&config, source, block_size)
        .map_err(|error| format!("FLAC encoding failed: {error:?}"))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    use flacenc::component::BitRepr;
    stream
        .write(&mut sink)
        .map_err(|error| format!("FLAC encoding failed: {error:?}"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("Could not create FLAC file: {error}"))?;
    file.write_all(sink.as_slice())
        .map_err(|error| format!("Could not write FLAC file: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("Could not durably write FLAC file: {error}"))?;
    Ok(())
}

/// Reads an entire recorder-owned PCM16 WAV to interleaved samples, with the
/// same header checks `transcription_engine::validate_wav` applies (kept
/// independent rather than shared visibility, since the two call sites --
/// bounded chunked transcription decode vs. whole-track compression input --
/// have different memory/latency shapes).
pub fn read_wav_pcm16(path: &Path) -> Result<(AudioInfo, Vec<i16>), String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|error| format!("Could not read saved WAV: {error}"))?;
    let spec = reader.spec();
    if spec.channels == 0 || spec.sample_rate == 0 {
        return Err("WAV has an invalid channel count or sample rate.".into());
    }
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err("Only 16-bit PCM WAV tracks are supported for compression.".into());
    }
    let mut samples = Vec::with_capacity(reader.len() as usize);
    for sample in reader.samples::<i16>() {
        samples.push(sample.map_err(|error| format!("Invalid PCM sample: {error}"))?);
    }
    let frames = samples.len() as u64 / u64::from(spec.channels);
    Ok((
        AudioInfo {
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            bits_per_sample: 16,
            frames: Some(frames),
        },
        samples,
    ))
}

/// Removes a leftover encode temp file (from `audio_compress.rs`, or a
/// pre-existing one left after a crash between encode and rename). Missing
/// files are not an error -- this is best-effort startup cleanup.
pub fn remove_orphan(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not remove orphaned temp file: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let path = env::temp_dir().join(format!(
            "meeting-recorder-audio-file-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_test_wav(path: &Path, sample_rate: u32, channels: u16, samples: &[i16]) {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for sample in samples {
            writer.write_sample(*sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn flac_round_trips_stereo_pcm16_bit_exact() {
        let dir = temp_dir("roundtrip-stereo");
        let samples: Vec<i16> = (0..4000)
            .map(|i| ((i as f32 * 0.05).sin() * 20000.0) as i16)
            .collect();
        let flac_path = dir.join("track.flac");
        encode_flac_new(&flac_path, 48_000, 2, &samples).unwrap();
        let (info, decoded) = read_flac_pcm16(&flac_path).unwrap();
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 2);
        assert_eq!(decoded, samples);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn flac_round_trips_mono_pcm16_bit_exact() {
        let dir = temp_dir("roundtrip-mono");
        let mut samples: Vec<i16> = vec![0, 1, -1, i16::MIN, i16::MAX, -12345, 12345];
        samples.extend(std::iter::repeat_n(0, 32 - samples.len()));
        let flac_path = dir.join("track.flac");
        encode_flac_new(&flac_path, 16_000, 1, &samples).unwrap();
        let (_, decoded) = read_flac_pcm16(&flac_path).unwrap();
        assert_eq!(decoded, samples);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn flac_round_trips_empty_track() {
        let dir = temp_dir("roundtrip-empty");
        let flac_path = dir.join("track.flac");
        encode_flac_new(&flac_path, 48_000, 1, &[]).unwrap();
        let (_, decoded) = read_flac_pcm16(&flac_path).unwrap();
        assert!(decoded.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// FLAC's format minimum block size (32 samples; see `flacenc::constant::MIN_BLOCK_SIZE`)
    /// is the smallest track this crate pairing can encode -- `audio_compress.rs`
    /// leaves anything shorter than that as WAV (see its own eligibility check).
    #[test]
    fn flac_round_trips_at_the_minimum_block_size() {
        let dir = temp_dir("roundtrip-tiny");
        let samples: Vec<i16> = (0..32).map(|i| i * 100).collect();
        let flac_path = dir.join("track.flac");
        encode_flac_new(&flac_path, 8_000, 1, &samples).unwrap();
        let (_, decoded) = read_flac_pcm16(&flac_path).unwrap();
        assert_eq!(decoded, samples);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn flac_cursor_matches_read_mono_chunk_semantics() {
        let dir = temp_dir("cursor");
        let mut samples: Vec<i16> = vec![0, 0, 32767, -32768, 100, -100];
        samples.extend(std::iter::repeat_n(0, 64 - samples.len()));
        let flac_path = dir.join("track.flac");
        encode_flac_new(&flac_path, 8_000, 2, &samples).unwrap();
        let (info, mut cursor) = FlacSampleCursor::open(&flac_path).unwrap();
        let mut collected = Vec::new();
        loop {
            let chunk = cursor.read_mono_chunk(info.channels, 1).unwrap();
            if chunk.is_empty() {
                break;
            }
            collected.extend(chunk);
        }
        assert_eq!(collected.len(), 32);
        assert!((collected[0]).abs() < 1e-6);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn encode_refuses_to_overwrite_existing_file() {
        let dir = temp_dir("no-overwrite");
        let path = dir.join("track.flac");
        std::fs::write(&path, b"do not touch").unwrap();
        assert!(encode_flac_new(&path, 8_000, 1, &[1, 2, 3]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"do not touch");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wav_reader_reads_back_what_it_wrote() {
        let dir = temp_dir("wav");
        let path = dir.join("track.wav");
        let samples = vec![0i16, 1, -1, 12345, -12345];
        write_test_wav(&path, 44_100, 1, &samples);
        let (info, read) = read_wav_pcm16(&path).unwrap();
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 1);
        assert_eq!(read, samples);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn is_flac_path_checks_extension_case_insensitively() {
        assert!(is_flac_path(Path::new("track.FLAC")));
        assert!(is_flac_path(Path::new("track.flac")));
        assert!(!is_flac_path(Path::new("track.wav")));
    }

    #[test]
    fn remove_orphan_is_idempotent() {
        let dir = temp_dir("orphan");
        let path = dir.join("missing.tmp");
        assert!(remove_orphan(&path).is_ok());
        std::fs::write(&path, b"x").unwrap();
        assert!(remove_orphan(&path).is_ok());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
