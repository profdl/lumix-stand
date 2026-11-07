//! Where requests and datagrams go: the real network ([`crate::net`]) or the simulator
//! ([`crate::sim`]).

use crate::Controller;

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
}

/// One HTTP GET to the camera.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub kind: RequestKind,
    /// The path and query, e.g. `/cam.cgi?mode=getstate`.
    pub path: String,
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
    /// The next live-view datagram into `buf`, waiting at most `wait_ms`; `Ok(None)` when none
    /// came. Datagrams from anyone but the camera are dropped by the transport.
    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Result<Option<usize>, String>;
    /// The UDP port live view comes in on.
    fn udp_port(&self) -> u16;
}

/// One turn: send the controller's next request (if any) and hand it the answer, then hand it the
/// datagrams that came, waiting at most `wait_ms` for the first. `now` gives the time (ms).
pub fn pump(ctl: &mut Controller, t: &mut dyn Transport, buf: &mut [u8], wait_ms: u64, now: &dyn Fn() -> u64) {
    if let Some(req) = ctl.poll(now()) {
        let reply = t.get(&req.path);
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
