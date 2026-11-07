//! Build `Lumix Stand.app` (macOS): the release binary, an icon drawn here, an `Info.plist`, an
//! ad-hoc signature.
//!
//! ```text
//! cargo run -p lumix-stand --example bundle [-- --install]
//! ```
//! The app lands in `target/bundle/Lumix Stand.app`; `--install` also copies it to
//! `/Applications` (replacing an older copy). Uses the system's `sips`, `iconutil` and `codesign`.
//!
//! The `Info.plist` carries `NSLocalNetworkUsageDescription`, so macOS asks once whether Lumix
//! Stand may reach the camera on the local network (System Settings ▸ Privacy & Security ▸ Local
//! Network), and the app has its own entry there instead of borrowing the terminal's.

use std::path::{Path, PathBuf};
use std::process::Command;

const APP_NAME: &str = "Lumix Stand";
const EXECUTABLE: &str = "lumix-stand";
const BUNDLE_ID: &str = "io.github.profdl.lumix-stand";
/// The icon's side (px); the smaller sizes are made from it.
const ICON: u32 = 1024;

fn main() -> Result<(), String> {
    let install = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--install") => true,
        Some(other) => return Err(format!("unknown argument {other} (only --install)")),
    };
    let root = workspace_root()?;
    run(Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--release", "-p", "lumix-stand"])
        .current_dir(&root))?;

    let out = root.join("target").join("bundle");
    let app = out.join(format!("{APP_NAME}.app"));
    if app.exists() {
        std::fs::remove_dir_all(&app).map_err(|e| format!("can't remove the old {}: {e}", app.display()))?;
    }
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    for d in [&macos, &resources] {
        std::fs::create_dir_all(d).map_err(|e| format!("can't make {}: {e}", d.display()))?;
    }
    let bin = root.join("target").join("release").join(EXECUTABLE);
    std::fs::copy(&bin, macos.join(EXECUTABLE)).map_err(|e| format!("can't copy {}: {e}", bin.display()))?;
    std::fs::write(contents.join("Info.plist"), info_plist()).map_err(|e| format!("can't write Info.plist: {e}"))?;
    make_icns(&out, &resources.join("AppIcon.icns"))?;
    run(Command::new("codesign").args(["--force", "--deep", "--sign", "-"]).arg(&app))?;
    println!("built {}", app.display());

    if install {
        let dest = Path::new("/Applications").join(format!("{APP_NAME}.app"));
        if dest.exists() {
            std::fs::remove_dir_all(&dest).map_err(|e| format!("can't replace {}: {e}", dest.display()))?;
        }
        run(Command::new("ditto").arg(&app).arg(&dest))?;
        // Tell Finder and the Dock about the new icon.
        let _ = Command::new("touch").arg(&dest).status(); // only a refresh hint
        println!("installed {}", dest.display());
    }
    Ok(())
}

fn workspace_root() -> Result<PathBuf, String> {
    // This file is apps/lumix-stand/examples/bundle.rs; the workspace is two folders above the package.
    let pkg = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    pkg.parent().and_then(Path::parent).map(Path::to_path_buf).ok_or_else(|| "can't find the workspace".into())
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let status = cmd.status().map_err(|e| format!("can't run {:?}: {e}", cmd.get_program()))?;
    if status.success() { Ok(()) } else { Err(format!("{:?} failed ({status})", cmd.get_program())) }
}

fn info_plist() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>{APP_NAME}</string>
    <key>CFBundleDisplayName</key><string>{APP_NAME}</string>
    <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
    <key>CFBundleExecutable</key><string>{EXECUTABLE}</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.photography</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSLocalNetworkUsageDescription</key><string>Lumix Stand talks to your Lumix camera over its Wi-Fi for live view and capture.</string>
</dict>
</plist>
"#
    )
}

/// Draw the icon, make every size macOS wants, and pack them into `.icns`.
fn make_icns(work: &Path, icns: &Path) -> Result<(), String> {
    let png = work.join("AppIcon-1024.png");
    let rgba = draw_icon(ICON);
    image::save_buffer(&png, &rgba, ICON, ICON, image::ExtendedColorType::Rgba8).map_err(|e| format!("can't write {}: {e}", png.display()))?;
    let set = work.join("AppIcon.iconset");
    let _ = std::fs::remove_dir_all(&set); // a fresh set each time
    std::fs::create_dir_all(&set).map_err(|e| format!("can't make {}: {e}", set.display()))?;
    for size in [16u32, 32, 128, 256, 512] {
        for (scale, suffix) in [(1, ""), (2, "@2x")] {
            let px = size * scale;
            let file = set.join(format!("icon_{size}x{size}{suffix}.png"));
            run(Command::new("sips")
                .args(["-z", &px.to_string(), &px.to_string()])
                .arg(&png)
                .arg("--out")
                .arg(&file)
                .stdout(std::process::Stdio::null()))?;
        }
    }
    run(Command::new("iconutil").args(["-c", "icns"]).arg(&set).arg("-o").arg(icns))
}

// ---------- the icon ----------

type Rgba = [f32; 4];

/// A Lumix-style camera hanging from a copy-stand arm, on a dark tile, with a red shutter button.
fn draw_icon(side: u32) -> Vec<u8> {
    let s = side as f32 / 1024.0;
    let mut px = vec![0u8; (side * side * 4) as usize];
    for y in 0..side {
        for x in 0..side {
            let (fx, fy) = ((x as f32 + 0.5) / s, (y as f32 + 0.5) / s);
            let c = shade(fx, fy, 1.0 / s);
            let i = ((y * side + x) * 4) as usize;
            if let Some(p) = px.get_mut(i..i + 4) {
                p.copy_from_slice(&c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8));
            }
        }
    }
    px
}

/// The colour at (x, y) in 1024-unit space; `aa` is one pixel in those units.
fn shade(x: f32, y: f32, aa: f32) -> Rgba {
    let mut c: Rgba = [0.0; 4];
    // The tile (macOS icon grid: 824 units, corner 185), a dark vertical gradient.
    let t = (y - 100.0) / 824.0;
    let tile = mix([0.22, 0.23, 0.26, 1.0], [0.09, 0.09, 0.11, 1.0], t);
    over(&mut c, tile, cover(rrect(x, y, 512.0, 512.0, 412.0, 412.0, 185.0), aa));
    // The copy stand: a column at the back and its arm.
    over(&mut c, [0.55, 0.56, 0.60, 1.0], cover(rrect(x, y, 512.0, 250.0, 22.0, 115.0, 8.0), aa));
    over(&mut c, [0.45, 0.46, 0.50, 1.0], cover(rrect(x, y, 512.0, 358.0, 70.0, 26.0, 10.0), aa));
    // Paper on the stand's baseboard.
    over(&mut c, [0.93, 0.90, 0.82, 1.0], cover(rrect(x, y, 512.0, 805.0, 250.0, 28.0, 10.0), aa));
    // The camera body, its top plate and grip.
    let body = mix([0.86, 0.87, 0.90, 1.0], [0.66, 0.67, 0.71, 1.0], (y - 380.0) / 330.0);
    over(&mut c, body, cover(rrect(x, y, 512.0, 545.0, 270.0, 165.0, 46.0), aa));
    over(&mut c, [0.17, 0.17, 0.19, 1.0], cover(rrect(x, y, 512.0, 470.0, 270.0, 18.0, 4.0), aa));
    over(&mut c, [0.20, 0.20, 0.22, 1.0], cover(rrect(x, y, 300.0, 560.0, 55.0, 145.0, 30.0), aa));
    // The lens: barrel, glass, reflection.
    over(&mut c, [0.12, 0.12, 0.14, 1.0], cover(circle(x, y, 540.0, 560.0, 148.0), aa));
    over(&mut c, [0.30, 0.31, 0.34, 1.0], cover(circle(x, y, 540.0, 560.0, 122.0), aa));
    let glass = mix([0.16, 0.25, 0.48, 1.0], [0.05, 0.06, 0.12, 1.0], ((x - 440.0) + (y - 460.0)) / 280.0);
    over(&mut c, glass, cover(circle(x, y, 540.0, 560.0, 100.0), aa));
    over(&mut c, [1.0, 1.0, 1.0, 0.55], cover(circle(x, y, 500.0, 520.0, 26.0), aa));
    // The red shutter button.
    over(&mut c, [0.86, 0.22, 0.16, 1.0], cover(rrect(x, y, 700.0, 362.0, 40.0, 16.0, 8.0), aa));
    c
}

/// Signed distance to a rounded rectangle centred at (cx, cy) with half-sizes (hw, hh).
#[allow(clippy::too_many_arguments)]
fn rrect(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let (qx, qy) = ((x - cx).abs() - hw + r, (y - cy).abs() - hh + r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

fn circle(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> f32 {
    ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r
}

/// How much of the pixel a shape covers, from its signed distance.
fn cover(d: f32, aa: f32) -> f32 {
    (0.5 - d / aa).clamp(0.0, 1.0)
}

fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    [0, 1, 2, 3].map(|i| a.get(i).copied().unwrap_or(0.0) * (1.0 - t) + b.get(i).copied().unwrap_or(0.0) * t)
}

/// Paint `src` over `dst` with coverage `k` (straight alpha).
fn over(dst: &mut Rgba, src: Rgba, k: f32) {
    let a = src[3] * k;
    if a <= 0.0 {
        return;
    }
    let out_a = a + dst[3] * (1.0 - a);
    for i in 0..3 {
        dst[i] = if out_a > 0.0 { (src[i] * a + dst[i] * dst[3] * (1.0 - a)) / out_a } else { 0.0 };
    }
    dst[3] = out_a;
}
