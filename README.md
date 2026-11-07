# Lumix Stand

A macOS desktop app that turns a Panasonic Lumix camera into a fixed animation-stand camera: live
view on screen, the camera's settings at hand, and each capture saved as the next numbered frame in
a folder, at the camera's full resolution. It talks to the camera over the camera's own Wi-Fi
remote control (HTTP commands, UDP live view, the card over DLNA), with no Panasonic software.

Written in Rust. Tested with a **DMC-GX85** (GX80/GX7 Mark II, firmware 2.91). Other Lumix models
with Wi-Fi remote shooting may work, but haven't been tried.

## What it does

- **Live view** from the camera at up to 27 fps.
- **Capture** with the button, Space or Enter. Frames go to a folder you choose as
  `frame_0001.jpg`, `frame_0002.jpg`, … (prefix of your choice). An existing file is never replaced.
- **Full-size frames** downloaded from the camera's card in the background (about 10–20 s each over
  Wi-Fi) while you keep shooting. Each capture keeps its own number. If a download fails, the
  live-view frame is saved under that number, so the sequence has no gaps. Turn full size off to
  save the 640×360 live-view frame at once.
- **Camera settings** from the app: shutter speed, aperture, ISO, exposure compensation, white
  balance, metering, focus mode, AF area, peaking, aspect ratio, picture size, quality, photo style,
  filter, drive mode, flash and live-view size. Choices the camera won't take right now are greyed
  out. Changes made on the camera itself are read back every 30 s.
- **Largest size**, one button: 4:3 at 16M, the whole sensor (4592×3448 on the GX85).
- **Focus from the app** in MF: small and large focus steps, and autofocus once.
- **A simulated camera** (`sim`) to try everything without one.

## Install (macOS)

You need [Rust](https://rustup.rs) (1.95 or later).

```sh
git clone https://github.com/profdl/lumix-stand
cd lumix-stand
cargo run -p lumix-stand --example bundle -- --install
```

This builds `Lumix Stand.app` and copies it to `/Applications`. Without `--install` it stays in
`target/bundle/`. To try it straight from the source instead:

```sh
cargo run -p lumix-stand -- --connect sim      # the simulated camera
```

## Connecting to the camera

1. On the camera: **Wi-Fi ▸ Remote Shooting & View**, and choose a **Direct** connection. The camera
   starts its own Wi-Fi network.
2. Join that network from the Mac. The camera is at `192.168.54.1`.
3. In Lumix Stand, press **Connect**. The first time, the camera asks whether to accept this
   computer: accept it on the camera.
4. macOS asks whether Lumix Stand may find devices on the local network: allow it.

The Mac has no internet on the camera's network. To keep the internet, connect it another way too
(Ethernet, or an iPhone over USB) and put that connection above Wi-Fi in the network service
order. Turn off Auto-Join for your usual Wi-Fi network, or the Mac drifts back to it and loses the
camera. [`docs/gx85.md`](docs/gx85.md) has the details.

## Good to know

- **Live view shows white balance and style, not exposure.** On the GX85, live view stays at the
  same brightness whatever the shutter speed or ISO, because the camera brightens its preview. The
  camera's **Constant Preview** option (M mode) is meant to change that. It can't be set over Wi-Fi,
  so turn it on on the camera itself.
- **RAW files can't be downloaded** over Wi-Fi: with RAW + JPEG, the JPEG is saved. With RAW only,
  there's nothing to download.
- **Burst and 4K Photo drive modes** give one press several photos, so full-size downloads may
  pick the wrong one. Use single shots.
- **Exposure compensation is refused in M mode**, as on the camera itself.
- **Only the camera's own Wi-Fi (Direct) works.** On a shared network ("Via Network") the camera
  waits for the phone app and opens no ports.

## The camera protocol

Panasonic doesn't document it. Everything this app relies on was measured on a real GX85 and is
written up in [`docs/gx85.md`](docs/gx85.md): the access handshake, live-view datagrams, the
keep-alive, every setting and its values, focus steps, and listing and downloading the card. The
protocol code is a separate crate with no dependencies, [`crates/lumix`](crates/lumix)
(`lumix-link`), which also has a probe for exploring another camera:

```sh
cargo run -p lumix-link --example probe -- --settings
```

## Development

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

The tests run against the simulated camera, never the network. The rules the code follows (never
crash on what the camera sends, no `unwrap` outside tests, no unsafe code) are in
[`CLAUDE.md`](CLAUDE.md).

## Licence

[MIT No Attribution](LICENSE) (MIT-0): use, copy, change and sell it for anything, with no need to
credit anyone or keep a notice. The libraries it builds on (egui, image and others) keep their own
licences.

The camera crate was first written for [Picabia](https://github.com/profdl/picabia-vector) and moved
here. Nothing of Panasonic's (software, firmware or artwork) is included.
