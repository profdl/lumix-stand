# Lumix Stand

Live view and capture for a Panasonic Lumix camera (first target: the DMC-GX85) used as a fixed
animation-stand camera. A native Rust desktop app. It talks to the camera over its Wi-Fi remote
control: HTTP commands, UDP live view.

**Status:** setup only. The camera link (`crates/lumix`, package `lumix-link`) works with a real
GX85: live view at 27 fps, shutter, handshake. The app (`apps/lumix-stand`) is a placeholder.

```
cargo run -p lumix-stand                          # placeholder: streams from the simulated camera
cargo run -p lumix-link --example probe           # probe a real camera (see docs/gx85.md)
cargo test
```

Connecting to a real camera, and the macOS settings that matter: [`docs/gx85.md`](docs/gx85.md) ›
The setup that works.

Licence: MIT OR Apache-2.0.
