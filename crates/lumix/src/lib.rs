//! Lumix camera link (no dependencies): drive a Panasonic Lumix
//! camera (first target: the DMC-GX85) over its Wi-Fi remote control, as a fixed animation-stand
//! camera. See `docs/gx85.md`.
//!
//! The protocol is not documented by Panasonic. What this crate does comes from public community
//! notes and is checked against a real camera with the probe (`examples/probe/main.rs`);
//! everything that is still a guess is marked *hypothesis* where it is used.
//!
//! # Parts
//! - [`frame`]: the JPEG in a live-view datagram (the datagrams are untrusted: sizes capped, nothing
//!   indexes unchecked).
//! - [`reply`]: a `cam.cgi` XML reply read leniently ([`Reply`]).
//! - [`Controller`]: the protocol as a sans-IO state machine: connect (with the access handshake
//!   when the camera asks for one), record mode, live view, keep-alive, reconnect with backoff,
//!   capture, stop. Replies in with [`Controller::on_reply`], datagrams with
//!   [`Controller::on_datagram`], requests out of [`Controller::poll`], state out of
//!   [`Controller::snapshot`]. Time comes in as `now_ms`, so it runs the same natively, in the
//!   browser build and in tests.
//! - [`Transport`]: where requests and datagrams go ([`pump`] moves them between a transport and a
//!   controller).
//! - [`sim`]: a simulated camera behind the same [`Transport`].
//! - [`ssdp`]: finding cameras on a network (the M-SEARCH request and reading the answers).
//! - `net` (native only): the real transport over `std::net`, and SSDP discovery.
#![forbid(unsafe_code)]

mod controller;
pub mod frame;
#[cfg(not(target_arch = "wasm32"))]
pub mod net;
pub mod reply;
pub mod sim;
pub mod ssdp;
mod transport;

#[cfg(test)]
mod controller_tests;

pub use controller::{CaptureOutcome, Controller, Options, Phase, Snapshot};
pub use frame::{Frame, FrameError, extract_jpeg};
pub use reply::Reply;
pub use transport::{HttpReply, Request, RequestKind, Transport, pump};

/// The camera's address on its own Wi-Fi network.
pub const DEFAULT_IP: &str = "192.168.54.1";
/// The UDP port live view is asked to go to (*hypothesis*: any free port works; community notes use
/// 49199).
pub const DEFAULT_UDP_PORT: u16 = 49199;
