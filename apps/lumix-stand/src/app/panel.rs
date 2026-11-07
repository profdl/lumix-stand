//! The controls on the right: the link's state, then Connection (closed once connected),
//! Capture, Presets, Download photos and the camera's settings.

use eframe::egui::{self, Color32, Key, RichText, Ui, vec2};
use lumix_link::menu::{self, Group, Kind, Setting};
use lumix_link::{CameraSettings, FocusStep, Phase, apex};

use super::{Source, Stand};
use crate::frames;
use crate::worker::{Download, Status};

pub(super) const DIM: Color32 = Color32::from_gray(150);
pub(super) const WARN: Color32 = Color32::from_rgb(240, 180, 90);
pub(super) const GOOD: Color32 = Color32::from_rgb(140, 210, 150);
/// What differs from the shot before, and the selection.
pub(super) const ACCENT: Color32 = Color32::from_rgb(120, 190, 255);

pub(super) fn note(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).size(12.0).color(DIM));
}

pub(super) fn wrapped(ui: &mut Ui, text: impl Into<String>, color: Color32) {
    ui.add(egui::Label::new(RichText::new(text.into()).color(color)).wrap());
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(title).size(11.5).strong().color(DIM));
    ui.add_space(4.0);
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
            Phase::Connected if s.paused => "Connected, live view paused for downloads",
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

/// The side panel. `open_connection`: open (`true`) or close the Connection section this frame.
pub(super) fn controls(ui: &mut Ui, stand: &mut Stand, status: Option<&Status>, open_connection: Option<bool>) {
    ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
    ui.heading("Lumix Stand");
    ui.add_space(6.0);
    status_line(ui, status);
    ui.add_space(8.0);
    connection(ui, stand, status, open_connection);
    capture(ui, stand, status);
    exposure(ui, stand, status);
    presets(ui, stand, status);
    download_photos(ui, stand, status);
    if let Some(s) = status.filter(|s| s.open()) {
        camera_settings(ui, stand, s);
    }
}

fn connection(ui: &mut Ui, stand: &mut Stand, status: Option<&Status>, open: Option<bool>) {
    let connected = stand.connected();
    let title = match status.filter(|s| s.open()) {
        Some(s) if s.sim => "Connection \u{b7} simulated camera".to_string(),
        Some(s) => format!("Connection \u{b7} {}", s.address),
        None => "Connection".to_string(),
    };
    egui::CollapsingHeader::new(RichText::new(title).strong()).id_salt("connection").default_open(true).open(open).show(ui, |ui| {
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
                if (ui.add_enabled(!stand.address.trim().is_empty(), egui::Button::new("Connect")).clicked() || enter)
                    && !stand.address.trim().is_empty()
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
    });
}

fn capture(ui: &mut Ui, stand: &mut Stand, status: Option<&Status>) {
    section(ui, "CAPTURE");
    if stand.connected() {
        let live = status.is_some_and(|s| s.snap.live_view);
        let paused = status.is_some_and(|s| s.paused);
        ui.horizontal(|ui| {
            let label = if live || paused { "Stop Live View" } else { "Start Live View" };
            if ui.button(label).clicked() {
                stand.live_view(!(live || paused));
            }
            if paused {
                note(ui, "paused while photos download");
            }
        });
    }
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
        match stand.next_file() {
            Some(next) => note(ui, format!("next {next}")),
            None if stand.settings.folder.is_none() => {}
            None => note(ui, "next \u{2014}"),
        }
    });
    let mut full = stand.settings.full_size;
    if ui.checkbox(&mut full, "Full size from the camera's card").changed() {
        stand.set_full_size(full);
    }
    if full {
        let mut fast = stand.settings.fast_downloads;
        if ui
            .checkbox(&mut fast, "Fast downloads (pause live view)")
            .on_hover_text(
                "Live view stops while full-size photos download and comes back after. With live view running, a photo came at about half the speed.",
            )
            .changed()
        {
            stand.set_fast_downloads(fast);
        }
    }
    note(
        ui,
        if full {
            "Each photo downloads in the background (10\u{2013}35 s over the camera's Wi-Fi) while you keep shooting."
        } else {
            "Saves the live-view frame (640\u{d7}360) at once."
        },
    );
    for f in &stand.fetching {
        note(ui, format!("\u{2193} {}: {}", f.name(), download_words(&f.state)));
    }

    ui.add_space(8.0);
    let ready = status.is_some_and(Status::ready) && !stand.capturing();
    let button = egui::Button::new(RichText::new("Capture").size(17.0).strong())
        .fill(Color32::from_rgb(170, 50, 40))
        .min_size(vec2(ui.available_width(), 44.0));
    if ui.add_enabled(ready, button).clicked() {
        stand.capture();
    }
    let test = egui::Button::new(RichText::new("Test shot").size(14.0)).min_size(vec2(ui.available_width(), 30.0));
    if ui
        .add_enabled(ready, test)
        .on_hover_text("Fire the shutter and show the photo's preview beside live view, to judge the exposure. Nothing is saved to the folder; the photo stays on the card.")
        .clicked()
    {
        stand.test_shot();
    }
    note(ui, "Space or Enter captures; T takes a test shot.");
    if let Some((ok, msg)) = &stand.last {
        ui.add_space(4.0);
        wrapped(ui, msg.clone(), if *ok { GOOD } else { WARN });
    }
}

fn exposure(ui: &mut Ui, stand: &mut Stand, status: Option<&Status>) {
    section(ui, "EXPOSURE");
    let ready = status.is_some_and(Status::ready);
    let running = stand.assist.as_ref().is_some_and(super::assist::Assist::running);
    ui.horizontal(|ui| {
        if running {
            if ui.button("Stop").clicked() {
                stand.stop_exposure();
            }
            ui.spinner();
            note(ui, "finding the best exposure\u{2026}");
        } else if ui
            .add_enabled(ready && !stand.capturing(), egui::Button::new(RichText::new("Find the best exposure").strong()).min_size(vec2(230.0, 28.0)))
            .on_hover_text("Base ISO 200 (least grain), f/5.6 (sharpest), then test shots until the brightest tones sit just under clipping. In MF it focuses once first. The mode dial must be on M.")
            .clicked()
        {
            stand.find_exposure();
        }
    });
    let mut check = stand.settings.claude_check;
    if ui
        .checkbox(&mut check, format!("Then check with Claude ({})", crate::claude::model_name(&stand.settings.claude_model)))
        .on_hover_text("Claude looks at the final test shot for what numbers can't tell: white balance, colour casts, glare, uneven light.")
        .changed()
    {
        stand.set_claude_check(check);
    }
    if let Some(a) = &stand.assist {
        for line in a.log.iter().rev().take(6).rev() {
            note(ui, line.clone());
        }
        if let Some((ok, why)) = &a.outcome
            && a.log.last() != Some(why)
        {
            wrapped(ui, why.clone(), if *ok { GOOD } else { WARN });
        }
    }
    let mut apply = false;
    match &stand.advice {
        Some((_, Ok(r))) => {
            ui.add_space(4.0);
            wrapped(ui, format!("Claude: {}", r.advice.assessment), Color32::WHITE);
            note(
                ui,
                format!("{} \u{b7} {} + {} tokens \u{b7} ${:.4}", crate::claude::model_name(&r.model), r.input_tokens, r.output_tokens, r.cost()),
            );
            if !r.changes.is_empty() {
                ui.horizontal(|ui| {
                    note(ui, super::assist::words(&r.changes));
                    apply = ui.add_enabled(ready, egui::Button::new("Apply").small()).clicked();
                });
            }
        }
        Some((_, Err(e))) => wrapped(ui, format!("Claude: {e}"), WARN),
        None => {}
    }
    if apply {
        stand.apply_advice();
    }
    egui::CollapsingHeader::new(RichText::new("Claude").size(12.5)).id_salt("claude settings").default_open(false).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Model");
            let current = stand.settings.claude_model.clone();
            let mut chosen = None;
            egui::ComboBox::from_id_salt("claude model").width(200.0).selected_text(crate::claude::model_name(&current)).show_ui(ui, |ui| {
                for (id, name, i, o) in crate::claude::MODELS {
                    // About 2,000 tokens in and 300 out per question.
                    let each = (2000.0 * i + 300.0 * o) / 1_000_000.0;
                    if ui.selectable_label(current == id, format!("{name}  (~${each:.4} a question)")).clicked() {
                        chosen = Some(id);
                    }
                }
            });
            if let Some(id) = chosen {
                stand.set_claude_model(id);
            }
        });
        ui.label("API key in 1Password");
        let r = ui.add(egui::TextEdit::singleline(&mut stand.key_field).desired_width(300.0).hint_text("op://Private/Anthropic/credential"));
        if r.lost_focus() {
            let field = stand.key_field.clone();
            stand.set_claude_key(&field);
        }
        note(ui, "In 1Password, right-click the key's field \u{25b8} Copy Secret Reference. Needs the 1Password CLI (brew install 1password-cli) with Settings \u{25b8} Developer \u{25b8} Integrate with 1Password CLI on; it asks for Touch ID once a session. Empty: ANTHROPIC_API_KEY.");
    });
}

/// A download's state in plain words.
fn download_words(d: &Download) -> String {
    match d {
        Download::Waiting => "waiting for the card".to_string(),
        Download::Queued(_) => "waiting its turn".to_string(),
        Download::Finding(p) => format!("looking for {}", p.file_name()),
        Download::Downloading { got, total: Some(t), .. } => format!("{:.1} of {:.1} MB", *got as f64 / 1e6, *t as f64 / 1e6),
        Download::Downloading { got, .. } => format!("{:.1} MB", *got as f64 / 1e6),
        Download::Done { bytes, .. } => format!("saved, {:.1} MB", *bytes as f64 / 1e6),
        Download::Failed(e) => e.clone(),
    }
}

fn presets(ui: &mut Ui, stand: &mut Stand, status: Option<&Status>) {
    section(ui, "PRESETS");
    let ready = status.is_some_and(Status::ready);
    let current = stand.matching_preset();
    let mut apply = None;
    let mut update = None;
    let mut delete = None;
    let mut rename = None;
    let mut start_rename = None;
    let mut stop_rename = false;
    let summaries: Vec<String> = stand.settings.presets.iter().map(|p| super::shots::summary_text(&p.camera)).collect();
    for (i, p) in stand.settings.presets.iter().enumerate() {
        ui.horizontal(|ui| {
            if let Some((ri, name)) = stand.renaming.as_mut().filter(|(ri, _)| *ri == i) {
                let r = ui.add(egui::TextEdit::singleline(name).desired_width(200.0));
                r.request_focus();
                if r.lost_focus() {
                    if !ui.input(|i| i.key_pressed(Key::Escape)) {
                        rename = Some((*ri, name.clone()));
                    }
                    stop_rename = true;
                }
                return;
            }
            let on = current == Some(i);
            let label = if on { format!("\u{2713} {}", p.name) } else { p.name.clone() };
            let hover = format!(
                "{}\n{}{}",
                summaries.get(i).cloned().unwrap_or_default(),
                if p.full_size { "full size" } else { "live-view frames" },
                if p.prefix.is_empty() { String::new() } else { format!(", prefix {}", p.prefix) }
            );
            if ui
                .add_enabled(ready, egui::Button::selectable(on, label).min_size(vec2(230.0, 22.0)))
                .on_hover_text(hover)
                .on_disabled_hover_text("Connect to apply a preset.")
                .clicked()
            {
                apply = Some(i);
            }
            ui.menu_button("\u{22ef}", |ui| {
                if ui.add_enabled(ready, egui::Button::new("Update to the present settings")).clicked() {
                    update = Some(i);
                    ui.close();
                }
                if ui.button("Rename").clicked() {
                    start_rename = Some((i, p.name.clone()));
                    ui.close();
                }
                if ui.button("Delete").clicked() {
                    delete = Some(i);
                    ui.close();
                }
            });
        });
    }
    if stop_rename {
        stand.renaming = None;
    }
    if let Some((i, name)) = rename {
        stand.rename_preset(i, &name);
    }
    if let Some(r) = start_rename {
        stand.renaming = Some(r);
    }
    if let Some(i) = apply {
        stand.apply_preset(i);
    }
    if let Some(i) = update {
        stand.update_preset(i);
    }
    if let Some(i) = delete {
        stand.delete_preset(i);
    }
    if ui.add_enabled(ready, egui::Button::new("Save the present settings as a preset")).clicked() {
        stand.save_preset();
    }
    if stand.settings.presets.is_empty() {
        note(ui, "A preset keeps every camera setting and the capture options. The first is saved once the camera's settings are read.");
    }
}

fn download_photos(ui: &mut Ui, stand: &mut Stand, status: Option<&Status>) {
    egui::CollapsingHeader::new(RichText::new("DOWNLOAD PHOTOS").size(11.5).strong().color(DIM)).id_salt("download photos").default_open(false).show(
        ui,
        |ui| {
            let session = stand.session_photos();
            ui.horizontal(|ui| {
                ui.radio_value(&mut stand.source, Source::Session, format!("This session ({})", session.len()));
                ui.radio_value(&mut stand.source, Source::Range, "A range");
            });
            match stand.source {
                Source::Session => match (session.first(), session.last()) {
                    (Some(a), Some(b)) => note(ui, format!("{} to {}, full size", a.file_name(), b.file_name())),
                    _ => note(ui, "No capture this session is on the card yet."),
                },
                Source::Range => {
                    let newest = status.and_then(|s| s.snap.photos.newest);
                    if stand.range_to.is_empty()
                        && let Some(n) = newest
                    {
                        stand.range_to = n.file_name();
                        stand.range_from = lumix_link::PhotoId(n.0.saturating_sub(9).max(n.0 / 10_000 * 10_000 + 1)).file_name();
                    }
                    ui.horizontal(|ui| {
                        ui.label("From");
                        ui.add(egui::TextEdit::singleline(&mut stand.range_from).desired_width(100.0).hint_text("P1230201"));
                        ui.label("to");
                        ui.add(egui::TextEdit::singleline(&mut stand.range_to).desired_width(100.0).hint_text("P1230240"));
                    });
                    match (stand.chosen_photos(), newest) {
                        (Ok(p), Some(n)) => note(ui, format!("{} photos, full size. Newest on the card: {}", p.len(), n.file_name())),
                        (Ok(p), None) => note(ui, format!("{} photos, full size.", p.len())),
                        (Err(e), _) => note(ui, capital(&e)),
                    }
                }
            }
            let running = stand.batch.as_ref().is_some_and(|b| !b.done());
            ui.horizontal(|ui| {
                let can = stand.connected() && !running && stand.chosen_photos().is_ok();
                if ui
                    .add_enabled(can, egui::Button::new("Download to a folder\u{2026}"))
                    .on_hover_text("Saved under the camera's names (P1230211.JPG); a photo already in the folder is skipped.")
                    .clicked()
                {
                    stand.download_photos();
                }
                if running && ui.button("Stop").clicked() {
                    stand.cancel_batch();
                }
            });
            if let Some(b) = &stand.batch {
                let (saved, failed, all) = b.counts();
                let done = saved + failed;
                ui.add(
                    egui::ProgressBar::new(if all == 0 { 1.0 } else { done as f32 / all as f32 })
                        .text(format!("{saved} saved, {failed} not, of {all}"))
                        .desired_height(14.0),
                );
                if let Some((p, _, d)) = b.items.iter().find(|(_, _, d)| matches!(d, Download::Finding(_) | Download::Downloading { .. })) {
                    note(ui, format!("{}: {}", p.file_name(), download_words(d)));
                }
                for (p, _, d) in b.items.iter().filter(|(_, _, d)| matches!(d, Download::Failed(_))).take(6) {
                    note(ui, format!("{}: {}", p.file_name(), download_words(d)));
                }
            }
        },
    );
}

fn capital(s: &str) -> String {
    super::capitalized(s)
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
pub(super) fn refusal(cs: &CameraSettings) -> Option<String> {
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
