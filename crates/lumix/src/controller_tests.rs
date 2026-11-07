use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::frame::MAX_DATAGRAM;
use crate::photos::PhotoId;
use crate::sim::FIRST_PHOTO;
use crate::sim::{SimCamera, SimOptions};
use crate::{CaptureOutcome, Controller, FocusStep, Options, Originals, Phase, RequestKind, Transport, pump};

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

#[test]
fn setting_values_keep_slashes_and_colons() {
    assert_eq!(crate::controller::setting_value("1622/256"), "1622/256");
    assert_eq!(crate::controller::setting_value("-1/3"), "-1/3");
    assert_eq!(crate::controller::setting_value("16:9"), "16:9");
    assert_eq!(crate::controller::setting_value("a&b=c d"), "a%26b%3Dc%20d");
}

/// Connected and streaming, with the first reading of the settings done.
fn connected(f: impl FnOnce(&mut SimOptions)) -> Rig {
    let mut r = Rig::new(f);
    r.ctl.connect(r.now());
    r.run(1_500);
    assert_eq!(r.ctl.phase(), Phase::Streaming);
    r
}

#[test]
fn reads_the_settings_lens_and_menu_at_connect() {
    let r = connected(|_| {});
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.commands_pending, 0, "{:?}", s.log);
    let st = &s.settings;
    assert_eq!(st.value("shtrspeed"), Some("1622/256"));
    assert_eq!(st.value("iso"), Some("auto"));
    assert_eq!(st.value("afmode"), Some("49area"), "from the menu: getsetting can't read it");
    assert_eq!(st.lens.map(|l| l.av_wide), Some(927));
    assert!(st.menu_read);
    assert!(!st.enabled("pctsiz_16m"), "16:9 at first: no 16M");
    assert!(st.enabled("pctsiz_12m"));
    assert!(r.cam.stats().frames_sent > 5, "live view ran meanwhile");
}

#[test]
fn sets_a_setting_and_reads_it_back() {
    let mut r = connected(|_| {});
    r.ctl.set("iso", "400").unwrap();
    r.ctl.set("shtrspeed", &crate::apex::format(crate::apex::third(15))).unwrap();
    r.run(300);
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.settings.value("iso"), Some("400"));
    assert_eq!(r.cam.setting("shtrspeed").as_deref(), Some("1280/256"));
    assert_eq!(s.settings.value("shtrspeed"), Some("1280/256"));
    assert!(r.cam.stats().requests.iter().any(|q| q == "/cam.cgi?mode=setsetting&type=shtrspeed&value=1280/256"));
    assert_eq!(s.phase, Phase::Streaming, "live view kept going");

    // The largest size: 4:3, then 16M, which the menu now allows.
    r.ctl.set("aspectratio", "4:3").unwrap();
    r.run(200);
    assert!(r.ctl.snapshot(r.now()).settings.enabled("pctsiz_16m"));
    r.ctl.set("pictsize", "16m").unwrap();
    r.run(200);
    assert_eq!(r.ctl.snapshot(r.now()).settings.value("pictsize"), Some("16m"));
}

#[test]
fn a_refused_setting_is_kept_until_set_again() {
    let mut r = connected(|_| {});
    r.ctl.set("iso", "123").unwrap();
    r.run(200);
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.settings.refused, Some(("iso".into(), "123".into(), "err_param".into())));
    assert_eq!(s.settings.value("iso"), Some("auto"), "unchanged");
    r.ctl.set("iso", "200").unwrap();
    r.run(200);
    assert_eq!(r.ctl.snapshot(r.now()).settings.refused, None);
}

#[test]
fn the_cameras_own_changes_are_noticed() {
    let mut r = connected(|_| {});
    r.cam.set_setting("whitebalance", "shade");
    r.run(31_000);
    assert_eq!(r.ctl.snapshot(r.now()).settings.value("whitebalance"), Some("shade"));
    assert_eq!(r.ctl.phase(), Phase::Streaming);
}

#[test]
fn focus_steps_only_in_mf() {
    let mut r = connected(|_| {});
    r.ctl.focus(FocusStep::Wide).unwrap();
    r.run(100);
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.settings.refused, Some(("focus".into(), "wide-normal".into(), "err_reject".into())));
    r.ctl.set("focusmode", "mf").unwrap();
    r.ctl.focus(FocusStep::Wide).unwrap();
    r.ctl.focus(FocusStep::TeleFast).unwrap();
    r.ctl.one_shot_af().unwrap();
    r.run(300);
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.settings.focus, Some((512 + 14 - 130, 1024)));
    assert_eq!(s.settings.refused, None, "a step that worked clears the refusal");
}

#[test]
fn lists_the_card_and_expects_each_capture_next() {
    let mut r = connected(|_| {});
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.photos.newest, Some(PhotoId(FIRST_PHOTO.0 + 2)), "three photos on the card");
    assert_eq!((s.photos.total, s.photos.generation, s.photos.error.clone()), (3, 1, None));
    assert!(!r.cam.in_play_mode());
    let id = r.ctl.capture().unwrap();
    assert_eq!(r.ctl.capture_photo(id), None, "pending");
    r.run(30);
    assert_eq!(r.ctl.capture_photo(id), Some(PhotoId(FIRST_PHOTO.0 + 3)));
    assert_eq!(r.cam.card().last(), Some(&PhotoId(FIRST_PHOTO.0 + 3)), "the guess is the camera's number");
    r.run(600);
    let mut out = Vec::new();
    let got = r.cam.clone().original(PhotoId(FIRST_PHOTO.0 + 3), &mut out, &mut |_, _| {}).unwrap();
    assert_eq!(got, Some(out.len() as u64));
}

#[test]
fn a_failed_listing_costs_only_the_listing() {
    let mut r = connected(|o| o.refuse_browse = true);
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.phase, Phase::Streaming);
    assert!(s.photos.error.as_deref().is_some_and(|e| e.contains("HTTP 500")), "{:?}", s.photos);
    assert_eq!(s.photos.newest, None);
    let id = r.ctl.capture().unwrap();
    r.run(30);
    assert_eq!(r.ctl.capture_outcome(id), Some(CaptureOutcome::Done));
    assert_eq!(r.ctl.capture_photo(id), None, "no guess without a listing");
    r.cam.update(|o| o.refuse_browse = false);
    r.ctl.reindex_photos();
    r.run(1_000);
    let s = r.ctl.snapshot(r.now());
    assert_eq!(s.photos.newest, Some(PhotoId(FIRST_PHOTO.0 + 3)));
    assert_eq!(s.phase, Phase::Streaming, "live view came back");
}

#[test]
fn a_capture_during_a_listing_waits_for_record_mode() {
    let mut r = connected(|_| {});
    r.ctl.reindex_photos();
    r.run(10); // playmode sent
    assert!(r.cam.in_play_mode());
    let id = r.ctl.capture().unwrap();
    r.run(500);
    assert_eq!(r.ctl.capture_outcome(id), Some(CaptureOutcome::Done));
    assert_eq!(r.cam.stats().captures, 1);
    assert_eq!(r.ctl.capture_photo(id), Some(PhotoId(FIRST_PHOTO.0 + 3)));
}

#[test]
fn an_empty_card_and_no_listing_at_all() {
    let r = connected(|o| o.photos_on_card = 0);
    let s = r.ctl.snapshot(r.now());
    assert_eq!((s.photos.newest, s.photos.total, s.photos.error.clone()), (None, 0, None));

    let mut r = Rig::new(|_| {});
    r.ctl = Controller::new(Options { index_photos: false, ..Options::default() });
    r.ctl.connect(r.now());
    r.run(1_000);
    assert_eq!(r.cam.stats().browses, 0);
    assert_eq!(r.ctl.phase(), Phase::Streaming);
}
