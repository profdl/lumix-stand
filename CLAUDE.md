# Lumix Stand — instructions for agents

A native Rust desktop app: live view and capture from a Panasonic Lumix (DMC-GX85) used as a fixed
animation-stand camera. Moved out of Picabia (`../vectorcraft/picabia-vector`, PIC34) on 2026-11-07.

## Layout
- `crates/lumix` (`lumix-link`): the camera protocol, with no dependencies. A sans-IO `Controller`
  (handshake and the wait for the camera to accept a new computer, record mode, live view,
  keep-alive, reconnect with backoff, capture). Also `frame` (the JPEG in a datagram), `reply`
  (`cam.cgi` XML and the plain-text `accctrl` answer), `sim` (a simulated camera behind the same
  `Transport`), `ssdp`, `net` (std::net: HTTP GET, the UDP socket, discovery), and
  `examples/probe.rs`.
- `apps/lumix-stand`: the eframe app. `worker` (the link's thread, frame decoding), `frames`
  (numbered `<prefix>_<NNNN>.jpg`, never overwriting), `settings` (JSON in the OS config folder),
  `app` (`Stand`: state and actions, tested headless against `sim`; `StandApp`: the window).
  `cargo run -p lumix-stand -- --connect sim` tries it without a camera; `--settings FILE`,
  `--capture-at SECS` and `--screenshot FILE.png` are for checking the window.
- `docs/gx85.md`: the measured protocol, and the network setup that works.

## Rules
- **Rust only.**
- **Never crash.** Workspace clippy lints deny `unwrap`, `expect`, `panic!`, `unreachable!`,
  `todo!` and `unimplemented!` outside tests. Camera replies and datagrams are untrusted: cap sizes,
  use `.get()`, return `Result`. Every crate has `[lints] workspace = true` and
  `#![forbid(unsafe_code)]`.
- **Test against the simulator**, never the network. Tests use `sim::SimCamera` with a clock the
  test moves. Real hardware is checked by hand, and the results are written in `docs/gx85.md`.
- **Don't commit captured images or probe output.** `.gitignore` covers `*.jpg` and `probe.log`.
- Gate before every commit: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`.

## Talking to the real camera (read before trying)
- The camera runs its own Wi-Fi ("Direct", camera at 192.168.54.1). The Mac gets internet from an
  iPhone over USB, which is above Wi-Fi in the service order, with Auto-Join off on the studio
  network. Without that, the Mac drops back to the studio network and the session loses the
  camera.
- **macOS Local Network permission**: programs started from VS Code count as VS Code, which must
  be allowed under Privacy & Security ▸ Local Network. `curl` is exempt, so it's no test, and with
  the iPhone hotspot use `curl -4`. Network tests from Claude's shell need the sandbox off.
- The camera knows this computer by the handshake id and name (currently
  `4D454930-0100-1000-8000-PICABIA00001` / "Picabia" in the probe and the defaults). Changing them
  means accepting the computer on the camera once more.
- "Via Network" (camera on a shared Wi-Fi) doesn't work yet: the camera searches with SSDP for a
  `MediaRenderer` (the phone app) and opens no ports until something answers. See `docs/gx85.md`.

## Next steps
1. Check the app with the real camera.
2. Optional: send each capture to a running Picabia through its control port (`file.place` with
   `link: true`).
3. Later: full-resolution download over DLNA (port 60606), lock focus and exposure (`setsetting`),
   and Via Network mode.
