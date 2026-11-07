//! The JPEG in a live-view datagram.
//!
//! *Hypothesis* (community notes, to be checked by the probe): each UDP datagram the camera sends
//! is one binary header followed by one whole JPEG of about 640×480. The header's layout isn't
//! known, so the frame is found by its markers instead: from the first SOI (`FF D8`) to the last
//! EOI (`FF D9`) after it. The last, not the first: a JPEG may carry an embedded thumbnail with its
//! own EOI.

use std::fmt;

/// The largest datagram looked at. UDP over IPv4 carries at most 65,507 bytes; anything larger
/// can't be one datagram.
pub const MAX_DATAGRAM: usize = 65_536;
/// The smallest JPEG worth keeping (SOI, a few segments, EOI): shorter runs are noise.
pub const MIN_JPEG: usize = 64;

const SOI: [u8; 2] = [0xFF, 0xD8];
const EOI: [u8; 2] = [0xFF, 0xD9];

/// Why a datagram held no frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    TooLarge(usize),
    NoStart,
    NoEnd,
    TooShort(usize),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::TooLarge(n) => write!(f, "a datagram of {n} bytes is larger than UDP allows"),
            FrameError::NoStart => f.write_str("no JPEG start marker (FF D8)"),
            FrameError::NoEnd => f.write_str("no JPEG end marker (FF D9) after the start"),
            FrameError::TooShort(n) => write!(f, "a JPEG of {n} bytes is too short to be a frame"),
        }
    }
}

/// One live-view frame: the JPEG bytes and the header bytes before them (kept for the probe).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub header_len: usize,
    pub jpeg: Vec<u8>,
}

/// The JPEG in `datagram` (see the module docs).
pub fn extract_jpeg(datagram: &[u8]) -> Result<Frame, FrameError> {
    if datagram.len() > MAX_DATAGRAM {
        return Err(FrameError::TooLarge(datagram.len()));
    }
    let start = find(datagram, SOI).ok_or(FrameError::NoStart)?;
    let rest = datagram.get(start..).ok_or(FrameError::NoStart)?;
    let end = rfind(rest, EOI).filter(|&e| e >= 2).ok_or(FrameError::NoEnd)?;
    let jpeg = rest.get(..end + 2).ok_or(FrameError::NoEnd)?;
    if jpeg.len() < MIN_JPEG {
        return Err(FrameError::TooShort(jpeg.len()));
    }
    Ok(Frame { header_len: start, jpeg: jpeg.to_vec() })
}

/// The first place `pat` starts in `hay`.
fn find(hay: &[u8], pat: [u8; 2]) -> Option<usize> {
    hay.windows(2).position(|w| w == pat)
}

/// The last place `pat` starts in `hay`.
fn rfind(hay: &[u8], pat: [u8; 2]) -> Option<usize> {
    hay.windows(2).rposition(|w| w == pat)
}

/// `bytes` as hex, 16 to a line, at most `cap` bytes (the probe's dumps).
pub fn hex_dump(bytes: &[u8], cap: usize) -> String {
    let shown = bytes.get(..cap.min(bytes.len())).unwrap_or_default();
    let mut out = String::new();
    for (i, line) in shown.chunks(16).enumerate() {
        let hex: Vec<String> = line.iter().map(|b| format!("{b:02x}")).collect();
        out.push_str(&format!("{:04x}  {}\n", i * 16, hex.join(" ")));
    }
    if bytes.len() > shown.len() {
        out.push_str(&format!("… {} more bytes\n", bytes.len() - shown.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(body: usize) -> Vec<u8> {
        let mut v = SOI.to_vec();
        v.extend(std::iter::repeat_n(0x11, body));
        v.extend(EOI);
        v
    }

    #[test]
    fn finds_the_jpeg_after_a_header() {
        let mut d = vec![0u8, 1, 2, 3, 0xFF, 0x00, 9];
        d.extend(jpeg(100));
        d.extend([0, 0, 0]); // trailing padding is dropped
        let f = extract_jpeg(&d).unwrap();
        assert_eq!(f.header_len, 7);
        assert_eq!(f.jpeg, jpeg(100));
    }

    #[test]
    fn keeps_an_inner_end_marker() {
        let mut j = SOI.to_vec();
        j.extend(std::iter::repeat_n(0x22, 80));
        j.extend(EOI); // a thumbnail's
        j.extend(std::iter::repeat_n(0x33, 80));
        j.extend(EOI);
        assert_eq!(extract_jpeg(&j).unwrap().jpeg, j);
    }

    #[test]
    fn refuses_junk() {
        assert_eq!(extract_jpeg(&[]), Err(FrameError::NoStart));
        assert_eq!(extract_jpeg(&[1, 2, 3]), Err(FrameError::NoStart));
        assert_eq!(extract_jpeg(&[0xFF, 0xD8, 1, 2]), Err(FrameError::NoEnd));
        // FF D8 D9: the end marker overlaps the start.
        assert_eq!(extract_jpeg(&[0xFF, 0xD8, 0xD9]), Err(FrameError::NoEnd));
        assert_eq!(extract_jpeg(&jpeg(4)), Err(FrameError::TooShort(8)));
        assert!(matches!(extract_jpeg(&vec![0; MAX_DATAGRAM + 1]), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn never_panics_on_random_bytes() {
        // A small xorshift, so the test needs no dependencies.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for len in 0..600 {
            let d: Vec<u8> = (0..len)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    // Mostly markers, so starts and ends turn up often.
                    [0xFF, 0xD8, 0xD9, (x >> 24) as u8][(x % 4) as usize]
                })
                .collect();
            if let Ok(f) = extract_jpeg(&d) {
                assert!(f.jpeg.starts_with(&SOI) && f.jpeg.ends_with(&EOI));
                assert!(f.header_len + f.jpeg.len() <= d.len());
            }
        }
    }

    #[test]
    fn hex_dump_caps() {
        let s = hex_dump(&[0xAB; 40], 20);
        assert!(s.starts_with("0000  ab ab"));
        assert!(s.contains("… 20 more bytes"));
    }
}
