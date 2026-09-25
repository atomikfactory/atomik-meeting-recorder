//! Linux system-output ("what the speakers play") capture via PulseAudio's
//! client protocol, which PipeWire also serves through its `pipewire-pulse`
//! compatibility layer on modern distros (Ubuntu 22.04+, Fedora, Debian 12,
//! Arch). See docs/audio-devices.md for the design decision and alternatives
//! considered (PipeWire-native API, JACK).
//!
//! `target_os = "linux"` only; pure/testable pieces (id scheme, sample
//! conversion, fragment sizing) live in `system_output.rs` so they compile
//! and run under `cargo test` on every platform.

use crate::recording::{CaptureDevice, CaptureFormat, CaptureSink, CaptureStream};
use crate::recording_wav::AudioPacket;
use crate::system_output::{bytes_per_fragment, f32le_bytes_to_samples, frames_in};
use libpulse_binding as pulse;
use libpulse_simple_binding::Simple;
use pulse::callbacks::ListResult;
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::def::BufferAttr;
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::proplist::Proplist;
use pulse::sample::{Format as PulseFormat, Spec};
use pulse::stream::Direction;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const APP_NAME: &str = "meeting-recorder";
/// Fragment size for blocking reads: small enough to keep loopback latency
/// low and the capture thread responsive to Stop, large enough to avoid
/// excessive syscall/wakeup overhead.
const FRAGMENT_MS: u32 = 20;
/// Upper bound on how long enumeration will wait for the server to connect
/// or answer a query. `Mainloop::iterate(true)` blocks on the underlying
/// socket with no built-in timeout, so a stale/unreachable `$PULSE_SERVER`
/// (e.g. a remote TCP address that never refutes the connection, unlike a
/// local socket with nothing listening) or a server that accepts the
/// connection but never replies would otherwise hang this call, and with it
/// the UI's device-refresh path, indefinitely.
const ENUMERATE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(crate) struct PulseSinkInfo {
    pub(crate) sink_name: String,
    pub(crate) description: String,
    pub(crate) monitor_source_name: String,
    pub(crate) is_default: bool,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
}

/// Connects to the user's PulseAudio/PipeWire server, lists sinks and their
/// monitor sources, and disconnects. A short-lived connection per call keeps
/// this consistent with the CPAL inventory snapshot's own "point in time,
/// re-run on refresh" contract (see audio_devices.rs) rather than holding a
/// persistent context just for enumeration.
pub(crate) fn enumerate_sinks() -> Result<Vec<PulseSinkInfo>, String> {
    let mut proplist =
        Proplist::new().ok_or_else(|| "Could not create a PulseAudio property list".to_string())?;
    proplist
        .set_str(pulse::proplist::properties::APPLICATION_NAME, APP_NAME)
        .map_err(|_| "Could not set the PulseAudio application name".to_string())?;
    let mut mainloop =
        Mainloop::new().ok_or_else(|| "Could not create a PulseAudio mainloop".to_string())?;
    let context = Rc::new(RefCell::new(
        Context::new_with_proplist(&mainloop, APP_NAME, &proplist)
            .ok_or_else(|| "Could not create a PulseAudio context".to_string())?,
    ));
    context
        .borrow_mut()
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .map_err(|e| format!("Could not connect to the PulseAudio/PipeWire server: {e}"))?;
    let deadline = Instant::now() + ENUMERATE_TIMEOUT;
    wait_for_context_ready(&mut mainloop, &context, deadline)?;

    let default_sink_name = Rc::new(RefCell::new(String::new()));
    {
        let default_sink_name = default_sink_name.clone();
        let done = Rc::new(RefCell::new(false));
        let done_cb = done.clone();
        context
            .borrow()
            .introspect()
            .get_server_info(move |info| {
                if let Some(name) = &info.default_sink_name {
                    *default_sink_name.borrow_mut() = name.to_string();
                }
                *done_cb.borrow_mut() = true;
            });
        pump_until(&mut mainloop, &done, deadline)?;
    }

    let sinks = Rc::new(RefCell::new(Vec::new()));
    let done = Rc::new(RefCell::new(false));
    {
        let sinks = sinks.clone();
        let done_cb = done.clone();
        let default_sink_name = default_sink_name.borrow().clone();
        context
            .borrow()
            .introspect()
            .get_sink_info_list(move |result| match result {
                ListResult::Item(item) => {
                    let name = item.name.as_deref().unwrap_or_default().to_string();
                    if name.is_empty() {
                        return;
                    }
                    let monitor_source_name = item
                        .monitor_source_name
                        .as_deref()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("{name}.monitor"));
                    let description = item.description.as_deref().unwrap_or(&name).to_string();
                    sinks.borrow_mut().push(PulseSinkInfo {
                        is_default: name == default_sink_name,
                        sample_rate: item.sample_spec.rate,
                        channels: u16::from(item.sample_spec.channels),
                        sink_name: name,
                        description,
                        monitor_source_name,
                    });
                }
                ListResult::End | ListResult::Error => {
                    *done_cb.borrow_mut() = true;
                }
            });
        pump_until(&mut mainloop, &done, deadline)?;
    }

    context.borrow_mut().disconnect();
    Ok(Rc::try_unwrap(sinks)
        .map(RefCell::into_inner)
        .unwrap_or_default())
}

fn wait_for_context_ready(
    mainloop: &mut Mainloop,
    context: &Rc<RefCell<Context>>,
    deadline: Instant,
) -> Result<(), String> {
    loop {
        // Non-blocking: `iterate(true)` blocks on the socket with no timeout
        // of its own, which would defeat `deadline` for a server that never
        // answers (see `ENUMERATE_TIMEOUT`'s doc comment).
        match mainloop.iterate(false) {
            IterateResult::Quit(_) | IterateResult::Err(_) => {
                return Err("PulseAudio mainloop stopped while connecting".into())
            }
            IterateResult::Success(_) => {}
        }
        match context.borrow().get_state() {
            ContextState::Ready => return Ok(()),
            ContextState::Failed | ContextState::Terminated => {
                return Err(
                    "Could not connect to the PulseAudio/PipeWire server (pipewire-pulse or pulseaudio not running?)"
                        .into(),
                )
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(
                "Timed out connecting to the PulseAudio/PipeWire server".into(),
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn pump_until(
    mainloop: &mut Mainloop,
    done: &Rc<RefCell<bool>>,
    deadline: Instant,
) -> Result<(), String> {
    while !*done.borrow() {
        match mainloop.iterate(false) {
            IterateResult::Quit(_) | IterateResult::Err(_) => {
                return Err("PulseAudio mainloop stopped mid-query".into())
            }
            IterateResult::Success(_) => {}
        }
        if *done.borrow() {
            break;
        }
        if Instant::now() >= deadline {
            return Err("Timed out waiting for a reply from the PulseAudio/PipeWire server".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// A selected sink's monitor source, resolved to a concrete `CaptureDevice`.
/// Mirrors `recording.rs`'s `NativeDevice` (a resolved CPAL device) so the
/// coordinator's `prepare()`/`build()` flow does not need to know which
/// backend a track came from.
pub(crate) struct PulseMonitorDevice {
    monitor_source_name: String,
    format: CaptureFormat,
}

impl PulseMonitorDevice {
    pub(crate) fn resolve(sink_name: &str) -> Result<Self, String> {
        let sinks = enumerate_sinks()?;
        let sink = sinks
            .into_iter()
            .find(|s| s.sink_name == sink_name)
            .ok_or_else(|| {
                format!("System-output source '{sink_name}' is no longer available")
            })?;
        Ok(Self {
            monitor_source_name: sink.monitor_source_name,
            format: CaptureFormat {
                sample_rate: sink.sample_rate,
                channels: sink.channels,
            },
        })
    }
}

impl CaptureDevice for PulseMonitorDevice {
    fn format(&self) -> CaptureFormat {
        self.format
    }
    fn build(&self, sink: CaptureSink) -> Result<Box<dyn CaptureStream>, String> {
        if self.format.channels == 0 || self.format.sample_rate == 0 {
            return Err("Monitor source reported an invalid sample format".into());
        }
        Ok(Box::new(PulseCaptureStream {
            monitor_source_name: self.monitor_source_name.clone(),
            format: self.format,
            sink: Mutex::new(Some(sink)),
            stop: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
        }))
    }
}

struct PulseCaptureStream {
    monitor_source_name: String,
    format: CaptureFormat,
    sink: Mutex<Option<CaptureSink>>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl CaptureStream for PulseCaptureStream {
    fn play(&self) -> Result<(), String> {
        let sink = self
            .sink
            .lock()
            .map_err(|_| "PulseAudio capture sink lock is unavailable".to_string())?
            .take()
            .ok_or_else(|| "PulseAudio capture stream was already started".to_string())?;
        let spec = Spec {
            format: PulseFormat::F32le,
            channels: self.format.channels as u8,
            rate: self.format.sample_rate,
        };
        if !spec.is_valid() {
            return Err("PulseAudio monitor reported an invalid sample spec".into());
        }
        let fragment_bytes =
            bytes_per_fragment(self.format.sample_rate, self.format.channels, FRAGMENT_MS);
        // Low-latency, small-buffer capture: request tight attrs and let the
        // server pick sane defaults for anything left at u32::MAX.
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: fragment_bytes,
        };
        let simple = Simple::new(
            None, // default server: honors $PULSE_SERVER (used for WSLg verification)
            APP_NAME,
            Direction::Record,
            Some(&self.monitor_source_name),
            "system output",
            &spec,
            None,
            Some(&attr),
        )
        .map_err(|e| {
            format!(
                "Could not open PulseAudio monitor '{}': {e}",
                self.monitor_source_name
            )
        })?;
        let stop = self.stop.clone();
        let format = self.format;
        let source_name = self.monitor_source_name.clone();
        let handle = thread::Builder::new()
            .name("pulse-monitor-capture".into())
            .spawn(move || capture_loop(simple, sink, stop, format, fragment_bytes as usize, source_name))
            .map_err(|e| format!("Could not start the PulseAudio capture thread: {e}"))?;
        *self
            .thread
            .lock()
            .map_err(|_| "PulseAudio capture thread lock is unavailable".to_string())? =
            Some(handle);
        Ok(())
    }
}

impl Drop for PulseCaptureStream {
    fn drop(&mut self) {
        // Stop is cooperative: the blocking `read()` below returns at the
        // next fragment boundary (<= FRAGMENT_MS) and the loop then exits.
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.thread.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

/// Blocking capture loop run on its own thread. Feeds the exact same
/// `CaptureSink` (bounded handoff to the WAV writer, capture meter,
/// `FrameTimeline`) that CPAL-backed tracks use, so downstream behavior
/// (dropped-packet accounting, "settled" start-up grace, gap/timestamp
/// adjustment counts) is identical regardless of backend.
///
/// PulseAudio's blocking simple API has no per-fragment hardware timestamp
/// (unlike CPAL's WASAPI `InputCallbackInfo`), so `measured` is derived from
/// wall-clock elapsed time since the stream started rather than from a
/// device clock; a stall inside `read()` (schedule delay, server-side
/// underrun) still shows up as `measured` running ahead of the emitted frame
/// cursor and is retained/counted by `FrameTimeline` exactly as a CPAL
/// timestamp jump would be.
///
/// Unlike WASAPI loopback, a monitor source keeps delivering real (silent)
/// frames when the sink is idle rather than delivering nothing, so the
/// existing "no audio frames received" check in `recording.rs::finalize`
/// does not fire for a quiet meeting on Linux; it still fires if the sink
/// itself disappears or the source never opens.
fn capture_loop(
    simple: Simple,
    sink: CaptureSink,
    stop: Arc<AtomicBool>,
    format: CaptureFormat,
    fragment_bytes: usize,
    source_name: String,
) {
    let mut buf = vec![0u8; fragment_bytes.max(4)];
    let mut timeline = crate::recording::FrameTimeline::default();
    while !stop.load(Ordering::Relaxed) {
        match simple.read(&mut buf) {
            Ok(()) => {
                let now_ms = sink.started.elapsed().as_millis() as u64;
                sink.last_buffer_ms.store(now_ms, Ordering::Relaxed);
                let samples = f32le_bytes_to_samples(&buf);
                let frames = frames_in(samples.len(), format.channels);
                if frames == 0 {
                    continue;
                }
                let session_frames = (sink.started.elapsed().as_secs_f64()
                    * f64::from(format.sample_rate))
                .round() as u64;
                let offset = match timeline.offset(
                    Some(session_frames),
                    format.sample_rate,
                    frames,
                    session_frames,
                    &sink.timestamp_adjustments,
                ) {
                    Ok(offset) => offset,
                    Err(error) => {
                        let _ = sink.errors.try_send(format!("{}: {error}", sink.source_name));
                        continue;
                    }
                };
                if timeline.is_settled() {
                    sink.settled.store(true, Ordering::Relaxed);
                }
                sink.meter.observe(&samples, now_ms);
                if sink
                    .sender
                    .try_send(AudioPacket {
                        samples,
                        offset_frames: offset,
                    })
                    .is_err()
                {
                    sink.dropped
                        .fetch_add(frames * u64::from(format.channels), Ordering::Relaxed);
                }
            }
            Err(error) => {
                let _ = sink.errors.try_send(format!(
                    "{source_name}: PulseAudio monitor read error: {error}"
                ));
                break;
            }
        }
    }
}
