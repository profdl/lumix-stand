//! The camera's settings: what can be set, to what, and which choices the camera allows now.
//!
//! The catalogue comes from the GX85's own menu (`getinfo&type=allmenu`, saved 2026-11-07): each
//! entry names its `setsetting` type and value, and its menu id. `getsetting&type=<type>` reads
//! a setting (`<settingvalue iso="400">`, about 20 ms), except AF mode, which answers
//! `err_critical` and is read from the current menu instead. `getinfo&type=curmenu` (about 110 ms,
//! 21 KB) gives every menu item's `enable` flag for the camera's present state (the mode dial,
//! the aspect ratio…): `<item id="menu_item_id_pctsiz_16m" enable="no"/>` while the aspect is
//! 16:9. Shutter speed and aperture aren't menu items: they are APEX values ([`crate::apex`]).
//!
//! Left out on purpose: video settings, bracketing, scene guide, and white balance by colour
//! temperature (`color_temp` takes a second value whose form isn't known yet).

use crate::xml;

/// Where a setting is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Exposure,
    Focus,
    Image,
    Drive,
    LiveView,
}

impl Group {
    pub const ALL: [Group; 5] = [Group::Exposure, Group::Focus, Group::Image, Group::Drive, Group::LiveView];

    pub fn title(self) -> &'static str {
        match self {
            Group::Exposure => "Exposure",
            Group::Focus => "Focus",
            Group::Image => "Image",
            Group::Drive => "Drive & flash",
            Group::LiveView => "Live view",
        }
    }
}

/// How a setting's values are chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One of a list.
    Menu(Vec<Choice>),
    /// A shutter speed (Tv × 256), in the lens's range.
    Shutter,
    /// An aperture (Av × 256), in the lens's range.
    Aperture,
}

/// One value of a menu setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// What `setsetting` takes.
    pub value: String,
    pub label: String,
    /// Its id in the camera's menu, without `menu_item_id_` (its `enable` flag in `curmenu`).
    pub menu_id: String,
}

/// One setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting {
    /// The `getsetting`/`setsetting` type.
    pub key: &'static str,
    pub label: &'static str,
    pub group: Group,
    pub kind: Kind,
    /// Its id in the camera's menu, without `menu_item_id_` (where `curmenu` has its value).
    pub menu_id: &'static str,
    /// `getsetting` can't read it (AF mode): its value comes from `curmenu`.
    pub menu_only: bool,
}

impl Setting {
    /// The label for `value` (the value itself when it isn't in the list).
    pub fn value_label(&self, value: &str) -> String {
        match &self.kind {
            Kind::Shutter => crate::apex::parse(value).map_or_else(|| value.to_string(), crate::apex::shutter_label),
            Kind::Aperture => crate::apex::parse(value).map_or_else(|| value.to_string(), crate::apex::aperture_label),
            Kind::Menu(c) => c.iter().find(|c| c.value == value).map_or_else(|| value.to_string(), |c| c.label.clone()),
        }
    }

    pub fn choices(&self) -> &[Choice] {
        match &self.kind {
            Kind::Menu(c) => c,
            _ => &[],
        }
    }
}

/// `(value, label, menu id)` lists.
fn list(prefix: &str, items: &[(&str, &str)]) -> Kind {
    Kind::Menu(items.iter().map(|(v, l)| Choice { value: (*v).into(), label: (*l).into(), menu_id: format!("{prefix}_{v}") }).collect())
}

fn with_ids(items: &[(&str, &str, &str)]) -> Kind {
    Kind::Menu(items.iter().map(|(v, l, id)| Choice { value: (*v).into(), label: (*l).into(), menu_id: (*id).into() }).collect())
}

/// Exposure compensation, −5 to +5 EV in thirds, as the camera writes it (`-14/3`, `0`, `5`).
fn exposure_values() -> Kind {
    let mut out = Vec::new();
    for k in -15i32..=15 {
        let value = if k % 3 == 0 { (k / 3).to_string() } else { format!("{k}/3") };
        let (whole, part) = (k.abs() / 3, k.abs() % 3);
        let sign = match k.signum() {
            -1 => "\u{2212}",
            1 => "+",
            _ => "",
        };
        let frac = ["", "\u{2153}", "\u{2154}"].get(usize::try_from(part).unwrap_or(0)).copied().unwrap_or("");
        let label = match (whole, part) {
            (0, 0) => "0".to_string(),
            (0, _) => format!("{sign}{frac}"),
            _ => format!("{sign}{whole}{frac}"),
        };
        // The menu ids: exposure_m14_3, exposure_0, exposure_p1, exposure_p12_3 (+4), exposure_p15_3 (+5).
        let id = match k {
            0 => "exposure_0".to_string(),
            12 => "exposure_p12_3".into(),
            15 => "exposure_p15_3".into(),
            _ if k % 3 == 0 => format!("exposure_{}{}", if k < 0 { "m" } else { "p" }, k.abs() / 3),
            _ => format!("exposure_{}{}_3", if k < 0 { "m" } else { "p" }, k.abs()),
        };
        out.push(Choice { value, label, menu_id: id });
    }
    Kind::Menu(out)
}

/// The ISO settings (the GX85's base is 200; 100 is its extended L.100).
fn iso_values() -> Kind {
    let mut out = vec![
        Choice { value: "auto".into(), label: "Auto".into(), menu_id: "sensitivity_auto".into() },
        Choice { value: "i_iso".into(), label: "Intelligent".into(), menu_id: "sensitivity_i_iso".into() },
    ];
    for v in [
        100, 125, 160, 200, 250, 320, 400, 500, 640, 800, 1000, 1250, 1600, 2000, 2500, 3200, 4000, 5000, 6400, 8000, 10000, 12800, 16000, 20000,
        25600,
    ] {
        let label = if v == 100 { "L.100".to_string() } else { v.to_string() };
        out.push(Choice { value: v.to_string(), label, menu_id: format!("sensitivity_{v}") });
    }
    Kind::Menu(out)
}

/// Every setting, in the order shown.
pub fn catalogue() -> Vec<Setting> {
    let s = |key, label, group, kind, menu_id| Setting { key, label, group, kind, menu_id, menu_only: false };
    vec![
        s("shtrspeed", "Shutter", Group::Exposure, Kind::Shutter, "shutter_speed"),
        s("focal", "Aperture", Group::Exposure, Kind::Aperture, "aperture"),
        s("iso", "ISO", Group::Exposure, iso_values(), "sensitivity"),
        s("exposure", "Exposure comp.", Group::Exposure, exposure_values(), "exposure3"),
        s(
            "whitebalance",
            "White balance",
            Group::Exposure,
            list(
                "whitebalance",
                &[
                    ("auto", "Auto"),
                    ("daylight", "Daylight"),
                    ("cloudy", "Cloudy"),
                    ("shade", "Shade"),
                    ("halogen", "Incandescent"),
                    ("flash", "Flash"),
                    ("white_set1", "White set 1"),
                    ("white_set2", "White set 2"),
                    ("white_set3", "White set 3"),
                    ("white_set4", "White set 4"),
                ],
            ),
            "whitebalance",
        ),
        s(
            "lightmetering",
            "Metering",
            Group::Exposure,
            list("lightmet", &[("multi", "Multiple"), ("center", "Center-weighted"), ("spot", "Spot")]),
            "lightmet",
        ),
        s(
            "focusmode",
            "Focus mode",
            Group::Focus,
            list("focusmode", &[("afs", "AF-S"), ("aff", "AF-F"), ("afc", "AF-C"), ("mf", "MF")]),
            "focusmode",
        ),
        Setting {
            key: "afmode",
            label: "AF area",
            group: Group::Focus,
            kind: list(
                "afmode",
                &[
                    ("facedetection", "Face/eye detection"),
                    ("aftracking", "Tracking"),
                    ("49area", "49-area"),
                    ("1area", "1-area"),
                    ("pinpoint", "Pinpoint"),
                ],
            ),
            menu_id: "afmode",
            menu_only: true,
        },
        s("peaking", "Peaking", Group::Focus, list("peaking", &[("off", "Off"), ("low", "Low"), ("high", "High")]), "peaking"),
        s(
            "aspectratio",
            "Aspect ratio",
            Group::Image,
            list("asprat", &[("4:3", "4:3 (full sensor)"), ("3:2", "3:2"), ("16:9", "16:9"), ("1:1", "1:1")]),
            "asprat",
        ),
        s(
            "pictsize",
            "Picture size",
            Group::Image,
            list(
                "pctsiz",
                &[
                    ("16m", "16M (L, 4:3)"),
                    ("14m", "14M (L, 3:2)"),
                    ("12m", "12M (L, 16:9)"),
                    ("11.5m", "11.5M (L, 1:1)"),
                    ("8m", "8M"),
                    ("7m", "7M"),
                    ("6m", "6M"),
                    ("4.5m", "4.5M"),
                    ("4m", "4M"),
                    ("3.5m", "3.5M"),
                    ("3m", "3M"),
                    ("2m", "2M"),
                ],
            ),
            "pctsiz",
        ),
        s(
            "quality",
            "Quality",
            Group::Image,
            list(
                "quality",
                &[
                    ("fine", "Fine JPEG"),
                    ("standard", "Standard JPEG"),
                    ("raw_fine", "RAW + fine JPEG"),
                    ("raw_standard", "RAW + standard JPEG"),
                    ("raw", "RAW only (no JPEG to download)"),
                ],
            ),
            "quality",
        ),
        s(
            "colormode",
            "Photo style",
            Group::Image,
            list(
                "ph_sty",
                &[
                    ("standard", "Standard"),
                    ("vivid", "Vivid"),
                    ("natural", "Natural"),
                    ("bw", "Monochrome"),
                    ("l_bw", "L.Monochrome"),
                    ("scenery", "Scenery"),
                    ("portrait", "Portrait"),
                    ("custom", "Custom"),
                ],
            ),
            "ph_sty",
        ),
        s(
            "filter_setting",
            "Filter",
            Group::Image,
            list(
                "filter_set",
                &[
                    ("noeffect", "None"),
                    ("pop", "Expressive"),
                    ("retro", "Retro"),
                    ("old_days", "Old days"),
                    ("high_key", "High key"),
                    ("low_key", "Low key"),
                    ("sepia", "Sepia"),
                    ("monochro", "Monochrome"),
                    ("dynamic_monochro", "Dynamic monochrome"),
                    ("rough_monochro", "Rough monochrome"),
                    ("silky_monochro", "Silky monochrome"),
                    ("impressive_art", "Impressive art"),
                    ("high_dynamic", "High dynamic"),
                    ("cross_proc", "Cross process"),
                    ("toy_photo", "Toy effect"),
                    ("toy_pop", "Toy pop"),
                    ("bleach_bypass", "Bleach bypass"),
                    ("diorama", "Miniature"),
                    ("soft_focus", "Soft focus"),
                    ("fantasy", "Fantasy"),
                    ("cross_filter", "Star filter"),
                    ("one_point_color", "One point colour"),
                    ("sunshine", "Sunshine"),
                ],
            ),
            "filter_set",
        ),
        s(
            "drivemode",
            "Drive",
            Group::Drive,
            with_ids(&[
                ("normal", "Single", "1shoot"),
                ("burst", "Burst", "burst_h"),
                ("selftimer", "Self-timer", "selftimer_2sec"),
                ("4k_photo", "4K photo", "4kphoto_burst"),
            ]),
            "drvmod",
        ),
        s(
            "flash",
            "Flash",
            Group::Drive,
            list(
                "flash",
                &[
                    ("forcedflashoff", "Off"),
                    ("forcedflashon", "On"),
                    ("forced_on_redeye", "On, red-eye"),
                    ("auto", "Auto"),
                    ("iauto", "iAuto"),
                    ("slowsync", "Slow sync"),
                    ("slowsyncredeye", "Slow sync, red-eye"),
                ],
            ),
            "flash",
        ),
        s(
            "liveviewsize",
            "Live view size",
            Group::LiveView,
            list("liveviewsize", &[("vga", "VGA"), ("qvga", "QVGA (lighter on Wi-Fi)")]),
            "liveview_quality",
        ),
    ]
}

/// The setting named `key`.
pub fn find(key: &str) -> Option<Setting> {
    catalogue().into_iter().find(|s| s.key == key)
}

/// The largest picture size for an aspect ratio (all from the full width of the sensor).
pub fn largest_size(aspect: &str) -> &'static str {
    match aspect {
        "3:2" => "14m",
        "16:9" => "12m",
        "1:1" => "11.5m",
        _ => "16m",
    }
}

/// The value `getsetting&type=<key>` answered (`<settingvalue key="value">`).
pub fn setting_value(body: &[u8], key: &str) -> Option<String> {
    let text = String::from_utf8_lossy(body.get(..body.len().min(4096)).unwrap_or_default()).to_string();
    xml::tags(&text).iter().find(|t| t.name == "settingvalue").and_then(|t| t.attr(key))
}

/// One item of `curmenu`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    /// Without `menu_item_id_`.
    pub id: String,
    pub enabled: bool,
    pub value: Option<String>,
}

/// The most of a `curmenu` answer that is read.
pub const MAX_MENU: usize = 256 * 1024;
/// The most items kept.
const MAX_ITEMS: usize = 2000;

/// The items of a `curmenu` answer (an empty list when it isn't one).
pub fn parse_curmenu(body: &[u8]) -> Vec<MenuItem> {
    let text = String::from_utf8_lossy(body.get(..body.len().min(MAX_MENU)).unwrap_or_default()).to_string();
    xml::tags(&text)
        .iter()
        .filter(|t| t.name == "item")
        .filter_map(|t| {
            let id = t.attr("id")?;
            let id = id.strip_prefix("menu_item_id_").unwrap_or(&id).to_string();
            Some(MenuItem { id, enabled: t.attr("enable").as_deref() != Some("no"), value: t.attr("value") })
        })
        .take(MAX_ITEMS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_is_whole() {
        let c = catalogue();
        let mut keys: Vec<_> = c.iter().map(|s| s.key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), c.len(), "each setting once");
        for s in &c {
            if let Kind::Menu(ch) = &s.kind {
                assert!(!ch.is_empty(), "{}", s.key);
                assert!(ch.iter().all(|c| !c.value.is_empty() && !c.label.is_empty()));
            }
        }
        assert!(Group::ALL.iter().all(|g| c.iter().any(|s| s.group == *g)));
    }

    #[test]
    fn exposure_comp_matches_the_cameras_menu() {
        let s = find("exposure").unwrap();
        let ch = s.choices();
        assert_eq!(ch.len(), 31);
        let by = |v: &str| ch.iter().find(|c| c.value == v).unwrap().clone();
        assert_eq!((by("-14/3").label, by("-14/3").menu_id), ("\u{2212}4\u{2154}".into(), "exposure_m14_3".into()));
        assert_eq!((by("-1").label, by("-1").menu_id), ("\u{2212}1".into(), "exposure_m1".into()));
        assert_eq!((by("1/3").label, by("1/3").menu_id), ("+\u{2153}".into(), "exposure_p1_3".into()));
        assert_eq!(by("4").menu_id, "exposure_p12_3");
        assert_eq!(by("5").menu_id, "exposure_p15_3");
        assert_eq!(by("0").label, "0");
    }

    #[test]
    fn labels_values() {
        assert_eq!(find("shtrspeed").unwrap().value_label("1622/256"), "1/80");
        assert_eq!(find("focal").unwrap().value_label("1280/256"), "f/5.6");
        assert_eq!(find("iso").unwrap().value_label("auto"), "Auto");
        assert_eq!(find("iso").unwrap().value_label("weird"), "weird");
        assert_eq!(largest_size("16:9"), "12m");
        assert_eq!(largest_size("4:3"), "16m");
    }

    #[test]
    fn reads_setting_values() {
        let b = br#"<?xml version="1.0"?><camrply><result>ok</result><settingvalue colormode="standard">0/0/0/0</settingvalue></camrply>"#;
        assert_eq!(setting_value(b, "colormode").as_deref(), Some("standard"));
        assert_eq!(setting_value(b, "iso"), None);
        assert_eq!(setting_value(b"<camrply><result>err_param</result></camrply>", "iso"), None);
    }

    #[test]
    fn reads_the_current_menu() {
        let b = br#"<camrply><result>ok</result><menuinfo><photosettings>
<item id="menu_item_id_afmode" enable="yes" value="49area" />
<item id="menu_item_id_afmode_23area" enable="no" />
<item id="menu_item_id_pctsiz_16m" enable="no" /></photosettings></menuinfo></camrply>"#;
        let m = parse_curmenu(b);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0], MenuItem { id: "afmode".into(), enabled: true, value: Some("49area".into()) });
        assert!(!m[2].enabled);
        assert!(parse_curmenu(b"err").is_empty());
    }
}
