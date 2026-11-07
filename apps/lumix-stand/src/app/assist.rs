//! Find the best exposure, and ask Claude about a test shot.
//!
//! **Find the best exposure** runs on its own, one step per [`Stand::tick`]:
//! 1. In MF, focus once (AF), so the frame is sharp.
//! 2. Base ISO and the sharpest aperture ([`exposure::base_settings`]).
//! 3. A test shot; the shutter moved by what the shot measures ([`exposure::correction`]); again,
//!    until the brightest tones sit just under clipping (at most [`MAX_SHOTS`] shots).
//! 4. With "Check with Claude" on, Claude looks at the final shot for what numbers can't tell
//!    (white balance, colour casts, glare, uneven light). Its changes are applied and checked with
//!    another test shot; Claude is asked at most [`MAX_ASKS`] times.
//!
//! The camera must be in M (shutter and aperture are refused otherwise): a refusal stops it with
//! that advice.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use super::Stand;
use super::shots::ShotState;
use crate::claude::{self, Reply};
use crate::exposure;

/// The most test shots one search takes.
pub const MAX_SHOTS: u32 = 7;
/// The most times one search asks Claude.
pub const MAX_ASKS: u32 = 2;
/// The longest a setting change may take to be answered.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(8);
/// Lines of the search's log kept for the window.
const MAX_LOG: usize = 12;

#[derive(Debug)]
enum Step {
    /// Settings sent (these): wait for the camera to answer them.
    Settle(Instant, Vec<String>),
    /// Take the next test shot.
    Shoot,
    /// Waiting for test shot `n`.
    Shot(u32),
    /// Waiting for Claude about test shot `ticket`.
    Asking,
    Done,
}

/// A search for the best exposure under way (or finished: its outcome).
#[derive(Debug)]
pub struct Assist {
    step: Step,
    pub shots: u32,
    asks: u32,
    /// The slowest shutter seen to clip (Tv × 256).
    clipped: Option<i32>,
    with_claude: bool,
    /// What happened, in plain words, oldest first.
    pub log: Vec<String>,
    /// Finished: (well exposed, what to say).
    pub outcome: Option<(bool, String)>,
}

impl Assist {
    pub fn running(&self) -> bool {
        self.outcome.is_none()
    }

    fn note(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > MAX_LOG {
            self.log.remove(0);
        }
    }

    fn finish(&mut self, ok: bool, why: impl Into<String>) {
        let why = why.into();
        self.note(why.clone());
        self.outcome = Some((ok, why));
        self.step = Step::Done;
    }
}

/// Claude's answer on its way: (the test shot, the answer, the key it used).
pub(super) type Asking = Receiver<(u64, Result<Reply, String>, Option<String>)>;

impl Stand {
    /// Start looking for the best exposure.
    pub fn find_exposure(&mut self) {
        if self.assist.as_ref().is_some_and(Assist::running) {
            return;
        }
        let Some(st) = self.status().filter(|s| s.ready()) else { return self.fail("Connect to the camera first.") };
        let cs = st.snap.settings.clone();
        let mut a =
            Assist { step: Step::Shoot, shots: 0, asks: 0, clipped: None, with_claude: self.settings.claude_check, log: Vec::new(), outcome: None };
        let mut sent = Vec::new();
        if cs.value("focusmode") == Some("mf") {
            self.one_shot_af();
            a.note("Focusing once (AF).");
        }
        let base = exposure::base_settings(&cs);
        for (k, v) in &base {
            self.set_camera(k, v);
            sent.push(k.clone());
        }
        if !base.is_empty() {
            a.note(format!("Least grain and sharpest: {}.", words(&base)));
        }
        a.step = Step::Settle(Instant::now(), sent);
        self.assist = Some(a);
        self.last = Some((true, "Finding the best exposure\u{2026}".into()));
    }

    pub fn stop_exposure(&mut self) {
        if let Some(a) = self.assist.as_mut().filter(|a| a.running()) {
            a.finish(false, "Stopped.");
        }
    }

    /// One step of the search.
    pub(super) fn poll_assist(&mut self) {
        let Some(mut a) = self.assist.take() else { return };
        if a.running() {
            self.step_assist(&mut a);
        }
        self.assist = Some(a);
    }

    fn step_assist(&mut self, a: &mut Assist) {
        let Some(st) = self.status().filter(|s| s.open()) else { return a.finish(false, "The camera was disconnected.") };
        let cs = st.snap.settings.clone();
        match std::mem::replace(&mut a.step, Step::Done) {
            Step::Settle(since, sent) => {
                let answered = st.snap.commands_pending == 0 && !st.settling;
                if !answered && since.elapsed() < SETTLE_TIMEOUT {
                    a.step = Step::Settle(since, sent);
                    return;
                }
                if let Some((key, _, result)) = &cs.refused
                    && sent.contains(key)
                {
                    let why = if result == "err_reject" {
                        "The camera refused a setting: set the mode dial to M, then try again.".to_string()
                    } else {
                        format!("The camera refused {key} ({result}).")
                    };
                    return a.finish(false, why);
                }
                a.step = Step::Shoot;
            }
            Step::Shoot => {
                if self.capturing() {
                    a.step = Step::Shoot;
                    return;
                }
                self.test_shot();
                a.shots += 1;
                a.step = Step::Shot(self.tests_taken + 1);
            }
            Step::Shot(n) => {
                let Some(t) = self.tests.iter().find(|t| t.n == n) else {
                    // Not fired yet, or the shutter failed (the window says why).
                    if self.capturing() || self.test_wanted.is_some() {
                        a.step = Step::Shot(n);
                    } else {
                        a.finish(false, "The test shot failed.");
                    }
                    return;
                };
                let (ticket, look) = match &t.state {
                    ShotState::Fetching(_) => {
                        a.step = Step::Shot(n);
                        return;
                    }
                    ShotState::Failed(e) => return a.finish(false, format!("{}: {e}", t.title())),
                    ShotState::Ready { look, .. } => (t.ticket, (**look).clone()),
                };
                let title = t.title();
                let stops = exposure::correction(&look);
                if look.high > exposure::CLIPPED
                    && let Some(tv) = t.camera.iter().find(|(k, _)| k == "shtrspeed").and_then(|(_, v)| lumix_link::apex::parse(v))
                {
                    a.clipped = Some(a.clipped.map_or(tv, |c| c.max(tv)));
                }
                // Right, or as close as third stops get without going back to a shutter that clipped.
                let closest = look.high <= exposure::CLIPPED
                    && stops.abs() < exposure::THIRD
                    && exposure::shutter_for(&cs, stops, a.clipped).is_ok_and(|(_, moved)| moved.abs() < 0.1);
                if exposure::right(&look) || closest {
                    a.note(format!("{title}: well exposed, nothing clipped."));
                    if a.with_claude && a.asks < MAX_ASKS {
                        a.note(format!("Asking {}\u{2026}", claude::model_name(&self.settings.claude_model)));
                        self.ask_claude(ticket);
                        a.step = Step::Asking;
                    } else {
                        a.finish(
                            true,
                            format!(
                                "Done in {} test shot{}: {}.",
                                a.shots,
                                if a.shots == 1 { "" } else { "s" },
                                super::shots::summary_text(&t.camera)
                            ),
                        );
                    }
                    return;
                }
                if a.shots >= MAX_SHOTS {
                    return a.finish(false, format!("Not settled after {MAX_SHOTS} test shots: is the light changing?"));
                }
                match exposure::shutter_for(&cs, stops, a.clipped) {
                    Ok((_, moved)) if moved.abs() < 0.1 => {
                        let what = if stops > 0.0 {
                            "too dark at the slowest shutter speed: add light"
                        } else {
                            "too bright at the fastest shutter speed: dim the lights"
                        };
                        a.finish(false, format!("{title}: {what}."));
                    }
                    Ok((v, moved)) => {
                        let label = lumix_link::apex::parse(&v).map(lumix_link::apex::shutter_label).unwrap_or_default();
                        let why =
                            if look.high > exposure::CLIPPED { format!("{:.1}% blown", look.high * 100.0) } else { format!("{:+.1} stops", stops) };
                        a.note(format!("{title}: {why}; shutter to {label} ({moved:+.1})."));
                        self.set_camera("shtrspeed", &v);
                        a.step = Step::Settle(Instant::now(), vec!["shtrspeed".into()]);
                    }
                    Err(e) => a.finish(false, capital(&e)),
                }
            }
            Step::Asking => {
                if self.asking.is_some() {
                    a.step = Step::Asking;
                    return;
                }
                let Some((_, reply)) = &self.advice else { return a.finish(true, "Exposure set; Claude didn't answer.") };
                let reply = reply.clone();
                a.asks += 1;
                match reply {
                    Err(e) => a.finish(true, format!("Exposure set by measurement; Claude couldn't be asked: {e}")),
                    Ok(r) => {
                        a.note(format!("Claude: {} (${:.4})", r.advice.assessment, r.cost()));
                        if r.changes.is_empty() {
                            a.finish(true, format!("Done in {} test shots; Claude agrees.", a.shots));
                        } else if a.shots >= MAX_SHOTS {
                            a.finish(true, "Exposure set; Claude's suggestions are below (out of test shots).");
                        } else {
                            a.note(format!("Applying Claude's suggestions: {}.", words(&r.changes)));
                            let sent = r.changes.iter().map(|(k, _)| k.clone()).collect();
                            for (k, v) in &r.changes {
                                self.set_camera(k, v);
                            }
                            a.step = Step::Settle(Instant::now(), sent);
                        }
                    }
                }
            }
            Step::Done => {}
        }
    }

    /// Ask Claude about test shot `ticket` (its answer goes to [`Stand::advice`]).
    pub fn ask_claude(&mut self, ticket: u64) {
        if self.asking.is_some() {
            return;
        }
        let Some(st) = self.status() else { return self.fail("Connect to the camera first.") };
        let Some(t) = self.tests.iter().find(|t| t.ticket == ticket) else { return };
        let ShotState::Ready { frame, look } = &t.state else { return self.fail("That test shot has no preview to show Claude.") };
        // The camera's lists now, with the settings the shot was taken with.
        let mut cs = (*st.snap.settings).clone();
        for (k, v) in &t.camera {
            if let Some(slot) = cs.values.iter_mut().find(|(key, _)| key == k) {
                slot.1 = v.clone();
            }
        }
        let (frame, look) = (frame.clone(), (**look).clone());
        let (model, reference, cached) = (self.settings.claude_model.clone(), self.settings.claude_key.clone(), self.api_key.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let repaint = self.repaint.clone();
        let spawned = std::thread::Builder::new().name("lumix-claude".into()).spawn(move || {
            let run = || -> (Result<Reply, String>, Option<String>) {
                let key = match cached.map_or_else(|| claude::api_key(&reference), Ok) {
                    Ok(k) => k,
                    Err(e) => return (Err(e), None),
                };
                let r = claude::jpeg_of(&frame).and_then(|jpeg| claude::ask(&key, &claude::request(&model, &jpeg, &look, &cs), &cs));
                (r, Some(key))
            };
            let (r, key) = std::panic::catch_unwind(run).unwrap_or_else(|_| (Err("asking Claude failed (internal error)".into()), None));
            let _ = tx.send((ticket, r, key)); // the window may have closed
            repaint();
        });
        match spawned {
            Ok(_) => {
                self.asking = Some(rx);
                self.advice = None;
                self.last = Some((true, format!("Asking {} about {}\u{2026}", claude::model_name(&self.settings.claude_model), t.title())));
            }
            Err(e) => self.fail(format!("can't ask Claude: {e}")),
        }
    }

    /// Claude's answer, when it comes.
    pub(super) fn poll_claude(&mut self) {
        let Some(rx) = &self.asking else { return };
        let (ticket, r, key) = match rx.try_recv() {
            Ok(x) => x,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => (0, Err("asking Claude stopped".into()), None),
        };
        self.asking = None;
        if key.is_some() {
            self.api_key = key;
        }
        if let Err(e) = &r {
            // A refused key is forgotten, so the next ask reads it again.
            if e.contains("401") {
                self.api_key = None;
            }
            self.last = Some((false, format!("Claude: {e}")));
        } else if let Ok(reply) = &r {
            self.last = Some((true, format!("Claude: {}", reply.advice.assessment)));
        }
        self.advice = Some((ticket, r));
    }

    /// Apply Claude's last suggestions.
    pub fn apply_advice(&mut self) {
        let Some((_, Ok(r))) = &self.advice else { return };
        let changes = r.changes.clone();
        for (k, v) in &changes {
            self.set_camera(k, v);
        }
        self.last = Some((true, format!("Applied: {}. Take a test shot to see it.", words(&changes))));
    }

    /// Claude's model.
    pub fn set_claude_model(&mut self, id: &str) {
        if self.settings.claude_model != id && claude::MODELS.iter().any(|m| m.0 == id) {
            self.settings.claude_model = id.to_string();
            self.save_settings();
        }
    }

    /// The 1Password reference for the key (empty: `ANTHROPIC_API_KEY`).
    pub fn set_claude_key(&mut self, reference: &str) {
        let r = reference.trim().to_string();
        if self.settings.claude_key != r {
            self.settings.claude_key = r;
            self.api_key = None;
            self.save_settings();
        }
    }

    pub fn set_claude_check(&mut self, on: bool) {
        if self.settings.claude_check != on {
            self.settings.claude_check = on;
            self.save_settings();
        }
    }

    /// Claude is being asked.
    pub fn asking_claude(&self) -> bool {
        self.asking.is_some()
    }
}

/// `ISO 200, Aperture f/5.6`.
pub fn words(changes: &[(String, String)]) -> String {
    changes
        .iter()
        .map(|(k, v)| lumix_link::menu::find(k).map_or_else(|| format!("{k} {v}"), |s| format!("{} {}", s.label, s.value_label(v))))
        .collect::<Vec<_>>()
        .join(", ")
}

fn capital(s: &str) -> String {
    super::capitalized(s)
}
