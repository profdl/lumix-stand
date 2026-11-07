//! What the app remembers: a small JSON file in the OS config folder
//! (`~/Library/Application Support/lumix-stand/settings.json` on macOS).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The largest settings file read (bytes); anything bigger is ignored.
const MAX_FILE: u64 = 64 * 1024;

/// The settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// The last camera address connected to (never `sim`).
    pub address: String,
    /// The capture folder (`None`: not chosen yet; the first Capture asks).
    pub folder: Option<PathBuf>,
    /// The file-name prefix (empty: `frame`).
    pub prefix: String,
    /// Our id in the camera's access handshake. The camera knows this computer by it: changing it
    /// means accepting the computer on the camera again.
    pub device_id: String,
    /// Save each capture's full-size photo from the camera's card (else the live-view frame).
    pub full_size: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            address: lumix_link::DEFAULT_IP.into(),
            folder: None,
            prefix: String::new(),
            device_id: lumix_link::Options::default().device_id,
            full_size: true,
        }
    }
}

/// The settings file's usual place; `None` when the OS has no config folder.
pub fn default_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("lumix-stand").join("settings.json"))
}

impl Settings {
    /// Read `path`; the defaults when it is missing, too big or unreadable (with the reason, for
    /// the window, unless the file just isn't there yet).
    pub fn load(path: &Path) -> (Settings, Option<String>) {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Settings::default(), None),
            Err(e) => return (Settings::default(), Some(format!("can't read the settings ({e}); using the defaults"))),
        };
        if meta.len() > MAX_FILE {
            return (Settings::default(), Some("the settings file is too big; using the defaults".into()));
        }
        let read = std::fs::read(path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Settings>(&b).map_err(|e| e.to_string()));
        match read {
            Ok(mut s) => {
                s.tidy();
                (s, None)
            }
            Err(e) => (Settings::default(), Some(format!("can't read the settings ({e}); using the defaults"))),
        }
    }

    /// Write to `path` (through a temporary file, so a crash never leaves half a file).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("can't write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("can't write {}: {e}", path.display()))
    }

    /// Values from an edited file made usable: empty fields back to their defaults, a bad prefix
    /// dropped.
    fn tidy(&mut self) {
        let d = Settings::default();
        self.address = self.address.trim().to_string();
        if self.address.is_empty() || self.address.len() > 64 {
            self.address = d.address;
        }
        if self.device_id.trim().is_empty() || self.device_id.len() > 64 {
            self.device_id = d.device_id;
        }
        self.prefix = crate::frames::check_prefix(&self.prefix).unwrap_or_default();
        if self.folder.as_ref().is_some_and(|f| f.as_os_str().is_empty()) {
            self.folder = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::tests::TempDir;

    #[test]
    fn defaults_keep_the_accepted_handshake_id() {
        let s = Settings::default();
        assert_eq!(s.device_id, "4D454930-0100-1000-8000-PICABIA00001");
        assert_eq!(s.address, "192.168.54.1");
        assert_eq!(s.folder, None);
        assert!(s.full_size);
    }

    #[test]
    fn round_trip_and_bad_files() {
        let d = TempDir::new("settings");
        let path = d.0.join("sub").join("settings.json");
        assert_eq!(Settings::load(&path), (Settings::default(), None), "missing: defaults, quietly");
        let s = Settings { address: "10.0.0.5".into(), folder: Some(d.0.clone()), prefix: "take".into(), device_id: "ID".into(), full_size: false };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).0, s);

        std::fs::write(&path, b"{not json").unwrap();
        let (s2, why) = Settings::load(&path);
        assert_eq!(s2, Settings::default());
        assert!(why.is_some());

        std::fs::write(&path, br#"{"address": " ", "prefix": "../x", "deviceId": "", "extra": 1}"#).unwrap();
        assert_eq!(Settings::load(&path), (Settings::default(), None), "tidied");
    }
}
