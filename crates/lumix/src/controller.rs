//! The camera protocol as a sans-IO state machine. See the crate docs.
//!
//! # Connecting
//! `getstate` first (is anyone there?). If the camera refuses us as a client, the access handshake
//! (`accctrl`) goes out and the refused step is tried again. Seen on a GX85 (firmware D2.91,
//! 2026-11-07): `getstate` answers `err_unsuitable_app` until the handshake; `accctrl` answers in
//! plain text, `ok,GX85-B3B1DA,remote,encrypted` for a device it knows, and for a new one
//! `ok_under_research,…` while every command is refused with `err_reject`, until the camera takes it
//! on. While it is pending ([`Snapshot::awaiting_accept`]) `getstate` is tried every second and the
//! handshake sent again every 5 s, for at most 90 s. Then record mode, then live view
//! when it is wanted. A connect that gets no answer goes to [`Phase::Lost`] and is tried again
//! after a backoff (1 s, doubling, at most 30 s), for as long as the link is open.
//!
//! # Keeping live view going
//! Measured on a GX85: live view stops about 11.3 s after the last `startstream`. So `getstate`
//! goes out every `keepalive_ms` and `startstream` again every `restream_ms` (5 s each); when no frame has come for `frame_timeout_ms`, `startstream` goes out at once
//! and [`Snapshot::no_frames`] says so (the usual cause on macOS is a refused Local Network
//! permission). Several unanswered requests in a row count as the camera gone.

use std::collections::VecDeque;

use crate::frame::{Frame, extract_jpeg};
use crate::reply::Reply;
use crate::transport::{HttpReply, Request, RequestKind};

/// Log lines kept.
pub const LOG_LINES: usize = 50;
/// The longest log line kept (chars).
const LOG_LINE_CHARS: usize = 240;

/// Timings and identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Where live view is asked to go.
    pub udp_port: u16,
    /// Our id in the access handshake (kept, so the camera sees the same device each time).
    pub device_id: String,
    /// Our name in the access handshake (the camera may show it).
    pub device_name: String,
    pub keepalive_ms: u64,
    pub restream_ms: u64,
    pub frame_timeout_ms: u64,
    /// The longest wait for the camera to accept a new device.
    pub accept_timeout_ms: u64,
    /// Unanswered requests in a row before the camera counts as gone.
    pub max_failures: u32,
    pub backoff_min_ms: u64,
    pub backoff_max_ms: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            udp_port: crate::DEFAULT_UDP_PORT,
            device_id: "4D454930-0100-1000-8000-PICABIA00001".into(),
            device_name: "Picabia".into(),
            keepalive_ms: 5_000,
            restream_ms: 5_000,
            frame_timeout_ms: 3_000,
            accept_timeout_ms: 90_000,
            max_failures: 3,
            backoff_min_ms: 1_000,
            backoff_max_ms: 30_000,
        }
    }
}

/// Where the link is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// Not connected, and not trying.
    #[default]
    Closed,
    /// Connecting (or reconnecting).
    Connecting,
    /// In record mode, live view off.
    Connected,
    /// In record mode, live view asked for.
    Streaming,
    /// No answer: trying again after a backoff.
    Lost,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Closed => "closed",
            Phase::Connecting => "connecting",
            Phase::Connected => "connected",
            Phase::Streaming => "streaming",
            Phase::Lost => "lost",
        }
    }

    /// The camera answers commands.
    pub fn ready(self) -> bool {
        matches!(self, Phase::Connected | Phase::Streaming)
    }
}

/// What became of a capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureOutcome {
    Pending,
    Done,
    Failed(String),
}

/// The link's state, for status reports.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub phase: Phase,
    /// Live view is wanted (it streams once connected).
    pub live_view: bool,
    pub handshaken: bool,
    /// The camera hasn't taken this device on yet (`ok_under_research`, then `err_reject`).
    pub awaiting_accept: bool,
    /// Frames received so far, and the last one's number (the same; 0: none yet).
    pub frame_seq: u64,
    /// Datagrams that held no frame.
    pub dropped: u64,
    /// Why the last datagram was dropped.
    pub last_drop: Option<String>,
    /// Frames in the last second.
    pub fps: u32,
    /// Streaming, but no frame for `frame_timeout_ms`.
    pub no_frames: bool,
    pub last_error: Option<String>,
    /// Unanswered requests in a row.
    pub failures: u32,
    /// When lost: ms until the next try.
    pub retry_in_ms: Option<u64>,
    /// The camera's last `getstate` leaves (battery, mode, card…).
    pub state: Vec<(String, String)>,
    pub log: Vec<String>,
}

/// The protocol (see the module docs).
#[derive(Debug)]
pub struct Controller {
    opts: Options,
    phase: Phase,
    live_view: bool,
    queue: VecDeque<RequestKind>,
    handshake_tried: bool,
    handshaken: bool,
    /// What to send again once the handshake is through.
    after_handshake: Option<RequestKind>,
    /// Waiting for the camera to accept us, since (ms).
    accept_since: Option<u64>,
    /// While waiting: the next try, and the last handshake sent.
    accept_retry_at: u64,
    accept_last_ask: u64,
    last_keepalive: u64,
    last_restream: u64,
    stream_since: u64,
    last_frame: Option<u64>,
    frame_times: VecDeque<u64>,
    failures: u32,
    backoff_ms: u64,
    retry_at: Option<u64>,
    frame: Option<Frame>,
    frame_seq: u64,
    dropped: u64,
    last_drop: Option<String>,
    last_error: Option<String>,
    state: Vec<(String, String)>,
    captures: u64,
    capture: Option<(u64, CaptureOutcome)>,
    /// Closing: one best-effort `stopstream` still to go.
    closing: bool,
    log: VecDeque<String>,
}

impl Controller {
    pub fn new(opts: Options) -> Self {
        let backoff_ms = opts.backoff_min_ms;
        Controller {
            opts,
            phase: Phase::Closed,
            live_view: true,
            queue: VecDeque::new(),
            handshake_tried: false,
            handshaken: false,
            after_handshake: None,
            accept_since: None,
            accept_retry_at: 0,
            accept_last_ask: 0,
            last_keepalive: 0,
            last_restream: 0,
            stream_since: 0,
            last_frame: None,
            frame_times: VecDeque::new(),
            failures: 0,
            backoff_ms,
            retry_at: None,
            frame: None,
            frame_seq: 0,
            dropped: 0,
            last_drop: None,
            last_error: None,
            state: Vec::new(),
            captures: 0,
            capture: None,
            closing: false,
            log: VecDeque::new(),
        }
    }

    pub fn options(&self) -> &Options {
        &self.opts
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Start connecting (live view starts once connected, unless [`Controller::stop_live_view`]).
    pub fn connect(&mut self, now: u64) {
        self.closing = false;
        self.start_connect(now);
    }

    /// Close the link: live view is asked to stop (best effort), and nothing more goes out.
    pub fn disconnect(&mut self) {
        let was_ready = self.phase.ready();
        self.phase = Phase::Closed;
        self.queue.clear();
        self.retry_at = None;
        self.closing = was_ready;
        self.fail_capture("the camera was disconnected");
        self.note("closed");
    }

    /// Turn live view on (it starts at once when connected).
    pub fn start_live_view(&mut self) {
        self.live_view = true;
        if self.phase == Phase::Connected {
            self.push(RequestKind::StartStream);
        }
    }

    /// Turn live view off: the camera is asked to stop streaming; the link stays connected.
    pub fn stop_live_view(&mut self) {
        self.live_view = false;
        self.queue.retain(|k| *k != RequestKind::StartStream);
        if self.phase == Phase::Streaming {
            self.phase = Phase::Connected;
            self.push(RequestKind::StopStream);
        }
    }

    /// Fire the shutter. The capture's number, to ask [`Controller::capture_outcome`] about; an
    /// error when the camera isn't ready or a capture is still waiting for its answer.
    pub fn capture(&mut self) -> Result<u64, String> {
        if !self.phase.ready() {
            return Err(format!("the camera isn't connected ({})", self.phase.as_str()));
        }
        if matches!(self.capture, Some((_, CaptureOutcome::Pending))) {
            return Err("the last capture hasn't finished yet".into());
        }
        self.captures += 1;
        self.capture = Some((self.captures, CaptureOutcome::Pending));
        // A capture goes before everything else waiting.
        self.queue.push_front(RequestKind::Capture);
        Ok(self.captures)
    }

    /// What became of capture `id`; `None`: not the last capture.
    pub fn capture_outcome(&self, id: u64) -> Option<CaptureOutcome> {
        self.capture.as_ref().filter(|(n, _)| *n == id).map(|(_, o)| o.clone())
    }

    /// The newest frame.
    pub fn frame(&self) -> Option<&Frame> {
        self.frame.as_ref()
    }

    /// The newest frame's number (0: none yet).
    pub fn frame_seq(&self) -> u64 {
        self.frame_seq
    }

    pub fn snapshot(&self, now: u64) -> Snapshot {
        Snapshot {
            phase: self.phase,
            live_view: self.live_view,
            handshaken: self.handshaken,
            awaiting_accept: self.accept_since.is_some(),
            frame_seq: self.frame_seq,
            dropped: self.dropped,
            last_drop: self.last_drop.clone(),
            fps: self.fps(now),
            no_frames: self.no_frames(now),
            last_error: self.last_error.clone(),
            failures: self.failures,
            retry_in_ms: self.retry_at.map(|t| t.saturating_sub(now)),
            state: self.state.clone(),
            log: self.log.iter().cloned().collect(),
        }
    }

    /// The next request to send, if any.
    pub fn poll(&mut self, now: u64) -> Option<Request> {
        if self.closing {
            self.closing = false;
            return Some(self.request(RequestKind::StopStream));
        }
        if self.phase == Phase::Lost && self.retry_at.is_some_and(|t| now >= t) {
            self.note("trying again");
            self.start_connect(now);
        }
        if self.phase == Phase::Closed || self.phase == Phase::Lost {
            return None;
        }
        if self.queue.is_empty() {
            self.schedule(now);
        }
        let kind = self.queue.pop_front()?;
        match kind {
            RequestKind::KeepAlive => self.last_keepalive = now,
            RequestKind::StartStream => self.last_restream = now,
            _ => {}
        }
        Some(self.request(kind))
    }

    /// The answer to a `kind` request.
    pub fn on_reply(&mut self, kind: RequestKind, reply: Result<HttpReply, String>, now: u64) {
        let reply = reply.and_then(|r| if r.status == 200 { Ok(r) } else { Err(format!("the camera answered HTTP {}", r.status)) });
        match reply {
            Err(e) => self.on_failure(kind, e, now),
            Ok(r) => {
                self.failures = 0;
                let reply = Reply::parse(&r.body);
                self.note(&format!("< {} {}", kind_name(kind), reply.result.as_deref().unwrap_or("(no result)")));
                self.on_answer(kind, &reply, now);
            }
        }
    }

    /// A live-view datagram.
    pub fn on_datagram(&mut self, datagram: &[u8], now: u64) {
        if !self.phase.ready() {
            return;
        }
        match extract_jpeg(datagram) {
            Ok(f) => {
                self.frame = Some(f);
                self.frame_seq += 1;
                self.last_frame = Some(now);
                self.frame_times.push_back(now);
                while self.frame_times.len() > 240 || self.frame_times.front().is_some_and(|t| now.saturating_sub(*t) > 1000) {
                    self.frame_times.pop_front();
                }
            }
            Err(e) => {
                self.dropped += 1;
                self.last_drop = Some(e.to_string());
            }
        }
    }

    /// The UDP socket failed.
    pub fn on_udp_error(&mut self, e: &str, _now: u64) {
        self.last_error = Some(format!("live view: {e}"));
        self.note(&format!("udp error: {e}"));
    }

    // ---------- inside ----------

    fn start_connect(&mut self, now: u64) {
        self.phase = Phase::Connecting;
        self.queue.clear();
        self.retry_at = None;
        self.after_handshake = None;
        self.accept_since = None;
        self.handshake_tried = false;
        self.stream_since = now;
        self.push(RequestKind::Probe);
    }

    /// Periodic requests while connected.
    fn schedule(&mut self, now: u64) {
        if self.phase == Phase::Connecting && self.accept_since.is_some() {
            self.schedule_accept(now);
            return;
        }
        if !self.phase.ready() {
            return;
        }
        let since_restream = now.saturating_sub(self.last_restream);
        // Live view wanted but refused last time: ask again now and then.
        if self.phase == Phase::Connected && self.live_view && since_restream >= self.opts.restream_ms {
            self.push(RequestKind::StartStream);
            return;
        }
        if self.phase == Phase::Streaming {
            if self.no_frames(now) && since_restream >= self.opts.frame_timeout_ms {
                self.push(RequestKind::StartStream);
                return;
            }
            if since_restream >= self.opts.restream_ms {
                self.push(RequestKind::StartStream);
                return;
            }
        }
        if now.saturating_sub(self.last_keepalive) >= self.opts.keepalive_ms {
            self.push(RequestKind::KeepAlive);
        }
    }

    /// Waiting to be accepted: `getstate` every second, the handshake again every 5 s.
    fn schedule_accept(&mut self, now: u64) {
        if now < self.accept_retry_at {
            return;
        }
        if now.saturating_sub(self.accept_last_ask) >= 5_000 {
            self.accept_last_ask = now;
            self.after_handshake = Some(RequestKind::Probe);
            self.push(RequestKind::Handshake);
        } else {
            self.push(RequestKind::Probe);
        }
    }

    /// Still not accepted: try again in a second, or give up after `accept_timeout_ms`.
    fn keep_waiting(&mut self, now: u64) {
        let since = *self.accept_since.get_or_insert(now);
        if now.saturating_sub(since) >= self.opts.accept_timeout_ms {
            self.accept_since = None;
            self.last_error =
                Some("the camera didn't accept this computer: on the camera, allow the connection from Picabia, then connect again".into());
            self.lose(now);
            return;
        }
        self.accept_retry_at = now + 1_000;
    }

    fn push(&mut self, kind: RequestKind) {
        if !self.queue.contains(&kind) {
            self.queue.push_back(kind);
        }
    }

    fn request(&mut self, kind: RequestKind) -> Request {
        let path = match kind {
            RequestKind::Probe | RequestKind::KeepAlive => "/cam.cgi?mode=getstate".to_string(),
            RequestKind::Handshake => format!(
                "/cam.cgi?mode=accctrl&type=req_acc&value={}&value2={}",
                query_value(&self.opts.device_id),
                query_value(&self.opts.device_name)
            ),
            RequestKind::RecMode => "/cam.cgi?mode=camcmd&value=recmode".to_string(),
            RequestKind::StartStream => format!("/cam.cgi?mode=startstream&value={}", self.opts.udp_port),
            RequestKind::Capture => "/cam.cgi?mode=camcmd&value=capture".to_string(),
            RequestKind::StopStream => "/cam.cgi?mode=stopstream".to_string(),
        };
        // The keep-alive would fill the log; the rest is worth seeing.
        if kind != RequestKind::KeepAlive {
            self.note(&format!("> {path}"));
        }
        Request { kind, path }
    }

    fn on_failure(&mut self, kind: RequestKind, e: String, now: u64) {
        self.note(&format!("! {}: {e}", kind_name(kind)));
        self.last_error = Some(e.clone());
        if kind == RequestKind::Capture {
            self.fail_capture(&e);
        }
        if self.phase == Phase::Closed {
            return;
        }
        self.failures += 1;
        if self.phase == Phase::Connecting || self.failures >= self.opts.max_failures {
            self.lose(now);
        }
    }

    fn lose(&mut self, now: u64) {
        self.phase = Phase::Lost;
        self.queue.clear();
        self.retry_at = Some(now + self.backoff_ms);
        self.note(&format!("no answer: trying again in {} s", self.backoff_ms / 1000));
        self.backoff_ms = (self.backoff_ms * 2).min(self.opts.backoff_max_ms);
        self.fail_capture("the camera stopped answering");
    }

    fn on_answer(&mut self, kind: RequestKind, r: &Reply, now: u64) {
        if self.phase == Phase::Closed {
            return;
        }
        if kind != RequestKind::Handshake && kind != RequestKind::Capture && r.wants_handshake() && !self.handshake_tried {
            self.after_handshake = Some(kind);
            self.accept_last_ask = now;
            self.queue.push_front(RequestKind::Handshake);
            return;
        }
        match kind {
            RequestKind::Probe if !r.ok() && self.handshake_tried && (r.wants_handshake() || self.accept_since.is_some()) => {
                self.keep_waiting(now);
            }
            RequestKind::Probe => {
                if self.accept_since.take().is_some() {
                    self.handshaken = true;
                    self.note("the camera accepted this computer");
                }
                self.state = r.fields.clone();
                self.backoff_ms = self.opts.backoff_min_ms;
                self.push(RequestKind::RecMode);
            }
            RequestKind::Handshake => self.on_handshake(r, now),
            RequestKind::RecMode => {
                self.refused(kind, r);
                self.phase = Phase::Connected;
                self.last_keepalive = now;
                if self.live_view {
                    self.push(RequestKind::StartStream);
                }
            }
            RequestKind::StartStream => self.on_start_stream(r, now),
            RequestKind::KeepAlive => {
                if !r.fields.is_empty() {
                    self.state = r.fields.clone();
                }
            }
            RequestKind::Capture => self.on_capture(r),
            RequestKind::StopStream => {}
        }
    }

    fn on_handshake(&mut self, r: &Reply, now: u64) {
        self.handshake_tried = true;
        self.handshaken = r.ok();
        if r.pending() {
            self.keep_waiting(now);
            self.after_handshake = None;
            return;
        }
        if !r.ok() {
            self.last_error = Some(format!("the camera refused the access request ({})", r.result.as_deref().unwrap_or("no result")));
        }
        if let Some(k) = self.after_handshake.take() {
            self.queue.push_front(k);
        }
    }

    fn on_start_stream(&mut self, r: &Reply, now: u64) {
        if !self.live_view || self.phase == Phase::Connecting {
            return;
        }
        if r.ok() {
            if self.phase != Phase::Streaming {
                self.stream_since = now;
                self.last_frame = None;
            }
            self.phase = Phase::Streaming;
        } else {
            self.refused(RequestKind::StartStream, r);
        }
    }

    fn on_capture(&mut self, r: &Reply) {
        let outcome = if r.ok() {
            CaptureOutcome::Done
        } else {
            CaptureOutcome::Failed(format!("the camera refused the capture ({})", r.result.as_deref().unwrap_or("no result")))
        };
        if let Some((_, o)) = self.capture.as_mut() {
            *o = outcome;
        }
    }

    /// Note a refusal (`err_*`) as the last error.
    fn refused(&mut self, kind: RequestKind, r: &Reply) {
        if !r.ok() {
            self.last_error = Some(format!("{}: the camera answered {}", kind_name(kind), r.result.as_deref().unwrap_or("no result")));
        }
    }

    fn fail_capture(&mut self, why: &str) {
        if let Some((_, o)) = self.capture.as_mut()
            && *o == CaptureOutcome::Pending
        {
            *o = CaptureOutcome::Failed(why.into());
        }
    }

    fn fps(&self, now: u64) -> u32 {
        let n = self.frame_times.iter().filter(|t| now.saturating_sub(**t) <= 1000).count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    fn no_frames(&self, now: u64) -> bool {
        self.phase == Phase::Streaming && now.saturating_sub(self.last_frame.unwrap_or(self.stream_since)) >= self.opts.frame_timeout_ms
    }

    fn note(&mut self, line: &str) {
        let line: String = line.chars().take(LOG_LINE_CHARS).collect();
        self.log.push_back(line);
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }
}

fn kind_name(k: RequestKind) -> &'static str {
    match k {
        RequestKind::Probe => "getstate",
        RequestKind::Handshake => "accctrl",
        RequestKind::RecMode => "recmode",
        RequestKind::StartStream => "startstream",
        RequestKind::KeepAlive => "keep-alive",
        RequestKind::Capture => "capture",
        RequestKind::StopStream => "stopstream",
    }
}

/// `s` for a query string: letters, digits and `-._~` kept, everything else percent-encoded.
pub(crate) fn query_value(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
