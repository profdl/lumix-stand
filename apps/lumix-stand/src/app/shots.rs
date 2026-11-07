//! Test shots, the last full-size photo, presets, and downloading photos from the card.
//!
//! A test shot fires the shutter and fetches the photo's 640×360 preview from the card (made from
//! the photo, so it shows the real exposure, which live view doesn't). It is kept in the window
//! only: nothing goes in the capture folder, and the photo stays on the card. A test shot waits
//! until setting changes have reached the camera, so it shows them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use lumix_link::PhotoId;

use super::{Pending, Stand, capitalized};
use crate::presets::{self, Preset};
use crate::review::{self, Look};
use crate::worker::{DecodedFrame, Download};

/// Test shots kept (the strip).
pub const MAX_TESTS: usize = 12;
/// The longest a test shot's preview may take to arrive.
pub const PREVIEW_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest a test shot waits for setting changes to reach the camera.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(3);
/// The most photos one card download takes.
pub const MAX_BATCH: usize = 500;

/// One test shot.
pub struct TestShot {
    pub ticket: u64,
    /// Its number this session (Test 1, Test 2…).
    pub n: u32,
    /// The camera's settings at the shutter.
    pub camera: Vec<(String, String)>,
    pub state: ShotState,
    started: Instant,
}

pub enum ShotState {
    /// The preview is on its way (what's happening, in plain words).
    Fetching(String),
    Ready {
        frame: DecodedFrame,
        look: Box<Look>,
    },
    Failed(String),
}

impl TestShot {
    pub fn fetching(&self) -> bool {
        matches!(self.state, ShotState::Fetching(_))
    }

    pub fn title(&self) -> String {
        format!("Test {}", self.n)
    }
}

/// What the comparison shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compare {
    /// The test shot selected in the strip.
    Test,
    /// The last full-size photo.
    Photo,
}

/// Which photos the Download photos section fetches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// This session's captures.
    Session,
    /// A range of the camera's photo numbers.
    Range,
}

/// Photos from the card on their way to a folder, under the camera's names.
pub struct CardBatch {
    pub folder: PathBuf,
    /// (photo, its ticket when sent, its state).
    pub items: Vec<(PhotoId, Option<u64>, Download)>,
    skip: Arc<AtomicBool>,
}

impl CardBatch {
    pub fn done(&self) -> bool {
        self.items.iter().all(|(_, _, d)| matches!(d, Download::Done { .. } | Download::Failed(_)))
    }

    /// (saved, failed or skipped, all).
    pub fn counts(&self) -> (usize, usize, usize) {
        let saved = self.items.iter().filter(|(_, _, d)| matches!(d, Download::Done { .. })).count();
        let failed = self.items.iter().filter(|(_, _, d)| matches!(d, Download::Failed(_))).count();
        (saved, failed, self.items.len())
    }

    pub fn cancelled(&self) -> bool {
        self.skip.load(Ordering::SeqCst)
    }
}

/// A photo number as typed: `1230211`, `P1230211` or `P1230211.JPG`.
pub fn parse_photo(s: &str) -> Option<PhotoId> {
    let s = s.trim();
    let s = s.strip_prefix(['P', 'p']).unwrap_or(s);
    let s = s.strip_suffix(".JPG").or_else(|| s.strip_suffix(".jpg")).unwrap_or(s);
    PhotoId::parse(s)
}

/// The photos from `from` to `to`, at most [`MAX_BATCH`].
pub fn photo_range(from: PhotoId, to: PhotoId) -> Result<Vec<PhotoId>, String> {
    if to < from {
        return Err(format!("{} comes after {}", from.file_name(), to.file_name()));
    }
    let mut out = vec![from];
    let mut p = from;
    while p < to {
        p = p.next();
        out.push(p);
        if out.len() > MAX_BATCH {
            return Err(format!("that is more than {MAX_BATCH} photos: download them in parts"));
        }
    }
    Ok(out)
}

impl Stand {
    /// Something is under way that the window should follow (test shots, a photo being read, a
    /// card download).
    pub fn busy(&self) -> bool {
        self.testing()
            || self.loading.is_some()
            || self.batch.as_ref().is_some_and(|b| !b.done())
            || self.asking.is_some()
            || self.assist.as_ref().is_some_and(super::assist::Assist::running)
    }

    /// Take a test shot: once setting changes have reached the camera, fire the shutter and fetch
    /// the photo's preview ([`Stand::tick`]).
    pub fn test_shot(&mut self) {
        if self.pending.is_some() || self.test_wanted.is_some() {
            return self.fail("The last capture hasn't finished yet.");
        }
        if !self.status().is_some_and(|s| s.ready()) {
            return self.fail("Connect to the camera first.");
        }
        self.test_wanted = Some(Instant::now());
        self.last = Some((true, "Taking a test shot\u{2026}".into()));
    }

    /// The shutter fired for a test shot: fetch its preview.
    pub(super) fn fetch_test(&mut self, p: Pending) {
        self.tests_taken += 1;
        let r = self.worker.as_ref().map_or_else(|| Err("the camera isn't connected".to_string()), |w| w.preview(p.ticket));
        let state = match r {
            Ok(()) => ShotState::Fetching("waiting for the card".into()),
            Err(e) => ShotState::Failed(capitalized(&e)),
        };
        self.tests.push(TestShot { ticket: p.ticket, n: self.tests_taken, camera: p.camera, state, started: Instant::now() });
        if self.tests.len() > MAX_TESTS {
            self.tests.remove(0);
        }
        self.selected = Some(p.ticket);
        self.compare = super::Compare::Test;
        self.last = Some((true, format!("Test {}: fetching its preview\u{2026}", self.tests_taken)));
    }

    /// Fire a test shot asked for, follow the previews and the photo being read, and the card
    /// download.
    pub(super) fn poll_shots(&mut self) {
        if let Some(since) = self.test_wanted
            && self.pending.is_none()
        {
            let settled = self.status().is_none_or(|s| s.snap.commands_pending == 0 && !s.settling);
            if settled || since.elapsed() >= SETTLE_TIMEOUT {
                self.test_wanted = None;
                self.fire(None, String::new());
            }
        }
        let mut news = None;
        for t in &mut self.tests {
            let ShotState::Fetching(_) = t.state else { continue };
            let w = self.worker.as_ref();
            let state = w.and_then(|w| w.download_state(t.ticket));
            t.state = match state {
                Some(Download::Done { .. }) => match w.and_then(|w| w.preview_frame(t.ticket)) {
                    Some(frame) => {
                        let look = Box::new(review::look(&frame));
                        news = Some((true, format!("{}: {}", t.title(), summary_text(&t.camera))));
                        ShotState::Ready { frame, look }
                    }
                    None => ShotState::Failed("the preview was lost".into()),
                },
                Some(Download::Failed(e)) => {
                    news = Some((false, format!("{}: the preview didn't come ({e}).", t.title())));
                    ShotState::Failed(capitalized(&e))
                }
                None => ShotState::Failed("the camera was disconnected".into()),
                _ if t.started.elapsed() >= PREVIEW_TIMEOUT => ShotState::Failed(format!("it took longer than {} s", PREVIEW_TIMEOUT.as_secs())),
                Some(Download::Downloading { got, .. }) => ShotState::Fetching(format!("{} KB", got / 1000)),
                Some(Download::Queued(_) | Download::Finding(_)) => ShotState::Fetching("looking on the card".into()),
                Some(Download::Waiting) => ShotState::Fetching("listing the card".into()),
            };
        }
        if let Some(n) = news {
            self.last = Some(n);
        }
        self.poll_photo();
        self.poll_batch();
        // A capture's photo number arrives with the camera's answer.
        if let Some(w) = &self.worker {
            for (_, photo, ticket) in &mut self.session {
                if photo.is_none() {
                    *photo = w.capture_photo(*ticket);
                }
            }
        }
    }

    /// The previews under way end (the link closed).
    pub(super) fn end_tests(&mut self, why: &str) {
        for t in &mut self.tests {
            if t.fetching() {
                t.state = ShotState::Failed(capitalized(why));
            }
        }
    }

    pub fn select_test(&mut self, ticket: u64) {
        self.selected = Some(ticket);
        self.compare = super::Compare::Test;
    }

    /// The test shot shown (the selected one, else the newest).
    pub fn shown_test(&self) -> Option<&TestShot> {
        self.selected.and_then(|t| self.tests.iter().find(|s| s.ticket == t)).or_else(|| self.tests.last())
    }

    /// A capture this session: remembered for the Download photos section.
    pub(super) fn remember_capture(&mut self, name: String, photo: Option<PhotoId>, ticket: u64) {
        match self.session.iter_mut().find(|(_, _, t)| *t == ticket) {
            Some(entry) => {
                entry.0 = name;
                entry.1 = photo.or(entry.1);
            }
            None => self.session.push((name, photo, ticket)),
        }
    }

    // ---------- the last full-size photo ----------

    /// Read the photo just saved at `path`, to show it (on its own thread).
    pub(super) fn load_photo(&mut self, path: &Path, camera: Vec<(String, String)>) {
        self.photos_shown += 1;
        let (id, path, repaint) = (self.photos_shown, path.to_path_buf(), self.repaint.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("lumix-photo".into()).spawn(move || {
            let r = std::panic::catch_unwind(|| review::load(id, &path, camera)).unwrap_or_else(|_| Err("the photo couldn't be read".into()));
            let _ = tx.send(r); // the window may have closed
            repaint();
        });
        if spawned.is_ok() {
            self.loading = Some(rx);
        }
    }

    fn poll_photo(&mut self) {
        let Some(rx) = &self.loading else { return };
        let r = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("the photo couldn't be read".into()),
        };
        self.loading = None;
        match r {
            Ok(p) => {
                self.photo = Some(p);
                self.compare = super::Compare::Photo;
            }
            // The photo is saved: only showing it failed.
            Err(e) => self.last = Some((false, format!("The photo is saved, but it can't be shown: {e}"))),
        }
    }

    /// A photo is being read to show.
    pub fn loading_photo(&self) -> bool {
        self.loading.is_some()
    }

    // ---------- presets ----------

    fn current_preset(&self, name: String) -> Option<Preset> {
        let s = self.status()?;
        let camera = presets::camera_values(&s.snap.settings);
        if camera.is_empty() {
            return None;
        }
        Some(Preset {
            name,
            camera,
            prefix: self.settings.prefix.clone(),
            full_size: self.settings.full_size,
            fast_downloads: self.settings.fast_downloads,
        })
    }

    /// Once the camera's settings have been read the first time, they become the first preset.
    pub(super) fn seed_preset(&mut self) {
        if self.settings.presets_seeded || !self.settings.presets.is_empty() {
            return;
        }
        let Some(s) = self.status().filter(|s| s.ready()) else { return };
        if !s.snap.settings.menu_read || s.snap.commands_pending > 0 {
            return;
        }
        if let Some(p) = self.current_preset("Preset 1".into()) {
            self.settings.presets.push(p);
            self.settings.presets_seeded = true;
            self.save_settings();
        }
    }

    /// Save the present settings as a new preset.
    pub fn save_preset(&mut self) {
        if self.settings.presets.len() >= presets::MAX_PRESETS {
            return self.fail(format!("There can be at most {} presets.", presets::MAX_PRESETS));
        }
        let name = presets::new_name(&self.settings.presets);
        match self.current_preset(name.clone()) {
            Some(p) => {
                self.settings.presets.push(p);
                self.settings.presets_seeded = true;
                self.save_settings();
                self.last = Some((true, format!("Saved the present settings as {name}.")));
            }
            None => self.fail("Connect to the camera first: a preset is saved from its settings."),
        }
    }

    /// Save the settings test shot `ticket` was taken with as a new preset (with the present
    /// capture options).
    pub fn save_test_as_preset(&mut self, ticket: u64) {
        let Some(t) = self.tests.iter().find(|t| t.ticket == ticket) else { return };
        if self.settings.presets.len() >= presets::MAX_PRESETS {
            return self.fail(format!("There can be at most {} presets.", presets::MAX_PRESETS));
        }
        let name = format!("{} ({})", presets::new_name(&self.settings.presets), t.title());
        let p = Preset {
            name: presets::clean_name(&name).unwrap_or_default(),
            camera: t.camera.clone(),
            prefix: self.settings.prefix.clone(),
            full_size: self.settings.full_size,
            fast_downloads: self.settings.fast_downloads,
        };
        let title = t.title();
        self.settings.presets.push(p);
        self.settings.presets_seeded = true;
        self.save_settings();
        self.last = Some((true, format!("Saved {title}'s settings as a preset.")));
    }

    /// Send preset `i` to the camera and take its capture options.
    pub fn apply_preset(&mut self, i: usize) {
        let Some(p) = self.settings.presets.get(i).cloned() else { return };
        let Some(s) = self.status().filter(|s| s.ready()) else { return self.fail("Connect to the camera first.") };
        let changes = presets::changes(&p.camera, &s.snap.settings);
        for (k, v) in &changes {
            self.set_camera(k, v);
        }
        self.prefix = p.prefix.clone();
        self.commit_prefix();
        self.set_full_size(p.full_size);
        self.set_fast_downloads(p.fast_downloads);
        self.last = Some((
            true,
            match changes.len() {
                0 => format!("{}: the camera is set so already.", p.name),
                1 => format!("{}: 1 setting sent to the camera.", p.name),
                n => format!("{}: {n} settings sent to the camera.", p.name),
            },
        ));
    }

    /// Preset `i` takes the present settings.
    pub fn update_preset(&mut self, i: usize) {
        let Some(name) = self.settings.presets.get(i).map(|p| p.name.clone()) else { return };
        match self.current_preset(name.clone()) {
            Some(p) => {
                if let Some(slot) = self.settings.presets.get_mut(i) {
                    *slot = p;
                }
                self.save_settings();
                self.last = Some((true, format!("{name} now has the present settings.")));
            }
            None => self.fail("Connect to the camera first: a preset is saved from its settings."),
        }
    }

    pub fn rename_preset(&mut self, i: usize, name: &str) {
        let Some(name) = presets::clean_name(name) else { return };
        if let Some(p) = self.settings.presets.get_mut(i)
            && p.name != name
        {
            p.name = name;
            self.save_settings();
        }
    }

    pub fn delete_preset(&mut self, i: usize) {
        if i < self.settings.presets.len() {
            let p = self.settings.presets.remove(i);
            self.renaming = None;
            self.save_settings();
            self.last = Some((true, format!("Deleted {}.", p.name)));
        }
    }

    /// The preset the camera and the app are set to now.
    pub fn matching_preset(&self) -> Option<usize> {
        let s = self.status()?;
        self.settings
            .presets
            .iter()
            .position(|p| p.matches(&s.snap.settings, &self.settings.prefix, self.settings.full_size, self.settings.fast_downloads))
    }

    // ---------- downloading photos from the card ----------

    /// This session's captures whose photo on the card is known.
    pub fn session_photos(&self) -> Vec<PhotoId> {
        let mut v: Vec<PhotoId> = self.session.iter().filter_map(|(_, p, _)| *p).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// The photos the section would download now.
    pub fn chosen_photos(&self) -> Result<Vec<PhotoId>, String> {
        match self.source {
            Source::Session => {
                let v = self.session_photos();
                if v.is_empty() { Err("no capture this session is on the card yet".into()) } else { Ok(v) }
            }
            Source::Range => {
                let from = parse_photo(&self.range_from).ok_or("type the first photo's number, e.g. P1230201")?;
                let to = parse_photo(&self.range_to).ok_or("type the last photo's number, e.g. P1230240")?;
                photo_range(from, to)
            }
        }
    }

    /// Download the chosen photos at full size into a folder picked now, under the camera's names
    /// (one already in the folder is skipped).
    pub fn download_photos(&mut self) {
        if self.batch.as_ref().is_some_and(|b| !b.done()) {
            return self.fail("A download from the card is under way.");
        }
        if self.worker.is_none() {
            return self.fail("Connect to the camera first.");
        }
        let photos = match self.chosen_photos() {
            Ok(p) => p,
            Err(e) => return self.fail(format!("Nothing to download: {e}.")),
        };
        let Some(folder) = (self.picker)("Choose where to save the photos") else { return };
        if let Err(e) = crate::frames::check_folder(&folder) {
            return self.fail(format!("{e}: choose another folder."));
        }
        let skip = Arc::new(AtomicBool::new(false));
        let mut items = Vec::new();
        for p in photos {
            let path = folder.join(p.file_name());
            if std::fs::symlink_metadata(&path).is_ok() {
                items.push((p, None, Download::Failed("in the folder already".into())));
                continue;
            }
            let r = self.worker.as_mut().map_or_else(|| Err("the camera isn't connected".to_string()), |w| w.download_card(p, path, skip.clone()));
            match r {
                Ok(t) => items.push((p, Some(t), Download::Queued(p))),
                Err(e) => items.push((p, None, Download::Failed(e))),
            }
        }
        let n = items.len();
        self.batch = Some(CardBatch { folder, items, skip });
        self.last = Some((true, format!("Downloading {n} photo{} from the card\u{2026}", if n == 1 { "" } else { "s" })));
    }

    /// Stop the card download after the photo under way.
    pub fn cancel_batch(&mut self) {
        if let Some(b) = &self.batch {
            b.skip.store(true, Ordering::SeqCst);
        }
    }

    pub(super) fn end_batch(&mut self, why: &str) {
        if let Some(b) = &mut self.batch {
            b.skip.store(true, Ordering::SeqCst);
            for (_, _, d) in &mut b.items {
                if !matches!(d, Download::Done { .. } | Download::Failed(_)) {
                    *d = Download::Failed(why.to_string());
                }
            }
        }
    }

    fn poll_batch(&mut self) {
        let Some(b) = &mut self.batch else { return };
        if b.done() {
            return;
        }
        let cancelled = b.cancelled();
        for (_, ticket, d) in &mut b.items {
            if matches!(d, Download::Done { .. } | Download::Failed(_)) {
                continue;
            }
            let state = ticket.and_then(|t| self.worker.as_ref().and_then(|w| w.download_state(t)));
            *d = match state {
                Some(Download::Failed(e)) if cancelled && e == "cancelled" => Download::Failed("cancelled".into()),
                Some(s) => s,
                None => Download::Failed("the download was lost".into()),
            };
        }
        if b.done() {
            let (saved, failed, all) = b.counts();
            let folder = b.folder.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
            self.last = Some((
                failed == 0,
                format!(
                    "Saved {saved} of {all} photos in {folder}{}.",
                    if failed > 0 { format!(" ({failed} not: see the list)") } else { String::new() }
                ),
            ));
        }
    }
}

/// `1/60 · f/8 · ISO 200 · Daylight`.
pub fn summary_text(camera: &[(String, String)]) -> String {
    presets::summary(camera).into_iter().map(|(_, t)| t).collect::<Vec<_>>().join(" \u{b7} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photo_numbers_as_typed() {
        assert_eq!(parse_photo(" P1230211.JPG "), Some(PhotoId(1_230_211)));
        assert_eq!(parse_photo("p1230211"), Some(PhotoId(1_230_211)));
        assert_eq!(parse_photo("1230211"), Some(PhotoId(1_230_211)));
        assert_eq!(parse_photo("P123"), None);
        let r = photo_range(PhotoId(1_239_998), PhotoId(1_240_002)).unwrap();
        assert_eq!(r, [PhotoId(1_239_998), PhotoId(1_239_999), PhotoId(1_240_001), PhotoId(1_240_002)], "across folders");
        assert!(photo_range(PhotoId(1_230_005), PhotoId(1_230_001)).is_err());
        assert!(photo_range(PhotoId(1_000_001), PhotoId(1_009_000)).is_err(), "too many at once");
    }
}
