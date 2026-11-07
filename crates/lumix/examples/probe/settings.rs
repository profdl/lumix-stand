//! `--settings`: what the camera says about its settings, and which `setsetting` values it takes.
//!
//! Everything here is a *hypothesis* from community notes on other Lumix models (`getinfo` with
//! `type=capability|allmenu|curmenu|lens`, `getsetting&type=…`, `setsetting&type=…&value=…`,
//! `camctrl&type=focus`), to be checked on the GX85. The names and value formats in [`SETTINGS`]
//! are guesses, several per setting, so the log shows which form the camera understands. Every
//! change is undone at once: the original value is written back and read again.

use std::time::Instant;

use lumix_link::Transport;
use lumix_link::net::NetTransport;

use super::{Args, Log};

/// The most of an answer printed in the log (the whole answer goes in a file when it is a dump).
const SHOWN: usize = 1500;

/// A setting to explore: its `type`, and values to try (guesses in several formats).
struct Setting {
    label: &'static str,
    ty: &'static str,
    guesses: &'static [&'static str],
}

/// Settings tried with `setsetting` (*hypothesis*: names and formats from notes on other models).
const SETTINGS: &[Setting] = &[
    Setting { label: "shutter speed", ty: "shtrspeed", guesses: &["1/60", "60", "1536/256", "1/125"] },
    Setting { label: "aperture", ty: "focal", guesses: &["5.6", "F5.6", "1280/256", "8.0"] },
    Setting { label: "ISO", ty: "iso", guesses: &["400", "auto", "200"] },
    Setting { label: "exposure compensation", ty: "exposure", guesses: &["0", "1/3", "+1/3", "-1/3", "0.3"] },
    Setting { label: "white balance", ty: "whitebalance", guesses: &["auto", "daylight", "cloudy", "incandescent"] },
    Setting { label: "focus mode", ty: "focusmode", guesses: &["mf", "af", "afs"] },
    Setting { label: "AF mode", ty: "afmode", guesses: &["1area", "49area", "tracking"] },
];

/// Settings only read (they show what else `getsetting` knows).
const READ_ONLY: &[&str] =
    &["exposuremode", "wb_ct", "colortemperature", "quality", "photostyle", "imgaspect", "flash", "drivemode", "mf_assist", "liveviewsize"];

/// `getinfo` types dumped to files.
const INFO: &[&str] = &["capability", "allmenu", "curmenu", "lens"];

/// Other read-only commands worth a look.
const EXTRA: &[&str] = &["/cam.cgi?mode=getlensdata", "/cam.cgi?mode=getcurmenu", "/cam.cgi?mode=getinfo&type=curmenu"];

/// What became of one setting, for the summary.
#[derive(Default)]
struct Outcome {
    label: &'static str,
    ty: &'static str,
    read: String,
    original: Option<String>,
    same_value: Option<String>,
    accepted: Vec<String>,
    refused: Vec<String>,
    restored: Option<bool>,
}

/// The `--settings` run.
pub fn explore(a: &Args, t: &mut NetTransport, log: &mut Log) {
    log.line("\n=== settings: getinfo dumps ===");
    for ty in INFO {
        let path = format!("/cam.cgi?mode=getinfo&type={ty}");
        if let Some(body) = get(t, log, &path) {
            let file = a.out.join(format!("getinfo_{ty}.xml"));
            match std::fs::write(&file, &body) {
                Ok(()) => log.line(&format!("  saved in full: {}", file.display())),
                Err(e) => log.line(&format!("  ! can't save {}: {e}", file.display())),
            }
        }
    }
    for path in EXTRA {
        let _ = get(t, log, path); // logged; an unknown command is an answer too
    }
    log.line("\n=== settings: read only ===");
    for ty in READ_ONLY {
        let _ = read(t, log, ty); // logged
    }
    log.line(if a.no_set {
        "\n=== settings: tried settings (read only, --no-set) ==="
    } else {
        "\n=== settings: setsetting (each change undone at once) ==="
    });
    let mut outcomes: Vec<Outcome> = SETTINGS.iter().map(|s| try_setting(s, a.no_set, t, log)).collect();
    if a.try_focus {
        focus_steps(t, log);
    }
    // Last look: everything back as it was?
    log.line("\n=== settings: final check ===");
    for o in &mut outcomes {
        if let Some(orig) = &o.original {
            let (_, now) = read(t, log, o.ty);
            if now.as_deref() != Some(orig.as_str()) && o.restored.is_some() {
                o.restored = Some(false);
            }
        }
    }
    summary(&outcomes, log);
}

/// One setting: read, write the same value back, try the guesses, undo each change.
fn try_setting(s: &Setting, no_set: bool, t: &mut NetTransport, log: &mut Log) -> Outcome {
    log.line(&format!("\n--- {} ({}) ---", s.label, s.ty));
    let (result, original) = read(t, log, s.ty);
    let mut o = Outcome { label: s.label, ty: s.ty, read: result, original: original.clone(), ..Outcome::default() };
    if no_set {
        return o;
    }
    if let Some(cur) = &original {
        o.same_value = Some(set(t, log, s.ty, cur));
    }
    for g in s.guesses.iter().filter(|g| original.as_deref() != Some(**g)) {
        let r = set(t, log, s.ty, g);
        let (_, after) = read(t, log, s.ty);
        let changed = after.is_some() && after != original;
        if r == "ok" {
            o.accepted.push(format!("{g} (reads back {})", after.as_deref().unwrap_or("?")));
        } else {
            o.refused.push(format!("{g}: {r}"));
        }
        if changed || r == "ok" {
            o.restored = Some(restore(t, log, s.ty, original.as_deref()) && o.restored != Some(false));
        }
    }
    o
}

/// Write `original` back and check it; `true`: it reads back as before.
fn restore(t: &mut NetTransport, log: &mut Log, ty: &str, original: Option<&str>) -> bool {
    let Some(orig) = original else {
        log.line(&format!("  !! {ty} changed and its original value is unknown: check it on the camera"));
        return false;
    };
    let r = set(t, log, ty, orig);
    let (_, now) = read(t, log, ty);
    let ok = now.as_deref() == Some(orig);
    if ok {
        log.line(&format!("  restored {ty} = {orig}"));
    } else {
        log.line(&format!("  !! couldn't restore {ty} to {orig} (setsetting {r}, now {}): set it on the camera", now.as_deref().unwrap_or("?")));
    }
    ok
}

/// `--try-focus`: one step each way, then back (approximate: focus steps aren't exact).
fn focus_steps(t: &mut NetTransport, log: &mut Log) {
    log.line("\n=== focus steps (--try-focus): the lens moves one step out and back ===");
    let _ = read(t, log, "focusmode"); // logged
    for v in ["wide-normal", "tele-normal", "tele-fast", "wide-fast"] {
        let _ = get(t, log, &format!("/cam.cgi?mode=camctrl&type=focus&value={v}")); // logged: the answer may hold the position
    }
}

/// `getsetting`: (the result code, the current value when the answer holds one).
fn read(t: &mut NetTransport, log: &mut Log, ty: &str) -> (String, Option<String>) {
    let Some(body) = get(t, log, &format!("/cam.cgi?mode=getsetting&type={ty}")) else { return ("no answer".into(), None) };
    let reply = lumix_link::Reply::parse(body.as_bytes());
    let value = attr(&body, ty).or_else(|| reply.field(ty).map(str::to_string)).or_else(|| attr(&body, "value"));
    if let Some(v) = &value {
        log.line(&format!("  {ty} = {v}"));
    }
    (reply.result.unwrap_or_else(|| "(no result)".into()), value)
}

/// `setsetting`: the result code.
fn set(t: &mut NetTransport, log: &mut Log, ty: &str, value: &str) -> String {
    match get(t, log, &format!("/cam.cgi?mode=setsetting&type={ty}&value={}", query_value(value))) {
        Some(body) => lumix_link::Reply::parse(body.as_bytes()).result.unwrap_or_else(|| "(no result)".into()),
        None => "no answer".into(),
    }
}

/// One GET, logged (the answer shortened); the body, `None` when no answer came.
fn get(t: &mut NetTransport, log: &mut Log, path: &str) -> Option<String> {
    let started = Instant::now();
    match t.get(path) {
        Ok(r) => {
            let body = String::from_utf8_lossy(&r.body).to_string();
            let shown: String = body.chars().take(SHOWN).collect();
            let more = if shown.len() < body.len() { format!(" … ({} bytes in all)", body.len()) } else { String::new() };
            log.line(&format!("> {path}\n< HTTP {} in {} ms: {}{more}", r.status, started.elapsed().as_millis(), shown.trim()));
            Some(body)
        }
        Err(e) => {
            log.line(&format!("> {path}\n! {e}"));
            None
        }
    }
}

/// The value of attribute `name="…"` in `body` (the first one).
fn attr(body: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let start = body.find(&key)? + key.len();
    let rest = body.get(start..)?;
    let end = rest.find('"')?;
    rest.get(..end).map(str::to_string)
}

/// `s` for a query string: letters, digits and `-._~/` kept, the rest percent-encoded (`+` would
/// otherwise read as a space).
fn query_value(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn summary(outcomes: &[Outcome], log: &mut Log) {
    log.line("\n=== settings: summary ===");
    for o in outcomes {
        log.line(&format!("{} ({}): getsetting {}, value {}", o.label, o.ty, o.read, o.original.as_deref().unwrap_or("(none in the answer)")));
        if let Some(r) = &o.same_value {
            log.line(&format!("  same value written back: {r}"));
        }
        if !o.accepted.is_empty() {
            log.line(&format!("  accepted: {}", o.accepted.join(", ")));
        }
        if !o.refused.is_empty() {
            log.line(&format!("  refused: {}", o.refused.join("; ")));
        }
        match o.restored {
            Some(true) => log.line("  restored: yes"),
            Some(false) => log.line("  !! NOT restored: set it on the camera"),
            None => {}
        }
    }
}
