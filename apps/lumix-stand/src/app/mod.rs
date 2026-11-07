//! The window: live view and the comparison (a test shot or the last full-size photo) in the
//! middle with the test shots below, the controls on the right.
//!
//! Layout first ported from Picabia (`crates/ui-egui/src/picabia_camera_window.rs`). The state
//! and everything the buttons do live in [`Stand`], which runs without a window, so tests drive it
//! against the simulated camera; [`StandApp`] draws it ([`panel`]: the controls, [`compare`]:
//! the pictures). [`shots`] has test shots, the last photo, presets and card downloads.

mod assist;
mod compare;
mod panel;
mod shots;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, Modifiers, TextureHandle, TextureOptions, Ui};
use lumix_link::menu;
use lumix_link::{CaptureOutcome, FocusStep};

use crate::frames;
use crate::settings::Settings;
use crate::worker::{Download, SIM_ADDRESS, Status, Target, Worker};

pub use shots::{CardBatch, Compare, Source, TestShot};

/// The longest a capture waits for the camera to say it fired.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long Find searches.
const FIND_MS: u64 = 1500;
/// How long closing waits for the camera's last `stopstream`.
const CLOSE_WAIT: Duration = Duration::from_millis(500);
/// The longest a full-size download may take once it has started, finding the photo included
/// (a 12 MB photo takes about 35 s at the measured 0.35 MB/s).
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// The longest a download may wait for its turn behind the others.
pub const QUEUE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// The controls' width.
const SIDE_W: f32 = 370.0;

/// Shows the folder picker with a title; `None`: cancelled.
pub type Picker = Box<dyn FnMut(&str) -> Option<PathBuf>>;

/// A capture waiting for the camera's answer.
struct Pending {
    ticket: u64,
    jpeg: Option<Arc<Vec<u8>>>,
    /// Where the frame goes (`None`: a test shot, kept in the window only).
    folder: Option<PathBuf>,
    prefix: String,
    deadline: Instant,
    /// The camera's settings at the shutter.
    camera: Vec<(String, String)>,
}

/// A full-size photo on its way to the capture folder, under its reserved number.
pub struct Fetching {
    ticket: u64,
    folder: PathBuf,
    prefix: String,
    n: u32,
    path: PathBuf,
    /// The live-view frame from the shutter, saved instead if the download fails.
    fallback: Option<Arc<Vec<u8>>>,
    /// Asked for at; the deadline once the download has started.
    queued: Instant,
    deadline: Option<Instant>,
    /// The camera's settings at the shutter.
    camera: Vec<(String, String)>,
    /// The last state seen.
    pub state: Download,
}

impl Fetching {
    pub fn name(&self) -> String {
        frames::file_name(&self.prefix, self.n)
    }
}

/// The app's state and actions (no drawing).
pub struct Stand {
    pub settings: Settings,
    /// Where the settings are saved (`None`: nowhere, e.g. no config folder).
    settings_path: Option<PathBuf>,
    /// The address field.
    pub address: String,
    /// The prefix field.
    pub prefix: String,
    worker: Option<Worker>,
    pending: Option<Pending>,
    /// Full-size downloads under way, oldest first.
    pub fetching: Vec<Fetching>,
    /// The last capture's result (or an error), for the window: (ok, message).
    pub last: Option<(bool, String)>,
    /// What Find found: (address, label).
    pub found: Vec<(String, String)>,
    finding: Option<Receiver<Result<Vec<lumix_link::ssdp::Found>, String>>>,
    next_file: Option<(Instant, Option<String>)>,
    picker: Picker,
    repaint: Arc<dyn Fn() + Send + Sync>,
    /// Test shots, oldest first, and the one shown.
    pub tests: Vec<TestShot>,
    pub selected: Option<u64>,
    tests_taken: u32,
    /// A test shot asked for (it waits for setting changes to reach the camera), since.
    test_wanted: Option<Instant>,
    /// What the comparison shows.
    pub compare: Compare,
    /// The last full-size photo, and one being read.
    pub photo: Option<crate::review::Photo>,
    loading: Option<Receiver<Result<crate::review::Photo, String>>>,
    photos_shown: u64,
    /// This session's captures on the card: (frame name, photo, the capture's ticket).
    pub session: Vec<(String, Option<lumix_link::PhotoId>, u64)>,
    /// The Download photos section.
    pub source: Source,
    pub range_from: String,
    pub range_to: String,
    pub batch: Option<CardBatch>,
    /// A preset being renamed: (index, the name field).
    pub renaming: Option<(usize, String)>,
    /// Finding the best exposure (or how the last search ended).
    pub assist: Option<assist::Assist>,
    /// Claude's answer on its way, and the last one: (the test shot, the answer).
    asking: Option<assist::Asking>,
    pub advice: Option<(u64, Result<crate::claude::Reply, String>)>,
    /// The API key, once read (never saved).
    api_key: Option<String>,
    /// The 1Password reference field.
    pub key_field: String,
}

impl Stand {
    pub fn new(settings: Settings, settings_path: Option<PathBuf>, picker: Picker, repaint: Arc<dyn Fn() + Send + Sync>) -> Stand {
        Stand {
            address: settings.address.clone(),
            prefix: settings.prefix.clone(),
            key_field: settings.claude_key.clone(),
            settings,
            settings_path,
            worker: None,
            pending: None,
            fetching: Vec::new(),
            last: None,
            found: Vec::new(),
            finding: None,
            next_file: None,
            picker,
            repaint,
            tests: Vec::new(),
            selected: None,
            tests_taken: 0,
            test_wanted: None,
            compare: Compare::Test,
            photo: None,
            loading: None,
            photos_shown: 0,
            session: Vec::new(),
            source: Source::Session,
            range_from: String::new(),
            range_to: String::new(),
            batch: None,
            renaming: None,
            assist: None,
            asking: None,
            advice: None,
            api_key: None,
        }
    }

    /// The link's state (`None`: not connected).
    pub fn status(&self) -> Option<Status> {
        self.worker.as_ref().map(Worker::status)
    }

    pub fn connected(&self) -> bool {
        self.worker.is_some()
    }

    pub fn capturing(&self) -> bool {
        self.pending.is_some() || self.test_wanted.is_some()
    }

    /// A test shot is being taken or fetched.
    pub fn testing(&self) -> bool {
        self.test_wanted.is_some() || self.pending.as_ref().is_some_and(|p| p.folder.is_none()) || self.tests.iter().any(TestShot::fetching)
    }

    pub fn finding(&self) -> bool {
        self.finding.is_some()
    }

    fn worker_frame(&self) -> Option<crate::worker::DecodedFrame> {
        self.worker.as_ref().and_then(Worker::decoded)
    }

    fn save_settings(&mut self) {
        let Some(path) = &self.settings_path else { return };
        if let Err(e) = self.settings.save(path) {
            self.last = Some((false, format!("The settings weren't saved: {e}")));
        }
    }

    fn fail(&mut self, msg: impl Into<String>) {
        self.last = Some((false, msg.into()));
    }

    /// Connect to the address in the field (`sim`: the simulated camera).
    pub fn connect(&mut self) {
        let address = self.address.trim().to_string();
        let target = match Target::parse(&address) {
            Ok(t) => t,
            Err(e) => return self.fail(format!("{e}. Type the camera's IP address (on its own Wi-Fi, 192.168.54.1) or \u{201c}sim\u{201d}.")),
        };
        self.connect_to(target, &address);
    }

    /// Connect to `target`, shown as `address`.
    pub fn connect_to(&mut self, target: Target, address: &str) {
        let address = address.to_string();
        self.disconnect();
        match Worker::start(target, &address, &self.settings.device_id, self.repaint.clone()) {
            Ok(w) => {
                let _ = w.fast_downloads(self.settings.fast_downloads); // a worker that ended at once says so in its status
                self.worker = Some(w);
            }
            Err(e) => return self.fail(e),
        }
        self.last = None;
        if !address.eq_ignore_ascii_case(SIM_ADDRESS) && self.settings.address != address {
            self.settings.address = address;
            self.save_settings();
        }
    }

    /// Close the link (the camera is asked to stop streaming). Downloads under way stop, and
    /// their live-view frames are saved instead.
    pub fn disconnect(&mut self) {
        self.test_wanted = None;
        self.stop_exposure();
        if self.pending.take().is_some() {
            self.fail("Disconnected before the camera confirmed the capture; nothing was saved.");
        }
        if let Some(w) = self.worker.take() {
            w.close(CLOSE_WAIT);
        }
        self.abandon_downloads("the camera was disconnected");
        self.end_tests("the camera was disconnected");
        self.end_batch("the camera was disconnected");
    }

    /// Change a camera setting.
    pub fn set_camera(&mut self, key: &str, value: &str) {
        let r = self.worker.as_ref().map_or_else(|| Err("the camera isn't connected".to_string()), |w| w.set(key, value));
        if let Err(e) = r {
            self.fail(capitalized(&e));
        }
    }

    /// The most pixels the camera takes: the whole sensor (4:3) at its largest size, with a fine
    /// JPEG beside any RAW (a RAW-only photo has no JPEG to download).
    pub fn largest_size(&mut self) {
        let quality = self.status().and_then(|s| s.snap.settings.value("quality").map(str::to_string));
        self.set_camera("aspectratio", "4:3");
        self.set_camera("pictsize", menu::largest_size("4:3"));
        let better = match quality.as_deref() {
            Some("standard") => Some("fine"),
            Some("raw" | "raw_standard") => Some("raw_fine"),
            _ => None,
        };
        if let Some(q) = better {
            self.set_camera("quality", q);
        }
    }

    pub fn focus(&mut self, step: FocusStep) {
        let r = self.worker.as_ref().map_or_else(|| Err("the camera isn't connected".to_string()), |w| w.focus(step));
        if let Err(e) = r {
            self.fail(capitalized(&e));
        }
    }

    pub fn one_shot_af(&mut self) {
        let r = self.worker.as_ref().map_or_else(|| Err("the camera isn't connected".to_string()), Worker::one_shot_af);
        if let Err(e) = r {
            self.fail(capitalized(&e));
        }
    }

    /// Read every camera setting again.
    pub fn refresh_camera(&mut self) {
        if let Some(Err(e)) = self.worker.as_ref().map(Worker::refresh) {
            self.fail(capitalized(&e));
        }
    }

    /// Save full size (`true`) or the live-view frame.
    pub fn set_full_size(&mut self, on: bool) {
        if self.settings.full_size != on {
            self.settings.full_size = on;
            self.save_settings();
        }
    }

    /// Pause live view while full-size photos download (`true`), or keep it running.
    pub fn set_fast_downloads(&mut self, on: bool) {
        if self.settings.fast_downloads != on {
            self.settings.fast_downloads = on;
            self.save_settings();
        }
        if let Some(w) = &self.worker {
            let _ = w.fast_downloads(on); // an ended worker says so in its status
        }
    }

    pub fn live_view(&mut self, on: bool) {
        let r = match &self.worker {
            Some(w) => w.live_view(on),
            None => Err("the camera isn't connected".into()),
        };
        if let Err(e) = r {
            self.fail(e);
        }
    }

    /// Start a search for cameras on this computer's networks.
    pub fn find(&mut self) {
        if self.finding.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let repaint = self.repaint.clone();
        let spawned = std::thread::Builder::new().name("lumix-find".into()).spawn(move || {
            let _ = tx.send(lumix_link::net::discover(FIND_MS)); // the window may have closed
            repaint();
        });
        match spawned {
            Ok(_) => self.finding = Some(rx),
            Err(e) => self.fail(format!("can't search: {e}")),
        }
    }

    fn poll_find(&mut self) {
        let Some(rx) = &self.finding else { return };
        let r = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("the search stopped".into()),
        };
        self.finding = None;
        match r {
            Ok(found) => {
                let mut found: Vec<_> = found.into_iter().collect();
                found.sort_by_key(|f| !f.likely_lumix);
                self.found = found.iter().map(|f| (f.ip.clone(), if f.likely_lumix { format!("{} (Lumix)", f.ip) } else { f.ip.clone() })).collect();
                self.last = Some((true, if self.found.is_empty() { "No camera answered the search.".into() } else { "Pick a camera below.".into() }));
            }
            Err(e) => self.fail(format!("The search failed: {e}")),
        }
    }

    /// Choose the capture folder; `false`: cancelled.
    pub fn choose_folder(&mut self) -> bool {
        let Some(folder) = (self.picker)("Choose the capture folder") else { return false };
        if let Err(e) = frames::check_folder(&folder) {
            self.fail(e);
            return false;
        }
        self.settings.folder = Some(folder);
        self.next_file = None;
        self.save_settings();
        true
    }

    /// Take the prefix field (checked; a bad one is put back).
    pub fn commit_prefix(&mut self) {
        match frames::check_prefix(&self.prefix) {
            Ok(p) => {
                self.prefix = p.clone();
                if p != self.settings.prefix {
                    self.settings.prefix = p;
                    self.next_file = None;
                    self.save_settings();
                }
            }
            Err(e) => {
                self.prefix = self.settings.prefix.clone();
                self.fail(e);
            }
        }
    }

    /// The prefix in use.
    pub fn prefix_in_use(&self) -> &str {
        frames::prefix_or_default(&self.settings.prefix)
    }

    /// The next capture's file name (looked up at most once a second).
    pub fn next_file(&mut self) -> Option<String> {
        if let Some((at, name)) = &self.next_file
            && at.elapsed() < Duration::from_secs(1)
        {
            return name.clone();
        }
        let prefix = self.prefix_in_use().to_string();
        let name = self.settings.folder.clone().and_then(|f| self.free_number(&f, &prefix).ok()).map(|n| frames::file_name(&prefix, n));
        self.next_file = Some((Instant::now(), name.clone()));
        name
    }

    /// The next frame number in `folder`, past the ones reserved for downloads under way.
    fn free_number(&self, folder: &std::path::Path, prefix: &str) -> Result<u32, String> {
        let on_disk = frames::next_number(folder, prefix)?;
        let reserved = self.fetching.iter().filter(|f| f.folder == folder && f.prefix == prefix).map(|f| f.n + 1).max().unwrap_or(0);
        Ok(on_disk.max(reserved))
    }

    /// The shutter fired: reserve the next number and download the photo into it.
    fn start_download(&mut self, p: Pending) -> (bool, String) {
        let Some(folder) = p.folder.clone() else { return (false, "a test shot has no folder".into()) };
        let n = match self.free_number(&folder, &p.prefix) {
            Ok(n) => n,
            Err(e) => return (false, format!("The shutter fired, but the frame wasn't saved: {e}")),
        };
        let path = folder.join(frames::file_name(&p.prefix, n));
        let started = self.worker.as_ref().map_or_else(|| Err("the camera isn't connected".to_string()), |w| w.download(p.ticket, path.clone()));
        let f = Fetching {
            ticket: p.ticket,
            folder,
            prefix: p.prefix,
            n,
            path,
            fallback: p.jpeg,
            queued: Instant::now(),
            deadline: None,
            camera: p.camera,
            state: Download::Waiting,
        };
        let name = f.name();
        if let Err(e) = started {
            return finish_download(&f, Err(e));
        }
        let photo = self.worker.as_ref().and_then(|w| w.capture_photo(f.ticket));
        self.remember_capture(name.clone(), photo, f.ticket);
        self.fetching.push(f);
        (true, format!("Shutter fired; downloading the full-size photo as {name}\u{2026}"))
    }

    /// Follow the downloads; a finished one becomes the last message.
    fn poll_downloads(&mut self) {
        let mut done = Vec::new();
        for (i, f) in self.fetching.iter_mut().enumerate() {
            let state = self.worker.as_ref().map_or(Some(Download::Failed("the camera was disconnected".into())), |w| w.download_state(f.ticket));
            // The clock starts when the download does, not while it waits behind others.
            if f.deadline.is_none() && matches!(state, Some(Download::Finding(_) | Download::Downloading { .. })) {
                f.deadline = Some(Instant::now() + DOWNLOAD_TIMEOUT);
            }
            let state = match state {
                Some(Download::Done { .. } | Download::Failed(_)) => state,
                _ if f.deadline.is_some_and(|d| Instant::now() >= d) => {
                    Some(Download::Failed(format!("it took longer than {} s", DOWNLOAD_TIMEOUT.as_secs())))
                }
                _ if f.queued.elapsed() >= QUEUE_TIMEOUT => Some(Download::Failed("it waited too long for its turn".into())),
                None => Some(Download::Failed("the download was lost".into())),
                s => s,
            };
            if let Some(s) = state {
                f.state = s;
            }
            if matches!(f.state, Download::Done { .. } | Download::Failed(_)) {
                done.push(i);
            }
        }
        let mut finished: Vec<Fetching> = done.into_iter().rev().map(|i| self.fetching.remove(i)).collect();
        finished.reverse();
        for f in finished {
            let outcome = match &f.state {
                Download::Done { bytes, .. } => Ok(*bytes),
                Download::Failed(e) => Err(e.clone()),
                _ => Err("unfinished".into()),
            };
            if let Download::Done { photo, .. } = f.state {
                self.remember_capture(f.name(), Some(photo), f.ticket);
                self.load_photo(&f.path, f.camera.clone());
            }
            self.last = Some(finish_download(&f, outcome));
            self.next_file = None;
        }
    }

    /// Stop following the downloads, saving their live-view frames instead.
    fn abandon_downloads(&mut self, why: &str) {
        for f in std::mem::take(&mut self.fetching) {
            self.last = Some(finish_download(&f, Err(why.to_string())));
        }
        self.next_file = None;
    }

    /// Capture: the folder picker first when no folder is chosen, then the shutter; the frame is
    /// saved once the camera confirms ([`Stand::tick`]).
    pub fn capture(&mut self) {
        if self.pending.is_some() || self.test_wanted.is_some() {
            return self.fail("The last capture hasn't finished yet.");
        }
        if self.worker.is_none() {
            return self.fail("Connect to the camera first.");
        }
        if self.settings.folder.is_none() && !self.choose_folder() {
            if self.last.as_ref().is_none_or(|(ok, _)| *ok) {
                self.fail("No folder chosen, so nothing was captured.");
            }
            return;
        }
        let Some(folder) = self.settings.folder.clone() else { return };
        if let Err(e) = frames::check_folder(&folder) {
            return self.fail(format!("{e}: choose another folder."));
        }
        let prefix = self.prefix_in_use().to_string();
        self.fire(Some(folder), prefix);
    }

    /// Fire the shutter for a capture into `folder` (`None`: a test shot).
    fn fire(&mut self, folder: Option<PathBuf>, prefix: String) {
        let camera = self.status().map(|s| crate::presets::camera_values(&s.snap.settings)).unwrap_or_default();
        let asked = match self.worker.as_mut() {
            Some(w) => w.capture(),
            None => Err("the camera isn't connected".into()),
        };
        match asked {
            Ok((ticket, jpeg)) => {
                let what = if folder.is_some() { "Capturing\u{2026}" } else { "Taking a test shot\u{2026}" };
                self.pending = Some(Pending { ticket, jpeg, folder, prefix, deadline: Instant::now() + CAPTURE_TIMEOUT, camera });
                self.last = Some((true, what.into()));
            }
            Err(e) => self.fail(e),
        }
    }

    /// Follow up on what's under way (a capture, downloads, a search). Called on every repaint.
    pub fn tick(&mut self) {
        self.poll_find();
        self.poll_downloads();
        self.poll_shots();
        self.seed_preset();
        self.poll_claude();
        self.poll_assist();
        let Some(p) = &self.pending else { return };
        let outcome = self.worker.as_ref().map_or(CaptureOutcome::Failed("the camera was disconnected".into()), |w| w.capture_outcome(p.ticket));
        if outcome == CaptureOutcome::Done && p.folder.is_none() {
            if let Some(p) = self.pending.take() {
                self.fetch_test(p);
            }
            return;
        }
        if outcome == CaptureOutcome::Done && self.settings.full_size {
            if let Some(p) = self.pending.take() {
                self.last = Some(self.start_download(p));
                self.next_file = None;
            }
            return;
        }
        let result = match outcome {
            CaptureOutcome::Pending if Instant::now() < p.deadline => return,
            CaptureOutcome::Pending => {
                (false, format!("The camera didn't confirm the capture within {} s; nothing was saved.", CAPTURE_TIMEOUT.as_secs()))
            }
            CaptureOutcome::Failed(why) => (false, format!("{}; nothing was saved.", capitalized(&why))),
            CaptureOutcome::Done => {
                let photo = self.worker.as_ref().and_then(|w| w.capture_photo(p.ticket));
                let (ticket, folder) = (p.ticket, p.folder.clone().unwrap_or_default());
                let r = match &p.jpeg {
                    None => (false, "The shutter fired, but live view is off, so no frame was saved (the photo is on the camera's card).".into()),
                    Some(jpeg) => match frames::save_frame(&folder, &p.prefix, jpeg) {
                        Ok((path, _)) => (true, format!("Saved {}", path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default())),
                        Err(e) => (false, format!("The shutter fired, but the frame wasn't saved: {e}")),
                    },
                };
                let name = r.1.strip_prefix("Saved ").map_or_else(|| "(not saved)".to_string(), str::to_string);
                self.remember_capture(name, photo, ticket);
                r
            }
        };
        self.pending = None;
        self.next_file = None;
        self.last = Some(result);
    }

    /// The app is closing: downloads under way stop, and their live-view frames are saved.
    pub fn shutdown(&mut self) {
        if let Some(w) = self.worker.take() {
            w.close(CLOSE_WAIT);
        }
        self.abandon_downloads("the app was closed");
        self.end_batch("the app was closed");
    }
}

/// A download's end in the folder: the photo is there, or the live-view frame is saved in its
/// place so the sequence has no gap.
fn finish_download(f: &Fetching, outcome: Result<u64, String>) -> (bool, String) {
    let name = f.name();
    match outcome {
        Ok(bytes) => (true, format!("Saved {name} (full size, {:.1} MB)", bytes as f64 / 1_000_000.0)),
        Err(why) => {
            let why = why.trim_end_matches('.').to_string();
            match &f.fallback {
                Some(jpeg) => match frames::save_new(&f.path, jpeg) {
                    Ok(()) => (
                        false,
                        format!(
                            "{name}: the full-size photo wasn't downloaded ({why}); the live-view frame was saved instead. The photo is on the camera's card."
                        ),
                    ),
                    Err(e) => {
                        (false, format!("{name}: the full-size photo wasn't downloaded ({why}), and the live-view frame wasn't saved either: {e}"))
                    }
                },
                None => (
                    false,
                    format!(
                        "{name}: the full-size photo wasn't downloaded ({why}); nothing was saved (live view was off). The photo is on the camera's card."
                    ),
                ),
            }
        }
    }
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Scripted steps for checking the window (`--capture-at`, `--test-at`, `--screenshot`).
#[derive(Debug, Default)]
pub struct Script {
    /// Press Capture once, this many seconds after start.
    pub capture_at: Option<f64>,
    /// Press Test shot once, this many seconds after start.
    pub test_at: Option<f64>,
    tested: bool,
    /// Save a PNG of the window at this time (s), then quit.
    pub screenshot: Option<(PathBuf, f64)>,
    captured: bool,
    shot: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Script {
    pub fn new(capture_at: Option<f64>, test_at: Option<f64>, screenshot: Option<(PathBuf, f64)>) -> Script {
        Script { capture_at, test_at, screenshot, ..Script::default() }
    }
}

/// The eframe app.
pub struct StandApp {
    pub stand: Stand,
    texture: Option<TextureHandle>,
    texture_seq: u64,
    script: Script,
    /// The pictures' textures and view options.
    view: compare::View,
    /// The camera was ready at the last frame (the Connection section closes when it becomes
    /// ready and opens when it stops being).
    was_ready: Option<bool>,
}

impl StandApp {
    pub fn new(stand: Stand, script: Script) -> StandApp {
        StandApp { stand, texture: None, texture_seq: 0, script, view: compare::View::default(), was_ready: None }
    }

    /// One frame of the window (also run headless by the tests).
    pub fn frame(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.stand.tick();
        self.run_script(&ctx);
        // Space or Enter captures, unless a text field is being typed in. Taken before the
        // widgets see it, so a focused button doesn't also fire.
        if !ctx.text_edit_focused() {
            let pressed = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Space) | i.consume_key(Modifiers::NONE, Key::Enter));
            if pressed {
                self.stand.capture();
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::T)) {
                self.stand.test_shot();
            }
        }
        let status = self.stand.status();
        if status.as_ref().is_some_and(Status::open) || self.stand.capturing() || self.stand.finding() || self.stand.busy() {
            // The worker asks for a repaint on news; this keeps countdowns and timeouts moving.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        self.update_texture(&ctx);
        let ready = status.as_ref().is_some_and(Status::ready);
        let open_connection = (self.was_ready != Some(ready)).then_some(!ready);
        self.was_ready = Some(ready);
        egui::Panel::right("controls").exact_size(SIDE_W).resizable(false).frame(egui::Frame::side_top_panel(ui.style()).inner_margin(16)).show(
            ui,
            |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| panel::controls(ui, &mut self.stand, status.as_ref(), open_connection));
            },
        );
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::from_gray(14)).inner_margin(12)).show(ui, |ui| {
            compare::pictures(ui, &mut self.stand, &mut self.view, self.texture.as_ref(), status.as_ref());
        });
    }

    fn update_texture(&mut self, ctx: &egui::Context) {
        if !self.stand.connected() {
            self.texture = None;
            return;
        }
        let Some(frame) = self.stand.worker_frame() else { return };
        if self.texture.is_some() && frame.seq == self.texture_seq {
            return;
        }
        let (Ok(w), Ok(h)) = (usize::try_from(frame.width), usize::try_from(frame.height)) else { return };
        if frame.rgba.len() != w.saturating_mul(h).saturating_mul(4) {
            return;
        }
        let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &frame.rgba);
        match self.texture.as_mut() {
            Some(tex) => tex.set(image, TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("live_view", image, TextureOptions::LINEAR)),
        }
        self.texture_seq = frame.seq;
    }

    fn run_script(&mut self, ctx: &egui::Context) {
        let s = &mut self.script;
        if s.capture_at.is_none() && s.test_at.is_none() && s.screenshot.is_none() {
            return;
        }
        let t = ctx.input(|i| i.time);
        if let Some(at) = s.test_at
            && t >= at
            && !s.tested
        {
            s.tested = true;
            self.stand.test_shot();
        }
        if let Some(at) = s.capture_at
            && t >= at
            && !s.captured
        {
            s.captured = true;
            self.stand.capture();
        }
        if let Some((path, at)) = &s.screenshot {
            match &s.shot {
                None if t >= *at => {
                    // For whoever runs the script: what the window says, and the link's log.
                    if let Some((ok, m)) = &self.stand.last {
                        eprintln!("lumix-stand: last message ({}): {m}", if *ok { "ok" } else { "problem" });
                    }
                    for l in self.stand.status().map(|s| s.snap.log).unwrap_or_default() {
                        eprintln!("lumix-stand: log: {l}");
                    }
                    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let (d, path, c) = (done.clone(), path.clone(), ctx.clone());
                    ctx.request_screenshot(move |img| {
                        if let Err(e) = save_png(&path, &img) {
                            eprintln!("lumix-stand: the screenshot wasn't saved: {e}");
                        }
                        d.store(true, std::sync::atomic::Ordering::SeqCst);
                        c.request_repaint();
                    });
                    s.shot = Some(done);
                }
                Some(done) if done.load(std::sync::atomic::Ordering::SeqCst) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                _ => {}
            }
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn save_png(path: &std::path::Path, img: &egui::ColorImage) -> Result<(), String> {
    let [w, h] = img.size;
    let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else { return Err("the window is too big".into()) };
    let rgba: Vec<u8> = img.pixels.iter().flat_map(|p| p.to_array()).collect();
    image::save_buffer(path, &rgba, w, h, image::ExtendedColorType::Rgba8).map_err(|e| e.to_string())
}

impl eframe::App for StandApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }

    fn on_exit(&mut self) {
        self.stand.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::panel::refusal;
    use super::shots::ShotState;
    use super::*;
    use crate::frames::tests::TempDir;
    use lumix_link::Phase;

    fn stand(folder: Option<PathBuf>, pick: Option<PathBuf>, settings_path: Option<PathBuf>) -> Stand {
        let settings = Settings { folder, ..Settings::default() };
        Stand::new(settings, settings_path, Box::new(move |_: &str| pick.clone()), Arc::new(|| {}))
    }

    fn connect_sim(s: &mut Stand) {
        s.address = "sim".into();
        s.connect();
        for _ in 0..400 {
            if s.status().is_some_and(|st| st.snap.phase == Phase::Streaming && st.snap.frame_seq > 1) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the simulated camera didn't stream: {:?}", s.status());
    }

    /// Connected to a simulated camera changed by `f`.
    fn connect_sim_with(s: &mut Stand, f: impl FnOnce(&mut lumix_link::sim::SimOptions)) {
        let mut o = crate::worker::sim_options();
        f(&mut o);
        s.connect_to(Target::Sim(o), "sim");
        for _ in 0..400 {
            if s.status().is_some_and(|st| st.snap.phase == Phase::Streaming && st.snap.frame_seq > 1 && st.snap.photos.generation > 0) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the simulated camera didn't stream: {:?}", s.status());
    }

    /// Until the shutter is confirmed and every download has ended.
    fn finish_capture(s: &mut Stand) {
        for _ in 0..800 {
            s.tick();
            if !s.capturing() && s.fetching.is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the capture never finished: {:?}", s.last);
    }

    fn size_of(path: &std::path::Path) -> (u32, u32) {
        let f = crate::worker::decode(1, &std::fs::read(path).unwrap()).unwrap();
        (f.width, f.height)
    }

    fn saved_full(last: &Option<(bool, String)>, name: &str) -> bool {
        last.as_ref().is_some_and(|(ok, m)| *ok && m.starts_with(&format!("Saved {name} (full size, ")))
    }

    #[test]
    fn saves_the_full_size_photo_from_the_card() {
        let d = TempDir::new("app-full");
        let mut s = stand(Some(d.0.clone()), None, None);
        connect_sim(&mut s);
        s.capture();
        finish_capture(&mut s);
        assert!(saved_full(&s.last, "frame_0001.jpg"), "{:?}", s.last);
        assert_eq!(size_of(&d.0.join("frame_0001.jpg")), (1920, 1080), "the original, not live view");
        assert_eq!(std::fs::read_dir(&d.0).unwrap().count(), 1, "no .part file left");
    }

    #[test]
    fn captures_keep_coming_while_photos_download() {
        let d = TempDir::new("app-queue");
        let mut s = stand(Some(d.0.clone()), None, None);
        connect_sim_with(&mut s, |o| o.original_delay_ms = 1_500);
        for _ in 0..3 {
            s.capture();
            for _ in 0..200 {
                s.tick();
                if !s.capturing() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let names: Vec<String> = s.fetching.iter().map(Fetching::name).collect();
        assert_eq!(names, ["frame_0001.jpg", "frame_0002.jpg", "frame_0003.jpg"], "each its own number");
        assert_eq!(s.next_file().as_deref(), Some("frame_0004.jpg"), "past the reserved ones");
        finish_capture(&mut s);
        for n in 1..=3 {
            assert_eq!(size_of(&d.0.join(frames::file_name("frame", n))), (1920, 1080));
        }
    }

    #[test]
    fn a_download_cut_short_saves_the_live_view_frame() {
        let d = TempDir::new("app-cut");
        let mut s = stand(Some(d.0.clone()), None, None);
        connect_sim_with(&mut s, |o| o.original_delay_ms = 60_000);
        s.capture();
        for _ in 0..200 {
            s.tick();
            if !s.fetching.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(s.fetching.len(), 1);
        s.disconnect();
        assert!(s.last.as_ref().is_some_and(|(ok, m)| !ok && m.contains("live-view frame was saved instead")), "{:?}", s.last);
        assert_eq!(size_of(&d.0.join("frame_0001.jpg")), (640, 360));
    }

    #[test]
    fn live_view_frames_when_full_size_is_off() {
        let d = TempDir::new("app-lv");
        let mut s = stand(Some(d.0.clone()), None, None);
        s.set_full_size(false);
        connect_sim(&mut s);
        s.capture();
        finish_capture(&mut s);
        assert_eq!(s.last, Some((true, "Saved frame_0001.jpg".into())));
        assert_eq!(size_of(&d.0.join("frame_0001.jpg")), (640, 360));
    }

    #[test]
    fn changes_camera_settings() {
        let mut s = stand(None, None, None);
        connect_sim(&mut s);
        let value = |s: &Stand, k: &str| s.status().and_then(|st| st.snap.settings.value(k).map(str::to_string));
        for _ in 0..300 {
            if value(&s, "pictsize").is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(value(&s, "aspectratio").as_deref(), Some("16:9"));
        s.set_camera("iso", "800");
        s.largest_size();
        s.set_camera("focusmode", "mf");
        s.focus(FocusStep::Wide);
        for _ in 0..300 {
            if value(&s, "pictsize").as_deref() == Some("16m")
                && value(&s, "iso").as_deref() == Some("800")
                && s.status().is_some_and(|st| st.snap.settings.focus.is_some())
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let st = s.status().unwrap();
        assert_eq!((value(&s, "aspectratio").as_deref(), value(&s, "pictsize").as_deref()), (Some("4:3"), Some("16m")));
        assert_eq!(value(&s, "iso").as_deref(), Some("800"));
        assert_eq!(st.snap.settings.focus, Some((526, 1024)));
        assert_eq!(refusal(&st.snap.settings), None);
        s.set_camera("iso", "77");
        for _ in 0..300 {
            if s.status().is_some_and(|st| st.snap.settings.refused.is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(refusal(&s.status().unwrap().snap.settings).as_deref(), Some("ISO 77: the camera doesn't take that value."));
        s.shutdown();
    }

    #[test]
    fn captures_numbered_frames_into_the_folder() {
        let d = TempDir::new("app-capture");
        let mut s = stand(Some(d.0.clone()), None, None);
        connect_sim(&mut s);
        assert_eq!(s.next_file().as_deref(), Some("frame_0001.jpg"));
        s.capture();
        finish_capture(&mut s);
        assert!(saved_full(&s.last, "frame_0001.jpg"), "{:?}", s.last);
        let saved = std::fs::read(d.0.join("frame_0001.jpg")).unwrap();
        assert!(crate::worker::decode(1, &saved).is_some(), "a real JPEG");
        s.prefix = "take".into();
        s.commit_prefix();
        s.capture();
        finish_capture(&mut s);
        assert!(d.0.join("take_0001.jpg").exists());
        assert_eq!(s.next_file().as_deref(), Some("take_0002.jpg"));
        s.shutdown();
    }

    #[test]
    fn the_first_capture_asks_for_a_folder_and_remembers_it() {
        let d = TempDir::new("app-pick");
        let settings_file = d.0.join("settings.json");
        let mut s = stand(None, Some(d.0.clone()), Some(settings_file.clone()));
        connect_sim(&mut s);
        s.capture();
        finish_capture(&mut s);
        assert!(d.0.join("frame_0001.jpg").exists());
        assert_eq!(Settings::load(&settings_file).0.folder, Some(d.0.clone()), "saved");
        assert_eq!(Settings::load(&settings_file).0.address, lumix_link::DEFAULT_IP, "sim isn't remembered");
    }

    #[test]
    fn a_cancelled_picker_captures_nothing() {
        let mut s = stand(None, None, None);
        connect_sim(&mut s);
        s.capture();
        assert!(!s.capturing());
        assert_eq!(s.last, Some((false, "No folder chosen, so nothing was captured.".into())));
    }

    #[test]
    fn live_view_off_fires_but_saves_nothing() {
        let d = TempDir::new("app-off");
        let mut s = stand(Some(d.0.clone()), None, None);
        s.set_full_size(false);
        connect_sim(&mut s);
        s.live_view(false);
        for _ in 0..400 {
            if s.status().is_some_and(|st| st.snap.phase == Phase::Connected) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        s.capture();
        finish_capture(&mut s);
        assert!(matches!(&s.last, Some((false, m)) if m.contains("live view is off")), "{:?}", s.last);
        assert_eq!(std::fs::read_dir(&d.0).unwrap().count(), 0);

        // At full size, live view doesn't matter.
        s.set_full_size(true);
        s.capture();
        finish_capture(&mut s);
        assert!(saved_full(&s.last, "frame_0001.jpg"), "{:?}", s.last);
    }

    #[test]
    fn bad_input_is_explained() {
        let mut s = stand(None, None, None);
        s.capture();
        assert_eq!(s.last, Some((false, "Connect to the camera first.".into())));
        s.address = "not an address".into();
        s.connect();
        assert!(!s.connected());
        assert!(s.last.as_ref().is_some_and(|(ok, m)| !ok && m.contains("192.168.54.1")));
        s.prefix = "a/b".into();
        s.commit_prefix();
        assert_eq!(s.prefix, "", "put back");
    }

    /// Tick until `ok`, or fail saying `what`.
    fn until(s: &mut Stand, what: &str, ok: impl Fn(&Stand) -> bool) {
        for _ in 0..1000 {
            s.tick();
            if ok(s) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("never got to: {what} ({:?})", s.last);
    }

    fn settings_read(s: &Stand) -> bool {
        s.status().is_some_and(|st| st.snap.settings.menu_read && st.snap.settings.value("pictsize").is_some() && st.snap.commands_pending == 0)
    }

    #[test]
    fn a_test_shot_shows_the_preview_and_saves_nothing() {
        let d = TempDir::new("app-test-shot");
        let mut s = stand(Some(d.0.clone()), None, None);
        connect_sim(&mut s);
        until(&mut s, "settings read", settings_read);
        s.set_camera("iso", "400");
        s.test_shot();
        assert!(s.capturing(), "a capture waits for the test shot");
        until(&mut s, "the preview", |s| !s.testing());
        let t = s.shown_test().unwrap();
        assert!(matches!(&t.state, shots::ShotState::Ready { frame, .. } if (frame.width, frame.height) == (640, 360)), "{:?}", s.last);
        assert!(t.camera.contains(&("iso".to_string(), "400".to_string())), "taken after the change reached the camera");
        assert_eq!(s.compare, Compare::Test);
        assert_eq!(std::fs::read_dir(&d.0).unwrap().count(), 0, "nothing in the folder");
        assert!(s.session.is_empty(), "not one of the session's captures");

        s.save_test_as_preset(t.ticket);
        assert!(s.settings.presets.iter().any(|p| p.name.ends_with("(Test 1)") && p.camera.contains(&("iso".into(), "400".into()))));
        s.shutdown();
    }

    #[test]
    fn the_last_photo_is_shown_and_waiting_downloads_dont_time_out() {
        let d = TempDir::new("app-last");
        let mut s = stand(Some(d.0.clone()), None, None);
        connect_sim_with(&mut s, |o| o.original_ms = 600);
        s.capture();
        until(&mut s, "the first download", |s| s.fetching.len() == 1);
        s.capture();
        until(&mut s, "the second queued", |s| {
            s.fetching.len() == 2 && matches!(s.fetching[0].state, Download::Downloading { .. } | Download::Finding(_))
        });
        assert!(s.fetching[0].deadline.is_some(), "the clock runs once a download starts");
        assert!(s.fetching[1].deadline.is_none(), "not while it waits its turn");
        finish_capture(&mut s);
        until(&mut s, "the photo read", |s| s.photo.is_some() && !s.loading_photo());
        let p = s.photo.as_ref().unwrap();
        assert_eq!((p.name.as_str(), p.frame.width), ("frame_0002.jpg", 1920));
        assert_eq!(s.compare, Compare::Photo);
        s.shutdown();
    }

    #[test]
    fn fast_downloads_pause_live_view_meanwhile() {
        let d = TempDir::new("app-fast");
        let mut s = stand(Some(d.0.clone()), None, None);
        s.set_fast_downloads(true);
        connect_sim_with(&mut s, |o| o.original_ms = 800);
        s.capture();
        until(&mut s, "live view paused", |s| s.status().is_some_and(|st| st.paused && st.snap.phase == Phase::Connected));
        finish_capture(&mut s);
        until(&mut s, "live view back", |s| s.status().is_some_and(|st| !st.paused && st.snap.phase == Phase::Streaming));
        assert!(saved_full(&s.last, "frame_0001.jpg"), "{:?}", s.last);
        s.shutdown();
    }

    #[test]
    fn presets_save_apply_and_persist() {
        let d = TempDir::new("app-presets");
        let file = d.0.join("settings.json");
        let mut s = stand(None, None, Some(file.clone()));
        connect_sim(&mut s);
        until(&mut s, "the first preset", |s| s.settings.presets.len() == 1);
        assert_eq!(s.settings.presets[0].name, "Preset 1");
        assert_eq!(s.matching_preset(), Some(0), "it is the present settings");

        s.set_camera("iso", "1600");
        s.prefix = "dark".into();
        s.commit_prefix();
        until(&mut s, "ISO changed", |s| settings_read(s) && s.status().unwrap().snap.settings.value("iso") == Some("1600"));
        assert_eq!(s.matching_preset(), None);
        s.save_preset();
        assert_eq!(s.settings.presets[1].name, "Preset 2");
        assert_eq!(s.settings.presets[1].prefix, "dark");

        s.apply_preset(0);
        until(&mut s, "Preset 1 back", |s| settings_read(s) && s.matching_preset() == Some(0));
        assert_eq!(s.prefix_in_use(), "frame", "the capture options too");

        s.rename_preset(1, "  Night  ");
        s.update_preset(1);
        assert_eq!(s.settings.presets[1].name, "Night");
        assert_eq!(s.matching_preset(), Some(0), "the first that matches");
        s.delete_preset(0);
        let (saved, _) = Settings::load(&file);
        assert_eq!(saved.presets.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Night"]);
        assert!(saved.presets_seeded, "deleting every preset doesn't bring the first back");
        s.shutdown();
    }

    #[test]
    fn downloads_photos_from_the_card_under_their_names() {
        let frames_dir = TempDir::new("app-card-frames");
        let out = TempDir::new("app-card-out");
        let mut s = stand(Some(frames_dir.0.clone()), Some(out.0.clone()), None);
        s.set_full_size(false);
        connect_sim_with(&mut s, |o| o.original_delay_ms = 0);
        for _ in 0..2 {
            s.capture();
            finish_capture(&mut s);
        }
        until(&mut s, "the session's photos", |s| s.session_photos().len() == 2);
        let first = s.session_photos()[0];
        std::fs::write(out.0.join(first.file_name()), b"already here").unwrap();
        s.download_photos();
        until(&mut s, "the download", |s| s.batch.as_ref().is_some_and(CardBatch::done));
        assert_eq!(s.batch.as_ref().unwrap().counts(), (1, 1, 2), "one saved, one skipped: {:?}", s.batch.as_ref().unwrap().items);
        assert_eq!(std::fs::read(out.0.join(first.file_name())).unwrap(), b"already here", "never replaced");
        assert_eq!(size_of(&out.0.join(first.next().file_name())), (1920, 1080), "full size");

        s.source = Source::Range;
        s.range_from = first.file_name();
        s.range_to = "P0000001".into();
        assert!(s.chosen_photos().is_err());
        let before = lumix_link::PhotoId(first.0 - 2);
        s.range_from = before.file_name();
        s.range_to = format!("P{:07}", before.0 + 1);
        s.download_photos();
        until(&mut s, "the range", |s| s.batch.as_ref().is_some_and(CardBatch::done));
        assert_eq!(s.batch.as_ref().unwrap().counts().0, 2, "{:?}", s.last);
        s.shutdown();
    }

    #[test]
    fn finds_the_best_exposure_by_measuring() {
        let mut s = stand(None, None, None);
        connect_sim(&mut s);
        until(&mut s, "settings read", settings_read);
        // Two stops too dark to start with, in MF.
        s.set_camera("iso", "200");
        s.set_camera("shtrspeed", &lumix_link::apex::format(8 * 256));
        s.set_camera("focusmode", "mf");
        until(&mut s, "set", |s| settings_read(s) && s.status().unwrap().snap.settings.value("focusmode") == Some("mf"));
        s.find_exposure();
        until(&mut s, "the search", |s| s.assist.as_ref().is_some_and(|a| !a.running()));
        let a = s.assist.as_ref().unwrap();
        assert_eq!(a.outcome.as_ref().map(|o| o.0), Some(true), "{:?}", a.log);
        assert!(a.log[0].contains("AF"), "focused first in MF: {:?}", a.log);
        assert!(a.shots >= 2, "it moved the shutter: {:?}", a.log);
        let cs = s.status().unwrap().snap.settings;
        assert_eq!((cs.value("iso"), cs.value("focal")), (Some("200"), Some("1280/256")), "least grain, sharpest");
        let ShotState::Ready { look, .. } = &s.tests.last().unwrap().state else { panic!("no preview") };
        assert!(crate::exposure::right(look), "the last shot is right");
        s.shutdown();
    }

    fn run(app: &mut StandApp, ctx: &egui::Context, events: Vec<egui::Event>) {
        let input = egui::RawInput { events, ..Default::default() };
        ctx.run_ui(input, |ui| app.frame(ui)).textures_delta.clear();
    }

    fn key(k: Key) -> egui::Event {
        egui::Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }
    }

    #[test]
    fn the_window_draws_and_space_captures() {
        let d = TempDir::new("app-window");
        let mut app = StandApp::new(stand(Some(d.0.clone()), None, None), Script::default());
        let ctx = egui::Context::default();
        run(&mut app, &ctx, vec![]);
        assert!(app.texture.is_none());
        connect_sim(&mut app.stand);
        run(&mut app, &ctx, vec![]);
        assert!(app.texture.is_some(), "live view uploaded");
        run(&mut app, &ctx, vec![key(Key::Space)]);
        assert!(app.stand.capturing(), "Space fired the shutter");
        finish_capture(&mut app.stand);
        assert!(d.0.join("frame_0001.jpg").exists());
        run(&mut app, &ctx, vec![key(Key::Enter)]);
        finish_capture(&mut app.stand);
        assert!(d.0.join("frame_0002.jpg").exists(), "Enter too");
        run(&mut app, &ctx, vec![key(Key::T)]);
        assert!(app.stand.testing(), "T takes a test shot");
        until(&mut app.stand, "the test shot", |s| !s.testing());
        for _ in 0..3 {
            run(&mut app, &ctx, vec![]);
        }
        assert!(!app.view.tests_empty(), "its texture made for the strip");
        app.stand.shutdown();
    }
}
