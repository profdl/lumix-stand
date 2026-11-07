//! Judging a frame: its brightness histogram and the pixels it clips, worked out once when the
//! frame arrives (off the window's thread for a full-size photo).

use std::sync::Arc;

use crate::worker::DecodedFrame;

/// Bars in the histogram.
pub const BINS: usize = 64;
/// A channel at or above this is blown out.
const HIGH: u8 = 254;
/// Every channel at or below this is crushed to black.
const LOW: u8 = 2;

/// What the histogram and the clipping warning show.
#[derive(Clone, Debug)]
pub struct Look {
    /// Pixels by brightness (Rec. 709 luma), dark to light.
    pub hist: [u32; BINS],
    /// The same at every level, 0-255.
    pub luma: [u32; 256],
    /// Pixels by their brightest channel, 0-255 (a channel clips before the brightness does, so
    /// the exposure aims this).
    pub peak: [u32; 256],
    /// The average red, green and blue, 0-255 (a colour cast shows here).
    pub mean_rgb: [f32; 3],
    /// Shares of the pixels blown out (any channel) and crushed (every channel), 0-1.
    pub high: f32,
    pub low: f32,
    /// RGBA8 the frame's size: blown pixels red, crushed ones blue, the rest transparent.
    pub overlay: Arc<Vec<u8>>,
}

/// Look at `f`.
pub fn look(f: &DecodedFrame) -> Look {
    let mut hist = [0u32; BINS];
    let mut luma = [0u32; 256];
    let mut peak = [0u32; 256];
    let mut sums = [0u64; 3];
    let mut overlay = vec![0u8; f.rgba.len()];
    let (mut high, mut low, mut n) = (0u64, 0u64, 0u64);
    for (px, out) in f.rgba.chunks_exact(4).zip(overlay.chunks_exact_mut(4)) {
        let [r, g, b, _] = [px.first(), px.get(1), px.get(2), px.get(3)].map(|c| c.copied().unwrap_or(0));
        let y = (u32::from(r) * 2126 + u32::from(g) * 7152 + u32::from(b) * 722) / 10_000;
        let level = usize::try_from(y).unwrap_or(0).min(255);
        if let Some(l) = luma.get_mut(level) {
            *l += 1;
        }
        if let Some(p) = peak.get_mut(usize::from(r.max(g).max(b))) {
            *p += 1;
        }
        for (sum, c) in sums.iter_mut().zip([r, g, b]) {
            *sum += u64::from(c);
        }
        let bin = level * BINS / 256;
        if let Some(h) = hist.get_mut(bin.min(BINS - 1)) {
            *h += 1;
        }
        n += 1;
        let tint = if r >= HIGH || g >= HIGH || b >= HIGH {
            high += 1;
            Some([255, 40, 40, 220])
        } else if r <= LOW && g <= LOW && b <= LOW {
            low += 1;
            Some([40, 110, 255, 220])
        } else {
            None
        };
        if let Some(t) = tint {
            out.copy_from_slice(&t);
        }
    }
    let share = |c: u64| if n == 0 { 0.0 } else { (c as f64 / n as f64) as f32 };
    let mean_rgb = sums.map(|c| if n == 0 { 0.0 } else { (c as f64 / n as f64) as f32 });
    Look { hist, luma, peak, mean_rgb, high: share(high), low: share(low), overlay: Arc::new(overlay) }
}

impl Look {
    /// The brightness (0-255) that share `q` (0-1) of the pixels are at or below.
    pub fn percentile(&self, q: f32) -> u8 {
        percentile(&self.luma, q)
    }

    /// The same for the brightest channel.
    pub fn peak_percentile(&self, q: f32) -> u8 {
        percentile(&self.peak, q)
    }
}

fn percentile(levels: &[u32; 256], q: f32) -> u8 {
    {
        let total: u64 = levels.iter().map(|&c| u64::from(c)).sum();
        if total == 0 {
            return 0;
        }
        let want = (q.clamp(0.0, 1.0) as f64 * total as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (level, &c) in levels.iter().enumerate() {
            seen += u64::from(c);
            if seen >= want {
                return u8::try_from(level).unwrap_or(u8::MAX);
            }
        }
        u8::MAX
    }
}

/// A full-size photo ready to show: read from `path`, decoded and looked at.
#[derive(Clone, Debug)]
pub struct Photo {
    /// One more for each photo shown (the window's texture key).
    pub id: u64,
    pub name: String,
    pub frame: DecodedFrame,
    pub look: Look,
    /// The settings it was taken with.
    pub camera: Vec<(String, String)>,
}

/// Read and decode the photo at `path` (run on its own thread: a 16 MP JPEG takes a moment).
pub fn load(id: u64, path: &std::path::Path, camera: Vec<(String, String)>) -> Result<Photo, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    if meta.len() > lumix_link::photos::MAX_ORIGINAL {
        return Err(format!("{} is too big to show", path.display()));
    }
    let jpeg = std::fs::read(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let frame = crate::worker::decode(id, &jpeg).ok_or_else(|| format!("{} isn't a JPEG this app can show", path.display()))?;
    let look = look(&frame);
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    Ok(Photo { id, name, frame, look, camera })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_marks_clipping() {
        // Four pixels: white, black, mid grey, pure red (its red channel blown).
        let rgba = vec![255, 255, 255, 255, 0, 0, 0, 255, 128, 128, 128, 255, 255, 0, 0, 255];
        let f = DecodedFrame { seq: 1, width: 2, height: 2, rgba: Arc::new(rgba) };
        let l = look(&f);
        assert_eq!(l.hist.iter().sum::<u32>(), 4);
        assert_eq!(l.hist[BINS - 1], 1, "white is the lightest bar");
        assert_eq!(l.hist[0], 1, "black the darkest");
        assert!((l.high - 0.5).abs() < 1e-6, "white and the red");
        assert!((l.low - 0.25).abs() < 1e-6);
        assert_eq!(&l.overlay[0..4], &[255, 40, 40, 220]);
        assert_eq!(&l.overlay[4..8], &[40, 110, 255, 220]);
        assert_eq!(&l.overlay[8..12], &[0, 0, 0, 0], "grey isn't marked");
        assert_eq!(l.percentile(0.0), 0);
        assert_eq!(l.percentile(0.5), 54, "the red pixel's luma");
        assert_eq!(l.percentile(1.0), 255);
        assert_eq!(l.peak_percentile(0.5), 128, "black, grey, then the red and white at 255");
        assert_eq!(l.peak_percentile(0.75), 255);
        assert!((l.mean_rgb[0] - 159.5).abs() < 0.01);
    }

    #[test]
    fn refuses_what_isnt_a_photo() {
        let d = crate::frames::tests::TempDir::new("review");
        let p = d.0.join("x.jpg");
        std::fs::write(&p, b"nope").unwrap();
        assert!(load(1, &p, Vec::new()).is_err());
        assert!(load(1, &d.0.join("missing.jpg"), Vec::new()).is_err());
    }
}
