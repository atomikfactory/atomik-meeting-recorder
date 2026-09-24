use super::*;
use std::sync::Mutex;

#[test]
fn shutdown_waits_for_inflight_manual_start_and_forbids_late_starts() {
    use std::sync::mpsc;
    struct BlockingRecorder {
        inner: Fake,
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl RecordingBoundary for BlockingRecorder {
        fn active_id(&self) -> Option<String> {
            self.inner.active_id()
        }
        fn start(&self) -> Result<String, String> {
            self.entered.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            self.inner.start()
        }
        fn stop(&self, id: &str) -> Result<(), String> {
            self.inner.stop(id)
        }
    }
    let (entered, entry) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let recorder = Arc::new(BlockingRecorder {
        inner: Fake::default(),
        entered,
        release: Mutex::new(released),
    });
    let service = DetectionService {
        core: Arc::new(Mutex::new(Core::new(&path("shutdown"), false))),
        recorder: recorder.clone(),
        stopped: Arc::new(AtomicBool::new(false)),
    };
    let start_service = service.clone();
    let start = thread::spawn(move || start_service.manual_start());
    entry.recv_timeout(Duration::from_secs(5)).unwrap();
    let exit_service = service.clone();
    let (done, completed) = mpsc::sync_channel(1);
    let exit = thread::spawn(move || {
        exit_service.shutdown();
        done.send(()).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !service.stopped.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::yield_now();
    }
    assert!(service.stopped.load(Ordering::Acquire));
    assert!(
        completed.try_recv().is_err(),
        "Shutdown must wait for the in-flight effect"
    );
    release.send(()).unwrap();
    assert!(start.join().unwrap().is_ok());
    completed.recv_timeout(Duration::from_secs(5)).unwrap();
    exit.join().unwrap();
    assert!(service.manual_start().is_err());
    assert!(service.approve_pending("old").is_err());
    assert_eq!(*recorder.inner.starts.lock().unwrap(), 1);
    service.manual_stop().unwrap();
}

#[test]
fn accepted_prompt_cannot_start_after_settings_disable_or_unknown_sample() {
    for disabled in [true, false] {
        let r = Fake::default();
        let mut c = Core::new(&path("accepted-expiry"), false);
        let id = prompt(&mut c, &r, 0);
        c.approve(&id).unwrap();
        c.step(
            Duration::from_secs(3),
            if disabled { yes() } else { bad() },
            if disabled {
                Ok(DetectionSettings::new(false, 2, 5))
            } else {
                set()
            },
            &r,
        );
        for t in 4..9 {
            c.step(Duration::from_secs(t), yes(), set(), &r);
        }
        assert_eq!(*r.starts.lock().unwrap(), 0);
        assert!(c.approve(&id).is_err());
        assert_eq!(c.state.phase, "awaiting_approval");
    }
}

#[derive(Default)]
struct Fake {
    active: Mutex<Option<String>>,
    starts: Mutex<u32>,
    attempts: Mutex<u32>,
    stops: Mutex<u32>,
    fail_start: Mutex<bool>,
    fail_stop: Mutex<bool>,
}
impl RecordingBoundary for Fake {
    fn active_id(&self) -> Option<String> {
        self.active.lock().unwrap().clone()
    }
    fn start(&self) -> Result<String, String> {
        *self.attempts.lock().unwrap() += 1;
        if *self.fail_start.lock().unwrap() {
            return Err("start failed".into());
        };
        let mut n = self.starts.lock().unwrap();
        *n += 1;
        let id = format!("r{n}");
        *self.active.lock().unwrap() = Some(id.clone());
        Ok(id)
    }
    fn stop(&self, id: &str) -> Result<(), String> {
        if *self.fail_stop.lock().unwrap() {
            return Err("stop failed".into());
        };
        if self.active_id().as_deref() != Some(id) {
            return Err("wrong recording".into());
        };
        *self.stops.lock().unwrap() += 1;
        *self.active.lock().unwrap() = None;
        Ok(())
    }
}
fn path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("mr-detect-{name}-{}.sqlite", uuid::Uuid::new_v4()))
}
fn yes() -> SignalSnapshot {
    SignalSnapshot {
        supported: true,
        meeting: Some("zoom".into()),
        detail: "Zoom".into(),
        error: None,
    }
}
fn no() -> SignalSnapshot {
    SignalSnapshot {
        supported: true,
        meeting: None,
        detail: "none".into(),
        error: None,
    }
}
fn bad() -> SignalSnapshot {
    SignalSnapshot {
        supported: true,
        meeting: None,
        detail: "bad".into(),
        error: Some("bad".into()),
    }
}
fn set() -> Result<DetectionSettings, String> {
    Ok(DetectionSettings::new(true, 2, 5))
}
fn prompt(c: &mut Core, r: &Fake, t: u64) -> String {
    assert!(!c.step(Duration::from_secs(t), yes(), set(), r));
    assert!(c.step(Duration::from_secs(t + 2), yes(), set(), r));
    c.state.approval_id.clone().unwrap()
}

#[test]
fn debounce_prompt_never_starts_before_approval() {
    let r = Fake::default();
    let mut c = Core::new(&path("debounce"), false);
    let id = prompt(&mut c, &r, 0);
    assert_eq!(*r.starts.lock().unwrap(), 0);
    assert_eq!(c.state.phase, "awaiting_approval");
    assert!(c.approve(&id).is_ok());
    c.step(Duration::from_secs(3), yes(), set(), &r);
    assert_eq!(*r.starts.lock().unwrap(), 1);
}
#[test]
fn wrong_and_repeated_approval_ids_are_rejected() {
    let r = Fake::default();
    let mut c = Core::new(&path("tokens"), false);
    let id = prompt(&mut c, &r, 0);
    assert!(c.approve("old").is_err());
    c.approve(&id).unwrap();
    assert!(c.approve(&id).is_err());
    c.step(Duration::from_secs(3), yes(), set(), &r);
    assert_eq!(*r.starts.lock().unwrap(), 1);
}
#[test]
fn provider_change_invalidates_prompt() {
    let r = Fake::default();
    let mut c = Core::new(&path("provider"), false);
    let id = prompt(&mut c, &r, 0);
    let mut teams = yes();
    teams.meeting = Some("teams".into());
    c.step(Duration::from_secs(3), teams, set(), &r);
    assert!(c.approve(&id).is_err());
    assert_eq!(*r.starts.lock().unwrap(), 0);
}
#[test]
fn absence_invalidates_prompt() {
    let r = Fake::default();
    let mut c = Core::new(&path("absence"), false);
    let id = prompt(&mut c, &r, 0);
    c.step(Duration::from_secs(3), no(), set(), &r);
    assert!(c.approve(&id).is_err());
}
#[test]
fn unknown_invalidates_prompt() {
    let r = Fake::default();
    let mut c = Core::new(&path("unknown-prompt"), false);
    let id = prompt(&mut c, &r, 0);
    c.step(Duration::from_secs(3), bad(), set(), &r);
    assert!(c.approve(&id).is_err());
}
#[test]
fn brief_loss_does_not_stop_owned_recording() {
    let r = Fake::default();
    let mut c = Core::new(&path("brief"), false);
    let id = prompt(&mut c, &r, 0);
    c.approve(&id).unwrap();
    c.step(Duration::from_secs(3), yes(), set(), &r);
    c.step(Duration::from_secs(4), no(), set(), &r);
    c.step(Duration::from_secs(7), yes(), set(), &r);
    assert_eq!(*r.stops.lock().unwrap(), 0);
}
#[test]
fn grace_stops_and_rearms_after_verified_absence() {
    let p = path("grace");
    let r = Fake::default();
    let mut c = Core::new(&p, false);
    let id = prompt(&mut c, &r, 0);
    c.approve(&id).unwrap();
    c.step(Duration::from_secs(3), yes(), set(), &r);
    c.step(Duration::from_secs(4), no(), set(), &r);
    c.step(Duration::from_secs(9), no(), set(), &r);
    assert_eq!(*r.stops.lock().unwrap(), 1);
    assert!(!c.persisted_latch);
}
#[test]
fn disable_stops_owned_but_retains_latch() {
    let r = Fake::default();
    let mut c = Core::new(&path("disable"), false);
    let id = prompt(&mut c, &r, 0);
    c.approve(&id).unwrap();
    c.step(Duration::from_secs(3), yes(), set(), &r);
    c.step(
        Duration::from_secs(4),
        yes(),
        Ok(DetectionSettings::new(false, 2, 5)),
        &r,
    );
    assert_eq!(*r.stops.lock().unwrap(), 1);
    assert!(c.persisted_latch);
}
#[test]
fn unknown_timeout_stops_owned_and_retains_latch() {
    let r = Fake::default();
    let mut c = Core::new(&path("unknown-stop"), false);
    let id = prompt(&mut c, &r, 0);
    c.approve(&id).unwrap();
    c.step(Duration::from_secs(3), yes(), set(), &r);
    c.step(Duration::from_secs(4), bad(), set(), &r);
    c.step(Duration::from_secs(9), bad(), set(), &r);
    assert_eq!(*r.stops.lock().unwrap(), 1);
    assert!(c.persisted_latch);
}
#[test]
fn restart_suppresses_until_absence_is_persisted() {
    let p = path("restart");
    let r = Fake::default();
    let mut a = Core::new(&p, false);
    let id = prompt(&mut a, &r, 0);
    a.dismiss(&id).unwrap();
    let mut b = Core::new(&p, false);
    b.step(Duration::from_secs(0), yes(), set(), &r);
    assert_eq!(b.state.phase, "suppressed");
    b.step(Duration::from_secs(1), no(), set(), &r);
    b.step(Duration::from_secs(6), no(), set(), &r);
    assert!(!b.persisted_latch);
    assert!(!Core::new(&p, false).persisted_latch);
}
#[test]
fn manual_recording_is_never_auto_stopped() {
    let r = Fake::default();
    r.start().unwrap();
    let mut c = Core::new(&path("manual"), false);
    c.step(Duration::from_secs(0), no(), set(), &r);
    c.step(Duration::from_secs(8), no(), set(), &r);
    assert_eq!(*r.stops.lock().unwrap(), 0);
}
#[test]
fn manual_stop_suppresses_episode() {
    let r = Fake::default();
    let mut c = Core::new(&path("manual-stop"), false);
    c.manual_start(&r).unwrap();
    c.manual_stop(&r).unwrap();
    c.step(Duration::from_secs(0), yes(), set(), &r);
    assert_eq!(c.state.phase, "suppressed");
}
#[test]
fn engine_failure_reconciles_without_stopping_replaced_manual() {
    let r = Fake::default();
    let mut c = Core::new(&path("reconcile"), false);
    let id = prompt(&mut c, &r, 0);
    c.approve(&id).unwrap();
    c.step(Duration::from_secs(3), yes(), set(), &r);
    *r.active.lock().unwrap() = Some("manual-new".into());
    c.step(Duration::from_secs(4), yes(), set(), &r);
    for t in 5..15 {
        c.step(Duration::from_secs(t), no(), set(), &r);
    }
    assert!(c.owned_id.is_none());
    assert_eq!(*r.stops.lock().unwrap(), 0);
}
#[test]
fn settings_error_never_starts() {
    let r = Fake::default();
    let mut c = Core::new(&path("settings"), false);
    c.step(
        Duration::from_secs(0),
        yes(),
        Err("db unreadable".into()),
        &r,
    );
    assert_eq!(*r.starts.lock().unwrap(), 0);
    assert_eq!(c.state.phase, "error");
}
#[test]
fn journal_failure_blocks_auto_but_not_manual() {
    let base = std::env::temp_dir().join(format!("mr-journal-file-{}", uuid::Uuid::new_v4()));
    fs::write(&base, b"x").unwrap();
    let r = Fake::default();
    let mut c = Core::new(&base.join("x.sqlite"), false);
    assert!(c.journal_error.is_some());
    assert!(!c.step(Duration::ZERO, yes(), set(), &r));
    assert_eq!(*r.attempts.lock().unwrap(), 0);
    c.manual_start(&r).unwrap();
    c.manual_stop(&r).unwrap();
    assert_eq!(*r.stops.lock().unwrap(), 1);
}
#[test]
fn stop_failure_keeps_owner_for_retry() {
    let r = Fake::default();
    let mut c = Core::new(&path("retry"), false);
    let id = prompt(&mut c, &r, 0);
    c.approve(&id).unwrap();
    c.step(Duration::from_secs(3), yes(), set(), &r);
    *r.fail_stop.lock().unwrap() = true;
    c.step(Duration::from_secs(4), no(), set(), &r);
    c.step(Duration::from_secs(9), no(), set(), &r);
    assert!(c.owned_id.is_some());
    *r.fail_stop.lock().unwrap() = false;
    c.step(Duration::from_secs(10), no(), set(), &r);
    assert!(c.owned_id.is_none());
}
#[test]
fn gap_invalidates_prompt_and_stops_owned_not_manual() {
    let r = Fake::default();
    let mut c = Core::new(&path("gap"), false);
    let id = prompt(&mut c, &r, 0);
    c.step(Duration::from_secs(10), yes(), set(), &r);
    assert!(c.approve(&id).is_err());
    c.manual_start(&r).unwrap();
    c.step(Duration::from_secs(20), no(), set(), &r);
    assert_eq!(*r.stops.lock().unwrap(), 0);
}
#[test]
fn observation_gap_stops_approved_capture_and_retains_restart_latch() {
    let p = path("owned-gap");
    let r = Fake::default();
    let mut c = Core::new(&p, false);
    let id = prompt(&mut c, &r, 0);
    c.approve(&id).unwrap();
    c.step(Duration::from_secs(3), yes(), set(), &r);
    c.step(Duration::from_secs(30), yes(), set(), &r);
    assert_eq!(*r.stops.lock().unwrap(), 1);
    assert!(c.owned_id.is_none());
    assert!(c.persisted_latch);
    let mut reopened = Core::new(&p, false);
    for t in 0..10 {
        reopened.step(Duration::from_secs(t), yes(), set(), &r);
    }
    assert_eq!(reopened.state.phase, "suppressed");
    assert_eq!(*r.attempts.lock().unwrap(), 1);
}
#[test]
fn inhibited_session_never_prompts_or_changes_the_persisted_latch() {
    let p = path("inhibited");
    let r = Fake::default();
    let mut c = Core::new(&p, true);
    for t in 0..10 {
        assert!(!c.step(Duration::from_secs(t), yes(), set(), &r));
    }
    assert!(c.state.automation_inhibited);
    assert!(!c.state.enabled);
    assert!(c.approve("anything").is_err());
    c.manual_start(&r).unwrap();
    c.manual_stop(&r).unwrap();
    assert!(!Core::new(&p, false).persisted_latch);
}
#[test]
fn write_failure_blocks_approved_auto_but_manual_works() {
    let r = Fake::default();
    let mut c = Core::new(&path("write"), false);
    let id = prompt(&mut c, &r, 0);
    c.journal
        .as_ref()
        .unwrap()
        .conn
        .execute_batch("PRAGMA query_only=ON")
        .unwrap();
    c.approve(&id).unwrap();
    c.step(Duration::from_secs(3), yes(), set(), &r);
    assert_eq!(*r.attempts.lock().unwrap(), 0);
    c.manual_start(&r).unwrap();
    assert_eq!(*r.starts.lock().unwrap(), 1);
}
#[test]
fn start_failure_is_latched_without_hot_retry() {
    let r = Fake::default();
    *r.fail_start.lock().unwrap() = true;
    let mut c = Core::new(&path("start-fail"), false);
    let id = prompt(&mut c, &r, 0);
    c.approve(&id).unwrap();
    c.step(Duration::from_secs(3), yes(), set(), &r);
    c.step(Duration::from_secs(4), yes(), set(), &r);
    assert_eq!(*r.attempts.lock().unwrap(), 1);
    assert!(c.persisted_latch);
}
