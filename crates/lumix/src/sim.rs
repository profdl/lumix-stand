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

use std::sync::{Arc, Mutex, MutexGuard};

use crate::transport::{HttpReply, Transport};

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
    stats: SimStats,
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
        SimCamera { opts: Arc::new(Mutex::new(opts)), state: Arc::new(Mutex::new(State::default())) }
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
        let result = match (mode, arg("value").unwrap_or_default()) {
            ("getstate", _) => {
                let cam = if st.rec_mode { "rec" } else { "play" };
                return Ok(reply("ok", &format!("<state><batt>3/3</batt><cammode>{cam}</cammode><sd_memory>set</sd_memory></state>")));
            }
            ("camcmd", "recmode") => {
                st.rec_mode = true;
                "ok"
            }
            ("camcmd", "capture") if !st.rec_mode => "err_reject",
            ("camcmd", "capture") if opts.refuse_capture => "err_busy",
            ("camcmd", "capture") => {
                st.stats.captures += 1;
                "ok"
            }
            ("startstream", port) if port.parse::<u16>().is_ok() && st.rec_mode => {
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

fn reply(result: &str, extra: &str) -> HttpReply {
    let body = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n<camrply><result>{result}</result>{extra}</camrply>");
    HttpReply { status: 200, body: body.into_bytes() }
}

impl Transport for SimCamera {
    fn get(&mut self, path: &str) -> Result<HttpReply, String> {
        self.answer(path)
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

/// A lock that a panicked holder doesn't poison for good.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
