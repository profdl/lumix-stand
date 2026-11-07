//! The camera link's worker thread and the transport it owns.
//!
//! Ported from Picabia (`crates/engine/src/cmd/picabia_camera/native.rs`). Every turn the worker
//! takes the window's messages, pumps the controller once (a request and its answer, then the
//! datagrams that came, waiting at most 20 ms), then publishes the state and, when a new frame
//! arrived, the frame (its JPEG, and decoded for the window). Nothing here waits on the camera
//! from the window's thread: a capture is asked for, and the window looks for its outcome on
//! each repaint ([`Worker::capture_outcome`]). Closing sends a last best-effort `stopstream`.

use std::net::IpAddr;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lumix_link::frame::MAX_DATAGRAM;
use lumix_link::net::NetTransport;
use lumix_link::sim::{SimCamera, SimOptions};
use lumix_link::{CaptureOutcome, Controller, Options, Phase, Snapshot, Transport, pump};

/// The simulated camera's address.
pub const SIM_ADDRESS: &str = "sim";
/// The widest and tallest frame decoded.
const MAX_FRAME_SIDE: u32 = 4096;
/// The simulated camera's picture: the GX85's live-view size.
const SIM_W: u32 = 640;
const SIM_H: u32 = 360;

/// What to connect to.
pub enum Target {
    Camera(IpAddr),
    Sim(SimOptions),
}

impl Target {
    /// `sim` (the simulated camera, on a real clock) or an IPv4/IPv6 address.
    pub fn parse(address: &str) -> Result<Target, String> {
        let a = address.trim();
        if a.eq_ignore_ascii_case(SIM_ADDRESS) {
            return Ok(Target::Sim(sim_options()));
        }
        lumix_link::net::parse_ip(a).map(Target::Camera)
    }
}

/// A decoded live-view frame.
#[derive(Clone, Debug)]
pub struct DecodedFrame {
    /// Its number (one more for each frame received).
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    /// RGBA8, `width × height × 4` bytes.
    pub rgba: Arc<Vec<u8>>,
}

/// A capture's ticket, and the live-view frame from just before the shutter.
pub type Fired = (u64, Option<Arc<Vec<u8>>>);

enum Msg {
    LiveView(bool),
    /// Fire the shutter; the number is the window's ticket for it.
    Capture(u64),
    Close,
}

#[derive(Default)]
struct Shared {
    snap: Snapshot,
    /// The newest frame's number and JPEG.
    jpeg: Option<(u64, Arc<Vec<u8>>)>,
    decoded: Option<DecodedFrame>,
    /// The last capture's ticket and what became of it.
    capture: Option<(u64, CaptureOutcome)>,
    /// The worker ended (why).
    ended: Option<String>,
}

/// The link's state for the window.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub snap: Snapshot,
    pub address: String,
    pub sim: bool,
    /// The worker ended (why).
    pub ended: Option<String>,
}

impl Status {
    /// The link is open (it may still be connecting or retrying).
    pub fn open(&self) -> bool {
        self.ended.is_none()
    }

    /// The camera answers commands.
    pub fn ready(&self) -> bool {
        self.open() && self.snap.phase.ready()
    }

    /// What to try, in plain words.
    pub fn hint(&self) -> Option<String> {
        let s = &self.snap;
        if self.ended.is_some() {
            return Some("Connect again.".into());
        }
        if s.awaiting_accept {
            return Some(format!(
                "Look at the camera: if it asks whether to connect to \u{201c}{}\u{201d}, accept. (The camera takes a new computer on once; after that it connects at once.)",
                lumix_link::Options::default().device_name
            ));
        }
        if s.phase == Phase::Lost {
            return Some(format!(
                "No answer from {}. Is the camera on, in Wi-Fi \u{25b8} Remote Shooting & View waiting for a smartphone, and is this computer on the camera's Wi-Fi?",
                self.address
            ));
        }
        if s.no_frames {
            return Some("Live view isn't arriving. On macOS, allow this app (or the terminal or editor that started it) under System Settings \u{25b8} Privacy & Security \u{25b8} Local Network, and check that no firewall blocks incoming UDP.".into());
        }
        None
    }
}

/// The worker (see the module docs).
pub struct Worker {
    tx: Sender<Msg>,
    shared: Arc<Mutex<Shared>>,
    address: String,
    sim: bool,
    tickets: u64,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").field("address", &self.address).field("sim", &self.sim).finish()
    }
}

fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Worker {
    /// Open the transport and start the worker; `address` is what the status shows. `repaint` is
    /// called when there is something new to show.
    pub fn start(target: Target, address: &str, device_id: &str, repaint: Arc<dyn Fn() + Send + Sync>) -> Result<Worker, String> {
        let (transport, sim): (Box<dyn Transport + Send>, bool) = match target {
            Target::Camera(addr) => (Box::new(NetTransport::open(addr, lumix_link::DEFAULT_UDP_PORT)?), false),
            Target::Sim(opts) => (Box::new(SimCamera::new(opts)), true),
        };
        let opts = Options { udp_port: transport.udp_port(), device_id: device_id.to_string(), ..Options::default() };
        let (tx, rx) = channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let sh = shared.clone();
        std::thread::Builder::new()
            .name("lumix-link".into())
            .spawn(move || run(Controller::new(opts), transport, &rx, &sh, repaint.as_ref()))
            .map_err(|e| format!("can't start the camera link: {e}"))?;
        Ok(Worker { tx, shared, address: address.trim().to_string(), sim, tickets: 0 })
    }

    /// End the worker and wait at most `wait` for its last `stopstream`.
    pub fn close(self, wait: Duration) {
        let _ = self.tx.send(Msg::Close); // a worker that already ended needs nothing
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline && lock(&self.shared).ended.is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn live_view(&self, on: bool) -> Result<(), String> {
        self.tx.send(Msg::LiveView(on)).map_err(|_| self.gone())
    }

    /// Fire the shutter. The ticket to ask [`Worker::capture_outcome`] about, and the live-view
    /// frame from just before, when live view is running.
    pub fn capture(&mut self) -> Result<Fired, String> {
        let frame = {
            let sh = lock(&self.shared);
            if let Some(e) = &sh.ended {
                return Err(e.clone());
            }
            let live = sh.snap.phase == Phase::Streaming && !sh.snap.no_frames;
            sh.jpeg.as_ref().filter(|_| live).map(|(_, j)| j.clone())
        };
        self.tickets += 1;
        self.tx.send(Msg::Capture(self.tickets)).map_err(|_| self.gone())?;
        Ok((self.tickets, frame))
    }

    /// What became of capture `ticket`; `Pending` until the worker has an answer.
    pub fn capture_outcome(&self, ticket: u64) -> CaptureOutcome {
        let sh = lock(&self.shared);
        match &sh.capture {
            Some((t, o)) if *t == ticket => o.clone(),
            _ if sh.ended.is_some() => CaptureOutcome::Failed("the camera link ended".into()),
            _ => CaptureOutcome::Pending,
        }
    }

    pub fn decoded(&self) -> Option<DecodedFrame> {
        lock(&self.shared).decoded.clone()
    }

    pub fn status(&self) -> Status {
        let sh = lock(&self.shared);
        Status { snap: sh.snap.clone(), address: self.address.clone(), sim: self.sim, ended: sh.ended.clone() }
    }

    fn gone(&self) -> String {
        lock(&self.shared).ended.clone().unwrap_or_else(|| "the camera link has ended: connect again".into())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Close); // the worker may have ended already: nothing to stop
    }
}

/// The worker loop.
fn run(mut ctl: Controller, mut t: Box<dyn Transport + Send>, rx: &Receiver<Msg>, shared: &Mutex<Shared>, repaint: &(dyn Fn() + Send + Sync)) {
    let t0 = Instant::now();
    let now = move || u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut last_seq = 0;
    let mut capture: Option<(u64, Result<u64, String>)> = None;
    ctl.connect(now());
    // A panic here would only end this thread; catching it lets the status say so.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop {
            if !take_messages(&mut ctl, rx, &mut capture) {
                ctl.disconnect();
                if let Some(req) = ctl.poll(now()) {
                    let _ = t.get(&req.path); // best effort: the camera may be gone already
                }
                return;
            }
            pump(&mut ctl, t.as_mut(), &mut buf, 20, &now);
            if publish(&ctl, shared, &mut last_seq, capture.as_ref(), now()) {
                repaint();
            }
        }
    }));
    let mut sh = lock(shared);
    sh.snap = ctl.snapshot(now());
    sh.ended = Some(if r.is_err() { "the camera link failed (internal error): connect again".into() } else { "disconnected".into() });
    drop(sh);
    repaint();
}

/// Handle the waiting messages; `false`: close.
fn take_messages(ctl: &mut Controller, rx: &Receiver<Msg>, capture: &mut Option<(u64, Result<u64, String>)>) -> bool {
    loop {
        match rx.try_recv() {
            Ok(Msg::LiveView(true)) => ctl.start_live_view(),
            Ok(Msg::LiveView(false)) => ctl.stop_live_view(),
            Ok(Msg::Capture(ticket)) => *capture = Some((ticket, ctl.capture())),
            Ok(Msg::Close) | Err(TryRecvError::Disconnected) => return false,
            Err(TryRecvError::Empty) => return true,
        }
    }
}

/// Publish the state; `true` when the window has something new to show.
fn publish(ctl: &Controller, shared: &Mutex<Shared>, last_seq: &mut u64, capture: Option<&(u64, Result<u64, String>)>, now: u64) -> bool {
    let fresh = (ctl.frame_seq() != *last_seq).then(|| ctl.frame().map(|f| Arc::new(f.jpeg.clone()))).flatten();
    // Decoded outside the lock: the window reads the last one meanwhile.
    let decoded = fresh.as_ref().and_then(|j| decode(ctl.frame_seq(), j));
    let mut sh = lock(shared);
    let snap = ctl.snapshot(now);
    let mut changed = snap.phase != sh.snap.phase || snap.awaiting_accept != sh.snap.awaiting_accept || snap.no_frames != sh.snap.no_frames;
    sh.snap = snap;
    if let Some(j) = fresh {
        *last_seq = ctl.frame_seq();
        sh.jpeg = Some((*last_seq, j));
        if decoded.is_some() {
            sh.decoded = decoded;
        }
        changed = true;
    }
    if let Some((ticket, asked)) = capture {
        let outcome = match asked {
            Ok(id) => ctl.capture_outcome(*id).unwrap_or(CaptureOutcome::Pending),
            Err(e) => CaptureOutcome::Failed(e.clone()),
        };
        let new = Some((*ticket, outcome));
        changed |= sh.capture != new;
        sh.capture = new;
    }
    changed
}

/// A JPEG as RGBA8, its size capped.
pub fn decode(seq: u64, jpeg: &[u8]) -> Option<DecodedFrame> {
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(jpeg), image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_FRAME_SIDE);
    limits.max_image_height = Some(MAX_FRAME_SIDE);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    Some(DecodedFrame { seq, width, height, rgba: Arc::new(img.into_raw()) })
}

/// The simulated camera on a real clock, drawing a test card at 15 fps.
pub fn sim_options() -> SimOptions {
    let t0 = Instant::now();
    SimOptions {
        fps: 15,
        clock: Arc::new(move || u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX)),
        real_time: true,
        frame: Arc::new(test_card),
        ..SimOptions::default()
    }
}

/// The simulated camera's picture for frame `n`: a soft gradient with a bar that sweeps across,
/// so live view visibly moves. Encoded as a real JPEG.
pub fn test_card(n: u64) -> Vec<u8> {
    let bar = u32::try_from(n.wrapping_mul(8) % u64::from(SIM_W)).unwrap_or(0);
    let mut rgb = Vec::with_capacity((SIM_W * SIM_H * 3) as usize);
    for y in 0..SIM_H {
        for x in 0..SIM_W {
            let on_bar = x >= bar && x < bar + 24;
            let px = if on_bar { [240, 90, 60] } else { [ramp(x, SIM_W), ramp(y, SIM_H), 150] };
            rgb.extend(px);
        }
    }
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 70);
    if image::ImageEncoder::write_image(enc, &rgb, SIM_W, SIM_H, image::ExtendedColorType::Rgb8).is_err() {
        return lumix_link::sim::stand_in_jpeg(n);
    }
    out
}

/// 30-230 across `0..len`.
fn ramp(i: u32, len: u32) -> u8 {
    u8::try_from(30 + i * 200 / len.max(1)).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sim_worker(opts: SimOptions) -> Worker {
        Worker::start(Target::Sim(opts), "sim", "TEST-ID", Arc::new(|| {})).unwrap()
    }

    pub(crate) fn wait_for(w: &Worker, what: &str, ok: impl Fn(&Status) -> bool) -> Status {
        for _ in 0..400 {
            let s = w.status();
            if ok(&s) {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the simulated camera never got to: {what} ({:?})", w.status());
    }

    #[test]
    fn streams_decodes_and_captures() {
        let mut w = sim_worker(sim_options());
        wait_for(&w, "streaming", |s| s.snap.phase == Phase::Streaming && s.snap.frame_seq > 1);
        let f = w.decoded().unwrap();
        assert_eq!((f.width, f.height), (SIM_W, SIM_H));
        assert_eq!(f.rgba.len(), (SIM_W * SIM_H * 4) as usize);
        let (ticket, jpeg) = w.capture().unwrap();
        assert!(jpeg.is_some(), "the frame from just before the shutter");
        for _ in 0..200 {
            if w.capture_outcome(ticket) != CaptureOutcome::Pending {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(w.capture_outcome(ticket), CaptureOutcome::Done);
        w.close(Duration::from_millis(500));
    }

    #[test]
    fn a_refused_shutter_is_reported() {
        let mut w = sim_worker(SimOptions { refuse_capture: true, ..sim_options() });
        wait_for(&w, "ready", Status::ready);
        let (ticket, _) = w.capture().unwrap();
        for _ in 0..200 {
            if w.capture_outcome(ticket) != CaptureOutcome::Pending {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(w.capture_outcome(ticket), CaptureOutcome::Failed(e) if e.contains("err_busy")));
    }

    #[test]
    fn live_view_turns_off_and_on() {
        let w = sim_worker(sim_options());
        wait_for(&w, "streaming", |s| s.snap.phase == Phase::Streaming);
        w.live_view(false).unwrap();
        wait_for(&w, "live view off", |s| s.snap.phase == Phase::Connected && !s.snap.live_view);
        w.live_view(true).unwrap();
        wait_for(&w, "streaming again", |s| s.snap.phase == Phase::Streaming);
    }

    #[test]
    fn hints_in_plain_words() {
        let w = sim_worker(SimOptions { offline: true, ..sim_options() });
        let s = wait_for(&w, "lost", |s| s.snap.phase == Phase::Lost);
        assert!(s.hint().unwrap().contains("No answer from sim"));

        let w = sim_worker(SimOptions { require_handshake: true, accept_after_ms: Some(60_000), ..sim_options() });
        let s = wait_for(&w, "awaiting accept", |s| s.snap.awaiting_accept);
        assert!(s.hint().unwrap().contains("Picabia"), "names what the camera shows");

        let ended = Status { ended: Some("disconnected".into()), ..Status::default() };
        assert_eq!(ended.hint().as_deref(), Some("Connect again."));
        let no_frames = Status { snap: Snapshot { phase: Phase::Streaming, no_frames: true, ..Snapshot::default() }, ..Status::default() };
        assert!(no_frames.hint().unwrap().contains("Local Network"));
    }

    #[test]
    fn closing_ends_the_worker() {
        let w = sim_worker(sim_options());
        wait_for(&w, "ready", Status::ready);
        let shared = w.shared.clone();
        w.close(Duration::from_millis(500));
        assert!(lock(&shared).ended.is_some());
    }

    #[test]
    fn addresses() {
        assert!(matches!(Target::parse(" SIM "), Ok(Target::Sim(_))));
        assert!(matches!(Target::parse("192.168.54.1"), Ok(Target::Camera(_))));
        assert!(Target::parse("camera.local").is_err());
        assert!(decode(1, b"not a jpeg").is_none());
    }
}
