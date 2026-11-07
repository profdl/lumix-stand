//! Shutter speed and aperture as the camera speaks them: APEX values times 256, written `n/256`.
//!
//! Measured on a GX85 (firmware D2.91, 2026-11-07): `getsetting&type=shtrspeed` read `1622/256`
//! (Tv 6⅓, 1/80 s) and `type=focal` read `1280/256` (Av 5, f/5.6). `setsetting` takes the same
//! form for both, stores the value as written (`1621/256` reads back `1621/256`), and clamps the
//! aperture to the lens (`1100/256` read back `1126/256`, the lens wide open at that zoom). Other
//! forms (`1/60`, `5.6`, `F5.6`) are refused or misread. Third stops are written as the camera
//! writes them, `ceil(k × 256 / 3)` (1/80 s: `1622/256`).
//!
//! `getinfo&type=lens` answers in plain text:
//! `ok,2304/256,1126/256,3072/256,-1536/256,0,off,32,12,on,128/1024,on`. Read as (*hypothesis*,
//! consistent with a 12-32 mm f/3.5-5.6 lens on a camera with a 1/4000-60 s mechanical shutter):
//! the narrowest aperture (Av 9, f/22), the widest at the current zoom (Av 4.4, f/4.6), the
//! fastest shutter (Tv 12, 1/4000 s), the slowest (Tv −6, 60 s), then the focal lengths in mm
//! (tele 32, wide 12). The other fields aren't known.

/// One third of a stop, in the camera's units.
const THIRD: f64 = 256.0 / 3.0;

/// Shutter speeds in third stops, from Tv 12 (1/4000 s) down to Tv −6 (60 s).
const SHUTTER: [&str; 55] = [
    "1/4000", "1/3200", "1/2500", "1/2000", "1/1600", "1/1250", "1/1000", "1/800", "1/640", "1/500", "1/400", "1/320", "1/250", "1/200", "1/160",
    "1/125", "1/100", "1/80", "1/60", "1/50", "1/40", "1/30", "1/25", "1/20", "1/15", "1/13", "1/10", "1/8", "1/6", "1/5", "1/4", "1/3", "0.4\"",
    "0.5\"", "0.6\"", "0.8\"", "1\"", "1.3\"", "1.6\"", "2\"", "2.5\"", "3.2\"", "4\"", "5\"", "6\"", "8\"", "10\"", "13\"", "15\"", "20\"", "25\"",
    "30\"", "40\"", "50\"", "60\"",
];
/// The third stop (`Tv × 3`) of [`SHUTTER`]'s first entry.
const SHUTTER_TOP: i32 = 36;

/// Apertures in third stops from Av 0 (f/1.0) to Av 9 (f/22).
const APERTURE: [&str; 28] = [
    "1.0", "1.1", "1.2", "1.4", "1.6", "1.8", "2.0", "2.2", "2.5", "2.8", "3.2", "3.5", "4.0", "4.5", "5.0", "5.6", "6.3", "7.1", "8.0", "9.0", "10",
    "11", "13", "14", "16", "18", "20", "22",
];

/// `"1622/256"` → 1622. Another denominator is scaled to 256; anything else is `None`.
pub fn parse(s: &str) -> Option<i32> {
    let (n, d) = s.trim().split_once('/')?;
    let n: i64 = n.trim().parse().ok()?;
    let d: i64 = d.trim().parse().ok()?;
    if d <= 0 || d > 65_536 || n.abs() > 1 << 24 {
        return None;
    }
    // ±100 stops is far past any lens or shutter, and keeps the arithmetic below from overflowing.
    i32::try_from(n * 256 / d).ok().filter(|v| v.abs() <= MAX_APEX)
}

/// The largest APEX × 256 value read.
const MAX_APEX: i32 = 100 * 256;

/// 1622 → `"1622/256"`.
pub fn format(v: i32) -> String {
    format!("{v}/256")
}

/// Third stop `k` (`APEX × 3`) in the camera's units, rounded up as the camera does.
pub fn third(k: i32) -> i32 {
    (k * 256 + 2).div_euclid(3)
}

/// The third stop nearest `v`, and whether `v` is on it (the camera's rounding is within 1/256).
fn nearest_third(v: i32) -> (i32, bool) {
    let x = f64::from(v) / THIRD;
    let k = x.round();
    // |v| ≤ i32::MAX, so |k| ≤ i32::MAX / 85: the cast can't overflow.
    #[allow(clippy::cast_possible_truncation)]
    let ki = k as i32;
    (ki, (x - k).abs() < 0.05)
}

/// A shutter speed for people: `1/80`, `0.5"`, `60"`.
pub fn shutter_label(tv: i32) -> String {
    let (k, on) = nearest_third(tv);
    if on && let Some(s) = usize::try_from(SHUTTER_TOP - k).ok().and_then(|i| SHUTTER.get(i)) {
        return (*s).to_string();
    }
    let secs = 2f64.powf(-f64::from(tv) / 256.0);
    if secs < 0.3 { format!("1/{:.0}", 1.0 / secs) } else { format!("{secs:.1}\"") }
}

/// An aperture for people: `f/5.6`.
pub fn aperture_label(av: i32) -> String {
    let (k, on) = nearest_third(av);
    if on && let Some(s) = usize::try_from(k).ok().and_then(|i| APERTURE.get(i)) {
        return format!("f/{s}");
    }
    format!("f/{:.1}", 2f64.powf(f64::from(av) / 512.0))
}

/// The shutter speeds from `slow` to `fast` (Tv × 256), fastest first, in third stops.
pub fn shutter_stops(slow: i32, fast: i32) -> Vec<i32> {
    let (lo, hi) = (slow.min(fast), slow.max(fast));
    let (lo, hi) = (lo.clamp(-MAX_APEX, MAX_APEX), hi.clamp(-MAX_APEX, MAX_APEX));
    let ks = (lo.div_euclid(86) - 1)..=(hi.div_euclid(85) + 1);
    let mut v: Vec<i32> = ks.map(third).filter(|t| (lo..=hi).contains(t)).collect();
    v.reverse();
    v.truncate(200);
    v
}

/// The apertures from `wide` (the lens wide open) to `narrow` (Av × 256), widest first: `wide`
/// itself, then every third stop above it.
pub fn aperture_stops(wide: i32, narrow: i32) -> Vec<i32> {
    let (lo, hi) = (wide.min(narrow), wide.max(narrow));
    let (lo, hi) = (lo.clamp(-MAX_APEX, MAX_APEX), hi.clamp(-MAX_APEX, MAX_APEX));
    let mut v = vec![lo];
    let ks = (lo.div_euclid(86))..=(hi.div_euclid(85) + 1);
    v.extend(ks.map(third).filter(|a| *a > lo + 20 && *a <= hi));
    v.truncate(200);
    v
}

/// What `getinfo&type=lens` tells (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lens {
    /// The narrowest aperture (Av × 256).
    pub av_narrow: i32,
    /// The widest aperture at the current zoom (Av × 256).
    pub av_wide: i32,
    /// The fastest shutter speed (Tv × 256).
    pub tv_fast: i32,
    /// The slowest (Tv × 256).
    pub tv_slow: i32,
    /// The focal lengths (mm), tele and wide (*hypothesis*).
    pub focal_tele: Option<u32>,
    pub focal_wide: Option<u32>,
}

impl Lens {
    /// Read the plain-text answer; `None` unless it starts with `ok` and has the four ranges.
    pub fn parse(body: &[u8]) -> Option<Lens> {
        let text = String::from_utf8_lossy(body.get(..body.len().min(1024)).unwrap_or_default()).to_string();
        let f: Vec<&str> = text.lines().next().unwrap_or_default().split(',').map(str::trim).collect();
        if f.first() != Some(&"ok") {
            return None;
        }
        let at = |i: usize| f.get(i).and_then(|s| parse(s));
        let mm = |i: usize| f.get(i).and_then(|s| s.parse::<u32>().ok()).filter(|v| (1..=2000).contains(v));
        Some(Lens { av_narrow: at(1)?, av_wide: at(2)?, tv_fast: at(3)?, tv_slow: at(4)?, focal_tele: mm(7), focal_wide: mm(8) })
    }

    /// The focal lengths for people: `12-32 mm`.
    pub fn focal_label(&self) -> Option<String> {
        match (self.focal_wide, self.focal_tele) {
            (Some(w), Some(t)) if w != t => Some(format!("{w}-{t} mm")),
            (Some(w), _) => Some(format!("{w} mm")),
            _ => None,
        }
    }
}

/// When the lens hasn't answered: the GX85's shutter range, f/1.0 to f/22.
impl Default for Lens {
    fn default() -> Self {
        Lens { av_narrow: 2304, av_wide: 0, tv_fast: 3072, tv_slow: -1536, focal_tele: None, focal_wide: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_the_camera_form() {
        assert_eq!(parse("1622/256"), Some(1622));
        assert_eq!(parse("-1536/256"), Some(-1536));
        assert_eq!(parse("128/1024"), Some(32));
        assert_eq!(parse("1/0"), None);
        assert_eq!(parse("abc"), None);
        assert_eq!(parse("1/60"), Some(4), "the camera's own misreading of 1/60");
        assert_eq!(format(1622), "1622/256");
    }

    #[test]
    fn third_stops_round_as_the_camera_does() {
        assert_eq!(third(19), 1622, "1/80 s, as read from the camera");
        assert_eq!(third(18), 1536);
        assert_eq!(third(15), 1280, "f/5.6");
        assert_eq!(third(-18), -1536);
        assert_eq!(third(-1), -85);
    }

    #[test]
    fn labels() {
        assert_eq!(shutter_label(1622), "1/80");
        assert_eq!(shutter_label(1621), "1/80");
        assert_eq!(shutter_label(3072), "1/4000");
        assert_eq!(shutter_label(-1536), "60\"");
        assert_eq!(shutter_label(0), "1\"");
        assert_eq!(shutter_label(3584), "1/16384", "past the table: worked out");
        assert_eq!(aperture_label(1280), "f/5.6");
        assert_eq!(aperture_label(2304), "f/22");
        assert_eq!(aperture_label(1126), "f/4.6", "off the grid: worked out");
        assert_eq!(aperture_label(-5000), "f/0.0");
    }

    #[test]
    fn ranges() {
        let s = shutter_stops(-1536, 3072);
        assert_eq!(s.len(), 55);
        assert_eq!(s.first().copied().map(shutter_label).as_deref(), Some("1/4000"));
        assert_eq!(s.last().copied().map(shutter_label).as_deref(), Some("60\""));
        assert!(s.contains(&1622));
        let a = aperture_stops(1126, 2304);
        assert_eq!(a.first(), Some(&1126), "wide open first");
        assert_eq!(a.get(1).copied().map(aperture_label).as_deref(), Some("f/5.0"));
        assert_eq!(a.last().copied().map(aperture_label).as_deref(), Some("f/22"));
        assert!(a.contains(&1280));
        assert_eq!(aperture_stops(2304, 1126), a, "either order");
    }

    #[test]
    fn reads_the_lens() {
        let l = Lens::parse(b"ok,2304/256,1126/256,3072/256,-1536/256,0,off,32,12,on,128/1024,on").unwrap();
        assert_eq!((l.av_narrow, l.av_wide, l.tv_fast, l.tv_slow), (2304, 1126, 3072, -1536));
        assert_eq!(l.focal_label().as_deref(), Some("12-32 mm"));
        assert!(Lens::parse(b"err_reject").is_none());
        assert!(Lens::parse(b"ok,1,2").is_none());
        assert!(Lens::parse(b"").is_none());
        assert!(Lens::parse(&[0xff; 4000]).is_none());
    }
}
