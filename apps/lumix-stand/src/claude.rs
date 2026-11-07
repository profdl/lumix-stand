//! Asking Claude about a test shot: is it well exposed, sharp-friendly and neutral, and what to
//! change. Raw HTTPS to the Messages API (there is no official Rust SDK).
//!
//! Set up so that a small model does well: the app measures the picture itself (brightness
//! percentiles, clipping, the average colour) and sends those numbers with the 640×360 preview;
//! every setting Claude may choose is an `enum` of exactly the values the camera allows now (with
//! `keep` for no change), so the answer is always valid; and the goals are spelled out. The answer
//! is checked again against the camera's lists before anything is sent to the camera.
//!
//! The key: read with the 1Password CLI (`op read <reference>`, which asks for Touch ID) when a
//! reference is set, else `ANTHROPIC_API_KEY`. Only the reference is saved; the key is kept in
//! memory for the session.

use std::time::Duration;

use base64::Engine as _;
use lumix_link::menu::{self, Kind};
use lumix_link::{CameraSettings, apex};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::review::Look;
use crate::worker::DecodedFrame;

const URL: &str = "https://api.anthropic.com/v1/messages";
const VERSION: &str = "2026-10-21";
/// The longest answer read.
const MAX_REPLY: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);
/// The settings Claude may change, in the order asked.
pub const KEYS: [&str; 4] = ["shtrspeed", "focal", "iso", "whitebalance"];

/// The models offered, cheapest first: (id, name, input and output $ per million tokens).
pub const MODELS: [(&str, &str, f64, f64); 3] =
    [("claude-haiku-5-5", "Haiku 5.5", 0.10, 0.50), ("claude-sonnet-5-5", "Sonnet 5.5", 2.0, 10.0), ("claude-opus-5-5", "Opus 5.5", 4.0, 20.0)];

/// The model's name, for the window.
pub fn model_name(id: &str) -> &str {
    MODELS.iter().find(|m| m.0 == id).map_or(id, |m| m.1)
}

const SYSTEM: &str = "You are a careful photographer setting up a fixed copy stand: a Panasonic Lumix GX85 on a stand, pointing straight down at flat artwork (drawings, paintings, paper cut-outs) lit by fixed lights, for stop-motion animation frames. Nothing moves, so long shutter speeds are fine.\n\nThe goal for every frame:\n- A balanced exposure: the brightest tones (usually the paper) just below clipping, nothing important blown out or crushed to black.\n- The least grain: base ISO 200. Only raise ISO if the shutter can't go slow enough.\n- The sharpest picture: f/5.6 to f/8 on this lens. Avoid f/11 and narrower (diffraction) and wide open (soft corners).\n- Neutral colour: white paper should look white, not blue, yellow, green or magenta. Choose the white balance preset that matches the lights.\n\nYou get the test shot (a downscaled JPEG preview of the real photo) and measurements the app made from all of its pixels. Trust the measurements over your impression for brightness and clipping. Use the picture for what numbers can't tell: colour casts, glare or reflections, uneven lighting, shadows across the artwork. Some problems can't be fixed with camera settings (glare, uneven light): name them in the assessment so the person can move the lights.\n\nAnswer with the settings to use. Choose \"keep\" for any setting that is already right. Set good to true only when nothing needs to change. Keep the assessment to two or three short sentences, in plain words.";

/// What Claude said.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Advice {
    pub assessment: String,
    pub good: bool,
    pub shutter: String,
    pub aperture: String,
    pub iso: String,
    pub white_balance: String,
}

/// An answer, with what it cost.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub advice: Advice,
    /// The changes, checked against the camera's lists: (setting, value).
    pub changes: Vec<(String, String)>,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Reply {
    /// What it cost in US dollars, at the listed prices.
    pub fn cost(&self) -> f64 {
        let (i, o) = MODELS.iter().find(|m| m.0 == self.model).map_or((0.0, 0.0), |m| (m.2, m.3));
        (self.input_tokens as f64 * i + self.output_tokens as f64 * o) / 1_000_000.0
    }
}

/// The choices for `key` now: (value, label).
fn choices(key: &str, cs: &CameraSettings) -> Vec<(String, String)> {
    let Some(s) = menu::find(key) else { return Vec::new() };
    let lens = cs.lens.unwrap_or_default();
    match &s.kind {
        Kind::Shutter => apex::shutter_stops(lens.tv_slow, lens.tv_fast).into_iter().map(|v| (apex::format(v), apex::shutter_label(v))).collect(),
        Kind::Aperture => {
            apex::aperture_stops(lens.av_wide, lens.av_narrow).into_iter().map(|v| (apex::format(v), apex::aperture_label(v))).collect()
        }
        Kind::Menu(c) => c.iter().filter(|c| cs.enabled(&c.menu_id)).map(|c| (c.value.clone(), c.label.clone())).collect(),
    }
}

/// The answer's field for `key`.
fn field(key: &str) -> &'static str {
    match key {
        "shtrspeed" => "shutter",
        "focal" => "aperture",
        "iso" => "iso",
        _ => "white_balance",
    }
}

/// The JSON schema of the answer: each setting one of the camera's labels, or `keep`.
pub fn schema(cs: &CameraSettings) -> Value {
    let mut props = serde_json::Map::new();
    props.insert("assessment".into(), json!({"type": "string"}));
    props.insert("good".into(), json!({"type": "boolean"}));
    for key in KEYS {
        let mut labels: Vec<String> = vec!["keep".into()];
        labels.extend(choices(key, cs).into_iter().map(|(_, l)| l));
        labels.dedup();
        props.insert(field(key).into(), json!({"type": "string", "enum": labels}));
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["assessment", "good", "shutter", "aperture", "iso", "white_balance"],
        "properties": props,
    })
}

/// What the app measured, and the settings now, in words for the prompt.
pub fn describe(look: &Look, cs: &CameraSettings) -> String {
    let label = |k: &str| menu::find(k).and_then(|s| cs.value(k).map(|v| s.value_label(v))).unwrap_or_else(|| "unknown".into());
    let [r, g, b] = look.mean_rgb;
    let mut t = String::new();
    t.push_str("Settings of this test shot:\n");
    for (k, name) in [("shtrspeed", "Shutter"), ("focal", "Aperture"), ("iso", "ISO"), ("whitebalance", "White balance")] {
        t.push_str(&format!("- {name}: {}\n", label(k)));
    }
    t.push_str("\nMeasured on every pixel of the photo (brightness 0-255, Rec. 709 luma):\n");
    for (q, name) in [(0.01, "1st percentile"), (0.5, "median"), (0.99, "99th percentile"), (0.995, "99.5th percentile")] {
        t.push_str(&format!("- {name}: {}\n", look.percentile(q)));
    }
    t.push_str(&format!("- brightest channel of each pixel, 99.5th percentile: {}\n", look.peak_percentile(0.995)));
    t.push_str(&format!("- blown out (a channel at 254-255): {:.2}% of the pixels\n", look.high * 100.0));
    t.push_str(&format!("- crushed to black (all channels at 0-2): {:.2}% of the pixels\n", look.low * 100.0));
    t.push_str(&format!("- average colour: red {r:.0}, green {g:.0}, blue {b:.0}\n"));
    t.push_str(&format!(
        "\nThe app aims the brightest channel's 99.5th percentile at {:.0}. Each stop of light moves a mid-bright tone by about a factor of 1.37 in these 0-255 values.",
        crate::exposure::TARGET_HIGH
    ));
    t
}

/// The request body.
pub fn request(model: &str, jpeg: &[u8], look: &Look, cs: &CameraSettings) -> Value {
    let image = base64::engine::general_purpose::STANDARD.encode(jpeg);
    let mut body = json!({
        "model": model,
        "max_tokens": 16000,
        "system": SYSTEM,
        "output_config": {"effort": "medium", "format": {"type": "json_schema", "schema": schema(cs)}},
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": image}},
                {"type": "text", "text": describe(look, cs)},
            ],
        }],
    });
    // On Opus and Sonnet, a request their safety classifiers decline is retried on another model.
    if model != "claude-haiku-5-5"
        && let Some(o) = body.as_object_mut()
    {
        o.insert("fallbacks".into(), json!("default"));
    }
    body
}

/// Read the answer: the advice, checked against the camera's lists.
pub fn parse_reply(body: &[u8], cs: &CameraSettings) -> Result<Reply, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("Claude's answer isn't JSON: {e}"))?;
    if let Some(e) = v.get("error") {
        let msg = e.get("message").and_then(Value::as_str).unwrap_or("unknown error");
        return Err(format!("the API refused: {msg}"));
    }
    match v.get("stop_reason").and_then(Value::as_str) {
        Some("refusal") => return Err("Claude declined to answer".into()),
        Some("max_tokens") => return Err("Claude's answer was cut off".into()),
        _ => {}
    }
    let text: String = v
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let advice: Advice = serde_json::from_str(text.trim()).map_err(|e| format!("Claude's answer isn't in the expected form: {e}"))?;
    let mut changes = Vec::new();
    for key in KEYS {
        let label = match key {
            "shtrspeed" => &advice.shutter,
            "focal" => &advice.aperture,
            "iso" => &advice.iso,
            _ => &advice.white_balance,
        };
        if label == "keep" {
            continue;
        }
        let value = choices(key, cs)
            .into_iter()
            .find(|(_, l)| l == label)
            .map(|(v, _)| v)
            .ok_or_else(|| format!("Claude chose {label}, which the camera doesn't offer now"))?;
        if cs.value(key) != Some(value.as_str()) {
            changes.push((key.to_string(), value));
        }
    }
    let usage = |k: &str| v.get("usage").and_then(|u| u.get(k)).and_then(Value::as_u64).unwrap_or(0);
    let model = v.get("model").and_then(Value::as_str).unwrap_or_default().to_string();
    Ok(Reply { advice, changes, model, input_tokens: usage("input_tokens"), output_tokens: usage("output_tokens") })
}

/// A preview as a JPEG to send (RGBA → RGB, quality 85).
pub fn jpeg_of(f: &DecodedFrame) -> Result<Vec<u8>, String> {
    let rgb: Vec<u8> = f.rgba.chunks_exact(4).flat_map(|p| [p.first(), p.get(1), p.get(2)].map(|c| c.copied().unwrap_or(0))).collect();
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
    image::ImageEncoder::write_image(enc, &rgb, f.width, f.height, image::ExtendedColorType::Rgb8)
        .map_err(|e| format!("can't encode the preview: {e}"))?;
    Ok(out)
}

/// The API key: from 1Password when `reference` is an `op://` reference, else `ANTHROPIC_API_KEY`.
pub fn api_key(reference: &str) -> Result<String, String> {
    let r = reference.trim();
    if r.is_empty() {
        return std::env::var("ANTHROPIC_API_KEY").ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).ok_or_else(|| {
            "no API key: set a 1Password reference (op://…) in the Claude section, or start the app with ANTHROPIC_API_KEY set".to_string()
        });
    }
    if !r.starts_with("op://") || r.len() > 512 || r.chars().any(char::is_control) {
        return Err("the 1Password reference should look like op://Vault/Item/field".into());
    }
    // An app opened from /Applications has a short PATH: look where Homebrew and the installer put `op`.
    let op = ["/opt/homebrew/bin/op", "/usr/local/bin/op"].into_iter().find(|p| std::path::Path::new(p).exists()).unwrap_or("op");
    let out = std::process::Command::new(op).args(["read", "--no-newline", r]).output().map_err(|e| {
        format!("can't run the 1Password CLI ({e}). Install it (brew install 1password-cli) and turn on Settings ▸ Developer ▸ Integrate with 1Password CLI in the 1Password app")
    })?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().chars().take(300).collect::<String>();
        return Err(format!("1Password didn't give the key: {why}"));
    }
    let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if key.is_empty() { Err("1Password gave an empty key".into()) } else { Ok(key) }
}

/// Send the request and read the answer (blocking: run it on its own thread).
pub fn ask(key: &str, body: &Value, cs: &CameraSettings) -> Result<Reply, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).http_status_as_error(false).build().into();
    let mut req = agent.post(URL).header("x-api-key", key).header("anthropic-version", VERSION).header("content-type", "application/json");
    if body.get("fallbacks").is_some() {
        req = req.header("anthropic-beta", "server-side-fallback-2026-07-01");
    }
    let mut resp = req.send(body.to_string()).map_err(|e| format!("can't reach the Claude API: {e}"))?;
    let status = resp.status().as_u16();
    let bytes = resp.body_mut().with_config().limit(MAX_REPLY).read_to_vec().map_err(|e| format!("can't read Claude's answer: {e}"))?;
    match status {
        200 => parse_reply(&bytes, cs),
        401 => Err("the API key was refused (401)".into()),
        429 => Err("too many requests just now (429): try again in a minute".into()),
        s => Err(parse_reply(&bytes, cs).err().unwrap_or_else(|| format!("the API answered HTTP {s}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn cs() -> CameraSettings {
        CameraSettings {
            values: [("shtrspeed", "1536/256"), ("focal", "1280/256"), ("iso", "200"), ("whitebalance", "auto")]
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            lens: apex::Lens::parse(lumix_link::sim::SIM_LENS.as_bytes()),
            ..CameraSettings::default()
        }
    }

    fn look() -> Look {
        let rgba = [200, 190, 170, 255].repeat(16);
        crate::review::look(&DecodedFrame { seq: 1, width: 4, height: 4, rgba: Arc::new(rgba) })
    }

    #[test]
    fn the_request_offers_only_what_the_camera_takes() {
        let body = request("claude-haiku-5-5", b"jpeg", &look(), &cs());
        assert_eq!(body["model"], "claude-haiku-5-5");
        assert!(body.get("fallbacks").is_none(), "Haiku has no server-side fallback");
        let props = &body["output_config"]["format"]["schema"]["properties"];
        let wb: Vec<&str> = props["white_balance"]["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(wb.first(), Some(&"keep"));
        assert!(wb.contains(&"Daylight"));
        assert!(props["aperture"]["enum"].as_array().unwrap().iter().any(|v| v == "f/5.6"));
        let text = body["messages"][0]["content"][1]["text"].as_str().unwrap();
        assert!(text.contains("99.5th percentile") && text.contains("White balance: Auto"), "{text}");
        assert_eq!(request("claude-opus-5-5", b"j", &look(), &cs())["fallbacks"], "default");
    }

    fn answer(advice: &str) -> Vec<u8> {
        json!({
            "model": "claude-haiku-5-5",
            "stop_reason": "end_turn",
            "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": advice}],
            "usage": {"input_tokens": 2000, "output_tokens": 200},
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn reads_and_checks_the_answer() {
        let a = r#"{"assessment":"A little warm.","good":false,"shutter":"keep","aperture":"f/5.6","iso":"keep","white_balance":"Daylight"}"#;
        let r = parse_reply(&answer(a), &cs()).unwrap();
        assert_eq!(r.changes, [("whitebalance".to_string(), "daylight".to_string())], "f/5.6 is already set");
        assert!((r.cost() - 0.0003).abs() < 1e-9);

        let bad = r#"{"assessment":"x","good":false,"shutter":"1/7777","aperture":"keep","iso":"keep","white_balance":"keep"}"#;
        assert!(parse_reply(&answer(bad), &cs()).is_err(), "not a shutter speed of this lens' list");
        assert!(parse_reply(br#"{"stop_reason":"refusal","content":[]}"#, &cs()).unwrap_err().contains("declined"));
        assert!(parse_reply(br#"{"type":"error","error":{"message":"invalid x-api-key"}}"#, &cs()).unwrap_err().contains("invalid x-api-key"));
        assert!(parse_reply(b"<html>", &cs()).is_err());
    }

    #[test]
    fn key_references_are_checked() {
        assert!(api_key("1234").unwrap_err().contains("op://"));
        assert!(api_key("op://a\nb").is_err());
    }
}
