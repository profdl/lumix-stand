use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::frame::MAX_DATAGRAM;
use crate::sim::{SimCamera, SimOptions};
use crate::{CaptureOutcome, Controller, Options, Phase, RequestKind, Transport, pump};

/// A simulated camera on a clock the test moves, and a controller.
struct Rig {
    clock: Arc<AtomicU64>,
    cam: SimCamera,
    ctl: Controller,
    buf: Vec<u8>,
}

impl Rig {
    fn new(f: impl FnOnce(&mut SimOptions)) -> Rig {
        let clock = Arc::new(AtomicU64::new(1_000));
        let c = clock.clone();
        let mut o = SimOptions { clock: Arc::new(move || c.load(Ordering::SeqCst)), ..Default::default() };
        f(&mut o);
        Rig { clock, cam: SimCamera::new(o), ctl: Controller::new(Options::default()), buf: vec![0; MAX_DATAGRAM] }
    }

    fn now(&self) -> u64 {
        self.clock.load(Ordering::SeqCst)
    }

    /// Run for `ms`, a pump every 10 ms.
    fn run(&mut self, ms: u64) {
        for _ in 0..ms / 10 {
            let c = self.clock.clone();
            pump(&mut self.ctl, &mut self.cam, &mut self.buf, 0, &move || c.load(Ordering::SeqCst));
            self.clock.fetch_add(10, Ordering::SeqCst);
        }
    }
}

#[test]
fn connects_and_streams() {
    let mut r = Rig::new(|_| {});
    r.ctl.connect(r.now());
    r.run(1_500);
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.phase, Phase::Streaming);
    assert!(s.frame_seq > 5, "{s:?}");
    assert!(s.fps >= 20, "{s:?}");
    assert!(!s.handshaken);
    assert_eq!(s.dropped, 0);
    assert_eq!(r.cam.stats().handshakes, 0, "no handshake when none is needed");
    assert!(r.ctl.frame().unwrap().jpeg.starts_with(&[0xFF, 0xD8]));
    assert_eq!(s.state.iter().find(|(k, _)| k == "batt").map(|(_, v)| v.as_str()), Some("3/3"));
}

#[test]
fn handshakes_when_refused() {
    let mut r = Rig::new(|o| o.require_handshake = true);
    r.ctl.connect(r.now());
    r.run(500);
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.phase, Phase::Streaming, "{:?}", s.log);
    assert!(s.handshaken);
    assert_eq!(r.cam.stats().handshakes, 1);
    let req = &r.cam.stats().requests;
    assert!(req.iter().any(|p| p.starts_with("/cam.cgi?mode=accctrl&type=req_acc&value=") && p.contains("&value2=Picabia")));
}

#[test]
fn waits_for_the_camera_to_accept_a_new_device() {
    // As a GX85 did the first time: ok_under_research, then err_reject until it takes us on.
    let mut r = Rig::new(|o| {
        o.require_handshake = true;
        o.accept_after_ms = Some(12_000);
    });
    r.ctl.connect(r.now());
    r.run(3_000);
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.phase, Phase::Connecting, "{:?}", s.log);
    assert!(s.awaiting_accept);
    r.run(12_000);
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.phase, Phase::Streaming, "{:?}", s.log);
    assert!(!s.awaiting_accept);
    assert!(s.handshaken);
    assert!(r.cam.stats().handshakes >= 2, "asked again while waiting");
}

#[test]
fn gives_up_when_never_accepted() {
    let mut r = Rig::new(|o| {
        o.require_handshake = true;
        o.accept_after_ms = Some(10_000_000);
    });
    r.ctl.connect(r.now());
    r.run(95_000);
    let s = r.ctl.snapshot(r.now());
    assert!(matches!(s.phase, Phase::Lost | Phase::Connecting), "{:?}", s.log);
    assert!(s.last_error.as_deref().is_some_and(|e| e.contains("didn't accept")), "{:?}", s.last_error);
}

#[test]
fn keep_alive_keeps_the_stream() {
    // A GX85 stops about 11.3 s after the last startstream (measured); the link must keep it going
    // for a minute.
    let mut r = Rig::new(|o| o.stream_timeout_ms = 11_000);
    r.ctl.connect(r.now());
    r.run(60_000);
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.phase, Phase::Streaming);
    assert!(!s.no_frames, "{s:?}");
    assert!(s.fps >= 20);
    assert!(r.cam.streaming());
}

#[test]
fn notices_missing_frames_and_asks_again() {
    let mut r = Rig::new(|_| {});
    r.ctl.connect(r.now());
    r.run(300);
    // The camera stops streaming on its own (as if the stream timed out).
    let mut other = r.cam.clone();
    let _ = other.get("/cam.cgi?mode=stopstream");
    let starts = r.cam.stats().start_streams;
    r.run(3_500);
    assert!(r.cam.stats().start_streams > starts, "startstream sent again");
    assert!(r.cam.streaming());
}

#[test]
fn reconnects_after_the_camera_goes_away() {
    let mut r = Rig::new(|_| {});
    r.ctl.connect(r.now());
    r.run(300);
    r.cam.set_offline(true);
    r.run(20_000);
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.phase, Phase::Lost, "{:?}", s.log);
    assert!(s.retry_in_ms.is_some());
    assert!(s.last_error.is_some());
    r.cam.set_offline(false);
    r.run(40_000);
    assert_eq!(r.ctl.phase(), Phase::Streaming, "{:?}", r.ctl.snapshot(r.now()).log);
}

#[test]
fn camera_off_at_connect_is_lost_not_stuck() {
    let mut r = Rig::new(|o| o.offline = true);
    r.ctl.connect(r.now());
    r.run(50);
    assert_eq!(r.ctl.phase(), Phase::Lost);
    assert_eq!(r.ctl.capture().unwrap_err(), "the camera isn't connected (lost)");
}

#[test]
fn captures() {
    let mut r = Rig::new(|_| {});
    r.ctl.connect(r.now());
    r.run(300);
    let id = r.ctl.capture().unwrap();
    assert_eq!(r.ctl.capture_outcome(id), Some(CaptureOutcome::Pending));
    assert!(r.ctl.capture().is_err(), "one at a time");
    r.run(20);
    assert_eq!(r.ctl.capture_outcome(id), Some(CaptureOutcome::Done));
    assert_eq!(r.cam.stats().captures, 1);
    assert_eq!(r.ctl.capture_outcome(id + 1), None);
}

#[test]
fn a_refused_capture_says_why() {
    let mut r = Rig::new(|o| o.refuse_capture = true);
    r.ctl.connect(r.now());
    r.run(300);
    let id = r.ctl.capture().unwrap();
    r.run(20);
    assert_eq!(r.ctl.capture_outcome(id), Some(CaptureOutcome::Failed("the camera refused the capture (err_busy)".into())));
    assert_eq!(r.ctl.phase(), Phase::Streaming, "a refusal isn't a lost camera");
}

#[test]
fn stop_and_restart_live_view() {
    let mut r = Rig::new(|_| {});
    r.ctl.connect(r.now());
    r.run(300);
    r.ctl.stop_live_view();
    r.run(200);
    assert_eq!(r.ctl.phase(), Phase::Connected);
    assert!(!r.cam.streaming());
    let seq = r.ctl.frame_seq();
    r.run(30_000);
    assert_eq!(r.ctl.frame_seq(), seq, "no frames while stopped");
    assert_eq!(r.ctl.phase(), Phase::Connected, "kept alive while stopped");
    // Capture still works with live view off.
    let id = r.ctl.capture().unwrap();
    r.run(20);
    assert_eq!(r.ctl.capture_outcome(id), Some(CaptureOutcome::Done));
    r.ctl.start_live_view();
    r.run(300);
    assert_eq!(r.ctl.phase(), Phase::Streaming);
    assert!(r.ctl.frame_seq() > seq);
}

#[test]
fn disconnect_stops_the_stream_and_goes_quiet() {
    let mut r = Rig::new(|_| {});
    r.ctl.connect(r.now());
    r.run(300);
    let id = r.ctl.capture().unwrap();
    r.ctl.disconnect();
    assert_eq!(r.ctl.capture_outcome(id), Some(CaptureOutcome::Failed("the camera was disconnected".into())));
    let req = r.ctl.poll(r.now()).unwrap();
    assert_eq!(req.kind, RequestKind::StopStream);
    assert!(r.ctl.poll(r.now() + 60_000).is_none());
    assert_eq!(r.ctl.phase(), Phase::Closed);
}

#[test]
fn junk_replies_and_datagrams_are_survived() {
    let mut ctl = Controller::new(Options::default());
    ctl.connect(0);
    let mut x: u64 = 7;
    for t in 0..5_000u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        if let Some(req) = ctl.poll(t * 10) {
            let body: Vec<u8> = (0..(x % 300)).map(|i| (x >> (i % 56)) as u8).collect();
            let reply = match x % 4 {
                0 => Err("timeout".into()),
                1 => Ok(crate::HttpReply { status: (x % 600) as u16, body }),
                2 => Ok(crate::HttpReply { status: 200, body: b"<camrply><result>ok</result></camrply>".to_vec() }),
                _ => Ok(crate::HttpReply { status: 200, body }),
            };
            ctl.on_reply(req.kind, reply, t * 10);
        }
        let d: Vec<u8> = (0..(x % 2000)).map(|i| [0xFF, 0xD8, 0xD9, (x >> (i % 50)) as u8][(i % 4) as usize]).collect();
        ctl.on_datagram(&d, t * 10);
        if x.is_multiple_of(97) {
            let _ = ctl.capture();
        }
        if x.is_multiple_of(501) {
            ctl.stop_live_view();
        }
        if x.is_multiple_of(503) {
            ctl.start_live_view();
        }
    }
    let s = ctl.snapshot(50_000);
    assert!(s.log.len() <= crate::controller::LOG_LINES);
}

#[test]
fn query_values_are_encoded() {
    assert_eq!(crate::controller::query_value("Pic abia&x=1"), "Pic%20abia%26x%3D1");
}
