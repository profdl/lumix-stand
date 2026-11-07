//! Finding the exposure from a test shot, by arithmetic: no guessing and no network.
//!
//! On a copy stand nothing moves, so the best photo comes from
//! - **the least grain**: ISO at the GX85's base, 200 (L.100 is an extended setting that gives up
//!   highlight range);
//! - **the sharpest picture**: the aperture at f/5.6, where the kit zoom is sharpest, short of the
//!   diffraction that softens it from about f/11 (clamped to what the lens allows at its zoom);
//! - **the most light without clipping**: the shutter set so the brightest tones (the 99.5th
//!   percentile of each pixel's brightest channel, so a few specular glints don't count, and a
//!   saturated colour counts by the channel that clips first) land at [`TARGET_HIGH`], just under
//!   white.
//!
//! The preview is a gamma-encoded JPEG, so brightness is turned into light (gamma 2.2) before
//! working out how many stops to move. A clipped shot hides how far over it is, so it steps down
//! by at least a stop and looks again.

use lumix_link::{CameraSettings, apex};

use crate::review::Look;

/// Where the brightest tones should land (0-255).
pub const TARGET_HIGH: f32 = 240.0;
/// The share of pixels that may be brighter than that (glints, small highlights).
const HIGH_SHARE: f32 = 0.995;
/// A shot is right when it is within this many stops of the target (the shutter moves in thirds).
pub const TOLERANCE: f32 = 0.2;
/// Closer than the camera's third stops can get.
pub const THIRD: f32 = 0.34;
/// More blown pixels than this is clipping.
pub const CLIPPED: f32 = 0.002;
/// The base ISO.
pub const BASE_ISO: &str = "200";
/// The sharpest aperture: f/5.6 (Av 5).
pub const SHARP_AV: i32 = 5 * 256;

fn linear(v: f32) -> f32 {
    (v.clamp(1.0, 255.0) / 255.0).powf(2.2)
}

/// How many stops brighter (positive) or darker the next shot should be.
pub fn correction(look: &Look) -> f32 {
    if look.high > CLIPPED {
        // How far over can't be read off a clipped shot: at least a stop, more the more is lost.
        return -(1.0 + (look.high * 8.0).min(2.0));
    }
    let high = f32::from(look.peak_percentile(HIGH_SHARE));
    (linear(TARGET_HIGH) / linear(high)).log2().clamp(-5.0, 5.0)
}

/// The shot is well exposed.
pub fn right(look: &Look) -> bool {
    look.high <= CLIPPED && correction(look).abs() <= TOLERANCE
}

/// The settings for the least grain and the sharpest picture, where they differ from now:
/// (setting, value).
pub fn base_settings(cs: &CameraSettings) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if cs.value("iso") != Some(BASE_ISO) {
        out.push(("iso".to_string(), BASE_ISO.to_string()));
    }
    let lens = cs.lens.unwrap_or_default();
    let stops = apex::aperture_stops(lens.av_wide, lens.av_narrow);
    if let Some(av) = stops.iter().copied().min_by_key(|a| (a - SHARP_AV).abs()) {
        let now = cs.value("focal").and_then(apex::parse);
        if now.is_none_or(|n| (n - av).abs() > 20) {
            out.push(("focal".to_string(), apex::format(av)));
        }
    }
    out
}

/// The shutter speed `stops` brighter than now, from the lens's third stops, never as bright as
/// `clipped` (the slowest shutter already seen to clip, as Tv × 256): (its value, the stops it
/// actually moves). An error when the camera's shutter speed isn't known.
pub fn shutter_for(cs: &CameraSettings, stops: f32, clipped: Option<i32>) -> Result<(String, f32), String> {
    let now = cs.value("shtrspeed").and_then(apex::parse).ok_or("the camera's shutter speed isn't known yet")?;
    let lens = cs.lens.unwrap_or_default();
    let choices: Vec<i32> = apex::shutter_stops(lens.tv_slow, lens.tv_fast).into_iter().filter(|t| clipped.is_none_or(|c| *t > c + 20)).collect();
    // Brighter is a longer exposure: a smaller Tv.
    let want = now - (stops * 256.0).round() as i32;
    let tv = choices.iter().copied().min_by_key(|t| (t - want).abs()).ok_or("the lens's shutter speeds aren't known yet")?;
    Ok((apex::format(tv), (now - tv) as f32 / 256.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A look whose pixels are all at `level`, with `blown` of them at 255.
    fn look(level: u8, blown: f32) -> Look {
        let n = 10_000u32;
        let mut luma = [0u32; 256];
        let b = (blown * n as f32) as u32;
        luma[usize::from(level)] = n - b;
        luma[255] += b;
        Look { hist: [0; crate::review::BINS], luma, peak: luma, mean_rgb: [0.0; 3], high: blown, low: 0.0, overlay: Arc::new(Vec::new()) }
    }

    fn settings(values: &[(&str, &str)]) -> CameraSettings {
        CameraSettings {
            values: values.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(),
            lens: apex::Lens::parse(lumix_link::sim::SIM_LENS.as_bytes()),
            ..CameraSettings::default()
        }
    }

    #[test]
    fn corrections_in_stops() {
        assert!(correction(&look(240, 0.0)).abs() < 0.01, "on target");
        assert!(right(&look(238, 0.0)));
        // Half the light, in gamma 2.2: 240 × 0.5^(1/2.2) ≈ 175.
        assert!((correction(&look(175, 0.0)) - 1.0).abs() < 0.05);
        assert!(correction(&look(120, 0.0)) > 1.9);
        assert!(correction(&look(200, 0.05)) <= -1.0, "clipped: at least a stop down");
        assert!(!right(&look(240, 0.05)));
    }

    #[test]
    fn base_iso_and_the_sharp_aperture() {
        let cs = settings(&[("iso", "auto"), ("focal", "2304/256")]);
        assert_eq!(base_settings(&cs), [("iso".to_string(), "200".to_string()), ("focal".to_string(), "1280/256".to_string())]);
        let done = settings(&[("iso", "200"), ("focal", "1280/256")]);
        assert!(base_settings(&done).is_empty());
    }

    #[test]
    fn shutter_steps() {
        // 1/60 (Tv 5.9) a stop brighter is about 1/30.
        let cs = settings(&[("shtrspeed", "1536/256")]);
        let (v, moved) = shutter_for(&cs, 1.0, None).unwrap();
        assert!((moved - 1.0).abs() < 0.2, "{v} moved {moved}");
        assert!(apex::parse(&v).unwrap() < 1536);
        let (_, darker) = shutter_for(&cs, -2.0, None).unwrap();
        assert!((darker + 2.0).abs() < 0.2);
        // Clipped a stop brighter already: no brighter than a third short of that.
        let (v, moved) = shutter_for(&cs, 1.0, Some(1536 - 256)).unwrap();
        assert!(apex::parse(&v).unwrap() > 1536 - 256 && moved < 1.0 && moved > 0.5, "{v} {moved}");
        assert!(shutter_for(&settings(&[]), 1.0, None).is_err());
    }
}
