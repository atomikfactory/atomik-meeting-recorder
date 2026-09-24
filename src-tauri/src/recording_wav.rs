//! Recoverable, PCM16 WAV output for a single captured audio track.
//!
//! The capture callback owns the senders.  Closing every sender is the only
//! shutdown signal: the writer deliberately drains all queued packets before
//! it finalizes the WAV header.

use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const HEADER_BYTES: u64 = 44;
const RIFF_MAX_DATA_BYTES: u64 = u32::MAX as u64 - 36;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);
const ZERO_BUFFER: [u8; 8192] = [0; 8192];

/// Normalized interleaved capture data. `offset_frames` is relative to the
/// beginning of this track's recording, rather than the callback's clock.
#[derive(Debug)]
pub struct AudioPacket {
    pub samples: Vec<f32>,
    pub offset_frames: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavWriterResult {
    pub frames_written: u64,
    pub data_bytes: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavWriterProgress {
    pub frames_written: u64,
    pub data_bytes: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WavWriterHealth(Arc<Mutex<WavWriterProgress>>);

impl WavWriterHealth {
    pub fn snapshot(&self) -> WavWriterProgress {
        self.0
            .lock()
            .map(|value| value.clone())
            .unwrap_or(WavWriterProgress {
                frames_written: 0,
                data_bytes: 0,
                error: Some("WAV writer health lock is unavailable".into()),
            })
    }
}

pub struct WavWriterHandle {
    pub sender: SyncSender<AudioPacket>,
    pub health: WavWriterHealth,
    pub join: JoinHandle<WavWriterResult>,
}

#[derive(Debug)]
pub struct WavError(String);

impl fmt::Display for WavError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for WavError {}

impl From<io::Error> for WavError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

/// A writer which creates and checkpoints a canonical 44-byte PCM16 header.
pub struct Pcm16WavWriter {
    file: File,
    sample_rate: u32,
    channels: u16,
    max_data_bytes: u64,
    data_bytes: u64,
    next_frame: u64,
    last_checkpoint: Instant,
}

impl Pcm16WavWriter {
    /// Creates the file and writes a valid zero-data header synchronously.
    /// This is intentionally called before audio streams are started.
    pub fn create(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        max_data_bytes: u64,
    ) -> Result<Self, WavError> {
        if sample_rate == 0 || channels == 0 {
            return Err(WavError(
                "WAV sample rate and channel count must be non-zero".into(),
            ));
        }
        let block_align = channels
            .checked_mul(2)
            .ok_or_else(|| WavError("WAV block alignment overflows".into()))?;
        let bytes_per_second = sample_rate
            .checked_mul(channels as u32)
            .and_then(|value| value.checked_mul(block_align as u32))
            .ok_or_else(|| WavError("WAV format values overflow".into()))?;
        let _ = bytes_per_second;
        // A recording ID must be unique. Do not ever truncate an existing
        // audio file if that invariant is broken upstream.
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        write_header(&mut file, sample_rate, channels, 0)?;
        // Catch permission, device, and initial disk errors before capture
        // starts, instead of reporting a misleading successful recording.
        file.sync_data()?;
        Ok(Self {
            file,
            sample_rate,
            channels,
            max_data_bytes: max_data_bytes.min(RIFF_MAX_DATA_BYTES),
            data_bytes: 0,
            next_frame: 0,
            last_checkpoint: Instant::now(),
        })
    }

    /// Spawns the sole file owner. It drains until every sender is dropped.
    #[cfg(test)]
    pub fn spawn(self, receiver: Receiver<AudioPacket>) -> JoinHandle<WavWriterResult> {
        thread::Builder::new()
            .name("pcm16-wav-writer".into())
            .spawn(move || run_writer(self, receiver, None))
            .expect("could not create PCM16 WAV writer thread")
    }

    fn write_packet(&mut self, packet: AudioPacket) -> Result<(), WavError> {
        // Callback wakeups can legitimately carry no frames. They are not a
        // timestamped audio packet and must not turn a gap into fake silence.
        if packet.samples.is_empty() {
            return Ok(());
        }
        let channel_count = self.channels as usize;
        if !packet.samples.len().is_multiple_of(channel_count) {
            return Err(WavError(format!(
                "Audio packet has {} samples, which is not aligned to {} channels",
                packet.samples.len(),
                self.channels
            )));
        }
        if packet.offset_frames < self.next_frame {
            return Err(WavError(format!(
                "Audio packet overlaps prior audio (offset {} before frame {})",
                packet.offset_frames, self.next_frame
            )));
        }
        self.write_silence(packet.offset_frames - self.next_frame)?;
        if packet.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(WavError("Audio packet contains a non-finite sample".into()));
        }
        let bytes = (packet.samples.len() as u64)
            .checked_mul(2)
            .ok_or_else(|| WavError("Audio packet is too large".into()))?;
        self.reserve(bytes)?;
        let mut encoded = Vec::with_capacity(packet.samples.len() * 2);
        for sample in packet.samples {
            // -1.0 maps to -32767, avoiding an asymmetric value that is not
            // representable by normalized positive input.  Values beyond the
            // normalized range are safely clipped.
            let pcm = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            encoded.extend_from_slice(&pcm.to_le_bytes());
        }
        self.file.write_all(&encoded)?;
        self.data_bytes += bytes;
        self.next_frame += (encoded.len() / 2 / channel_count) as u64;
        self.checkpoint_if_due()
    }

    fn write_silence(&mut self, frames: u64) -> Result<(), WavError> {
        let bytes = frames
            .checked_mul(self.channels as u64)
            .and_then(|value| value.checked_mul(2))
            .ok_or_else(|| WavError("Timestamp gap is too large".into()))?;
        self.reserve(bytes)?;
        let mut remaining = bytes;
        while remaining > 0 {
            let count = remaining.min(ZERO_BUFFER.len() as u64) as usize;
            self.file.write_all(&ZERO_BUFFER[..count])?;
            remaining -= count as u64;
        }
        self.data_bytes += bytes;
        self.next_frame += frames;
        self.checkpoint_if_due()
    }

    fn reserve(&self, bytes: u64) -> Result<(), WavError> {
        if self
            .data_bytes
            .checked_add(bytes)
            .filter(|total| *total <= self.max_data_bytes)
            .is_none()
        {
            return Err(WavError(
                "WAV size limit reached before writing more audio".into(),
            ));
        }
        Ok(())
    }

    fn checkpoint_if_due(&mut self) -> Result<(), WavError> {
        if self.last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL {
            self.checkpoint()?;
        }
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<(), WavError> {
        write_header(
            &mut self.file,
            self.sample_rate,
            self.channels,
            self.data_bytes,
        )?;
        self.file.sync_data()?;
        self.file.seek(SeekFrom::End(0))?;
        self.last_checkpoint = Instant::now();
        Ok(())
    }

    fn finish(&mut self) -> Result<(), WavError> {
        // Try every final durability step even after an earlier failure. A
        // partial, header-updated file is more recoverable than one abandoned
        // at the first finalization error.
        let mut first_error = None;
        if let Err(error) = write_header(
            &mut self.file,
            self.sample_rate,
            self.channels,
            self.data_bytes,
        ) {
            first_error = Some(error);
        }
        if let Err(error) = self.file.sync_data() {
            if first_error.is_none() {
                first_error = Some(error.into());
            }
        }
        if let Err(error) = self.file.sync_all() {
            if first_error.is_none() {
                first_error = Some(error.into());
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Creates the file synchronously and starts a bounded packet writer. The
/// coordinator must drop `sender` after its streams stop, then join the worker.
pub fn spawn_writer(
    path: &Path,
    sample_rate: u32,
    channels: u16,
    max_data_bytes: u64,
    queue_capacity: usize,
) -> Result<WavWriterHandle, WavError> {
    let writer = Pcm16WavWriter::create(path, sample_rate, channels, max_data_bytes)?;
    let (sender, receiver) = mpsc::sync_channel(queue_capacity);
    let health = WavWriterHealth(Arc::new(Mutex::new(WavWriterProgress {
        frames_written: 0,
        data_bytes: 0,
        error: None,
    })));
    let worker_health = health.clone();
    let join = thread::Builder::new()
        .name("pcm16-wav-writer".into())
        .spawn(move || run_writer(writer, receiver, Some(worker_health)))
        .map_err(WavError::from)?;
    Ok(WavWriterHandle {
        sender,
        health,
        join,
    })
}

fn update_health(
    health: &Option<WavWriterHealth>,
    writer: &Pcm16WavWriter,
    error: &Option<String>,
) {
    if let Some(health) = health {
        if let Ok(mut value) = health.0.lock() {
            *value = WavWriterProgress {
                frames_written: writer.next_frame,
                data_bytes: writer.data_bytes,
                error: error.clone(),
            };
        }
    }
}

fn run_writer(
    mut writer: Pcm16WavWriter,
    receiver: Receiver<AudioPacket>,
    health: Option<WavWriterHealth>,
) -> WavWriterResult {
    let mut error: Option<String> = None;
    // Never return early after an error: receiving to channel close releases
    // bounded callback senders and preserves the queue-drain shutdown rule.
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(packet) => {
                if error.is_none() {
                    if let Err(write_error) = writer.write_packet(packet) {
                        error = Some(write_error.to_string());
                    }
                }
                update_health(&health, &writer, &error);
            }
            Err(RecvTimeoutError::Timeout) => {
                if error.is_none() {
                    if let Err(checkpoint_error) = writer.checkpoint_if_due() {
                        error = Some(checkpoint_error.to_string());
                    }
                }
                update_health(&health, &writer, &error);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Err(finish_error) = writer.finish() {
        if error.is_none() {
            error = Some(finish_error.to_string());
        }
    }
    update_health(&health, &writer, &error);
    WavWriterResult {
        frames_written: writer.next_frame,
        data_bytes: writer.data_bytes,
        error,
    }
}

fn write_header(
    file: &mut File,
    sample_rate: u32,
    channels: u16,
    data_bytes: u64,
) -> Result<(), WavError> {
    if data_bytes > RIFF_MAX_DATA_BYTES {
        return Err(WavError("WAV data exceeds RIFF's 32-bit size limit".into()));
    }
    let byte_rate = sample_rate
        .checked_mul(channels as u32)
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(|| WavError("WAV format values overflow".into()))?;
    let block_align = channels
        .checked_mul(2)
        .ok_or_else(|| WavError("WAV block alignment overflows".into()))?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data_bytes as u32).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&(data_bytes as u32).to_le_bytes())?;
    debug_assert_eq!(file.stream_position()?, HEADER_BYTES);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::mpsc,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_file(name: &str) -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("meeting-recorder-wav-{stamp}"));
        fs::create_dir_all(&directory).unwrap();
        directory.join(name)
    }

    #[test]
    fn writes_a_canonical_pcm16_wav_header() {
        let path = temp_file("complete.wav");
        let (sender, receiver) = mpsc::sync_channel(2);
        let join = Pcm16WavWriter::create(&path, 48_000, 2, RIFF_MAX_DATA_BYTES)
            .unwrap()
            .spawn(receiver);
        sender
            .send(AudioPacket {
                samples: vec![0.0, 1.0, -1.0, 0.5],
                offset_frames: 0,
            })
            .unwrap();
        drop(sender);
        let result = join.join().unwrap();
        assert_eq!(
            result,
            WavWriterResult {
                frames_written: 2,
                data_bytes: 8,
                error: None
            }
        );
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 8);
        assert_eq!(bytes.len(), 52);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn drains_queued_packets_after_sender_is_dropped() {
        let path = temp_file("drain.wav");
        let (sender, receiver) = mpsc::sync_channel(4);
        let join = Pcm16WavWriter::create(&path, 8_000, 1, RIFF_MAX_DATA_BYTES)
            .unwrap()
            .spawn(receiver);
        for offset in 0..3 {
            sender
                .send(AudioPacket {
                    samples: vec![0.25],
                    offset_frames: offset,
                })
                .unwrap();
        }
        drop(sender);
        assert_eq!(join.join().unwrap().frames_written, 3);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn pads_timestamp_gaps_with_silence() {
        let path = temp_file("gap.wav");
        let (sender, receiver) = mpsc::channel();
        let join = Pcm16WavWriter::create(&path, 8_000, 1, RIFF_MAX_DATA_BYTES)
            .unwrap()
            .spawn(receiver);
        sender
            .send(AudioPacket {
                samples: vec![1.0],
                offset_frames: 2,
            })
            .unwrap();
        drop(sender);
        assert_eq!(join.join().unwrap().frames_written, 3);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[44..48], &[0, 0, 0, 0]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn reports_invalid_alignment_without_abandoning_the_receiver() {
        let path = temp_file("invalid.wav");
        let (sender, receiver) = mpsc::sync_channel(1);
        let join = Pcm16WavWriter::create(&path, 8_000, 2, RIFF_MAX_DATA_BYTES)
            .unwrap()
            .spawn(receiver);
        sender
            .send(AudioPacket {
                samples: vec![0.0],
                offset_frames: 0,
            })
            .unwrap();
        // This succeeds only if the worker continues receiving after it has
        // recorded the malformed packet as an error.
        assert!(sender
            .send(AudioPacket {
                samples: vec![0.0, 0.0],
                offset_frames: 0
            })
            .is_ok());
        drop(sender);
        let result = join.join().unwrap();
        assert!(result.error.unwrap().contains("not aligned"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn enforces_an_injectable_size_cap() {
        let path = temp_file("cap.wav");
        let (sender, receiver) = mpsc::channel();
        let join = Pcm16WavWriter::create(&path, 8_000, 1, 2)
            .unwrap()
            .spawn(receiver);
        sender
            .send(AudioPacket {
                samples: vec![0.0, 0.0],
                offset_frames: 0,
            })
            .unwrap();
        drop(sender);
        assert!(join.join().unwrap().error.unwrap().contains("size limit"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn reports_synchronous_open_failures() {
        let path = std::env::temp_dir()
            .join("meeting-recorder-no-such-parent")
            .join("missing.wav");
        assert!(Pcm16WavWriter::create(&path, 8_000, 1, 32).is_err());
    }

    #[test]
    fn refuses_to_overwrite_an_existing_file() {
        let path = temp_file("existing.wav");
        fs::write(&path, b"do not replace me").unwrap();
        assert!(Pcm16WavWriter::create(&path, 8_000, 1, 32).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"do not replace me");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn empty_packets_do_not_invent_timestamp_gap_audio() {
        let path = temp_file("empty.wav");
        let (sender, receiver) = mpsc::channel();
        let join = Pcm16WavWriter::create(&path, 8_000, 1, RIFF_MAX_DATA_BYTES)
            .unwrap()
            .spawn(receiver);
        sender
            .send(AudioPacket {
                samples: vec![],
                offset_frames: 1_000,
            })
            .unwrap();
        sender
            .send(AudioPacket {
                samples: vec![0.5],
                offset_frames: 0,
            })
            .unwrap();
        drop(sender);
        assert_eq!(join.join().unwrap().frames_written, 1);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn checkpoints_a_live_but_idle_writer_within_one_second() {
        let path = temp_file("checkpoint.wav");
        let handle = spawn_writer(&path, 8_000, 1, RIFF_MAX_DATA_BYTES, 2).unwrap();
        handle
            .sender
            .send(AudioPacket {
                samples: vec![0.5],
                offset_frames: 0,
            })
            .unwrap();
        // No subsequent packet arrives. The receive timeout must still update
        // and sync the recoverable header before the sender is closed.
        thread::sleep(Duration::from_millis(1_150));
        let bytes = fs::read(&path).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 2);
        drop(handle.sender);
        assert!(handle.join.join().unwrap().error.is_none());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
