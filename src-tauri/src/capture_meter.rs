use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

const FLOOR_DBFS: f32 = -96.0;
const SIGNAL_DBFS: f32 = -60.0;
const CLIP_LINEAR: f32 = 0.999;
const PEAK_HOLD_MS: u64 = 250;
const STALE_MS: u64 = 1_500;

#[derive(Clone, Debug, Serialize)]
pub struct SourceLevel {
    pub source_id: String,
    pub peak_dbfs: f32,
    pub rms_dbfs: f32,
    pub signal_detected: bool,
    pub clipping: bool,
    pub last_buffer_age_ms: Option<u64>,
}

/// Callback-safe signal telemetry. It never owns audio, allocates, locks, or
/// communicates with persistence; it only retains scalar measurements.
pub(crate) struct CaptureMeter {
    peak_bits: AtomicU32,
    latest_peak_bits: AtomicU32,
    rms_bits: AtomicU32,
    peak_until_ms: AtomicU64,
    last_buffer_ms: AtomicU64,
    seen: AtomicBool,
}

impl Default for CaptureMeter {
    fn default() -> Self {
        Self {
            peak_bits: AtomicU32::new(0.0f32.to_bits()),
            latest_peak_bits: AtomicU32::new(0.0f32.to_bits()),
            rms_bits: AtomicU32::new(0.0f32.to_bits()),
            peak_until_ms: AtomicU64::new(0),
            last_buffer_ms: AtomicU64::new(0),
            seen: AtomicBool::new(false),
        }
    }
}

impl CaptureMeter {
    pub(crate) fn observe(&self, samples: &[f32], now_ms: u64) {
        let mut peak = 0.0f32;
        let mut sum_squares = 0.0f64;
        let mut count = 0u64;
        for &sample in samples {
            if !sample.is_finite() {
                continue;
            }
            let magnitude = sample.abs().min(1.0);
            peak = peak.max(magnitude);
            sum_squares += f64::from(magnitude) * f64::from(magnitude);
            count += 1;
        }
        if count == 0 {
            return;
        }
        self.last_buffer_ms.store(now_ms, Ordering::Relaxed);
        self.seen.store(true, Ordering::Relaxed);
        self.rms_bits.store(
            ((sum_squares / count as f64).sqrt() as f32).to_bits(),
            Ordering::Relaxed,
        );
        self.latest_peak_bits
            .store(peak.to_bits(), Ordering::Relaxed);
        let held_until = self.peak_until_ms.load(Ordering::Relaxed);
        let held_peak = f32::from_bits(self.peak_bits.load(Ordering::Relaxed));
        if now_ms >= held_until || peak >= held_peak {
            self.peak_bits.store(peak.to_bits(), Ordering::Relaxed);
            self.peak_until_ms
                .store(now_ms.saturating_add(PEAK_HOLD_MS), Ordering::Relaxed);
        }
    }

    pub(crate) fn snapshot(&self, source_id: String, now_ms: u64) -> SourceLevel {
        let last = self.last_buffer_ms.load(Ordering::Relaxed);
        let age = self
            .seen
            .load(Ordering::Relaxed)
            .then(|| now_ms.saturating_sub(last));
        if age.map_or(true, |age| age > STALE_MS) {
            return SourceLevel {
                source_id,
                peak_dbfs: FLOOR_DBFS,
                rms_dbfs: FLOOR_DBFS,
                signal_detected: false,
                clipping: false,
                last_buffer_age_ms: age,
            };
        }
        let peak = if now_ms <= self.peak_until_ms.load(Ordering::Relaxed) {
            f32::from_bits(self.peak_bits.load(Ordering::Relaxed))
        } else {
            // The display hold has elapsed, but a fresh quiet/non-clipping
            // callback must not briefly become digital silence before the
            // next packet arrives.
            f32::from_bits(self.latest_peak_bits.load(Ordering::Relaxed))
        };
        let rms = f32::from_bits(self.rms_bits.load(Ordering::Relaxed));
        let peak_dbfs = dbfs(peak);
        let rms_dbfs = dbfs(rms);
        SourceLevel {
            source_id,
            peak_dbfs,
            rms_dbfs,
            signal_detected: peak_dbfs >= SIGNAL_DBFS,
            clipping: peak.is_finite() && peak >= CLIP_LINEAR,
            last_buffer_age_ms: age,
        }
    }
}

fn dbfs(value: f32) -> f32 {
    if !value.is_finite() || value <= 0.0 {
        FLOOR_DBFS
    } else {
        (20.0 * value.min(1.0).log10()).clamp(FLOOR_DBFS, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_telemetry_is_silent_and_bounded() {
        let meter = CaptureMeter::default();
        meter.observe(&[1.0, 0.5], 10);
        let live = meter.snapshot("source".into(), 20);
        assert_eq!(live.peak_dbfs, 0.0);
        assert!(live.clipping && live.signal_detected);
        let stale = meter.snapshot("source".into(), 1_511);
        assert_eq!(stale.peak_dbfs, FLOOR_DBFS);
        assert!(!stale.signal_detected && !stale.clipping);
    }

    #[test]
    fn peak_hold_expiry_keeps_fresh_latest_signal_and_zero_time_heartbeat() {
        let meter = CaptureMeter::default();
        meter.observe(&[0.5, f32::NAN], 0);
        assert!(meter.snapshot("source".into(), 250).signal_detected);
        assert!(meter.snapshot("source".into(), 251).signal_detected);
        assert_eq!(
            meter.snapshot("source".into(), 1).last_buffer_age_ms,
            Some(1)
        );
        assert!(!meter.snapshot("source".into(), 1_501).signal_detected);
    }

    #[test]
    fn peak_hold_expiry_uses_latest_live_buffer_without_zero_glitch() {
        let meter = CaptureMeter::default();
        meter.observe(&[1.0], 0);
        meter.observe(&[0.02], 100);
        let level = meter.snapshot("source".into(), 251);
        assert!(level.peak_dbfs > FLOOR_DBFS);
        assert!(!level.clipping);
    }

    #[test]
    fn clipping_clears_after_fresh_quiet_buffer_and_eventually_stales() {
        let meter = CaptureMeter::default();
        meter.observe(&[1.0], 0);
        assert!(meter.snapshot("source".into(), 10).clipping);
        meter.observe(&[0.01], 300);
        let quiet = meter.snapshot("source".into(), 301);
        assert!(!quiet.clipping);
        let stale = meter.snapshot("source".into(), 1_801);
        assert_eq!(stale.last_buffer_age_ms, Some(1_501));
        assert_eq!(stale.peak_dbfs, FLOOR_DBFS);
        assert!(!stale.clipping && !stale.signal_detected);
    }
}
