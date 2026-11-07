//! Lumix Stand: live view and capture for a Panasonic Lumix used as a fixed animation-stand camera.
//!
//! ```text
//! lumix-stand [--connect ADDRESS] [--settings FILE] [--capture-at SECS] [--screenshot FILE.png [--screenshot-at SECS]]
//! ```
//! `--connect sim` connects to the simulated camera at start. `--settings` uses another settings
//! file. `--capture-at` presses Capture once, and `--screenshot` saves the window as a PNG (at 3 s
//! unless `--screenshot-at`) and quits: both are for checking the window.
#![forbid(unsafe_code)]

mod app;
mod frames;
mod settings;
mod worker;

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;

const USAGE: &str = "usage: lumix-stand [--connect ADDRESS] [--settings FILE] [--capture-at SECS] [--screenshot FILE.png [--screenshot-at SECS]]";

#[derive(Debug, Default)]
struct Args {
    connect: Option<String>,
    settings: Option<PathBuf>,
    capture_at: Option<f64>,
    screenshot: Option<PathBuf>,
    screenshot_at: Option<f64>,
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args::default();
    let secs = |v: Option<String>, flag: &str| -> Result<f64, String> {
        v.and_then(|s| s.parse::<f64>().ok()).filter(|s| s.is_finite() && *s >= 0.0).ok_or_else(|| format!("{flag} needs a number of seconds"))
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--connect" => a.connect = Some(it.next().ok_or("--connect needs an address")?),
            "--settings" => a.settings = Some(it.next().ok_or("--settings needs a file")?.into()),
            "--capture-at" => a.capture_at = Some(secs(it.next(), "--capture-at")?),
            "--screenshot" => a.screenshot = Some(it.next().ok_or("--screenshot needs a file")?.into()),
            "--screenshot-at" => a.screenshot_at = Some(secs(it.next(), "--screenshot-at")?),
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn main() -> std::process::ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("lumix-stand: {e}");
            }
            eprintln!("{USAGE}");
            return std::process::ExitCode::from(2);
        }
    };
    let settings_path = args.settings.clone().or_else(settings::default_path);
    let (loaded, problem) = settings_path.as_deref().map(settings::Settings::load).unwrap_or_default();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("Lumix Stand").with_inner_size([1120.0, 640.0]).with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    let script = app::Script::new(args.capture_at, args.screenshot.map(|p| (p, args.screenshot_at.unwrap_or(3.0))));
    let r = eframe::run_native(
        "Lumix Stand",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            let ctx = cc.egui_ctx.clone();
            let picker: app::Picker = Box::new(|| rfd::FileDialog::new().set_title("Choose the capture folder").pick_folder());
            let mut stand = app::Stand::new(loaded, settings_path, picker, Arc::new(move || ctx.request_repaint()));
            stand.last = problem.map(|p| (false, p));
            if let Some(address) = args.connect {
                stand.address = address;
                stand.connect();
            }
            Ok(Box::new(app::StandApp::new(stand, script)))
        }),
    );
    match r {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lumix-stand: can't open the window: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Result<Args, String> {
        parse_args(s.split_whitespace().map(String::from))
    }

    #[test]
    fn arguments() {
        let a = args("--connect sim --screenshot /tmp/x.png --screenshot-at 4 --capture-at 2").unwrap();
        assert_eq!(a.connect.as_deref(), Some("sim"));
        assert_eq!((a.capture_at, a.screenshot_at), (Some(2.0), Some(4.0)));
        assert!(args("--connect").is_err());
        assert!(args("--capture-at soon").is_err());
        assert!(args("--wat").is_err());
    }
}
