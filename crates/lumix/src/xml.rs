//! A small, lenient reader for the camera's XML: its tags in order, each with its attributes and
//! the text that follows it. Enough for `getsetting` (`<settingvalue iso="400">`), `curmenu`
//! (`<item id="…" enable="yes" value="…"/>`) and a DLNA `Browse` result. The input is untrusted:
//! it is never indexed unchecked, and at most [`MAX_TAGS`] tags are read.

/// The most tags read from one document.
pub const MAX_TAGS: usize = 20_000;

/// One opening (or self-closing) tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag<'a> {
    /// The name, namespace prefix included (`dc:title`).
    pub name: &'a str,
    attrs: &'a str,
    /// The text between this tag and the next one, trimmed (not unescaped).
    pub text: &'a str,
}

impl Tag<'_> {
    /// The attribute `key`'s value, unescaped.
    pub fn attr(&self, key: &str) -> Option<String> {
        let mut rest = self.attrs;
        for _ in 0..64 {
            let eq = rest.find('=')?;
            let k = rest.get(..eq).unwrap_or_default().trim();
            let after = rest.get(eq + 1..).unwrap_or_default().trim_start();
            let quote = after.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let body = after.get(1..).unwrap_or_default();
            let end = body.find(quote)?;
            if k == key {
                return Some(unescape(body.get(..end).unwrap_or_default()));
            }
            rest = body.get(end + 1..).unwrap_or_default();
        }
        None
    }

    /// The text after the tag, unescaped.
    pub fn text(&self) -> String {
        unescape(self.text)
    }
}

/// Every opening or self-closing tag in `text`, in order (closing tags, `<?…?>` and `<!…>` are
/// skipped).
pub fn tags(text: &str) -> Vec<Tag<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    while out.len() < MAX_TAGS {
        let Some(open) = rest.find('<') else { break };
        rest = rest.get(open + 1..).unwrap_or_default();
        let Some(close) = rest.find('>') else { break };
        let inner = rest.get(..close).unwrap_or_default();
        rest = rest.get(close + 1..).unwrap_or_default();
        if inner.starts_with(['/', '?', '!']) {
            continue;
        }
        let inner = inner.strip_suffix('/').unwrap_or(inner);
        let (name, attrs) = inner.split_once(char::is_whitespace).unwrap_or((inner, ""));
        if !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            continue;
        }
        let text = rest.get(..rest.find('<').unwrap_or(rest.len())).unwrap_or_default().trim();
        out.push(Tag { name, attrs, text });
    }
    out
}

/// The five predefined XML entities.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tags_attributes_and_text() {
        let t = tags(
            r#"<?xml version="1.0"?><camrply><result>ok</result><settingvalue iso="400" x='a &amp; b'></settingvalue><item id="i" enable="no" /></camrply>"#,
        );
        let names: Vec<_> = t.iter().map(|t| t.name).collect();
        assert_eq!(names, ["camrply", "result", "settingvalue", "item"]);
        assert_eq!(t[1].text, "ok");
        assert_eq!(t[2].attr("iso").as_deref(), Some("400"));
        assert_eq!(t[2].attr("x").as_deref(), Some("a & b"));
        assert_eq!(t[2].attr("missing"), None);
        assert_eq!(t[3].attr("enable").as_deref(), Some("no"));
    }

    #[test]
    fn survives_junk() {
        assert!(tags("").is_empty());
        assert!(tags("<<<>>>").is_empty());
        assert_eq!(tags("<a b=\"unclosed>").len(), 1);
        assert_eq!(tags("<a b=\"unclosed>")[0].attr("b"), None);
        assert_eq!(tags("<a b=c>")[0].attr("b"), None);
        let many = "<x/>".repeat(MAX_TAGS + 10);
        assert_eq!(tags(&many).len(), MAX_TAGS);
    }
}
