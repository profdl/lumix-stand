//! Finding cameras on a network with SSDP (UPnP discovery, UPnP Device Architecture 2.0 §1.3).
//!
//! When the camera joins another network instead of hosting its own, its address isn't known.
//! *Hypothesis* (community notes): Lumix bodies answer an M-SEARCH as a DLNA media server, and
//! their answer's `SERVER` header names Panasonic. Everything that answers is listed; the ones that
//! look like a Lumix come first.

/// SSDP's multicast address and port.
pub const MULTICAST: &str = "239.255.255.250:1900";
/// The search targets asked for.
pub const TARGETS: [&str; 2] = ["urn:schemas-upnp-org:device:MediaServer:1", "upnp:rootdevice"];
/// The most answers kept.
pub const MAX_FOUND: usize = 64;

/// The M-SEARCH request for `target`, waiting at most `mx` seconds (1-5).
pub fn m_search(target: &str, mx: u8) -> String {
    format!("M-SEARCH * HTTP/1.1\r\nHOST: {MULTICAST}\r\nMAN: \"ssdp:discover\"\r\nMX: {}\r\nST: {target}\r\n\r\n", mx.clamp(1, 5))
}

/// A device that answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// The host in its `LOCATION` (where its device description is).
    pub ip: String,
    pub location: String,
    pub server: String,
    pub st: String,
    pub usn: String,
    /// The answer names Panasonic or Lumix.
    pub likely_lumix: bool,
}

/// Read an M-SEARCH answer; `None` when it isn't one (no `200` status line, no `LOCATION`).
pub fn parse_response(datagram: &[u8]) -> Option<Found> {
    let text = String::from_utf8_lossy(datagram.get(..4096.min(datagram.len()))?);
    let mut lines = text.split("\r\n").flat_map(|l| l.split('\n'));
    let status = lines.next()?;
    if !status.starts_with("HTTP/1.") || status.split_whitespace().nth(1).is_none_or(|c| c != "200") {
        return None;
    }
    let mut f = Found::default();
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim().to_string();
        match k.trim().to_ascii_lowercase().as_str() {
            "location" => f.location = v,
            "server" => f.server = v,
            "st" => f.st = v,
            "usn" => f.usn = v,
            _ => {}
        }
    }
    f.ip = host_of(&f.location)?;
    let all = format!("{} {} {}", f.server, f.usn, f.location).to_ascii_lowercase();
    f.likely_lumix = all.contains("panasonic") || all.contains("lumix");
    Some(f)
}

/// The host of an `http://host[:port]/…` URL (IPv4 or a bracketed IPv6 address).
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let authority = rest.split('/').next()?;
    let host = if let Some(v6) = authority.strip_prefix('[') { v6.split(']').next()? } else { authority.split(':').next()? };
    (!host.is_empty()).then(|| host.to_string())
}

/// Add `f` to `list` unless its address is there already; the likely cameras first.
pub fn add(list: &mut Vec<Found>, f: Found) {
    if list.len() >= MAX_FOUND {
        return;
    }
    if let Some(old) = list.iter_mut().find(|o| o.ip == f.ip) {
        if f.likely_lumix && !old.likely_lumix {
            *old = f;
        }
    } else {
        list.push(f);
    }
    list.sort_by_key(|f| !f.likely_lumix);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_answer() {
        let a = b"HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: http://192.168.1.42:60606/Dms/ddd.xml\r\nSERVER: Linux/2.6 UPnP/1.0 Panasonic-DLNA-DMS/1.0\r\nST: upnp:rootdevice\r\nUSN: uuid:4D454930::upnp:rootdevice\r\n\r\n";
        let f = parse_response(a).unwrap();
        assert_eq!(f.ip, "192.168.1.42");
        assert!(f.likely_lumix);
        assert_eq!(f.st, "upnp:rootdevice");
    }

    #[test]
    fn refuses_junk() {
        assert!(parse_response(b"").is_none());
        assert!(parse_response(b"NOTIFY * HTTP/1.1\r\nLOCATION: http://1.2.3.4/\r\n").is_none());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nSERVER: x\r\n").is_none(), "no location");
        assert!(parse_response(&[0xFF; 9000]).is_none());
        assert_eq!(host_of("http://[fe80::1]:80/x").as_deref(), Some("fe80::1"));
        assert_eq!(host_of("ftp://x/"), None);
        assert_eq!(host_of("http:///x"), None);
    }

    #[test]
    fn lists_cameras_first_once_each() {
        let mut l = Vec::new();
        add(&mut l, Found { ip: "10.0.0.2".into(), ..Default::default() });
        add(&mut l, Found { ip: "10.0.0.3".into(), likely_lumix: true, ..Default::default() });
        add(&mut l, Found { ip: "10.0.0.2".into(), ..Default::default() });
        assert_eq!(l.iter().map(|f| f.ip.as_str()).collect::<Vec<_>>(), ["10.0.0.3", "10.0.0.2"]);
    }

    #[test]
    fn search_request() {
        let s = m_search(TARGETS[0], 9);
        assert!(s.starts_with("M-SEARCH * HTTP/1.1\r\n"));
        assert!(s.contains("MX: 5\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
    }
}
