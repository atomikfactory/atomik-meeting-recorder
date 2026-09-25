use super::*;

#[test]
fn levels_are_source_mapped_independent_and_cleared_on_stop() {
    let dir = temp_dir("levels");
    let store = RecordingStore::open(&dir).unwrap();
    let state = Arc::new(Mutex::new(RecordingState::default()));
    let mut engine = Engine {
        current_library_root: Arc::new({ let dir = dir.clone(); move || Ok(dir.clone()) }),
        store,
        backend: backend(vec![("a", false, false, false), ("b", false, false, false)]),
        active: None,
        pending_save: None,
        state: state.clone(),
        title_provider: || None,
    };
    engine.start(vec![source("a"), source("b")]).unwrap();
    let active = engine.active.as_ref().unwrap();
    active.meters[0].observe(&[1.0, 0.5], 0);
    active.meters[1].observe(&[0.01, 0.0], 0);
    engine.monitor();
    let levels = state.lock().unwrap().levels.clone();
    assert_eq!(levels.len(), 2);
    assert_eq!(levels[0].source_id, "a");
    assert_eq!(levels[1].source_id, "b");
    assert!(levels[0].peak_dbfs > levels[1].peak_dbfs);
    engine.stop(None).unwrap();
    assert!(state.lock().unwrap().levels.is_empty());
    drop(engine);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn recoverable_xrun_continues_but_saved_audio_is_marked_with_gaps() {
    let dir = temp_dir("xrun");
    let store = RecordingStore::open(&dir).unwrap();
    let state = Arc::new(Mutex::new(RecordingState::default()));
    let mut engine = Engine {
        current_library_root: Arc::new({ let dir = dir.clone(); move || Ok(dir.clone()) }),
        store,
        backend: backend(vec![("a", false, false, false)]),
        active: None,
        pending_save: None,
        state: state.clone(),
        title_provider: || None,
    };
    engine.start(vec![source("a")]).unwrap();
    let (faults, fault_rx) = mpsc::sync_channel(1);
    // Settled: this Xrun is standing in for one that happens well after the
    // stream has caught up with real time, so it must still be counted.
    route_stream_error(
        cpal::ErrorKind::Xrun.into(),
        "a",
        &faults,
        &engine.active.as_ref().unwrap().gaps[0],
        &AtomicBool::new(true),
    );
    assert!(fault_rx.try_recv().is_err());
    engine.monitor();
    assert!(engine.active.is_some());
    assert_eq!(state.lock().unwrap().status, "recording");
    engine.stop(None).unwrap();
    let snapshot = state.lock().unwrap();
    assert_eq!(snapshot.recent[0].status, "interrupted");
    assert!(snapshot.recent[0].tracks[0].frames_written > 0);
    assert!(snapshot.recent[0]
        .error
        .as_ref()
        .unwrap()
        .contains("discontinuities"));
    route_stream_error(
        cpal::ErrorKind::DeviceNotAvailable.into(),
        "a",
        &faults,
        &AtomicU64::new(0),
        &AtomicBool::new(true),
    );
    let error = fault_rx.try_recv().unwrap();
    assert!(error.contains("a") && error.contains("audio stream error"));
    drop(snapshot);
    drop(engine);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn route_stream_error_ignores_unsettled_first_xrun() {
    // CPAL already ignores AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY on WASAPI's
    // very first buffer (device_position == 0), but real hardware routinely
    // flags the buffer right after that -- the boundary between device
    // pre-roll and steady delivery. Until the capture timeline has settled,
    // that reported discontinuity must not be counted.
    let (faults, _fault_rx) = mpsc::sync_channel(1);
    let gaps = AtomicU64::new(0);
    let settled = AtomicBool::new(false);
    route_stream_error(cpal::ErrorKind::Xrun.into(), "a", &faults, &gaps, &settled);
    assert_eq!(gaps.load(Ordering::Relaxed), 0);
}

#[test]
fn route_stream_error_counts_xrun_once_settled() {
    let (faults, _fault_rx) = mpsc::sync_channel(1);
    let gaps = AtomicU64::new(0);
    let settled = AtomicBool::new(true);
    route_stream_error(cpal::ErrorKind::Xrun.into(), "a", &faults, &gaps, &settled);
    assert_eq!(gaps.load(Ordering::Relaxed), 1);
}

#[test]
fn frame_timeline_absorbs_timestamp_jitter_without_overlap() {
    let mut clock = FrameTimeline::default();
    let adjustments = AtomicU64::new(0);
    assert_eq!(
        clock.offset(Some(0), 48_000, 480, 0, &adjustments).unwrap(),
        0
    );
    // The timestamp is one callback behind, but the captured packet remains
    // valid and must follow the previous packet in the WAV.
    assert_eq!(
        clock
            .offset(Some(400), 48_000, 480, 480, &adjustments)
            .unwrap(),
        480
    );
    assert_eq!(adjustments.load(Ordering::Relaxed), 0);
}

#[test]
fn frame_timeline_reanchors_timestamp_reset_and_retains_audio() {
    let mut clock = FrameTimeline::default();
    let adjustments = AtomicU64::new(0);
    assert_eq!(
        clock.offset(Some(0), 48_000, 480, 0, &adjustments).unwrap(),
        0
    );
    assert_eq!(
        clock
            .offset(Some(480), 48_000, 480, 480, &adjustments)
            .unwrap(),
        480
    );
    // A device reset makes the reported capture time move backwards. Packet
    // order, not the reset timestamp, is the safe durable timeline.
    assert_eq!(
        clock
            .offset(Some(20), 48_000, 480, 960, &adjustments)
            .unwrap(),
        960
    );
    // After rebase, a real post-reset forward gap is still retained.
    assert_eq!(
        clock
            .offset(Some(20_000), 48_000, 480, 20_940, &adjustments)
            .unwrap(),
        20_940
    );
    assert_eq!(adjustments.load(Ordering::Relaxed), 2);
}

#[test]
fn frame_timeline_epoch_reanchor_clears_prior_reset_bias() {
    let mut clock = FrameTimeline::default();
    let adjustments = AtomicU64::new(0);
    assert_eq!(
        clock.offset(Some(0), 48_000, 480, 0, &adjustments).unwrap(),
        0
    );
    assert_eq!(
        clock
            .offset(Some(480), 48_000, 480, 480, &adjustments)
            .unwrap(),
        480
    );
    assert_eq!(
        clock
            .offset(Some(20), 48_000, 480, 960, &adjustments)
            .unwrap(),
        960
    );
    // This mirrors a reset before PacketClock's original CPAL anchor.
    clock.reanchor_measurement_epoch();
    assert_eq!(
        clock
            .offset(Some(1_440), 48_000, 480, 1_440, &adjustments)
            .unwrap(),
        1_440
    );
}

#[test]
fn frame_timeline_preserves_meaningful_forward_gaps_with_warning() {
    let mut clock = FrameTimeline::default();
    let adjustments = AtomicU64::new(0);
    assert_eq!(
        clock.offset(Some(0), 48_000, 480, 0, &adjustments).unwrap(),
        0
    );
    // More than the 10 ms jitter budget is retained as an explicit WAV gap.
    assert_eq!(
        clock
            .offset(Some(10_000), 48_000, 480, 10_000, &adjustments)
            .unwrap(),
        10_000
    );
    assert_eq!(adjustments.load(Ordering::Relaxed), 1);
}

#[test]
fn frame_timeline_accepts_first_late_loopback_callback() {
    let mut clock = FrameTimeline::default();
    let adjustments = AtomicU64::new(0);
    assert_eq!(
        clock
            .offset(Some(48_000 * 60), 48_000, 480, 48_000 * 60, &adjustments)
            .unwrap(),
        48_000 * 60
    );
}

#[test]
fn frame_timeline_first_packet_offset_is_not_a_discontinuity() {
    // Every real capture starts with `next == 0` while the device's own
    // start-up latency/buffer prefill already places its first measured
    // timestamp well ahead of frame 0 (tens of ms is typical; this uses 20ms
    // @ 48kHz). There is nothing before the first packet to be discontinuous
    // with, so establishing the anchor must not itself count as a gap.
    let mut clock = FrameTimeline::default();
    let adjustments = AtomicU64::new(0);
    let offset = clock.offset(Some(960), 48_000, 480, 960, &adjustments).unwrap();
    assert_eq!(offset, 960);
    assert_eq!(adjustments.load(Ordering::Relaxed), 0);
    // This first packet did not land within budget, so the timeline is not
    // yet "settled" -- a second packet that also needs a small correction is
    // still start-up buffering, not a new discontinuity.
    assert!(!clock.is_settled());
}

#[test]
fn frame_timeline_settles_then_counts_a_genuine_gap() {
    // A packet that lands within the jitter budget marks the timeline
    // settled; only after that does a forward jump count as a real gap
    // rather than continued start-up catch-up.
    let mut clock = FrameTimeline::default();
    let adjustments = AtomicU64::new(0);
    assert_eq!(
        clock.offset(Some(0), 48_000, 480, 0, &adjustments).unwrap(),
        0
    );
    assert!(clock.is_settled());
    assert_eq!(
        clock
            .offset(Some(48_000), 48_000, 480, 48_000, &adjustments)
            .unwrap(),
        48_000
    );
    assert_eq!(adjustments.load(Ordering::Relaxed), 1);
}

#[test]
fn frame_timeline_forces_settlement_after_grace_window() {
    // If capture never once lands on schedule, the start-up grace must not
    // suppress discontinuities forever: after SETTLE_GRACE_PACKETS callbacks
    // corrections are counted even though the timeline never "settled"
    // naturally.
    let mut clock = FrameTimeline::default();
    let adjustments = AtomicU64::new(0);
    let mut next = 0u64;
    for _ in 0..SETTLE_GRACE_PACKETS {
        let measured = next + 960; // Always 20ms further ahead than expected.
        next = clock
            .offset(Some(measured), 48_000, 480, measured, &adjustments)
            .unwrap()
            + 480;
    }
    assert_eq!(adjustments.load(Ordering::Relaxed), 0);
    assert!(!clock.is_settled());
    let measured = next + 960;
    clock
        .offset(Some(measured), 48_000, 480, measured, &adjustments)
        .unwrap();
    assert_eq!(adjustments.load(Ordering::Relaxed), 1);
}

#[test]
fn frame_timeline_long_clock_drift_stays_monotonic() {
    let mut clock = FrameTimeline::default();
    let adjustments = AtomicU64::new(0);
    let mut previous = 0;
    for index in 0..10_000u64 {
        // 1 frame of clock drift every 100 callbacks stays within the jitter
        // budget rather than becoming thousands of fabricated mini-gaps.
        let offset = clock
            .offset(
                Some(index * 480 + index / 100),
                48_000,
                480,
                index * 480,
                &adjustments,
            )
            .unwrap();
        assert!(offset >= previous);
        previous = offset + 480;
    }
}

#[test]
fn frame_timeline_rejects_unbounded_silence_and_overflow() {
    let mut clock = FrameTimeline::default();
    let adjustments = AtomicU64::new(0);
    assert!(clock.offset(Some(0), 48_000, 480, 0, &adjustments).is_ok());
    // A loopback can begin late, or be quiet for a minute: wall-clock-consistent
    // silence is kept rather than rejected by an arbitrary 30 second gap.
    assert_eq!(
        clock
            .offset(Some(48_000 * 60), 48_000, 480, 48_000 * 60, &adjustments)
            .unwrap(),
        48_000 * 60
    );
    assert!(clock
        .offset(Some(48_000 * 500), 48_000, 480, 48_000 * 60, &adjustments)
        .is_err());
    let mut overflowing = FrameTimeline {
        next: u64::MAX - 4,
        last_measured: None,
        rebase_frames: 0,
        settled: true,
        packets: 10,
    };
    assert!(overflowing
        .offset(None, 48_000, 8, 0, &adjustments)
        .is_err());
}
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn temp_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "meeting-recorder-engine-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}
fn source(id: &str) -> AudioSourceSelection {
    AudioSourceSelection {
        id: id.into(),
        name: id.into(),
        direction: AudioSourceDirection::Input,
    }
}
fn wait_for(
    service: &RecordingService,
    predicate: impl Fn(&RecordingState) -> bool,
) -> RecordingState {
    for _ in 0..100 {
        let state = service.state();
        if predicate(&state) {
            return state;
        };
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "coordinator did not reach expected state: {:?}",
        service.state().status
    )
}

struct FakeCapture {
    devices: HashMap<String, FakeDevice>,
}
struct FakeDevice {
    format: CaptureFormat,
    fail_build: bool,
    fail_play: bool,
    emit_error: bool,
    sent: Arc<AtomicBool>,
    dropped_stream: Arc<AtomicU64>,
    drop_samples: u64,
    malformed: bool,
}
struct FakeStream {
    sink: Option<CaptureSink>,
    fail_play: bool,
    emit_error: bool,
    sent: Arc<AtomicBool>,
    dropped_stream: Arc<AtomicU64>,
    drop_samples: u64,
    malformed: bool,
}
impl AudioCapture for FakeCapture {
    fn resolve(&self, source: &AudioSourceSelection) -> Result<Box<dyn CaptureDevice>, String> {
        self.devices
            .get(&source.id)
            .cloned()
            .map(|x| Box::new(x) as Box<dyn CaptureDevice>)
            .ok_or_else(|| "missing fake device".into())
    }
}
impl Clone for FakeDevice {
    fn clone(&self) -> Self {
        Self {
            format: self.format,
            fail_build: self.fail_build,
            fail_play: self.fail_play,
            emit_error: self.emit_error,
            sent: self.sent.clone(),
            dropped_stream: self.dropped_stream.clone(),
            drop_samples: self.drop_samples,
            malformed: self.malformed,
        }
    }
}
impl CaptureDevice for FakeDevice {
    fn format(&self) -> CaptureFormat {
        self.format
    }
    fn build(&self, sink: CaptureSink) -> Result<Box<dyn CaptureStream>, String> {
        if self.fail_build {
            Err("build failure".into())
        } else {
            Ok(Box::new(FakeStream {
                sink: Some(sink),
                fail_play: self.fail_play,
                emit_error: self.emit_error,
                sent: self.sent.clone(),
                dropped_stream: self.dropped_stream.clone(),
                drop_samples: self.drop_samples,
                malformed: self.malformed,
            }))
        }
    }
}
impl Drop for FakeStream {
    fn drop(&mut self) {
        self.dropped_stream.fetch_add(1, Ordering::SeqCst);
    }
}
impl CaptureStream for FakeStream {
    fn play(&self) -> Result<(), String> {
        if self.fail_play {
            return Err("play failure".into());
        }
        if !self.sent.swap(true, Ordering::SeqCst) {
            let sink = self.sink.as_ref().unwrap();
            if self.drop_samples > 0 {
                sink.dropped.fetch_add(self.drop_samples, Ordering::SeqCst);
            }
            let _ = sink.sender.try_send(AudioPacket {
                samples: if self.malformed {
                    vec![0.25]
                } else {
                    vec![0.25; 1]
                },
                offset_frames: 0,
            });
            if self.emit_error {
                let _ = sink.errors.try_send("fake stream fault".into());
            }
        }
        Ok(())
    }
}
fn backend(devices: Vec<(&str, bool, bool, bool)>) -> Box<dyn AudioCapture> {
    let mut map = HashMap::new();
    for (id, build, play, error) in devices {
        map.insert(
            id.into(),
            FakeDevice {
                format: CaptureFormat {
                    sample_rate: 8_000,
                    channels: 1,
                },
                fail_build: build,
                fail_play: play,
                emit_error: error,
                sent: Arc::new(AtomicBool::new(false)),
                dropped_stream: Arc::new(AtomicU64::new(0)),
                drop_samples: 0,
                malformed: false,
            },
        );
    }
    Box::new(FakeCapture { devices: map })
}

fn backend_with_drop(drop_counter: Arc<AtomicU64>) -> Box<dyn AudioCapture> {
    let mut devices = HashMap::new();
    for (id, fail_build) in [("a", false), ("b", true)] {
        devices.insert(
            id.into(),
            FakeDevice {
                format: CaptureFormat {
                    sample_rate: 8_000,
                    channels: 1,
                },
                fail_build,
                fail_play: false,
                emit_error: false,
                sent: Arc::new(AtomicBool::new(false)),
                dropped_stream: drop_counter.clone(),
                drop_samples: 0,
                malformed: false,
            },
        );
    }
    Box::new(FakeCapture { devices })
}

fn fault_backend(channels: u16, drop_samples: u64, malformed: bool) -> Box<dyn AudioCapture> {
    let mut devices = HashMap::new();
    devices.insert(
        "a".into(),
        FakeDevice {
            format: CaptureFormat {
                sample_rate: 8_000,
                channels,
            },
            fail_build: false,
            fail_play: false,
            emit_error: false,
            sent: Arc::new(AtomicBool::new(false)),
            dropped_stream: Arc::new(AtomicU64::new(0)),
            drop_samples,
            malformed,
        },
    );
    Box::new(FakeCapture { devices })
}

#[test]
fn fake_two_source_start_stop_records_all_tracks() {
    let dir = temp_dir("two");
    let service = RecordingService::with_backend(dir.clone(), move || {
        backend(vec![("a", false, false, false), ("b", false, false, false)])
    });
    service.start(vec![source("a"), source("b")]).unwrap();
    service.stop().unwrap();
    let state = wait_for(&service, |s| s.status == "idle" && !s.recent.is_empty());
    assert_eq!(state.recent[0].tracks.len(), 2);
    assert!(state.recent[0].tracks.iter().all(|t| t.frames_written > 0));
    service.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn fake_stream_fault_finalizes_without_active_recording() {
    let dir = temp_dir("fault");
    let service = RecordingService::with_backend(dir.clone(), move || {
        backend(vec![("a", false, false, true)])
    });
    service.start(vec![source("a")]).unwrap();
    let state = wait_for(&service, |s| s.active.is_none() && !s.recent.is_empty());
    assert_eq!(state.recent[0].status, "failed");
    service.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn duplicate_start_keeps_one_recording_and_empty_sources_create_none() {
    let dir = temp_dir("duplicate");
    let service = RecordingService::with_backend(dir.clone(), move || {
        backend(vec![("a", false, false, false)])
    });
    assert!(service.start(vec![]).is_err());
    assert!(service.state().recent.is_empty());
    service.start(vec![source("a")]).unwrap();
    assert!(service.start(vec![source("a")]).is_err());
    service.stop().unwrap();
    let state = wait_for(&service, |s| s.status == "idle" && s.recent.len() == 1);
    assert_eq!(state.recent.len(), 1);
    service.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn partial_second_build_failure_drops_first_stream_and_persists_failure() {
    let dir = temp_dir("partial");
    let drops = Arc::new(AtomicU64::new(0));
    let expected = drops.clone();
    let service = RecordingService::with_backend(dir.clone(), move || backend_with_drop(expected));
    assert!(service.start(vec![source("a"), source("b")]).is_err());
    wait_for(&service, |s| s.active.is_none());
    assert!(drops.load(Ordering::SeqCst) >= 1);
    assert!(RecordingStore::open(&dir)
        .unwrap()
        .recent()
        .unwrap()
        .iter()
        .any(|r| r.status == "failed"));
    service.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn graceful_shutdown_drains_and_history_survives_restart() {
    let dir = temp_dir("restart");
    let service = RecordingService::with_backend(dir.clone(), move || {
        backend(vec![("a", false, false, false)])
    });
    service.start(vec![source("a")]).unwrap();
    service.shutdown().unwrap();
    let after = RecordingStore::open(&dir).unwrap().recent().unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].status, "recorded");
    let service2 = RecordingService::with_backend(dir.clone(), move || {
        backend(vec![("a", false, false, false)])
    });
    let state = wait_for(&service2, |s| !s.recent.is_empty());
    assert_eq!(state.recent.len(), 1);
    service2.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn dropped_samples_fault_finalizes_failed_with_exact_count() {
    let dir = temp_dir("drops");
    let service = RecordingService::with_backend(dir.clone(), move || fault_backend(1, 4, false));
    service.start(vec![source("a")]).unwrap();
    let state = wait_for(&service, |s| s.active.is_none() && !s.recent.is_empty());
    assert_eq!(state.recent[0].status, "failed");
    assert_eq!(state.recent[0].tracks[0].dropped_samples, 4);
    service.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn malformed_packet_writer_failure_never_records_success() {
    let dir = temp_dir("malformed");
    let service = RecordingService::with_backend(dir.clone(), move || fault_backend(2, 0, true));
    service.start(vec![source("a")]).unwrap();
    let state = wait_for(&service, |s| s.active.is_none() && !s.recent.is_empty());
    assert_eq!(state.recent[0].status, "failed");
    assert!(state.recent[0].tracks[0].error.is_some());
    service.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn invalid_database_startup_keeps_bytes_and_allows_shutdown() {
    let dir = temp_dir("invalid-db");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("recordings.sqlite3");
    let bytes = b"not sqlite";
    std::fs::write(&db, bytes).unwrap();
    let service = RecordingService::with_backend(dir.clone(), move || {
        backend(vec![("a", false, false, false)])
    });
    wait_for(&service, |s| s.status == "failed");
    assert!(service.shutdown().is_ok());
    assert_eq!(std::fs::read(&db).unwrap(), bytes);
    let _ = std::fs::remove_dir_all(dir);
}
