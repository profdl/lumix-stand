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
use lumix_link::{CaptureOutcome, Phase};

use crate::frames;
use crate::settings::Settings;
use crate::worker::{SIM_ADDRESS, Status, Target, Worker};

/// The longest a capture waits for the camera to say it fired.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long Find searches.
const FIND_MS: u64 = 1500;
/// How long closing waits for the camera's last `stopstream`.
const CLOSE_WAIT: Duration = Duration::from_millis(500);
/// The controls' width.
const SIDE_W: f32 = 330.0;

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

    /// Close the link (the camera is asked to stop streaming).
    pub fn disconnect(&mut self) {
        if self.pending.take().is_some() {
            self.fail("Disconnected before the camera confirmed the capture; nothing was saved.");
        }
        if let Some(w) = self.worker.take() {
            w.close(CLOSE_WAIT);
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
        let name = self.settings.folder.as_ref().and_then(|f| frames::next_number(f, &prefix).ok()).map(|n| frames::file_name(&prefix, n));
        self.next_file = Some((Instant::now(), name.clone()));
        name
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

    /// Follow up on what's under way (a capture, a search). Called on every repaint.
    pub fn tick(&mut self) {
        self.poll_find();
        let Some(p) = &self.pending else { return };
        let outcome = self.worker.as_ref().map_or(CaptureOutcome::Failed("the camera was disconnected".into()), |w| w.capture_outcome(p.ticket));
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

    /// The app is closing.
    pub fn shutdown(&mut self) {
        if let Some(w) = self.worker.take() {
            w.close(CLOSE_WAIT);
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
        egui::Panel::right("controls")
            .exact_size(SIDE_W)
            .resizable(false)
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(16))
            .show(ui, |ui| controls(ui, &mut self.stand, status.as_ref()));
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

    fn finish_capture(s: &mut Stand) {
        for _ in 0..400 {
            s.tick();
            if !s.capturing() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the capture never finished");
    }

    #[test]
    fn captures_numbered_frames_into_the_folder() {
        let d = TempDir::new("app-capture");
        let mut s = stand(Some(d.0.clone()), None, None);
        connect_sim(&mut s);
        assert_eq!(s.next_file().as_deref(), Some("frame_0001.jpg"));
        s.capture();
        finish_capture(&mut s);
        assert_eq!(s.last, Some((true, "Saved frame_0001.jpg".into())));
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
