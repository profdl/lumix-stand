//! The pictures: live view, and beside it the comparison (the selected test shot or the last
//! full-size photo, with its histogram, clipping and settings), and the test shots in a strip
//! below. With nothing to compare yet, live view takes the whole area.

use std::collections::HashMap;

use eframe::egui::{self, Color32, Rect, RichText, Sense, TextureHandle, TextureOptions, Ui, UiBuilder, pos2, vec2};
use lumix_link::Phase;

use super::panel::{ACCENT, DIM, WARN};
use super::shots::ShotState;
use super::{Compare, Stand};
use crate::review::{BINS, Look};
use crate::worker::{DecodedFrame, Status};

/// The strip's height.
const STRIP_H: f32 = 128.0;
/// A strip card's size.
const CARD: egui::Vec2 = vec2(168.0, 120.0);
/// The histogram's height.
const HIST_H: f32 = 54.0;
/// A clipped share worth warning about.
const CLIP_WARN: f32 = 0.002;
/// The gap between the pictures.
const GAP: f32 = 12.0;

/// A frame's textures: the picture, and its clipping overlay.
struct Tex {
    image: TextureHandle,
    overlay: TextureHandle,
}

/// The textures and the view options (the window's state, not the app's).
#[derive(Default)]
pub struct View {
    /// Test shots' textures by ticket.
    tests: HashMap<u64, Tex>,
    /// The last photo's, by its id.
    photo: Option<(u64, Tex)>,
    /// Show blown and crushed pixels.
    pub clipping: bool,
    /// Show the compared picture at 100%.
    pub zoom: bool,
}

fn texture(ctx: &egui::Context, name: &str, f: &DecodedFrame, look: &Look) -> Option<Tex> {
    let (Ok(w), Ok(h)) = (usize::try_from(f.width), usize::try_from(f.height)) else { return None };
    let max = ctx.input(|i| i.max_texture_side);
    if w > max || h > max || f.rgba.len() != w.saturating_mul(h).saturating_mul(4) || look.overlay.len() != f.rgba.len() {
        return None;
    }
    let image = ctx.load_texture(name, egui::ColorImage::from_rgba_unmultiplied([w, h], &f.rgba), TextureOptions::LINEAR);
    let overlay = ctx.load_texture(format!("{name}-clip"), egui::ColorImage::from_rgba_unmultiplied([w, h], &look.overlay), TextureOptions::NEAREST);
    Some(Tex { image, overlay })
}

impl View {
    #[cfg(test)]
    pub fn tests_empty(&self) -> bool {
        self.tests.is_empty()
    }

    /// Make the textures that are missing and drop those no longer needed.
    fn sync(&mut self, ctx: &egui::Context, stand: &Stand) {
        self.tests.retain(|t, _| stand.tests.iter().any(|s| s.ticket == *t));
        for s in &stand.tests {
            if let ShotState::Ready { frame, look } = &s.state
                && !self.tests.contains_key(&s.ticket)
                && let Some(t) = texture(ctx, &format!("test-{}", s.ticket), frame, look)
            {
                self.tests.insert(s.ticket, t);
            }
        }
        match &stand.photo {
            None => self.photo = None,
            Some(p) if self.photo.as_ref().is_none_or(|(id, _)| *id != p.id) => {
                self.photo = texture(ctx, &format!("photo-{}", p.id), &p.frame, &p.look).map(|t| (p.id, t));
            }
            Some(_) => {}
        }
    }
}

/// The central area.
pub(super) fn pictures(ui: &mut Ui, stand: &mut Stand, view: &mut View, live: Option<&TextureHandle>, status: Option<&Status>) {
    view.sync(ui.ctx(), stand);
    let room = ui.available_rect_before_wrap();
    let comparing = !stand.tests.is_empty() || stand.photo.is_some() || stand.loading_photo();
    if !comparing {
        in_rect(ui, room, |ui| live_view(ui, live, status));
        ui.allocate_rect(room, Sense::hover());
        return;
    }
    let strip_h = if stand.tests.is_empty() { 0.0 } else { STRIP_H };
    let top = Rect::from_min_max(room.min, pos2(room.max.x, room.max.y - strip_h - if strip_h > 0.0 { GAP } else { 0.0 }));
    let half = (top.width() - GAP) / 2.0;
    let left = Rect::from_min_size(top.min, vec2(half, top.height()));
    let right = Rect::from_min_size(pos2(top.min.x + half + GAP, top.min.y), vec2(half, top.height()));
    in_rect(ui, left, |ui| {
        ui.label(RichText::new("LIVE VIEW").size(11.0).strong().color(DIM));
        live_view(ui, live, status);
    });
    in_rect(ui, right, |ui| comparison(ui, stand, view));
    if strip_h > 0.0 {
        let strip = Rect::from_min_max(pos2(room.min.x, room.max.y - strip_h), room.max);
        in_rect(ui, strip, |ui| self::strip(ui, stand, view));
    }
    ui.allocate_rect(room, Sense::hover());
}

fn in_rect(ui: &mut Ui, rect: Rect, add: impl FnOnce(&mut Ui)) {
    ui.scope_builder(UiBuilder::new().max_rect(rect), add);
}

/// `rect`'s largest centred part with the aspect `h / w`.
fn fit(rect: Rect, aspect: f32) -> Rect {
    let w = rect.width().min(rect.height() / aspect).max(1.0);
    Rect::from_center_size(rect.center(), vec2(w, w * aspect))
}

fn aspect_of(t: &TextureHandle) -> f32 {
    let [w, h] = t.size();
    if w > 0 { h as f32 / w as f32 } else { 9.0 / 16.0 }
}

pub(super) fn live_view(ui: &mut Ui, texture: Option<&TextureHandle>, status: Option<&Status>) {
    let room = ui.available_rect_before_wrap();
    let rect = fit(room, texture.map_or(9.0 / 16.0, aspect_of));
    match (texture, status.is_some_and(Status::open)) {
        (Some(tex), true) if !status.is_some_and(|s| s.paused) => {
            egui::Image::new(tex).paint_at(ui, rect);
        }
        _ => {
            ui.painter().rect_filled(rect, 6.0, Color32::from_gray(28));
            let text = match status {
                Some(s) if s.paused => "Live view paused while photos download",
                Some(s) if s.open() && s.snap.phase == Phase::Connected => "Live view is off",
                Some(s) if s.open() => "Waiting for live view\u{2026}",
                _ => "No camera connected",
            };
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(15.0), DIM);
        }
    }
    ui.allocate_rect(room, Sense::hover());
}

/// What the comparison shows: (title, its settings, the settings to mark differences against,
/// the frame's look, its texture).
struct Shown<'a> {
    title: String,
    camera: Vec<(String, String)>,
    before: Option<Vec<(String, String)>>,
    look: Option<&'a Look>,
    tex: Option<&'a Tex>,
    /// Shown instead of a picture.
    words: Option<String>,
    test: Option<u64>,
}

fn shown<'a>(stand: &'a Stand, view: &'a View) -> Shown<'a> {
    match stand.compare {
        Compare::Photo => match &stand.photo {
            Some(p) => Shown {
                title: format!("Last photo \u{b7} {} \u{b7} {}\u{d7}{}", p.name, p.frame.width, p.frame.height),
                camera: p.camera.clone(),
                before: stand.shown_test().map(|t| t.camera.clone()),
                look: Some(&p.look),
                tex: view.photo.as_ref().filter(|(id, _)| *id == p.id).map(|(_, t)| t),
                words: None,
                test: None,
            },
            None => Shown {
                title: "Last photo".into(),
                camera: Vec::new(),
                before: None,
                look: None,
                tex: None,
                words: Some(if stand.loading_photo() { "Reading the photo\u{2026}" } else { "No full-size photo yet this session" }.into()),
                test: None,
            },
        },
        Compare::Test => match stand.shown_test() {
            Some(t) => {
                let i = stand.tests.iter().position(|s| s.ticket == t.ticket).unwrap_or(0);
                let before = i.checked_sub(1).and_then(|j| stand.tests.get(j)).map(|s| s.camera.clone());
                let (look, words) = match &t.state {
                    ShotState::Ready { look, .. } => (Some(&**look), None),
                    ShotState::Fetching(w) => (None, Some(format!("Fetching the preview: {w}\u{2026}"))),
                    ShotState::Failed(e) => (None, Some(format!("No preview: {e}"))),
                };
                Shown { title: t.title(), camera: t.camera.clone(), before, look, tex: view.tests.get(&t.ticket), words, test: Some(t.ticket) }
            }
            None => Shown {
                title: "Test shot".into(),
                camera: Vec::new(),
                before: None,
                look: None,
                tex: None,
                words: Some("Take a test shot (T) to see the real exposure here".into()),
                test: None,
            },
        },
    }
}

fn comparison(ui: &mut Ui, stand: &mut Stand, view: &mut View) {
    let mut save_preset = None;
    let mut ask = None;
    ui.horizontal(|ui| {
        ui.selectable_value(&mut stand.compare, Compare::Test, "Test shot");
        ui.add_enabled_ui(stand.photo.is_some() || stand.loading_photo(), |ui| {
            ui.selectable_value(&mut stand.compare, Compare::Photo, "Last photo");
        });
        ui.separator();
        ui.toggle_value(&mut view.clipping, "Clipping").on_hover_text("Blown highlights in red, crushed shadows in blue.");
        ui.toggle_value(&mut view.zoom, "100%")
            .on_hover_text("Show the picture pixel for pixel; drag to look around. Clicking the picture does the same.");
    });
    let (zoom, clipping) = (view.zoom, view.clipping);
    let s = shown(stand, view);
    ui.horizontal(|ui| {
        ui.label(RichText::new(&s.title).strong());
        if let Some(t) = s.test {
            if ui.small_button("Save as preset").on_hover_text("A new preset with the settings this test shot was taken with.").clicked() {
                save_preset = Some(t);
            }
            let asking = stand.asking_claude();
            if ui
                .add_enabled(s.look.is_some() && !asking, egui::Button::new(if asking { "Asking Claude\u{2026}" } else { "Ask Claude" }).small())
                .on_hover_text("Claude looks at this test shot and suggests settings (shown under Exposure, with Apply).")
                .clicked()
            {
                ask = Some(t);
            }
        }
    });
    settings_line(ui, &s.camera, s.before.as_deref(), 13.0);
    let below = HIST_H + 26.0;
    let room = ui.available_rect_before_wrap();
    let pic = Rect::from_min_max(room.min, pos2(room.max.x, (room.max.y - below).max(room.min.y + 40.0)));
    let mut clicked = false;
    in_rect(ui, pic, |ui| clicked = picture(ui, &s, zoom, clipping));
    ui.allocate_rect(pic, Sense::hover());
    if let Some(look) = s.look {
        histogram(ui, look);
    }
    if clicked {
        view.zoom = !view.zoom;
    }
    if let Some(t) = save_preset {
        stand.save_test_as_preset(t);
    }
    if let Some(t) = ask {
        stand.ask_claude(t);
    }
}

/// The compared picture; `true`: it was clicked (zoom in or out).
fn picture(ui: &mut Ui, s: &Shown<'_>, zoom: bool, clipping: bool) -> bool {
    let room = ui.available_rect_before_wrap();
    let Some(tex) = s.tex else {
        let rect = fit(room, 9.0 / 16.0);
        ui.painter().rect_filled(rect, 6.0, Color32::from_gray(28));
        let words = s.words.clone().unwrap_or_else(|| "This picture can't be shown".into());
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, words, egui::FontId::proportional(14.0), DIM);
        return false;
    };
    if zoom {
        let [w, h] = tex.image.size();
        let ppp = ui.ctx().pixels_per_point();
        let size = vec2(w as f32 / ppp, h as f32 / ppp);
        let mut clicked = false;
        egui::ScrollArea::both().id_salt("zoomed").auto_shrink([false, false]).scroll_source(egui::scroll_area::ScrollSource::ALL).show(ui, |ui| {
            let (rect, r) = ui.allocate_exact_size(size, Sense::click());
            paint(ui, tex, rect, clipping);
            clicked = r.clicked();
        });
        clicked
    } else {
        let rect = fit(room, aspect_of(&tex.image));
        let r = ui.allocate_rect(rect, Sense::click()).on_hover_cursor(egui::CursorIcon::ZoomIn);
        paint(ui, tex, rect, clipping);
        r.clicked()
    }
}

fn paint(ui: &Ui, tex: &Tex, rect: Rect, clipping: bool) {
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    ui.painter().image(tex.image.id(), rect, uv, Color32::WHITE);
    if clipping {
        // Blinks, so the picture under the marks can be seen too.
        let on = (ui.input(|i| i.time) * 2.0) as i64 % 2 == 0;
        if on {
            ui.painter().image(tex.overlay.id(), rect, uv, Color32::WHITE);
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }
}

fn histogram(ui: &mut Ui, look: &Look) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), HIST_H), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3.0, Color32::from_gray(24));
    let max = look.hist.iter().copied().max().unwrap_or(1).max(1) as f32;
    let bar = rect.width() / BINS as f32;
    for (i, &n) in look.hist.iter().enumerate() {
        // Square-root scale, so the quieter tones still show.
        let h = (n as f32 / max).sqrt() * (rect.height() - 4.0);
        let x = rect.min.x + i as f32 * bar;
        let color = match i {
            0 if look.low >= CLIP_WARN => Color32::from_rgb(70, 130, 255),
            _ if i == BINS - 1 && look.high >= CLIP_WARN => Color32::from_rgb(255, 80, 80),
            _ => Color32::from_gray(170),
        };
        p.rect_filled(Rect::from_min_max(pos2(x, rect.max.y - 2.0 - h), pos2(x + bar - 0.5, rect.max.y - 2.0)), 0.0, color);
    }
    let pct = |v: f32| format!("{:.1}%", v * 100.0);
    ui.horizontal(|ui| {
        let warn = |v: f32| if v >= CLIP_WARN { WARN } else { DIM };
        ui.label(RichText::new(format!("Blown {}", pct(look.high))).size(12.0).color(warn(look.high)));
        ui.label(RichText::new(format!("Crushed {}", pct(look.low))).size(12.0).color(warn(look.low)));
    });
}

/// The settings in short, each one that differs from `before` marked.
fn settings_line(ui: &mut Ui, camera: &[(String, String)], before: Option<&[(String, String)]>, size: f32) {
    let ours = crate::presets::summary(camera);
    if ours.is_empty() {
        return;
    }
    let theirs = before.map(crate::presets::summary);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (i, (key, text)) in ours.iter().enumerate() {
            if i > 0 {
                ui.label(RichText::new("\u{b7}").size(size).color(DIM));
            }
            let changed = theirs.as_ref().is_some_and(|t| t.iter().find(|(k, _)| k == key).is_none_or(|(_, v)| v != text));
            let rt = RichText::new(text).size(size);
            ui.label(if changed { rt.color(ACCENT).strong() } else { rt });
        }
    });
}

fn strip(ui: &mut Ui, stand: &mut Stand, view: &View) {
    let mut pick = None;
    let selected = stand.shown_test().map(|t| t.ticket);
    egui::ScrollArea::horizontal().id_salt("strip").auto_shrink([false, false]).stick_to_right(true).show(ui, |ui| {
        ui.horizontal(|ui| {
            for (i, t) in stand.tests.iter().enumerate() {
                let before = i.checked_sub(1).and_then(|j| stand.tests.get(j)).map(|s| s.camera.as_slice());
                let on = selected == Some(t.ticket) && stand.compare == Compare::Test;
                let r = ui.allocate_ui_with_layout(CARD, egui::Layout::top_down(egui::Align::Min), |ui| {
                    let frame = egui::Frame::NONE
                        .fill(Color32::from_gray(24))
                        .stroke(egui::Stroke::new(if on { 2.0 } else { 1.0 }, if on { ACCENT } else { Color32::from_gray(45) }))
                        .corner_radius(4.0)
                        .inner_margin(4.0);
                    frame
                        .show(ui, |ui| {
                            ui.set_width(CARD.x - 10.0);
                            let (thumb, _) = ui.allocate_exact_size(vec2(CARD.x - 10.0, 62.0), Sense::hover());
                            match view.tests.get(&t.ticket) {
                                Some(tex) => {
                                    egui::Image::new(&tex.image).paint_at(ui, fit(thumb, aspect_of(&tex.image)));
                                }
                                None => {
                                    let words = match &t.state {
                                        ShotState::Fetching(_) => "fetching\u{2026}",
                                        _ => "no preview",
                                    };
                                    ui.painter().rect_filled(thumb, 3.0, Color32::from_gray(32));
                                    ui.painter().text(thumb.center(), egui::Align2::CENTER_CENTER, words, egui::FontId::proportional(12.0), DIM);
                                }
                            }
                            ui.label(RichText::new(t.title()).size(11.5).strong());
                            settings_line(ui, &t.camera, before, 11.0);
                        })
                        .response
                });
                if r.response.interact(Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    pick = Some(t.ticket);
                }
            }
        });
    });
    if let Some(t) = pick {
        stand.select_test(t);
    }
}
