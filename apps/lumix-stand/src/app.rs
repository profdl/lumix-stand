//! The window: live view on the left, the camera and capture controls on the right.
//!
//! Layout ported from Picabia (`crates/ui-egui/src/picabia_camera_window.rs`). The state and
//! everything the buttons do live in [`Stand`], which runs without a window, so tests drive it
//! against the simulated camera; [`StandApp`] draws it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, Modifiers, RichText, TextureHandle, TextureOptions, Ui, vec2};
use lumix_link::menu::{self, Group, Kind, Setting};
use lumix_link::{CameraSettings, CaptureOutcome, FocusStep, Phase, apex};

use crate::frames;
use crate::settings::Settings;
use crate::worker::{Download, SIM_ADDRESS, Status, Target, Worker};

/// The longest a capture waits for the camera to say it fired.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long Find searches.
const FIND_MS: u64 = 1500;
/// How long closing waits for the camera's last `stopstream`.
const CLOSE_WAIT: Duration = Duration::from_millis(500);
/// The longest a full-size download may take, finding the photo included.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// The controls' width.
const SIDE_W: f32 = 370.0;

/// Shows the folder picker; `None`: cancelled.
pub type Picker = Box<dyn FnMut() -> Option<PathBuf>>;

/// A capture waiting for the camera's answer.
struct Pending {
    ticket: u64,
    jpeg: Option<Arc<Vec<u8>>>,
    folder: PathBuf,
    prefix: String,
    deadline: Instant,
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
    deadline: Instant,
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
}

impl Stand {
    pub fn new(settings: Settings, settings_path: Option<PathBuf>, picker: Picker, repaint: Arc<dyn Fn() + Send + Sync>) -> Stand {
        Stand {
            address: settings.address.clone(),
            prefix: settings.prefix.clone(),
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
        self.pending.is_some()
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
            Ok(w) => self.worker = Some(w),
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
        if self.pending.take().is_some() {
            self.fail("Disconnected before the camera confirmed the capture; nothing was saved.");
        }
        if let Some(w) = self.worker.take() {
            w.close(CLOSE_WAIT);
        }
        self.abandon_downloads("the camera was disconnected");
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
        let Some(folder) = (self.picker)() else { return false };
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
        let n = match self.free_number(&p.folder, &p.prefix) {
            Ok(n) => n,
            Err(e) => return (false, format!("The shutter fired, but the frame wasn't saved: {e}")),
        };
        let path = p.folder.join(frames::file_name(&p.prefix, n));
        let started = self.worker.as_ref().map_or_else(|| Err("the camera isn't connected".to_string()), |w| w.download(p.ticket, path.clone()));
        let f = Fetching {
            ticket: p.ticket,
            folder: p.folder,
            prefix: p.prefix,
            n,
            path,
            fallback: p.jpeg,
            deadline: Instant::now() + DOWNLOAD_TIMEOUT,
            state: Download::Waiting,
        };
        let name = f.name();
        if let Err(e) = started {
            return finish_download(&f, Err(e));
        }
        self.fetching.push(f);
        (true, format!("Shutter fired; downloading the full-size photo as {name}\u{2026}"))
    }

    /// Follow the downloads; a finished one becomes the last message.
    fn poll_downloads(&mut self) {
        let mut done = Vec::new();
        for (i, f) in self.fetching.iter_mut().enumerate() {
            let state = self.worker.as_ref().map_or(Some(Download::Failed("the camera was disconnected".into())), |w| w.download_state(f.ticket));
            let state = match state {
                Some(Download::Done { .. } | Download::Failed(_)) => state,
                _ if Instant::now() >= f.deadline => Some(Download::Failed(format!("it took longer than {} s", DOWNLOAD_TIMEOUT.as_secs()))),
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
        for i in done.into_iter().rev() {
            let f = self.fetching.remove(i);
            let outcome = match &f.state {
                Download::Done { bytes, .. } => Ok(*bytes),
                Download::Failed(e) => Err(e.clone()),
                _ => Err("unfinished".into()),
            };
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
        if self.pending.is_some() {
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
        let asked = match self.worker.as_mut() {
            Some(w) => w.capture(),
            None => Err("the camera isn't connected".into()),
        };
        match asked {
            Ok((ticket, jpeg)) => {
                self.pending = Some(Pending { ticket, jpeg, folder, prefix, deadline: Instant::now() + CAPTURE_TIMEOUT });
                self.last = Some((true, "Capturing\u{2026}".into()));
            }
            Err(e) => self.fail(e),
        }
    }

    /// Follow up on what's under way (a capture, downloads, a search). Called on every repaint.
    pub fn tick(&mut self) {
        self.poll_find();
        self.poll_downloads();
        let Some(p) = &self.pending else { return };
        let outcome = self.worker.as_ref().map_or(CaptureOutcome::Failed("the camera was disconnected".into()), |w| w.capture_outcome(p.ticket));
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
            CaptureOutcome::Done => match &p.jpeg {
                None => (false, "The shutter fired, but live view is off, so no frame was saved (the photo is on the camera's card).".into()),
                Some(jpeg) => match frames::save_frame(&p.folder, &p.prefix, jpeg) {
                    Ok((path, _)) => (true, format!("Saved {}", path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default())),
                    Err(e) => (false, format!("The shutter fired, but the frame wasn't saved: {e}")),
                },
            },
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

/// Scripted steps for checking the window (`--capture-at`, `--screenshot`).
#[derive(Debug, Default)]
pub struct Script {
    /// Press Capture once, this many seconds after start.
    pub capture_at: Option<f64>,
    /// Save a PNG of the window at this time (s), then quit.
    pub screenshot: Option<(PathBuf, f64)>,
    captured: bool,
    shot: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Script {
    pub fn new(capture_at: Option<f64>, screenshot: Option<(PathBuf, f64)>) -> Script {
        Script { capture_at, screenshot, ..Script::default() }
    }
}

/// The eframe app.
pub struct StandApp {
    pub stand: Stand,
    texture: Option<TextureHandle>,
    texture_seq: u64,
    script: Script,
}

impl StandApp {
    pub fn new(stand: Stand, script: Script) -> StandApp {
        StandApp { stand, texture: None, texture_seq: 0, script }
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
        }
        let status = self.stand.status();
        if status.as_ref().is_some_and(Status::open) || self.stand.capturing() || self.stand.finding() {
            // The worker asks for a repaint on news; this keeps countdowns and timeouts moving.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        self.update_texture(&ctx);
        egui::Panel::right("controls").exact_size(SIDE_W).resizable(false).frame(egui::Frame::side_top_panel(ui.style()).inner_margin(16)).show(
            ui,
            |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| controls(ui, &mut self.stand, status.as_ref()));
            },
        );
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::from_gray(14)).inner_margin(12)).show(ui, |ui| {
            live_view(ui, self.texture.as_ref(), status.as_ref());
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
        if s.capture_at.is_none() && s.screenshot.is_none() {
            return;
        }
        let t = ctx.input(|i| i.time);
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

// ---------- drawing ----------

const DIM: Color32 = Color32::from_gray(150);
const WARN: Color32 = Color32::from_rgb(240, 180, 90);
const GOOD: Color32 = Color32::from_rgb(140, 210, 150);

fn note(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).size(12.0).color(DIM));
}

fn wrapped(ui: &mut Ui, text: impl Into<String>, color: Color32) {
    ui.add(egui::Label::new(RichText::new(text.into()).color(color)).wrap());
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(title).size(11.5).strong().color(DIM));
    ui.add_space(4.0);
}

fn live_view(ui: &mut Ui, texture: Option<&TextureHandle>, status: Option<&Status>) {
    let room = ui.available_rect_before_wrap();
    let aspect = texture.map_or(9.0 / 16.0, |t| {
        let [w, h] = t.size();
        if w > 0 { h as f32 / w as f32 } else { 9.0 / 16.0 }
    });
    let w = room.width().min(room.height() / aspect).max(1.0);
    let rect = egui::Rect::from_center_size(room.center(), vec2(w, w * aspect));
    match (texture, status.is_some_and(Status::open)) {
        (Some(tex), true) => {
            egui::Image::new(tex).paint_at(ui, rect);
        }
        _ => {
            ui.painter().rect_filled(rect, 6.0, Color32::from_gray(28));
            let text = match status {
                Some(s) if s.open() && s.snap.phase == Phase::Connected => "Live view is off",
                Some(s) if s.open() => "Waiting for live view\u{2026}",
                _ => "No camera connected",
            };
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(15.0), DIM);
        }
    }
    ui.allocate_rect(room, egui::Sense::hover());
}

fn status_line(ui: &mut Ui, status: Option<&Status>) {
    let Some(s) = status.filter(|s| s.open()) else {
        ui.label(RichText::new("Not connected").size(15.0));
        if let Some(why) = status.and_then(|s| s.ended.clone()).filter(|w| w != "disconnected") {
            wrapped(ui, why, WARN);
        }
        return;
    };
    let snap = &s.snap;
    let headline = if snap.awaiting_accept {
        "Waiting for the camera to accept\u{2026}"
    } else {
        match snap.phase {
            Phase::Connecting => "Connecting\u{2026}",
            Phase::Connected => "Connected, live view off",
            Phase::Streaming => "Live view",
            Phase::Lost => "No answer",
            Phase::Closed => "Not connected",
        }
    };
    ui.label(RichText::new(headline).size(15.0).color(if snap.phase == Phase::Streaming && !snap.no_frames { GOOD } else { Color32::WHITE }));
    let mut details = vec![if s.sim { "simulated camera".to_string() } else { s.address.clone() }];
    if snap.phase == Phase::Streaming {
        details.push(format!("{} fps", snap.fps));
        details.push(format!("{} frames", snap.frame_seq));
    }
    if let Some((_, b)) = snap.state.iter().find(|(k, _)| k == "batt") {
        details.push(format!("battery {b}"));
    }
    if let Some(ms) = snap.retry_in_ms {
        details.push(format!("trying again in {} s", ms.div_ceil(1000)));
    }
    note(ui, details.join(" \u{b7} "));
    if let Some(h) = s.hint() {
        ui.add_space(4.0);
        wrapped(ui, h, WARN);
    }
    if let Some(e) = &snap.last_error {
        ui.add_space(4.0);
        wrapped(ui, e.clone(), DIM);
    }
}

fn controls(ui: &mut Ui, stand: &mut Stand, status: Option<&Status>) {
    ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
    ui.heading("Lumix Stand");
    ui.add_space(6.0);
    status_line(ui, status);

    let connected = stand.connected();
    section(ui, "CAMERA");
    ui.horizontal(|ui| {
        if connected {
            let mut shown = status.map(|s| s.address.clone()).unwrap_or_default();
            ui.add_enabled(false, egui::TextEdit::singleline(&mut shown).desired_width(130.0));
            if ui.button("Disconnect").clicked() {
                stand.disconnect();
            }
        } else {
            let r = ui.add(egui::TextEdit::singleline(&mut stand.address).desired_width(130.0).hint_text(lumix_link::DEFAULT_IP));
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if (ui.add_enabled(!stand.address.trim().is_empty(), egui::Button::new("Connect")).clicked() || enter) && !stand.address.trim().is_empty()
            {
                stand.connect();
            }
        }
        let label = if stand.finding() { "Finding\u{2026}" } else { "Find" };
        if ui.add_enabled(!connected && !stand.finding(), egui::Button::new(label)).clicked() {
            stand.find();
        }
    });
    if !connected && !stand.found.is_empty() {
        let mut chosen = None;
        ui.horizontal_wrapped(|ui| {
            for (ip, label) in &stand.found {
                if ui.small_button(label).clicked() {
                    chosen = Some(ip.clone());
                }
            }
        });
        if let Some(ip) = chosen {
            stand.address = ip;
        }
    }
    note(ui, "On the camera's own Wi-Fi it is 192.168.54.1. \u{201c}sim\u{201d} is a simulated camera.");
    if connected {
        let live = status.is_some_and(|s| s.snap.live_view);
        let label = if live { "Stop Live View" } else { "Start Live View" };
        if ui.button(label).clicked() {
            stand.live_view(!live);
        }
    }
    if let Some(s) = status.filter(|s| s.open()) {
        camera_settings(ui, stand, s);
    }

    section(ui, "CAPTURES");
    ui.horizontal(|ui| {
        let folder = stand.settings.folder.as_ref().map(|f| f.display().to_string());
        let shown = folder
            .as_deref()
            .and_then(|f| std::path::Path::new(f).file_name())
            .map_or_else(|| "Not chosen".into(), |n| n.to_string_lossy().to_string());
        if ui.button("Folder\u{2026}").clicked() {
            stand.choose_folder();
        }
        ui.add(egui::Label::new(RichText::new(shown).color(if folder.is_some() { Color32::WHITE } else { DIM })).truncate())
            .on_hover_text(folder.unwrap_or_default());
    });
    ui.horizontal(|ui| {
        ui.label("Prefix");
        let r = ui.add(egui::TextEdit::singleline(&mut stand.prefix).desired_width(120.0).hint_text(frames::DEFAULT_PREFIX));
        if r.lost_focus() {
            stand.commit_prefix();
        }
    });
    match stand.next_file() {
        Some(next) => note(ui, format!("Next: {next}")),
        None if stand.settings.folder.is_none() => note(ui, "The first Capture asks for a folder."),
        None => note(ui, "Next: \u{2014}"),
    }
    let mut full = stand.settings.full_size;
    if ui.checkbox(&mut full, "Full size from the camera's card").changed() {
        stand.set_full_size(full);
    }
    note(
        ui,
        if full {
            "Each photo downloads in the background, 10\u{2013}20 s over the camera's Wi-Fi, while you keep shooting."
        } else {
            "Saves the live-view frame (640\u{d7}360) at once."
        },
    );
    for f in &stand.fetching {
        let what = match &f.state {
            Download::Waiting => "waiting for the card".to_string(),
            Download::Finding(p) => format!("looking for {}", p.file_name()),
            Download::Downloading { got, total: Some(t), .. } => format!("{:.1} of {:.1} MB", *got as f64 / 1e6, *t as f64 / 1e6),
            Download::Downloading { got, .. } => format!("{:.1} MB", *got as f64 / 1e6),
            Download::Done { .. } => "done".into(),
            Download::Failed(e) => e.clone(),
        };
        note(ui, format!("\u{2193} {}: {what}", f.name()));
    }

    ui.add_space(12.0);
    let ready = status.is_some_and(Status::ready) && !stand.capturing();
    let button = egui::Button::new(RichText::new("Capture").size(17.0).strong())
        .fill(Color32::from_rgb(170, 50, 40))
        .min_size(vec2(ui.available_width(), 44.0));
    if ui.add_enabled(ready, button).clicked() {
        stand.capture();
    }
    note(ui, "Space or Enter captures too.");
    if let Some((ok, msg)) = &stand.last {
        ui.add_space(4.0);
        wrapped(ui, msg.clone(), if *ok { GOOD } else { WARN });
    }
}

/// The camera's settings, by group.
fn camera_settings(ui: &mut Ui, stand: &mut Stand, status: &Status) {
    let cs = status.snap.settings.clone();
    let ready = status.ready();
    section(ui, "CAMERA SETTINGS");
    if !ready {
        note(ui, "They can be changed once the camera is connected.");
    } else if cs.values.is_empty() {
        note(ui, "Reading the camera's settings\u{2026}");
    }
    let catalogue = menu::catalogue();
    for group in Group::ALL {
        egui::CollapsingHeader::new(group.title()).id_salt(group.title()).default_open(matches!(group, Group::Exposure | Group::Image)).show(ui, |ui| {
            for setting in catalogue.iter().filter(|s| s.group == group) {
                setting_row(ui, stand, setting, &cs, ready);
            }
            match group {
                Group::Focus => focus_row(ui, stand, &cs, ready),
                Group::Image => {
                    let largest = cs.value("aspectratio") == Some("4:3") && cs.value("pictsize") == Some(menu::largest_size("4:3"));
                    let label = if largest { "Largest size \u{2713}" } else { "Largest size" };
                    if ui.add_enabled(ready && !largest, egui::Button::new(label)).on_hover_text("4:3 and 16M: the whole sensor, 4592\u{d7}3448. A standard or RAW-only quality becomes fine (a RAW file can't be downloaded; its JPEG can).").clicked() {
                        stand.largest_size();
                    }
                }
                _ => {}
            }
        });
    }
    if let Some(l) = cs.lens {
        let mut lens = format!("Lens: {} to {}", apex::aperture_label(l.av_wide), apex::aperture_label(l.av_narrow));
        if let Some(f) = l.focal_label() {
            lens = format!("{lens}, {f}");
        }
        note(ui, lens);
    }
    if let Some(why) = refusal(&cs) {
        wrapped(ui, why, WARN);
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(ready, egui::Button::new("Read again").small())
            .on_hover_text("Settings changed on the camera are also read every 30 s.")
            .clicked()
        {
            stand.refresh_camera();
        }
        if status.snap.commands_pending > 0 {
            note(ui, "Talking to the camera\u{2026}");
        }
    });
}

/// One setting as a drop-down (choices the camera doesn't allow now are greyed).
fn setting_row(ui: &mut Ui, stand: &mut Stand, setting: &Setting, cs: &CameraSettings, ready: bool) {
    let current = cs.value(setting.key);
    let shown = current.map_or_else(|| "\u{2014}".to_string(), |v| setting.value_label(v));
    let mut chosen = None;
    ui.horizontal(|ui| {
        ui.add_sized([112.0, 20.0], egui::Label::new(setting.label).truncate());
        ui.add_enabled_ui(ready && current.is_some(), |ui| {
            egui::ComboBox::from_id_salt(setting.key).width(190.0).height(320.0).selected_text(shown).show_ui(ui, |ui| {
                for (value, label, allowed) in options(setting, cs) {
                    let on = current == Some(value.as_str());
                    if ui.add_enabled(allowed, egui::Button::selectable(on, label)).clicked() && !on {
                        chosen = Some(value);
                    }
                }
            });
        });
    });
    if let Some(v) = chosen {
        stand.set_camera(setting.key, &v);
    }
}

/// A setting's choices: (value, label, allowed now).
fn options(setting: &Setting, cs: &CameraSettings) -> Vec<(String, String, bool)> {
    let lens = cs.lens.unwrap_or_default();
    match &setting.kind {
        Kind::Shutter => {
            apex::shutter_stops(lens.tv_slow, lens.tv_fast).into_iter().map(|v| (apex::format(v), apex::shutter_label(v), true)).collect()
        }
        Kind::Aperture => {
            apex::aperture_stops(lens.av_wide, lens.av_narrow).into_iter().map(|v| (apex::format(v), apex::aperture_label(v), true)).collect()
        }
        Kind::Menu(choices) => choices.iter().map(|c| (c.value.clone(), c.label.clone(), cs.enabled(&c.menu_id))).collect(),
    }
}

/// Focus steps and AF once, in MF.
fn focus_row(ui: &mut Ui, stand: &mut Stand, cs: &CameraSettings, ready: bool) {
    let mf = cs.value("focusmode") == Some("mf");
    if !mf {
        note(ui, "Set Focus mode to MF to focus from here.");
        return;
    }
    ui.horizontal(|ui| {
        let steps = [
            ("\u{25c0}\u{25c0}", FocusStep::TeleFast, "A big step toward tele"),
            ("\u{25c0}", FocusStep::Tele, "A small step toward tele"),
            ("\u{25b6}", FocusStep::Wide, "A small step toward wide"),
            ("\u{25b6}\u{25b6}", FocusStep::WideFast, "A big step toward wide"),
        ];
        for (label, step, hover) in steps {
            if ui.add_enabled(ready, egui::Button::new(label)).on_hover_text(hover).clicked() {
                stand.focus(step);
            }
        }
        if ui.add_enabled(ready, egui::Button::new("AF once")).on_hover_text("Focus automatically once, then stay in MF.").clicked() {
            stand.one_shot_af();
        }
    });
    if let Some((pos, max)) = cs.focus {
        ui.add(egui::ProgressBar::new(pos as f32 / max.max(1) as f32).text(format!("focus {pos} of {max}")).desired_height(14.0));
    }
}

/// The camera's last refusal, in plain words.
fn refusal(cs: &CameraSettings) -> Option<String> {
    let (key, value, result) = cs.refused.as_ref()?;
    let what = match key.as_str() {
        "focus" => "The focus step".to_string(),
        "oneshot_af" => "AF once".to_string(),
        k => menu::find(k).map_or_else(|| k.to_string(), |s| format!("{} {}", s.label, s.value_label(value))),
    };
    let why = match result.as_str() {
        "err_reject" => "the camera won't take it in its present state (check the mode dial)".to_string(),
        "err_param" => "the camera doesn't take that value".to_string(),
        "err_busy" => "the camera is busy".to_string(),
        other => format!("the camera answered {other}"),
    };
    Some(format!("{what}: {why}."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::tests::TempDir;

    fn stand(folder: Option<PathBuf>, pick: Option<PathBuf>, settings_path: Option<PathBuf>) -> Stand {
        let settings = Settings { folder, ..Settings::default() };
        Stand::new(settings, settings_path, Box::new(move || pick.clone()), Arc::new(|| {}))
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
        app.stand.shutdown();
    }
}
