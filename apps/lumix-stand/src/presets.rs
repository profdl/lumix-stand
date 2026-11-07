//! Presets: a whole shooting setup saved by name (every camera setting the app knows, plus the
//! capture options), applied with one click. They live in the settings file.

use lumix_link::CameraSettings;
use lumix_link::menu;
use serde::{Deserialize, Serialize};

/// The most presets kept.
pub const MAX_PRESETS: usize = 64;
/// The longest preset name (characters).
pub const MAX_NAME: usize = 40;

/// One preset.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Preset {
    pub name: String,
    /// (setting, value), in [`menu::catalogue`] order.
    pub camera: Vec<(String, String)>,
    /// The file-name prefix (empty: the default).
    pub prefix: String,
    /// Save each capture's full-size photo from the card.
    pub full_size: bool,
    /// Pause live view while photos download.
    pub fast_downloads: bool,
}

/// The camera's settings worth saving: every catalogue setting that has a value, in catalogue
/// order (so applying them sets the aspect ratio before the picture size).
pub fn camera_values(cs: &CameraSettings) -> Vec<(String, String)> {
    menu::catalogue().iter().filter_map(|s| cs.value(s.key).map(|v| (s.key.to_string(), v.to_string()))).collect()
}

/// What applying `camera` would change: (setting, value), in catalogue order.
pub fn changes(camera: &[(String, String)], cs: &CameraSettings) -> Vec<(String, String)> {
    menu::catalogue()
        .iter()
        .filter_map(|s| camera.iter().find(|(k, _)| k == s.key))
        .filter(|(k, v)| cs.value(k) != Some(v.as_str()))
        .cloned()
        .collect()
}

impl Preset {
    /// The preset is what the camera and the app are set to now.
    pub fn matches(&self, cs: &CameraSettings, prefix: &str, full_size: bool, fast_downloads: bool) -> bool {
        !self.camera.is_empty()
            && changes(&self.camera, cs).is_empty()
            && self.prefix == prefix
            && self.full_size == full_size
            && self.fast_downloads == fast_downloads
    }

    /// Made usable after reading a file: a name, known settings only, sizes capped.
    fn tidy(&mut self, n: usize) {
        self.name = clean_name(&self.name).unwrap_or_else(|| format!("Preset {n}"));
        let known = menu::catalogue();
        self.camera.retain(|(k, v)| known.iter().any(|s| s.key == k) && !v.is_empty() && v.len() <= 64);
        self.camera.dedup_by(|a, b| a.0 == b.0);
        self.prefix = crate::frames::check_prefix(&self.prefix).unwrap_or_default();
    }
}

/// Tidy a list read from a file.
pub fn tidy(presets: &mut Vec<Preset>) {
    presets.truncate(MAX_PRESETS);
    for (i, p) in presets.iter_mut().enumerate() {
        p.tidy(i + 1);
    }
}

/// A name trimmed and capped; `None` when empty.
pub fn clean_name(name: &str) -> Option<String> {
    let n: String = name.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    (!n.is_empty()).then_some(n)
}

/// The first "Preset N" not taken.
pub fn new_name(presets: &[Preset]) -> String {
    (1..).map(|n| format!("Preset {n}")).find(|n| presets.iter().all(|p| &p.name != n)).unwrap_or_else(|| "Preset".into())
}

/// The settings a frame was taken with, in short: `1/60 · f/8 · ISO 200 · Daylight`, as
/// (setting, text) so that differences can be marked.
pub fn summary(camera: &[(String, String)]) -> Vec<(&'static str, String)> {
    let value = |k: &str| camera.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let mut out = Vec::new();
    for key in ["shtrspeed", "focal", "iso", "exposure", "whitebalance", "photostyle"] {
        let (Some(v), Some(s)) = (value(key), menu::find(key)) else { continue };
        if key == "exposure" && v == "0" {
            continue;
        }
        let label = s.value_label(v);
        let text = match key {
            "iso" => format!("ISO {label}"),
            "exposure" => format!("{label} EV"),
            _ => label,
        };
        out.push((s.key, text));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(values: &[(&str, &str)]) -> CameraSettings {
        CameraSettings { values: values.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(), ..CameraSettings::default() }
    }

    #[test]
    fn saves_and_compares_in_catalogue_order() {
        let now = cs(&[("pictsize", "l"), ("iso", "200"), ("aspectratio", "16:9"), ("unknown", "x")]);
        let values = camera_values(&now);
        let keys: Vec<&str> = values.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["iso", "aspectratio", "pictsize"], "catalogue order, known settings only");

        let p = Preset { name: "A".into(), camera: values, ..Preset::default() };
        assert!(p.matches(&now, "", false, false));
        assert!(!p.matches(&now, "take", false, false), "the capture options count");
        let later = cs(&[("pictsize", "m"), ("iso", "400"), ("aspectratio", "4:3")]);
        assert!(!p.matches(&later, "", false, false));
        let c = changes(&p.camera, &later);
        assert_eq!(c.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["iso", "aspectratio", "pictsize"], "aspect before size");
    }

    #[test]
    fn names_and_tidying() {
        assert_eq!(clean_name("  "), None);
        assert_eq!(clean_name(" Pencil\ntest ").as_deref(), Some("Penciltest"));
        assert_eq!(clean_name(&"x".repeat(100)).map(|n| n.len()), Some(MAX_NAME));
        let taken = [Preset { name: "Preset 1".into(), ..Preset::default() }];
        assert_eq!(new_name(&taken), "Preset 2");

        let mut ps = vec![Preset {
            name: " ".into(),
            camera: vec![("iso".into(), "200".into()), ("bogus".into(), "1".into())],
            prefix: "../x".into(),
            ..Preset::default()
        }];
        tidy(&mut ps);
        assert_eq!(ps[0].name, "Preset 1");
        assert_eq!(ps[0].camera, [("iso".to_string(), "200".to_string())]);
        assert_eq!(ps[0].prefix, "");
    }

    #[test]
    fn summaries() {
        let s = summary(&[("iso".into(), "200".into()), ("exposure".into(), "0".into()), ("whitebalance".into(), "daylight".into())]);
        assert_eq!(s, [("iso", "ISO 200".to_string()), ("whitebalance", "Daylight".to_string())]);
    }
}
