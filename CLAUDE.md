# Lumix Stand — instructions for agents

A native Rust desktop app: live view and capture from a Panasonic Lumix (DMC-GX85) used as a fixed
animation-stand camera. Moved out of Picabia (`../vectorcraft/picabia-vector`, PIC34) on 2026-11-07.

## Layout
- `crates/lumix` (`lumix-link`): the camera protocol, with no dependencies. A sans-IO `Controller`
  (handshake and the wait for the camera to accept a new computer, record mode, live view,
  keep-alive, reconnect with backoff, capture). Also `frame` (the JPEG in a datagram), `reply`
  (`cam.cgi` XML and the plain-text `accctrl` answer), `menu` (every setting, from the camera's
  `allmenu`, and `curmenu`'s enable flags), `apex` (shutter and aperture as `n/256`, the lens's
  ranges), `photos` (DLNA listing, the next photo's number), `xml` (a small tag reader), `sim` (a
  simulated camera behind the same `Transport`, settings and card included), `ssdp`, `net`
  (std::net: HTTP GET and SOAP POST, streaming chunked downloads, the UDP socket, discovery), and
  `examples/probe/main.rs`. The `Controller` also queues setting and focus `Command`s between the
  periodic requests and lists the card at connect.
- `apps/lumix-stand`: the eframe app. `worker` (the link's thread, frame decoding, and a second
  thread downloading full-size originals, so live view keeps going), `frames`
  (numbered `<prefix>_<NNNN>.jpg`, never overwriting), `settings` (JSON in the OS config folder,
  presets included), `presets`, `review` (histogram, clipping, reading a full-size photo),
  `exposure` (the exposure worked out from a test shot: base ISO, f/5.6, the shutter that puts the
  brightest channel's 99.5th percentile at 240), `claude` (asking Claude about a test shot over
  raw HTTPS: the camera's allowed values as JSON-schema enums, the key from 1Password's `op read`
  or `ANTHROPIC_API_KEY`; tests never touch the network), `app`
  (`Stand`: state and actions, tested headless against `sim`; `StandApp`: the window;
  `app/shots.rs`: test shots, the last photo, presets, card downloads; `app/assist.rs`: Find the
  best exposure and Ask Claude; `app/panel.rs`: the side
  panel; `app/compare.rs`: live view, the comparison and the test-shot strip).
  The worker has three threads besides the window: the link, full-size downloads, and test-shot
  previews (`DS…`, 640×360, made from the photo, so they show the real exposure).
  `cargo run -p lumix-stand -- --connect sim` tries it without a camera; `--settings FILE`,
  `--capture-at SECS`, `--test-at SECS` and `--screenshot FILE.png` are for checking the window.
  `cargo run -p lumix-stand --example bundle -- --install` builds `Lumix Stand.app` (icon drawn in
  `examples/bundle.rs`, `Info.plist` with the Local Network usage text) and copies it to /Applications.
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
0. Check on the camera (not measured yet): a test shot's preview (`DS…`) arriving while a full-size
   download runs on a second connection; how much faster a download is with live view stopped
   (Fast downloads) than the measured 18 s with it on (10 s was in playback mode); whether the
   file server pauses (the stall limit is now 20 s) while a new photo is written.
1. Live view that shows the exposure: turn on Constant Preview on the camera (M mode) and measure
   again (docs/gx85.md ▸ Live view and the settings).
2. Optional: send each capture to a running Picabia through its control port (`file.place` with
   `link: true`).
3. Later: Via Network mode; white balance by colour temperature (`color_temp`'s value form).
