//! Lumix Stand: live view and capture for a Panasonic Lumix used as a fixed animation-stand camera.
//!
//! Placeholder: the app is built in the next step (see CLAUDE.md › Next steps). For now it checks the
//! link crate against the simulated camera, so the workspace builds and runs end to end.
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lumix_link::sim::{SimCamera, SimOptions};
use lumix_link::{Controller, Options, pump};

fn main() {
    let clock = Arc::new(AtomicU64::new(0));
    let c = clock.clone();
    let mut cam = SimCamera::new(SimOptions { clock: Arc::new(move || c.load(Ordering::SeqCst)), ..Default::default() });
    let mut ctl = Controller::new(Options::default());
    let mut buf = vec![0u8; lumix_link::frame::MAX_DATAGRAM];
    ctl.connect(0);
    for _ in 0..200 {
        let c = clock.clone();
        pump(&mut ctl, &mut cam, &mut buf, 0, &move || c.load(Ordering::SeqCst));
        clock.fetch_add(10, Ordering::SeqCst);
    }
    let s = ctl.snapshot(clock.load(Ordering::SeqCst));
    println!("simulated camera: {} after 2 s, {} frames", s.phase.as_str(), s.frame_seq);
}
