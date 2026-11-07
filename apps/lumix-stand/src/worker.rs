//! The camera link's worker thread and the transport it owns.
//!
//! Ported from Picabia (`crates/engine/src/cmd/picabia_camera/native.rs`). Every turn the worker
//! takes the window's messages, pumps the controller once (a request and its answer, then the
//! datagrams that came, waiting at most 20 ms), then publishes the state and, when a new frame
//! arrived, the frame (its JPEG, and decoded for the window). Nothing here waits on the camera
//! from the window's thread: a capture is asked for, and the window looks for its outcome on
//! each repaint ([`Worker::capture_outcome`]). Closing sends a last best-effort `stopstream`.
//!
//! Full-size photos come from a second thread with its own connection ([`Originals`]): a
//! download takes 10-35 s on the camera's Wi-Fi, and live view and the keep-alive must go on
//! meanwhile. Test shots' previews come from a third, so a test shot never waits behind a
//! full-size download (*hypothesis*: the camera's file server serves two connections at once; a
//! preview that fails then says so). With fast downloads on, live view pauses while full-size
//! photos are downloading or waiting to, and comes back after. A capture's photo is the one the controller expects ([`Controller::capture_photo`]);
//! when that isn't known, or isn't on the card within [`FIND_TIMEOUT`], the card is listed again
//! once and the newest photo is taken.

use std::io::Write as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lumix_link::frame::MAX_DATAGRAM;
use lumix_link::net::{NetOriginals, NetTransport};
use lumix_link::sim::{SimCamera, SimOptions};
use lumix_link::{CaptureOutcome, Controller, FocusStep, Options, Originals, Phase, PhotoId, Snapshot, Transport, pump};

/// The simulated camera's address.
pub const SIM_ADDRESS: &str = "sim";
/// The widest and tallest frame decoded (the GX85's largest photo is 4592×3448).
const MAX_FRAME_SIDE: u32 = 8192;
/// Test-shot previews kept for the window.
const MAX_PREVIEWS: usize = 16;
/// The simulated camera's picture: the GX85's live-view size.
const SIM_W: u32 = 640;
const SIM_H: u32 = 360;
/// The simulated camera's full-size photos.
const SIM_FULL_W: u32 = 1920;
const SIM_FULL_H: u32 = 1080;
/// How long a capture's photo is looked for on the card (it was there 1-4 s after the shutter).
pub const FIND_TIMEOUT: Duration = Duration::from_secs(12);
/// How often a photo not on the card yet is asked for again.
const FIND_EVERY: Duration = Duration::from_millis(500);
/// Downloads remembered for the window.
const MAX_DOWNLOADS: usize = 32;

/// What became of a full-size download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Download {
    /// Waiting for the photo's number (the card is being listed).
    Waiting,
    /// Waiting for its turn.
    Queued(PhotoId),
    /// Looking for the photo on the card.
    Finding(PhotoId),
    Downloading {
        photo: PhotoId,
        got: u64,
        total: Option<u64>,
    },
    Done {
        photo: PhotoId,
        bytes: u64,
    },
    Failed(String),
}

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
    /// Download capture `ticket`'s photo to the path.
    Download(u64, PathBuf),
    /// Fetch capture `ticket`'s preview.
    Preview(u64),
    /// Download a photo from the card to the path (`ticket` is new).
    Card(u64, PhotoId, PathBuf, Arc<AtomicBool>),
    FastDownloads(bool),
    Set(String, String),
    Focus(FocusStep),
    OneShotAf,
    Refresh,
    Close,
}

/// What a download is for.
#[derive(Clone, Debug)]
enum Kind {
    /// A capture's photo, into the path: looked for until it is on the card.
    Capture(PathBuf),
    /// A photo already on the card, into the path; skipped once the flag is set.
    Card(PathBuf, Arc<AtomicBool>),
    /// A capture's preview, kept in memory: looked for until it is on the card.
    Preview,
}

impl Kind {
    /// Goes to the full-size thread.
    fn full_size(&self) -> bool {
        !matches!(self, Kind::Preview)
    }

    /// Looked for until it appears (a capture's photo takes 1-4 s to be on the card).
    fn wait_for_it(&self) -> bool {
        !matches!(self, Kind::Card(..))
    }
}

/// A job for a downloads thread.
struct Job {
    ticket: u64,
    photo: PhotoId,
    kind: Kind,
}

/// A download waiting for the card's listing.
struct Waiting {
    ticket: u64,
    kind: Kind,
    /// The listing generation it waits to pass.
    after: u64,
}

#[derive(Default)]
struct Shared {
    snap: Snapshot,
    /// The newest frame's number and JPEG.
    jpeg: Option<(u64, Arc<Vec<u8>>)>,
    decoded: Option<DecodedFrame>,
    /// The last capture's ticket and what became of it.
    capture: Option<(u64, CaptureOutcome)>,
    /// Downloads by ticket, oldest first.
    downloads: Vec<(u64, Download)>,
    /// Test-shot previews by ticket, oldest first.
    previews: Vec<(u64, DecodedFrame)>,
    /// The last capture's ticket and the photo it is expected to be on the card.
    capture_photo: Option<(u64, PhotoId)>,
    /// Full-size downloads under way or waiting their turn.
    full_jobs: usize,
    /// Live view is paused for them.
    paused: bool,
    /// Setting changes the link thread has taken and published the state of.
    changes_taken: u64,
    /// The worker ended (why).
    ended: Option<String>,
}

impl Shared {
    fn put_download(&mut self, ticket: u64, d: Download) {
        match self.downloads.iter_mut().find(|(t, _)| *t == ticket) {
            Some((_, old)) => *old = d,
            None => {
                self.downloads.push((ticket, d));
                if self.downloads.len() > MAX_DOWNLOADS {
                    self.downloads.remove(0);
                }
            }
        }
    }
}

/// The link's state for the window.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub snap: Snapshot,
    pub address: String,
    pub sim: bool,
    /// Live view is paused while full-size photos download (fast downloads).
    pub paused: bool,
    /// Setting changes are on their way to the link thread (so the camera's pending commands
    /// don't count them yet).
    pub settling: bool,
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
    /// Tells the downloads thread to stop.
    cancel: Arc<AtomicBool>,
    address: String,
    sim: bool,
    tickets: u64,
    /// Setting changes sent to the link thread.
    changes_sent: std::sync::atomic::AtomicU64,
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
        type Parts = (Box<dyn Transport + Send>, Box<dyn Originals>, Box<dyn Originals>, bool);
        let (transport, originals, previews, sim): Parts = match target {
            Target::Camera(addr) => (
                Box::new(NetTransport::open(addr, lumix_link::DEFAULT_UDP_PORT)?),
                Box::new(NetOriginals::new(addr)),
                Box::new(NetOriginals::new(addr)),
                false,
            ),
            Target::Sim(opts) => {
                let cam = SimCamera::new(opts);
                (Box::new(cam.clone()), Box::new(cam.clone()), Box::new(cam), true)
            }
        };
        let opts = Options { udp_port: transport.udp_port(), device_id: device_id.to_string(), ..Options::default() };
        let (tx, rx) = channel();
        let (jobs_tx, jobs_rx) = channel::<Job>();
        let (previews_tx, previews_rx) = channel::<Job>();
        let (missed_tx, missed_rx) = channel::<Job>();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        for (name, o, rx) in [("lumix-originals", originals, jobs_rx), ("lumix-previews", previews, previews_rx)] {
            let (sh, c, r, m) = (shared.clone(), cancel.clone(), repaint.clone(), missed_tx.clone());
            std::thread::Builder::new()
                .name(name.into())
                .spawn(move || downloads(o, &rx, &m, &sh, &c, r.as_ref()))
                .map_err(|e| format!("can't start the downloads: {e}"))?;
        }
        let sh = shared.clone();
        let link = Link {
            jobs: jobs_tx,
            previews: previews_tx,
            missed: missed_rx,
            waiting: Vec::new(),
            retried: Vec::new(),
            fast: false,
            live_wanted: true,
            paused: false,
            changes_taken: 0,
        };
        std::thread::Builder::new()
            .name("lumix-link".into())
            .spawn(move || run(Controller::new(opts), transport, link, &rx, &sh, repaint.as_ref()))
            .map_err(|e| format!("can't start the camera link: {e}"))?;
        Ok(Worker { tx, shared, cancel, address: address.trim().to_string(), sim, tickets: 0, changes_sent: 0.into() })
    }

    /// End the worker and wait at most `wait` for its last `stopstream`. A download under way
    /// stops (its partial file is removed).
    pub fn close(self, wait: Duration) {
        self.cancel.store(true, Ordering::SeqCst);
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

    /// Download capture `ticket`'s photo at full size to `path` (a new file: never replaced).
    pub fn download(&self, ticket: u64, path: PathBuf) -> Result<(), String> {
        lock(&self.shared).put_download(ticket, Download::Waiting);
        self.tx.send(Msg::Download(ticket, path)).map_err(|_| self.gone())
    }

    /// Fetch capture `ticket`'s preview; [`Worker::preview_frame`] has it once it came.
    pub fn preview(&self, ticket: u64) -> Result<(), String> {
        lock(&self.shared).put_download(ticket, Download::Waiting);
        self.tx.send(Msg::Preview(ticket)).map_err(|_| self.gone())
    }

    /// Capture `ticket`'s preview, decoded (once its download is done).
    pub fn preview_frame(&self, ticket: u64) -> Option<DecodedFrame> {
        lock(&self.shared).previews.iter().find(|(t, _)| *t == ticket).map(|(_, f)| f.clone())
    }

    /// Download `photo` from the card to `path` (a new file: never replaced), unless `skip` is set
    /// before its turn. Its ticket, for [`Worker::download_state`].
    pub fn download_card(&mut self, photo: PhotoId, path: PathBuf, skip: Arc<AtomicBool>) -> Result<u64, String> {
        self.tickets += 1;
        lock(&self.shared).put_download(self.tickets, Download::Queued(photo));
        self.tx.send(Msg::Card(self.tickets, photo, path, skip)).map_err(|_| self.gone())?;
        Ok(self.tickets)
    }

    /// The photo capture `ticket` is expected to be on the card (known once the shutter fired and
    /// the card was listed).
    pub fn capture_photo(&self, ticket: u64) -> Option<PhotoId> {
        lock(&self.shared).capture_photo.filter(|(t, _)| *t == ticket).map(|(_, p)| p)
    }

    /// Pause live view while full-size photos download (faster downloads).
    pub fn fast_downloads(&self, on: bool) -> Result<(), String> {
        self.tx.send(Msg::FastDownloads(on)).map_err(|_| self.gone())
    }

    /// What became of capture `ticket`'s download (`None`: none was asked for, or it's long gone).
    pub fn download_state(&self, ticket: u64) -> Option<Download> {
        let sh = lock(&self.shared);
        let d = sh.downloads.iter().find(|(t, _)| *t == ticket).map(|(_, d)| d.clone());
        match d {
            Some(Download::Done { .. } | Download::Failed(_)) | None => d,
            Some(_) if sh.ended.is_some() => Some(Download::Failed("the camera link ended".into())),
            Some(_) => d,
        }
    }

    /// Change a camera setting (`key` from [`lumix_link::menu::catalogue`]).
    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        self.changes_sent.fetch_add(1, Ordering::SeqCst);
        self.tx.send(Msg::Set(key.into(), value.into())).map_err(|_| self.gone())
    }

    pub fn focus(&self, step: FocusStep) -> Result<(), String> {
        self.changes_sent.fetch_add(1, Ordering::SeqCst);
        self.tx.send(Msg::Focus(step)).map_err(|_| self.gone())
    }

    pub fn one_shot_af(&self) -> Result<(), String> {
        self.changes_sent.fetch_add(1, Ordering::SeqCst);
        self.tx.send(Msg::OneShotAf).map_err(|_| self.gone())
    }

    /// Read every setting again.
    pub fn refresh(&self) -> Result<(), String> {
        self.changes_sent.fetch_add(1, Ordering::SeqCst);
        self.tx.send(Msg::Refresh).map_err(|_| self.gone())
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
        Status {
            snap: sh.snap.clone(),
            address: self.address.clone(),
            sim: self.sim,
            paused: sh.paused,
            settling: sh.changes_taken < self.changes_sent.load(Ordering::SeqCst),
            ended: sh.ended.clone(),
        }
    }

    fn gone(&self) -> String {
        lock(&self.shared).ended.clone().unwrap_or_else(|| "the camera link has ended: connect again".into())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Msg::Close); // the worker may have ended already: nothing to stop
    }
}

/// The link thread's side of the downloads.
struct Link {
    /// To the full-size thread.
    jobs: Sender<Job>,
    /// To the previews thread.
    previews: Sender<Job>,
    /// Photos a downloads thread didn't find.
    missed: Receiver<Job>,
    waiting: Vec<Waiting>,
    /// Downloads listed for a second time already.
    retried: Vec<u64>,
    /// Pause live view while full-size photos download.
    fast: bool,
    /// Live view as the window asked for it.
    live_wanted: bool,
    /// Paused for downloads now.
    paused: bool,
    /// Setting changes taken (published after the state that has them queued).
    changes_taken: u64,
}

impl Link {
    /// Hand on the downloads whose photo is known now; list the card again for those missed.
    fn follow_up(&mut self, ctl: &mut Controller, shared: &Mutex<Shared>, now: u64) {
        while let Ok(job) = self.missed.try_recv() {
            // Listed once more; a second miss is final.
            if self.retried.contains(&job.ticket) {
                lock(shared).put_download(job.ticket, Download::Failed(format!("{} wasn't on the card", job.photo.file_name())));
                continue;
            }
            self.retried.push(job.ticket);
            if self.retried.len() > MAX_DOWNLOADS {
                self.retried.remove(0);
            }
            ctl.reindex_photos();
            let after = ctl.snapshot(now).photos.generation;
            self.waiting.push(Waiting { ticket: job.ticket, kind: job.kind, after });
        }
        if self.waiting.is_empty() {
            return;
        }
        let photos = ctl.snapshot(now).photos;
        let mut still = Vec::new();
        for w in std::mem::take(&mut self.waiting) {
            if photos.generation <= w.after || photos.listing {
                still.push(w);
                continue;
            }
            // The newest photo is the capture's: after a miss it may have got another number, or
            // only have been slow to appear.
            match (photos.newest, &photos.error) {
                (Some(p), _) => self.send(shared, Job { ticket: w.ticket, photo: p, kind: w.kind }),
                (None, Some(e)) => lock(shared).put_download(w.ticket, Download::Failed(format!("the photo wasn't found: {e}"))),
                (None, None) => lock(shared).put_download(w.ticket, Download::Failed("the photo wasn't found on the card".into())),
            }
        }
        self.waiting = still;
    }

    fn send(&self, shared: &Mutex<Shared>, job: Job) {
        let ticket = job.ticket;
        let full = job.kind.full_size();
        {
            let mut sh = lock(shared);
            sh.put_download(ticket, Download::Queued(job.photo));
            if full {
                sh.full_jobs += 1;
            }
        }
        let sent = if full { self.jobs.send(job) } else { self.previews.send(job) };
        if sent.is_err() {
            let mut sh = lock(shared);
            sh.put_download(ticket, Download::Failed("the downloads stopped".into()));
            if full {
                sh.full_jobs = sh.full_jobs.saturating_sub(1);
            }
        }
    }

    /// A capture's download asked for: send it on, or wait for a listing.
    fn start(&mut self, ctl: &mut Controller, shared: &Mutex<Shared>, ticket: u64, kind: Kind, photo: Option<PhotoId>, now: u64) {
        match photo {
            Some(p) => self.send(shared, Job { ticket, photo: p, kind }),
            None => {
                ctl.reindex_photos();
                let after = ctl.snapshot(now).photos.generation;
                self.waiting.push(Waiting { ticket, kind, after });
            }
        }
    }

    /// Live view as the window wants it, paused while full-size photos download (fast downloads).
    fn pace_live_view(&mut self, ctl: &mut Controller, shared: &Mutex<Shared>) {
        let busy = lock(shared).full_jobs > 0;
        let pause = self.fast && busy && self.live_wanted;
        if pause && !self.paused {
            self.paused = true;
            ctl.stop_live_view();
        } else if !pause && self.paused {
            self.paused = false;
            if self.live_wanted {
                ctl.start_live_view();
            }
        }
        lock(shared).paused = self.paused;
    }

    fn live_view(&mut self, ctl: &mut Controller, on: bool) {
        self.live_wanted = on;
        if self.paused {
            // Back on (or not) once the downloads are done.
            if !on {
                self.paused = false;
            }
            return;
        }
        if on {
            ctl.start_live_view();
        } else {
            ctl.stop_live_view();
        }
    }
}

/// The worker loop.
fn run(
    mut ctl: Controller,
    mut t: Box<dyn Transport + Send>,
    mut link: Link,
    rx: &Receiver<Msg>,
    shared: &Mutex<Shared>,
    repaint: &(dyn Fn() + Send + Sync),
) {
    let t0 = Instant::now();
    let now = move || u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut last_seq = 0;
    let mut capture: Option<(u64, Result<u64, String>)> = None;
    ctl.connect(now());
    // A panic here would only end this thread; catching it lets the status say so.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop {
            if !take_messages(&mut ctl, rx, &mut capture, &mut link, shared, now()) {
                ctl.disconnect();
                if let Some(req) = ctl.poll(now()) {
                    let _ = t.get(&req.path); // best effort: the camera may be gone already
                }
                return;
            }
            pump(&mut ctl, t.as_mut(), &mut buf, 20, &now);
            link.follow_up(&mut ctl, shared, now());
            link.pace_live_view(&mut ctl, shared);
            let news = publish(&ctl, shared, &mut last_seq, capture.as_ref(), now());
            // After the state with their commands queued, so a test shot never sees neither.
            lock(shared).changes_taken = link.changes_taken;
            if news {
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
fn take_messages(
    ctl: &mut Controller,
    rx: &Receiver<Msg>,
    capture: &mut Option<(u64, Result<u64, String>)>,
    link: &mut Link,
    shared: &Mutex<Shared>,
    now: u64,
) -> bool {
    loop {
        // A change asked for while not connected is dropped: the window greys the controls then.
        let refused = match rx.try_recv() {
            Ok(Msg::LiveView(on)) => {
                link.live_view(ctl, on);
                Ok(())
            }
            Ok(Msg::FastDownloads(on)) => {
                link.fast = on;
                Ok(())
            }
            Ok(Msg::Capture(ticket)) => {
                *capture = Some((ticket, ctl.capture()));
                Ok(())
            }
            Ok(Msg::Download(ticket, path)) => {
                let photo = capture.as_ref().filter(|(t, _)| *t == ticket).and_then(|(_, id)| id.as_ref().ok()).and_then(|id| ctl.capture_photo(*id));
                link.start(ctl, shared, ticket, Kind::Capture(path), photo, now);
                Ok(())
            }
            Ok(Msg::Preview(ticket)) => {
                let photo = capture.as_ref().filter(|(t, _)| *t == ticket).and_then(|(_, id)| id.as_ref().ok()).and_then(|id| ctl.capture_photo(*id));
                link.start(ctl, shared, ticket, Kind::Preview, photo, now);
                Ok(())
            }
            Ok(Msg::Card(ticket, photo, path, skip)) => {
                link.send(shared, Job { ticket, photo, kind: Kind::Card(path, skip) });
                Ok(())
            }
            Ok(Msg::Set(k, v)) => {
                link.changes_taken += 1;
                ctl.set(&k, &v)
            }
            Ok(Msg::Focus(step)) => {
                link.changes_taken += 1;
                ctl.focus(step)
            }
            Ok(Msg::OneShotAf) => {
                link.changes_taken += 1;
                ctl.one_shot_af()
            }
            Ok(Msg::Refresh) => {
                link.changes_taken += 1;
                ctl.refresh_settings();
                Ok(())
            }
            Ok(Msg::Close) | Err(TryRecvError::Disconnected) => return false,
            Err(TryRecvError::Empty) => return true,
        };
        drop(refused);
    }
}

/// A downloads thread: each job's photo is looked for until it is on the card (a card photo
/// only once), then written to `<path>.part` and moved to `path` (never over an existing file),
/// or, for a preview, decoded and kept for the window.
fn downloads(
    mut originals: Box<dyn Originals>,
    jobs: &Receiver<Job>,
    missed: &Sender<Job>,
    shared: &Mutex<Shared>,
    cancel: &AtomicBool,
    repaint: &(dyn Fn() + Send + Sync),
) {
    while let Ok(job) = jobs.recv() {
        if cancel.load(Ordering::SeqCst) {
            return;
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fetch(originals.as_mut(), &job, shared, cancel, repaint)));
        let full = job.kind.full_size();
        let state = match r {
            Ok(Ok(Some(bytes))) => Download::Done { photo: job.photo, bytes },
            Ok(Ok(None)) if job.kind.wait_for_it() => {
                // Not on the card where expected: the link lists the card and tries once more.
                let (ticket, photo) = (job.ticket, job.photo);
                let mut sh = lock(shared);
                sh.put_download(ticket, Download::Waiting);
                if full {
                    sh.full_jobs = sh.full_jobs.saturating_sub(1);
                }
                drop(sh);
                if missed.send(job).is_err() {
                    lock(shared).put_download(ticket, Download::Failed(format!("{} wasn't on the card", photo.file_name())));
                }
                repaint();
                continue;
            }
            Ok(Ok(None)) => Download::Failed(format!("{} isn't on the card", job.photo.file_name())),
            Ok(Err(e)) => Download::Failed(e),
            Err(_) => Download::Failed("the download failed (internal error)".into()),
        };
        let mut sh = lock(shared);
        sh.put_download(job.ticket, state);
        if full {
            sh.full_jobs = sh.full_jobs.saturating_sub(1);
        }
        drop(sh);
        repaint();
    }
}

/// Look for `job`'s photo (for at most [`FIND_TIMEOUT`] when it is a capture's) and download it.
/// `Ok(None)`: not found.
fn fetch(
    originals: &mut dyn Originals,
    job: &Job,
    shared: &Mutex<Shared>,
    cancel: &AtomicBool,
    repaint: &(dyn Fn() + Send + Sync),
) -> Result<Option<u64>, String> {
    let deadline = Instant::now() + FIND_TIMEOUT;
    if let Kind::Card(path, skip) = &job.kind {
        if skip.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        if std::fs::symlink_metadata(path).is_ok() {
            return Err(format!("{} is in the folder already", job.photo.file_name()));
        }
    }
    lock(shared).put_download(job.ticket, Download::Finding(job.photo));
    repaint();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("stopped".into());
        }
        let mut last_shown = Instant::now();
        let mut progress = |got: u64, total: Option<u64>| {
            if last_shown.elapsed() >= Duration::from_millis(200) {
                last_shown = Instant::now();
                lock(shared).put_download(job.ticket, Download::Downloading { photo: job.photo, got, total });
                repaint();
            }
        };
        let r = match &job.kind {
            Kind::Capture(path) | Kind::Card(path, _) => to_file(originals, job.photo, path, cancel, &mut progress),
            Kind::Preview => {
                let mut out = Cancellable { inner: Vec::new(), cancel };
                originals.preview(job.photo, &mut out, &mut progress).and_then(|got| match got {
                    None => Ok(None),
                    Some(bytes) => {
                        let f = decode(job.ticket, &out.inner).ok_or("the preview isn't a JPEG this app can read")?;
                        let mut sh = lock(shared);
                        sh.previews.push((job.ticket, f));
                        if sh.previews.len() > MAX_PREVIEWS {
                            sh.previews.remove(0);
                        }
                        Ok(Some(bytes))
                    }
                })
            }
        };
        match r {
            Ok(None) if job.kind.wait_for_it() && Instant::now() < deadline => {
                lock(shared).put_download(job.ticket, Download::Finding(job.photo));
                std::thread::sleep(FIND_EVERY);
            }
            r => return r,
        }
    }
}

/// Photo `id`'s original into `path`, through `<path>.part`.
fn to_file(
    originals: &mut dyn Originals,
    id: PhotoId,
    path: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Option<u64>, String> {
    let part = part_path(path);
    let file = new_part(&part)?;
    let mut out = Cancellable { inner: std::io::BufWriter::new(file), cancel };
    let r = originals.original(id, &mut out, progress).and_then(|got| {
        out.flush().map_err(|e| format!("can't write {}: {e}", part.display()))?;
        Ok(got)
    });
    drop(out);
    let r = match r {
        Ok(Some(bytes)) => place(&part, path).map(|()| Some(bytes)),
        r => r,
    };
    let _ = std::fs::remove_file(&part); // gone already once placed, and nothing worth keeping otherwise
    r
}

/// A fresh, empty `part`. A file (or link) left there is removed first, and the new one is
/// created only if nothing is there, so a link planted at that name is never followed.
fn new_part(part: &Path) -> Result<std::fs::File, String> {
    match std::fs::remove_file(part) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("can't replace {}: {e}", part.display())),
    }
    std::fs::OpenOptions::new().write(true).create_new(true).open(part).map_err(|e| format!("can't write {}: {e}", part.display()))
}

/// `<path>.part`.
fn part_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
}

/// Move `part` to `path`, never replacing a file there.
fn place(part: &Path, path: &Path) -> Result<(), String> {
    match std::fs::hard_link(part, path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(format!("{} appeared meanwhile, so it wasn't replaced", path.display())),
        Err(e) => Err(format!("can't write {}: {e}", path.display())),
    }
}

/// A writer that fails once `cancel` is set (so a download stops when the app closes).
struct Cancellable<'a, W> {
    inner: W,
    cancel: &'a AtomicBool,
}

impl<W: std::io::Write> std::io::Write for Cancellable<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("stopped"));
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
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
        if let Ok(id) = asked
            && let Some(p) = ctl.capture_photo(*id)
        {
            sh.capture_photo = Some((*ticket, p));
        }
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
        original: Arc::new(full_test_card),
        preview: Arc::new(preview_test_card),
        ..SimOptions::default()
    }
}

/// The simulated camera's full-size photo `n`: the test card at 1920×1080.
pub fn full_test_card(n: u64) -> Vec<u8> {
    card_jpeg(n, SIM_FULL_W, SIM_FULL_H, 85)
}

/// The simulated camera's preview of photo `n`, taken `ev` stops from a correct exposure: the
/// test card at 640×360, brighter or darker (so automatic exposure can be tried on it).
pub fn preview_test_card(n: u64, ev: f32) -> Vec<u8> {
    card_jpeg_exposed(n, SIM_W, SIM_H, 80, ev)
}

/// The simulated camera's picture for frame `n`: a soft gradient with a bar that sweeps across,
/// so live view visibly moves. Encoded as a real JPEG.
pub fn test_card(n: u64) -> Vec<u8> {
    card_jpeg(n, SIM_W, SIM_H, 70)
}

fn card_jpeg(n: u64, w: u32, h: u32, quality: u8) -> Vec<u8> {
    card_jpeg_exposed(n, w, h, quality, 0.0)
}

/// The test card `ev` stops brighter (in linear light, so it clips as a photo does).
fn card_jpeg_exposed(n: u64, w: u32, h: u32, quality: u8, ev: f32) -> Vec<u8> {
    let gain = 2f32.powf(ev.clamp(-8.0, 8.0));
    let lut: Vec<u8> = (0..=255u16)
        .map(|v| {
            let lin = (f32::from(v) / 255.0).powf(2.2) * gain;
            (lin.min(1.0).powf(1.0 / 2.2) * 255.0).round() as u8
        })
        .collect();
    let expose = |c: u8| lut.get(usize::from(c)).copied().unwrap_or(c);
    // Live view (no exposure change) skips the table: it is drawn 15 times a second.
    let exposed = ev.abs() > 0.001;
    let bar = u32::try_from(n.wrapping_mul(8) * u64::from(w) / u64::from(SIM_W) % u64::from(w)).unwrap_or(0);
    let bar_w = 24 * w / SIM_W;
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let on_bar = x >= bar && x < bar + bar_w;
            let px = if on_bar { [240, 90, 60] } else { [ramp(x, w), ramp(y, h), 150] };
            if exposed {
                rgb.extend(px.map(expose));
            } else {
                rgb.extend(px);
            }
        }
    }
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    if image::ImageEncoder::write_image(enc, &rgb, w, h, image::ExtendedColorType::Rgb8).is_err() {
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

    #[cfg(unix)]
    #[test]
    fn a_link_at_the_part_name_is_never_followed() {
        let d = crate::frames::tests::TempDir::new("part-link");
        let victim = d.0.join("victim.txt");
        std::fs::write(&victim, b"keep me").unwrap();
        let part = d.0.join("frame_0001.jpg.part");
        std::os::unix::fs::symlink(&victim, &part).unwrap();
        let mut f = new_part(&part).unwrap();
        std::io::Write::write_all(&mut f, b"photo").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
        assert!(!std::fs::symlink_metadata(&part).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&part).unwrap(), b"photo");
    }

    #[test]
    fn addresses() {
        assert!(matches!(Target::parse(" SIM "), Ok(Target::Sim(_))));
        assert!(matches!(Target::parse("192.168.54.1"), Ok(Target::Camera(_))));
        assert!(Target::parse("camera.local").is_err());
        assert!(decode(1, b"not a jpeg").is_none());
    }
}
