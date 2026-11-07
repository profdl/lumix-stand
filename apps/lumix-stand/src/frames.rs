//! Numbered frames in the capture folder: `<prefix>_<NNNN>.jpg`, never replacing a file.
//!
//! Ported from Picabia (`crates/engine/src/cmd/picabia_camera.rs`).

use std::path::{Path, PathBuf};

/// The file-name prefix when none is set.
pub const DEFAULT_PREFIX: &str = "frame";
/// The longest prefix (chars).
pub const MAX_PREFIX: usize = 40;
/// The most directory entries looked at to find the next number.
const MAX_SCAN: usize = 100_000;
/// The highest frame number.
const MAX_NUMBER: u32 = 999_999;

/// `f` is an existing folder we may write to.
pub fn check_folder(f: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(f).map_err(|_| format!("the folder {} doesn't exist", f.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} isn't a folder", f.display()));
    }
    if meta.permissions().readonly() {
        return Err(format!("the folder {} is read-only", f.display()));
    }
    Ok(())
}

/// A prefix made safe for file names: trimmed, at most 40 letters, digits, spaces, `-` or `_`
/// (empty: [`DEFAULT_PREFIX`] is used).
pub fn check_prefix(raw: &str) -> Result<String, String> {
    let p = raw.trim();
    if p.chars().count() > MAX_PREFIX {
        return Err(format!("the prefix can be at most {MAX_PREFIX} characters"));
    }
    if !p.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_')) {
        return Err("the prefix may hold only letters, digits, spaces, - and _".into());
    }
    Ok(p.to_string())
}

/// The prefix to use: `prefix`, or [`DEFAULT_PREFIX`] when it is empty.
pub fn prefix_or_default(prefix: &str) -> &str {
    if prefix.trim().is_empty() { DEFAULT_PREFIX } else { prefix }
}

/// `<prefix>_<NNNN>.jpg`.
pub fn file_name(prefix: &str, n: u32) -> String {
    format!("{prefix}_{n:04}.jpg")
}

/// One more than the highest `<prefix>_<digits>.jpg` (or `.jpeg`, any case) in `folder`; 1 when
/// there is none.
pub fn next_number(folder: &Path, prefix: &str) -> Result<u32, String> {
    let rd = std::fs::read_dir(folder).map_err(|e| format!("can't read {}: {e}", folder.display()))?;
    let lead = format!("{prefix}_");
    let mut max = 0u32;
    for entry in rd.take(MAX_SCAN).flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(n) = number_in(&name, &lead) {
            max = max.max(n);
        }
    }
    if max >= MAX_NUMBER {
        return Err(format!("the folder already has frame {MAX_NUMBER}: choose another folder or prefix"));
    }
    Ok(max + 1)
}

fn number_in(name: &str, lead: &str) -> Option<u32> {
    let rest = name.strip_prefix(lead)?;
    let (digits, ext) = rest.rsplit_once('.')?;
    if !matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg")
        || digits.is_empty()
        || digits.len() > 6
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

/// Write `jpeg` to `path`, which must not exist yet.
pub fn save_new(path: &Path, jpeg: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    f.write_all(jpeg).and_then(|()| f.sync_all()).map_err(|e| format!("can't write {}: {e}", path.display()))
}

/// Write `jpeg` as the next numbered frame in `folder`, without ever replacing a file: a name
/// taken in the meantime moves on to the next number.
pub fn save_frame(folder: &Path, prefix: &str, jpeg: &[u8]) -> Result<(PathBuf, u32), String> {
    use std::io::Write as _;
    let mut n = next_number(folder, prefix)?;
    for _ in 0..100 {
        let path = folder.join(file_name(prefix, n));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(jpeg).and_then(|()| f.sync_all()).map_err(|e| format!("can't write {}: {e}", path.display()))?;
                return Ok((path, n));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(format!("can't write {}: {e}", path.display())),
        }
    }
    Err("couldn't find a free file name".into())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh, empty folder under the system's temp folder (removed when dropped).
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> TempDir {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!("lumix-stand-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn numbers_follow_the_highest_and_never_overwrite() {
        let d = TempDir::new("numbers");
        assert_eq!(next_number(&d.0, "frame").unwrap(), 1);
        std::fs::write(d.0.join("frame_0007.jpg"), b"x").unwrap();
        std::fs::write(d.0.join("frame_0003.JPEG"), b"x").unwrap();
        std::fs::write(d.0.join("other_0099.jpg"), b"x").unwrap();
        std::fs::write(d.0.join("frame_12a.jpg"), b"x").unwrap();
        std::fs::write(d.0.join("frame_0050.png"), b"x").unwrap();
        assert_eq!(next_number(&d.0, "frame").unwrap(), 8);
        let (path, n) = save_frame(&d.0, "frame", b"jpeg").unwrap();
        assert_eq!((n, path.file_name().unwrap().to_str().unwrap()), (8, "frame_0008.jpg"));
        assert_eq!(std::fs::read(&path).unwrap(), b"jpeg");
        let (_, n) = save_frame(&d.0, "frame", b"next").unwrap();
        assert_eq!(n, 9);
        assert_eq!(std::fs::read(d.0.join("frame_0007.jpg")).unwrap(), b"x", "untouched");
    }

    #[test]
    fn the_last_number_is_refused() {
        let d = TempDir::new("full");
        std::fs::write(d.0.join("f_999999.jpg"), b"x").unwrap();
        assert!(next_number(&d.0, "f").is_err());
        assert!(save_frame(&d.0, "f", b"x").is_err());
    }

    #[test]
    fn prefixes_are_checked() {
        assert_eq!(check_prefix("  shot A-1_ ").unwrap(), "shot A-1_");
        assert_eq!(check_prefix("").unwrap(), "");
        assert!(check_prefix("../up").is_err());
        assert!(check_prefix("a/b").is_err());
        assert!(check_prefix(&"x".repeat(41)).is_err());
        assert_eq!(prefix_or_default(""), DEFAULT_PREFIX);
        assert_eq!(prefix_or_default("take"), "take");
    }

    #[test]
    fn folders_are_checked() {
        let d = TempDir::new("folder");
        assert!(check_folder(&d.0).is_ok());
        assert!(check_folder(&d.0.join("missing")).is_err());
        let f = d.0.join("file");
        std::fs::write(&f, b"x").unwrap();
        assert!(check_folder(&f).is_err());
    }
}
