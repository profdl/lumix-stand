//! Where requests and datagrams go: the real network ([`crate::net`]) or the simulator
//! ([`crate::sim`]).

use crate::Controller;
use crate::photos::PhotoId;

/// What a request is for (the controller matches the reply to it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    /// `getstate`, when connecting.
    Probe,
    /// `accctrl`: the access handshake.
    Handshake,
    /// `camcmd recmode`.
    RecMode,
    /// `startstream`.
    StartStream,
    /// `getstate`, as the keep-alive.
    KeepAlive,
    /// `camcmd capture`.
    Capture,
    /// `stopstream`.
    StopStream,
    /// A setting, the lens, the menu or the focus ([`crate::Command`]).
    Command,
    /// `camcmd playmode`, to list the card.
    PlayMode,
    /// A DLNA `Browse` for the number of photos on the card.
    BrowseCount,
    /// A DLNA `Browse` for the newest photo.
    BrowseNewest,
}

/// A SOAP request (an HTTP POST) to another port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Post {
    pub port: u16,
    /// The `SOAPACTION` header (quoted).
    pub action: String,
    pub body: String,
}

/// One request to the camera: an HTTP GET to `cam.cgi`, or a POST when `post` is set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub kind: RequestKind,
    /// The path and query, e.g. `/cam.cgi?mode=getstate`.
    pub path: String,
    pub post: Option<Post>,
}

/// The camera's answer to a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// A camera link: HTTP GETs out, live-view datagrams in.
pub trait Transport {
    /// Send `path` (path and query) and wait for the answer. An error: no answer (refused, timed
    /// out, unreachable), with a plain reason.
    fn get(&mut self, path: &str) -> Result<HttpReply, String>;
    /// POST `body` to `path` on `port` with a `SOAPACTION` header, and wait for the answer.
    fn post(&mut self, port: u16, path: &str, action: &str, body: &str) -> Result<HttpReply, String>;
    /// The next live-view datagram into `buf`, waiting at most `wait_ms`; `Ok(None)` when none
    /// came. Datagrams from anyone but the camera are dropped by the transport.
    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Result<Option<usize>, String>;
    /// The UDP port live view comes in on.
    fn udp_port(&self) -> u16;
}

/// Downloads photos from the card at full size, apart from the control link (a download takes
/// many seconds, and the link must keep live view going meanwhile).
pub trait Originals: Send {
    /// Write photo `id`'s original into `out`, telling `progress` (bytes so far, of how many when
    /// known). `Ok(None)`: the card has no such photo (yet); `Ok(Some(bytes))`: done.
    fn original(&mut self, id: PhotoId, out: &mut dyn std::io::Write, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<Option<u64>, String>;
    /// The same for photo `id`'s 640×360 preview (made from the photo, so it shows its exposure).
    fn preview(&mut self, id: PhotoId, out: &mut dyn std::io::Write, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<Option<u64>, String>;
}

/// One turn: send the controller's next request (if any) and hand it the answer, then hand it the
/// datagrams that came, waiting at most `wait_ms` for the first. `now` gives the time (ms).
pub fn pump(ctl: &mut Controller, t: &mut dyn Transport, buf: &mut [u8], wait_ms: u64, now: &dyn Fn() -> u64) {
    if let Some(req) = ctl.poll(now()) {
        let reply = match &req.post {
            Some(p) => t.post(p.port, &req.path, &p.action, &p.body),
            None => t.get(&req.path),
        };
        ctl.on_reply(req.kind, reply, now());
    }
    let mut wait = wait_ms;
    // At most a burst's worth per turn, so requests (a capture, the keep-alive) don't wait long.
    for _ in 0..16 {
        match t.recv(buf, wait) {
            Ok(Some(n)) => ctl.on_datagram(buf.get(..n).unwrap_or_default(), now()),
            Ok(None) => break,
            Err(e) => {
                ctl.on_udp_error(&e, now());
                break;
            }
        }
        wait = 0;
    }
}
