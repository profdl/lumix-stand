//! A simulated Lumix camera behind the same [`Transport`] as the real one, for tests and for
//! trying the app without a camera (address `sim`).
//!
//! It answers `cam.cgi` as a GX85 did when probed (firmware D2.91, 2026-11-07): `getstate`,
//! `accctrl` in plain text (required first when [`SimOptions::require_handshake`]; with
//! [`SimOptions::accept_after_ms`] it is `ok_under_research` and commands are refused with
//! `err_reject` until that long after the first request), `recmode`,
//! `startstream` (it then streams for [`SimOptions::stream_timeout_ms`] after the last command,
//! unless refreshed), `capture` and `stopstream`. Each frame is a 32-byte header and a JPEG from
//! [`SimOptions::frame`]. It can be taken offline (every request times out) and back.
//!
//! Settings: `getsetting` and `setsetting` for everything in [`crate::menu::catalogue`] (a value
//! not in the list is `err_param`; AF mode can't be read, as on the GX85), shutter and aperture
//! as `n/256` (the aperture clamped to the lens), `getinfo` for the lens and the current menu
//! (picture sizes of other aspect ratios disabled), and the focus steps and one-shot AF, which
//! work only in MF. The card: each capture adds a photo, `playmode` stops live view, the DLNA
//! `Browse` answers only in playback mode, and [`Originals`] serves each photo's original from
//! [`SimOptions::original_delay_ms`] after its capture.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::menu;
use crate::photos::{self, PhotoId};
use crate::transport::{HttpReply, Originals, Transport};

/// Makes frame `n`'s JPEG (the default: a marker-only stand-in that no decoder reads).
pub type FrameMaker = Arc<dyn Fn(u64) -> Vec<u8> + Send + Sync>;
/// The simulator's clock (ms).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// How the simulated camera behaves.
#[derive(Clone)]
pub struct SimOptions {
    /// Commands are refused (`err_unsuitable_app`) until the access handshake.
    pub require_handshake: bool,
    /// A new device is taken on only this long after its first `accctrl` (the camera's own
    /// confirmation); `None`: at once.
    pub accept_after_ms: Option<u64>,
    /// Every request times out.
    pub offline: bool,
    /// Frames per second while streaming (1-60).
    pub fps: u32,
    /// The camera stops streaming this long after the last command.
    pub stream_timeout_ms: u64,
    /// `capture` answers `err_busy`.
    pub refuse_capture: bool,
    pub frame: FrameMaker,
    /// Makes the original of the `n`th photo on the card (the default: a stand-in).
    pub original: FrameMaker,
    /// Photos on the card before the first capture.
    pub photos_on_card: u32,
    /// An original can be downloaded this long after its capture.
    pub original_delay_ms: u64,
    /// The DLNA `Browse` fails (HTTP 500), as it does in record mode on a GX85.
    pub refuse_browse: bool,
    /// The clock. The default is a clock at 0 that never moves; tests pass their own, the app a
    /// real one.
    pub clock: Clock,
    /// [`Transport::recv`] sleeps up to its wait for the next frame (the app); off, it never sleeps
    /// (tests with their own clock).
    pub real_time: bool,
}

impl Default for SimOptions {
    fn default() -> Self {
        SimOptions {
            require_handshake: false,
            accept_after_ms: None,
            offline: false,
            fps: 30,
            stream_timeout_ms: 8_000,
            refuse_capture: false,
            frame: Arc::new(stand_in_jpeg),
            original: Arc::new(stand_in_jpeg),
            photos_on_card: 3,
            original_delay_ms: 500,
            refuse_browse: false,
            clock: Arc::new(|| 0),
            real_time: false,
        }
    }
}

impl std::fmt::Debug for SimOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimOptions")
            .field("require_handshake", &self.require_handshake)
            .field("offline", &self.offline)
            .field("fps", &self.fps)
            .field("stream_timeout_ms", &self.stream_timeout_ms)
            .field("refuse_capture", &self.refuse_capture)
            .finish()
    }
}

/// A JPEG-shaped stand-in: SOI, frame `n`'s number, filler, EOI.
pub fn stand_in_jpeg(n: u64) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    v.extend(n.to_be_bytes());
    v.extend(std::iter::repeat_n(0x5A, 120));
    v.extend([0xFF, 0xD9]);
    v
}

/// What the simulated camera has seen (tests read it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SimStats {
    pub requests: Vec<String>,
    pub browses: u32,
    pub downloads: u32,
    pub handshakes: u32,
    pub captures: u32,
    pub start_streams: u32,
    pub frames_sent: u64,
}

#[derive(Debug, Default)]
struct State {
    handshaken: bool,
    /// When the pending device is taken on (ms).
    accepted_at: Option<u64>,
    rec_mode: bool,
    /// Streaming until this time (ms).
    streaming_until: Option<u64>,
    next_frame_at: u64,
    /// In playback mode (between `playmode` and `recmode`).
    play_mode: bool,
    /// (type, value).
    settings: Vec<(String, String)>,
    focus: u32,
    /// The card: each photo and when it was taken (ms).
    card: Vec<(PhotoId, u64)>,
    stats: SimStats,
}

/// The first photo on the simulated card.
pub const FIRST_PHOTO: PhotoId = PhotoId(1_000_001);
/// The simulated lens: f/3.5-22 at this zoom (Av 3.6 to 9), 1/4000 s to 60 s, 12-32 mm.
pub const SIM_LENS: &str = "ok,2304/256,927/256,3072/256,-1536/256,0,off,32,12,on,128/1024,on";

/// The simulated camera's settings at first.
fn first_settings() -> Vec<(String, String)> {
    [
        ("shtrspeed", "1622/256"),
        ("focal", "1280/256"),
        ("iso", "auto"),
        ("exposure", "0"),
        ("whitebalance", "auto"),
        ("lightmetering", "multi"),
        ("focusmode", "afs"),
        ("afmode", "49area"),
        ("peaking", "off"),
        ("aspectratio", "16:9"),
        ("pictsize", "12m"),
        ("quality", "fine"),
        ("colormode", "standard"),
        ("filter_setting", "noeffect"),
        ("drivemode", "normal"),
        ("flash", "forcedflashoff"),
        ("liveviewsize", "vga"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect()
}

/// The picture sizes of each aspect ratio (L, M, S), as the GX85's menu allows them.
fn sizes(aspect: &str) -> [&'static str; 3] {
    match aspect {
        "3:2" => ["14m", "7m", "3.5m"],
        "16:9" => ["12m", "6m", "3m"],
        "1:1" => ["11.5m", "4.5m", "2m"],
        _ => ["16m", "8m", "4m"],
    }
}

/// The simulated camera. Clones share it, so a test can keep one to change and read it while the
/// link owns another.
#[derive(Clone)]
pub struct SimCamera {
    opts: Arc<Mutex<SimOptions>>,
    state: Arc<Mutex<State>>,
}

/// The most requests [`SimStats::requests`] keeps.
const MAX_REQUESTS: usize = 500;

impl SimCamera {
    pub fn new(opts: SimOptions) -> Self {
        let n = opts.photos_on_card.min(5000);
        let mut id = FIRST_PHOTO;
        let mut card = Vec::new();
        for _ in 0..n {
            card.push((id, 0));
            id = id.next();
        }
        let state = State { settings: first_settings(), focus: 512, card, ..State::default() };
        SimCamera { opts: Arc::new(Mutex::new(opts)), state: Arc::new(Mutex::new(state)) }
    }

    /// A setting's value now.
    pub fn setting(&self, key: &str) -> Option<String> {
        lock(&self.state).settings.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    /// Change a setting as if on the camera's own dials.
    pub fn set_setting(&self, key: &str, value: &str) {
        let mut st = lock(&self.state);
        put(&mut st.settings, key, value);
    }

    /// The photos on the card, oldest first.
    pub fn card(&self) -> Vec<PhotoId> {
        lock(&self.state).card.iter().map(|(p, _)| *p).collect()
    }

    /// In playback mode now.
    pub fn in_play_mode(&self) -> bool {
        lock(&self.state).play_mode
    }

    /// Change how the camera behaves.
    pub fn update(&self, f: impl FnOnce(&mut SimOptions)) {
        f(&mut lock(&self.opts));
    }

    /// Take the camera offline (`true`) or bring it back.
    pub fn set_offline(&self, offline: bool) {
        lock(&self.opts).offline = offline;
        if offline {
            lock(&self.state).streaming_until = None;
        }
    }

    pub fn stats(&self) -> SimStats {
        lock(&self.state).stats.clone()
    }

    /// Streaming now.
    pub fn streaming(&self) -> bool {
        let now = (lock(&self.opts).clock)();
        lock(&self.state).streaming_until.is_some_and(|t| now < t)
    }

    fn answer(&self, path: &str) -> Result<HttpReply, String> {
        let opts = lock(&self.opts).clone();
        let now = (opts.clock)();
        let mut st = lock(&self.state);
        if st.stats.requests.len() < MAX_REQUESTS {
            st.stats.requests.push(path.to_string());
        }
        if opts.offline {
            return Err("timed out (the simulated camera is offline)".into());
        }
        let Some(query) = path.strip_prefix("/cam.cgi?") else { return Ok(HttpReply { status: 404, body: Vec::new() }) };
        let arg = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(k).and_then(|v| v.strip_prefix('=')));
        let mode = arg("mode").unwrap_or_default();
        if mode == "accctrl" {
            st.stats.handshakes += 1;
            let at = *st.accepted_at.get_or_insert(now + opts.accept_after_ms.unwrap_or(0));
            st.handshaken = true;
            let body = if now < at { "ok_under_research,SIM-000000,remote,open" } else { "ok,SIM-000000,remote,encrypted" };
            return Ok(HttpReply { status: 200, body: body.as_bytes().to_vec() });
        }
        if opts.require_handshake && !st.handshaken {
            return Ok(reply("err_unsuitable_app", ""));
        }
        if st.accepted_at.is_some_and(|t| now < t) {
            return Ok(reply("err_reject", ""));
        }
        // Any command keeps a stream going.
        if st.streaming_until.is_some_and(|t| now < t) {
            st.streaming_until = Some(now + opts.stream_timeout_ms);
        }
        match mode {
            "getsetting" => return Ok(get_setting(&st, arg("type").unwrap_or_default())),
            "setsetting" => return Ok(set_setting(&mut st, arg("type").unwrap_or_default(), &unpercent(arg("value").unwrap_or_default()))),
            "getinfo" if arg("type") == Some("lens") => return Ok(HttpReply { status: 200, body: SIM_LENS.as_bytes().to_vec() }),
            "getinfo" if arg("type") == Some("curmenu") => return Ok(reply("ok", &curmenu(&st))),
            "camctrl" if arg("type") == Some("focus") => {
                let body = if setting_of(&st, "focusmode") == "mf" {
                    let step: i64 = match arg("value").unwrap_or_default() {
                        "tele-fast" => -130,
                        "tele-normal" => -14,
                        "wide-normal" => 14,
                        "wide-fast" => 130,
                        _ => return Ok(HttpReply { status: 200, body: b"err_param,0,0".to_vec() }),
                    };
                    st.focus = u32::try_from((i64::from(st.focus) + step).clamp(0, 1024)).unwrap_or(0);
                    format!("ok,{},1024", st.focus)
                } else {
                    "err_reject,0,0".to_string()
                };
                return Ok(HttpReply { status: 200, body: body.into_bytes() });
            }
            _ => {}
        }
        let result = match (mode, arg("value").unwrap_or_default()) {
            ("getstate", _) => {
                let cam = if st.rec_mode && !st.play_mode { "rec" } else { "play" };
                return Ok(reply("ok", &format!("<state><batt>3/3</batt><cammode>{cam}</cammode><sd_memory>set</sd_memory></state>")));
            }
            ("camcmd", "recmode") => {
                st.rec_mode = true;
                st.play_mode = false;
                "ok"
            }
            ("camcmd", "playmode") => {
                st.play_mode = true;
                st.streaming_until = None;
                "ok"
            }
            ("camcmd", "oneshot_af") if setting_of(&st, "focusmode") == "mf" => "ok",
            ("camcmd", "oneshot_af") => "err_reject",
            ("camcmd", "capture") if !st.rec_mode || st.play_mode => "err_reject",
            ("camcmd", "capture") if opts.refuse_capture => "err_busy",
            ("camcmd", "capture") => {
                st.stats.captures += 1;
                let next = st.card.last().map_or(FIRST_PHOTO, |(p, _)| p.next());
                if st.card.len() < 10_000 {
                    st.card.push((next, now));
                }
                "ok"
            }
            ("startstream", port) if port.parse::<u16>().is_ok() && st.rec_mode && !st.play_mode => {
                st.stats.start_streams += 1;
                if st.streaming_until.is_none_or(|t| now >= t) {
                    st.next_frame_at = now;
                }
                st.streaming_until = Some(now + opts.stream_timeout_ms);
                "ok"
            }
            ("startstream", _) => "err_param",
            ("stopstream", _) => {
                st.streaming_until = None;
                "ok"
            }
            _ => "err_non_support",
        };
        Ok(reply(result, ""))
    }
}

fn put(settings: &mut Vec<(String, String)>, key: &str, value: &str) {
    match settings.iter_mut().find(|(k, _)| k == key) {
        Some((_, v)) => *v = value.to_string(),
        None => settings.push((key.to_string(), value.to_string())),
    }
}

fn setting_of<'a>(st: &'a State, key: &str) -> &'a str {
    st.settings.iter().find(|(k, _)| k == key).map_or("", |(_, v)| v.as_str())
}

fn get_setting(st: &State, key: &str) -> HttpReply {
    if key == "afmode" {
        return reply("err_critical", "");
    }
    match st.settings.iter().find(|(k, _)| k == key) {
        Some((k, v)) => reply("ok", &format!("<settingvalue {k}=\"{v}\"></settingvalue>")),
        None => reply("err_param", ""),
    }
}

fn set_setting(st: &mut State, key: &str, value: &str) -> HttpReply {
    let Some(setting) = menu::find(key) else { return reply("err_param", "") };
    let value = match setting.kind {
        menu::Kind::Shutter => match crate::apex::parse(value) {
            Some(v) => crate::apex::format(v.clamp(-1536, 3072)),
            None => return reply("err_param", ""),
        },
        menu::Kind::Aperture => match crate::apex::parse(value) {
            Some(v) => crate::apex::format(v.clamp(927, 2304)),
            None => return reply("err_param", ""),
        },
        menu::Kind::Menu(choices) => {
            if !choices.iter().any(|c| c.value == value) {
                return reply("err_param", "");
            }
            if key == "pictsize" && !sizes(setting_of(st, "aspectratio")).contains(&value) {
                return reply("err_param", "");
            }
            value.to_string()
        }
    };
    if key == "aspectratio" {
        // The size keeps its class (L, M, S) in the new aspect ratio.
        let class = sizes(setting_of(st, "aspectratio")).iter().position(|s| *s == setting_of(st, "pictsize")).unwrap_or(0);
        let size = sizes(&value).get(class).copied().unwrap_or("16m");
        put(&mut st.settings, "pictsize", size);
    }
    put(&mut st.settings, key, &value);
    reply("ok", "")
}

/// A `curmenu` answer: each setting's value, and its choices with their `enable` flags.
fn curmenu(st: &State) -> String {
    let sizes = sizes(setting_of(st, "aspectratio"));
    let mut out = String::from("<menuinfo model=\"SIM\" version=\"2.0\"><photosettings>");
    for s in menu::catalogue() {
        out.push_str(&format!("<item id=\"menu_item_id_{}\" enable=\"yes\" value=\"{}\" />", s.menu_id, setting_of(st, s.key)));
        for c in s.choices() {
            let enabled = s.key != "pictsize" || sizes.contains(&c.value.as_str());
            out.push_str(&format!("<item id=\"menu_item_id_{}\" enable=\"{}\" />", c.menu_id, if enabled { "yes" } else { "no" }));
        }
    }
    out.push_str("</photosettings></menuinfo>");
    out
}

/// `%2F` and the like decoded (values come in as the controller sends them).
fn unpercent(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match (c, hex) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// A `Browse` answer for `count` photos from `start`.
fn browse(st: &State, body: &str) -> HttpReply {
    let num = |tag: &str| {
        body.split_once(&format!("<{tag}>")).and_then(|(_, r)| r.split_once('<')).and_then(|(n, _)| n.trim().parse::<usize>().ok()).unwrap_or(0)
    };
    let (start, count) = (num("StartingIndex"), num("RequestedCount").min(100));
    let items: String = st
        .card
        .iter()
        .skip(start)
        .take(count)
        .map(|(p, _)| format!("&lt;item id=&quot;{:07}&quot; parentID=&quot;0&quot;&gt;&lt;/item&gt;", p.0))
        .collect();
    let returned = st.card.len().saturating_sub(start).min(count);
    let xml = format!(
        "<s:Envelope><s:Body><u:BrowseResponse><Result>&lt;DIDL-Lite&gt;{items}&lt;/DIDL-Lite&gt;</Result><NumberReturned>{returned}</NumberReturned><TotalMatches>{}</TotalMatches></u:BrowseResponse></s:Body></s:Envelope>",
        st.card.len()
    );
    HttpReply { status: 200, body: xml.into_bytes() }
}

fn reply(result: &str, extra: &str) -> HttpReply {
    let body = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n<camrply><result>{result}</result>{extra}</camrply>");
    HttpReply { status: 200, body: body.into_bytes() }
}

impl Transport for SimCamera {
    fn get(&mut self, path: &str) -> Result<HttpReply, String> {
        self.answer(path)
    }

    fn post(&mut self, port: u16, path: &str, action: &str, body: &str) -> Result<HttpReply, String> {
        let opts = lock(&self.opts).clone();
        let mut st = lock(&self.state);
        if st.stats.requests.len() < MAX_REQUESTS {
            st.stats.requests.push(format!("POST {port}{path}"));
        }
        if opts.offline {
            return Err("timed out (the simulated camera is offline)".into());
        }
        if port != photos::DLNA_PORT || path != photos::BROWSE_PATH || !action.contains("Browse") {
            return Ok(HttpReply { status: 404, body: Vec::new() });
        }
        st.stats.browses += 1;
        if opts.refuse_browse || !st.play_mode {
            return Ok(HttpReply { status: 500, body: b"<s:Fault><errorCode>501</errorCode></s:Fault>".to_vec() });
        }
        Ok(browse(&st, body))
    }

    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Result<Option<usize>, String> {
        let opts = lock(&self.opts).clone();
        let now = (opts.clock)();
        let mut st = lock(&self.state);
        let streaming = st.streaming_until.is_some_and(|t| now < t) && !opts.offline;
        if !streaming || now < st.next_frame_at {
            let wait = if streaming { st.next_frame_at.saturating_sub(now).min(wait_ms) } else { wait_ms };
            drop(st);
            if opts.real_time && wait > 0 {
                std::thread::sleep(std::time::Duration::from_millis(wait.min(50)));
            }
            return Ok(None);
        }
        let interval = 1000 / u64::from(opts.fps.clamp(1, 60));
        st.next_frame_at = st.next_frame_at.saturating_add(interval).max(now);
        st.stats.frames_sent += 1;
        let n = st.stats.frames_sent;
        drop(st);
        let mut datagram = vec![0u8; 32];
        if let Some(h) = datagram.get_mut(..8) {
            h.copy_from_slice(&n.to_be_bytes());
        }
        datagram.extend((opts.frame)(n));
        let len = datagram.len().min(buf.len());
        let (Some(dst), Some(src)) = (buf.get_mut(..len), datagram.get(..len)) else { return Ok(None) };
        dst.copy_from_slice(src);
        Ok(Some(len))
    }

    fn udp_port(&self) -> u16 {
        crate::DEFAULT_UDP_PORT
    }
}

impl Originals for SimCamera {
    fn original(&mut self, id: PhotoId, out: &mut dyn std::io::Write, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<Option<u64>, String> {
        let opts = lock(&self.opts).clone();
        let now = (opts.clock)();
        if opts.offline {
            return Err("no answer (the simulated camera is offline)".into());
        }
        let n = {
            let mut st = lock(&self.state);
            let Some(i) = st.card.iter().position(|(p, at)| *p == id && now >= at.saturating_add(opts.original_delay_ms)) else { return Ok(None) };
            st.stats.downloads += 1;
            u64::try_from(i).unwrap_or(0) + 1
        };
        let jpeg = (opts.original)(n);
        let len = jpeg.len() as u64;
        out.write_all(&jpeg).map_err(|e| format!("can't write the photo: {e}"))?;
        progress(len, Some(len));
        Ok(Some(len))
    }
}

/// A lock that a panicked holder doesn't poison for good.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
