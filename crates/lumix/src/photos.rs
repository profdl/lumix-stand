//! The photos on the camera's card, at full resolution.
//!
//! Measured on a GX85 (firmware D2.91, 2026-11-07), with the Mac on the camera's own Wi-Fi:
//! - The card is listed by the DLNA media server on port 60606: a UPnP ContentDirectory `Browse`
//!   (SOAP over HTTP POST) to `/Server0/CDS_control`. It answers in about 60 ms **in playback
//!   mode** (`camcmd&value=playmode`). In record mode it once answered, then failed (`Action
//!   Failed`, after 27 s). The list is oldest first: `<TotalMatches>` gives the count, and
//!   asking from `count − 1` gives the newest.
//! - Each item's id is the photo's folder and number (`1230211`: `P1230211.JPG` in folder 123).
//!   It offers the original (`http://<camera>:50001/DO1230211.JPG`), a 640×360 preview (`DS…`)
//!   and a thumbnail (`DT…`). RAW files aren't offered: with RAW + JPEG, the JPEG is.
//! - The original downloads in **record mode** too, with no mode switch: 6.5 MB (4592×2584) in
//!   18 s (10 s in playback mode). A missing one answers 404 at once.
//! - After a capture, the new photo is the next number, and is there 1-4 s after the shutter.
//!
//! So the link lists the card once (at connect, in playback mode, before live view), then
//! expects each capture to be the next number, and lists again only when one isn't found.

/// The media server's port.
pub const DLNA_PORT: u16 = 60606;
/// The ContentDirectory control path.
pub const BROWSE_PATH: &str = "/Server0/CDS_control";
/// The SOAP action of a `Browse`.
pub const BROWSE_ACTION: &str = "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"";
/// The port the photos are served on.
pub const FILE_PORT: u16 = 50001;
/// The largest original downloaded (the GX85's are 3-10 MB).
pub const MAX_ORIGINAL: u64 = 64 * 1024 * 1024;
/// The largest preview downloaded (the GX85's 640×360 previews are well under 1 MB).
pub const MAX_PREVIEW: u64 = 4 * 1024 * 1024;

/// A photo on the card: its folder × 10000 + its number (`1230211`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhotoId(pub u32);

impl PhotoId {
    /// From the camera's id (`"1230211"`): folder 100-999, number 1-9999.
    pub fn parse(s: &str) -> Option<PhotoId> {
        let s = s.trim();
        if s.len() != 7 || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let id: u32 = s.parse().ok()?;
        (id / 10_000 >= 100 && id % 10_000 >= 1).then_some(PhotoId(id))
    }

    /// The number the camera gives the next photo (*hypothesis* for the end of a folder: 9999 is
    /// followed by the next folder's 0001; when a photo isn't found where expected, the card is
    /// listed again).
    pub fn next(self) -> PhotoId {
        if self.0 % 10_000 >= 9_999 { PhotoId((self.0 / 10_000 + 1) * 10_000 + 1) } else { PhotoId(self.0 + 1) }
    }

    /// The original's path on [`FILE_PORT`].
    pub fn original_path(self) -> String {
        format!("/DO{:07}.JPG", self.0)
    }

    /// The 640×360 preview's path on [`FILE_PORT`] (made by the camera from the photo itself, so
    /// it shows the real exposure, unlike live view).
    pub fn preview_path(self) -> String {
        format!("/DS{:07}.JPG", self.0)
    }

    /// The camera's own file name (`P1230211.JPG`).
    pub fn file_name(self) -> String {
        format!("P{:07}.JPG", self.0)
    }
}

/// A `Browse` of the card's top level, `count` items from `start`.
pub fn browse_body(start: u32, count: u32) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
<u:Browse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\"><ObjectID>0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag>\
<Filter>*</Filter><StartingIndex>{start}</StartingIndex><RequestedCount>{count}</RequestedCount><SortCriteria></SortCriteria></u:Browse>\
</s:Body></s:Envelope>"
    )
}

/// What a `Browse` answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// Photos on the card.
    pub total: u32,
    /// The photos answered, in order.
    pub photos: Vec<PhotoId>,
}

/// The most of a `Browse` answer that is read.
pub const MAX_BROWSE: usize = 512 * 1024;

/// Read a `Browse` answer; `None` when it isn't one.
pub fn parse_browse(body: &[u8]) -> Option<Listing> {
    let text = String::from_utf8_lossy(body.get(..body.len().min(MAX_BROWSE)).unwrap_or_default()).to_string();
    let outer = crate::xml::tags(&text);
    let total = outer.iter().find(|t| t.name == "TotalMatches")?.text.trim().parse().ok()?;
    // The DIDL-Lite list is escaped inside <Result>.
    let didl = outer.iter().find(|t| t.name == "Result").map(crate::xml::Tag::text).unwrap_or_default();
    let photos = crate::xml::tags(&didl)
        .iter()
        .filter(|t| t.name == "item")
        .filter_map(|t| t.attr("id"))
        .filter_map(|id| PhotoId::parse(&id))
        .take(1000)
        .collect();
    Some(Listing { total, photos })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A camera answer, cut down (2026-11-07).
    const BROWSE: &str = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><Result>&lt;DIDL-Lite xmlns=&quot;urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/&quot;&gt;&lt;item id=&quot;1230210&quot; parentID=&quot;0&quot; restricted=&quot;0&quot;&gt;&lt;dc:title&gt;123-0210&lt;/dc:title&gt;&lt;res protocolInfo=&quot;http-get:*:image/jpeg:DLNA.ORG_PN=JPEG_LRG;PANASONIC.COM_PN=CAM_ORG&quot; size=&quot;4909056&quot;&gt;http://192.168.54.1:50001/DO1230210.JPG&lt;/res&gt;&lt;/item&gt;&lt;item id=&quot;1230211&quot; parentID=&quot;0&quot;&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</Result><NumberReturned>2</NumberReturned><TotalMatches>293</TotalMatches><UpdateID>1</UpdateID></u:BrowseResponse></s:Body></s:Envelope>"#;

    #[test]
    fn reads_a_listing() {
        let l = parse_browse(BROWSE.as_bytes()).unwrap();
        assert_eq!(l.total, 293);
        assert_eq!(l.photos, [PhotoId(1_230_210), PhotoId(1_230_211)]);
        assert!(parse_browse(b"<s:Fault>Action Failed</s:Fault>").is_none());
        assert!(parse_browse(b"").is_none());
        let empty = parse_browse(b"<Result></Result><TotalMatches>0</TotalMatches>").unwrap();
        assert_eq!(empty, Listing::default());
    }

    #[test]
    fn photo_ids() {
        let p = PhotoId::parse("1230211").unwrap();
        assert_eq!(p.original_path(), "/DO1230211.JPG");
        assert_eq!(p.file_name(), "P1230211.JPG");
        assert_eq!(p.next(), PhotoId(1_230_212));
        assert_eq!(p.original_path(), "/DO1230211.JPG");
        assert_eq!(p.preview_path(), "/DS1230211.JPG");
        assert_eq!(PhotoId(1_239_999).next(), PhotoId(1_240_001));
        assert!(PhotoId::parse("123021").is_none());
        assert!(PhotoId::parse("0990001").is_none(), "folders start at 100");
        assert!(PhotoId::parse("1230000").is_none(), "numbers start at 1");
        assert!(PhotoId::parse("12302x1").is_none());
    }

    #[test]
    fn the_browse_request() {
        let b = browse_body(292, 1);
        assert!(b.contains("<StartingIndex>292</StartingIndex><RequestedCount>1</RequestedCount>"));
        assert!(b.contains("<ObjectID>0</ObjectID>"));
    }
}
