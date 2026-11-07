//! The real camera link over `std::net` (native builds only): HTTP GETs on port 80, live view on
//! a UDP socket, and SSDP discovery. Every call has a timeout, so a camera that is off or out of
//! range gives an error, never a hang.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use crate::ssdp::{self, Found};
use crate::transport::{HttpReply, Transport};

/// Connecting to the camera's web server.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(2000);
/// Waiting for its answer.
pub const READ_TIMEOUT: Duration = Duration::from_millis(3000);
/// The largest HTTP answer read (a device description; `cam.cgi` replies are far smaller).
pub const MAX_HTTP: usize = 512 * 1024;

/// The camera's address: an IPv4 or IPv6 address (no host names: nothing is looked up).
pub fn parse_ip(s: &str) -> Result<IpAddr, String> {
    s.trim().parse::<IpAddr>().map_err(|_| format!("\"{}\" isn't an IP address (e.g. 192.168.54.1)", s.chars().take(60).collect::<String>()))
}

/// The real transport.
#[derive(Debug)]
pub struct NetTransport {
    camera: IpAddr,
    http_port: u16,
    udp: UdpSocket,
    udp_port: u16,
}

impl NetTransport {
    /// Talk to the camera at `camera`, live view coming in on `udp_port` (or, when it is taken,
    /// any free port: the camera is told which).
    pub fn open(camera: IpAddr, udp_port: u16) -> Result<Self, String> {
        let any: IpAddr = if camera.is_ipv4() { [0, 0, 0, 0].into() } else { [0u16; 8].into() };
        let udp = UdpSocket::bind(SocketAddr::new(any, udp_port))
            .or_else(|_| UdpSocket::bind(SocketAddr::new(any, 0)))
            .map_err(|e| format!("can't open a UDP port for live view: {e}"))?;
        let udp_port = udp.local_addr().map_err(|e| e.to_string())?.port();
        Ok(NetTransport { camera, http_port: 80, udp, udp_port })
    }

    pub fn camera(&self) -> IpAddr {
        self.camera
    }
}

impl Transport for NetTransport {
    fn get(&mut self, path: &str) -> Result<HttpReply, String> {
        http_get(SocketAddr::new(self.camera, self.http_port), &self.camera.to_string(), path)
    }

    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Result<Option<usize>, String> {
        let wait = Duration::from_millis(wait_ms.clamp(1, 1000));
        self.udp.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
        match self.udp.recv_from(buf) {
            // Only the camera's datagrams count.
            Ok((n, from)) if from.ip() == self.camera => Ok(Some(n)),
            Ok(_) => Ok(None),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn udp_port(&self) -> u16 {
        self.udp_port
    }
}

/// GET `path` from `addr` (`host` for the Host header).
pub fn http_get(addr: SocketAddr, host: &str, path: &str) -> Result<HttpReply, String> {
    let mut s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| no_answer(addr, &e))?;
    s.set_read_timeout(Some(READ_TIMEOUT)).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(READ_TIMEOUT)).map_err(|e| e.to_string())?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: Picabia\r\nAccept: */*\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).map_err(|e| no_answer(addr, &e))?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend(chunk.get(..n).unwrap_or_default());
                if raw.len() > MAX_HTTP {
                    return Err(format!("the answer from {addr} is larger than {} KiB", MAX_HTTP / 1024));
                }
                if complete(&raw) {
                    break;
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) && !raw.is_empty() => break,
            Err(e) => return Err(no_answer(addr, &e)),
        }
    }
    parse_http(&raw)
}

/// GET an `http://host[:port]/path` URL (a device description from SSDP).
pub fn fetch(url: &str) -> Result<HttpReply, String> {
    let host = ssdp::host_of(url).ok_or_else(|| format!("not an http:// URL: {url}"))?;
    let rest = url.strip_prefix("http://").unwrap_or_default();
    let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, "/".into()));
    let port = authority.rsplit_once(':').filter(|_| !authority.ends_with(']')).and_then(|(_, p)| p.parse::<u16>().ok()).unwrap_or(80);
    let ip = parse_ip(&host)?;
    http_get(SocketAddr::new(ip, port), authority, &path)
}

fn no_answer(addr: SocketAddr, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            format!("no answer from {} (is the camera on, and on this network?)", addr.ip())
        }
        std::io::ErrorKind::ConnectionRefused => format!("{} refused the connection (is it the camera?)", addr.ip()),
        _ => format!("can't reach {}: {e}", addr.ip()),
    }
}

/// The whole answer has arrived (headers, and a body of its `Content-Length`).
fn complete(raw: &[u8]) -> bool {
    let Some(end) = header_end(raw) else { return false };
    let head = String::from_utf8_lossy(raw.get(..end).unwrap_or_default()).to_ascii_lowercase();
    head.lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .is_some_and(|len| raw.len() >= end + 4 + len)
}

fn header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Read an HTTP/1.x answer: status, headers and body (a chunked body decoded; one with a
/// `Content-Length` cut to it).
pub fn parse_http(raw: &[u8]) -> Result<HttpReply, String> {
    let end = header_end(raw).ok_or("the camera's answer was cut short")?;
    let head = String::from_utf8_lossy(raw.get(..end).unwrap_or_default()).to_string();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    if !status_line.starts_with("HTTP/1.") {
        return Err("the answer isn't HTTP".into());
    }
    let status = status_line.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()).ok_or("the answer has no status code")?;
    let mut chunked = false;
    let mut length = None;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
            "content-length" => length = v.trim().parse::<usize>().ok(),
            _ => {}
        }
    }
    let body = raw.get(end + 4..).unwrap_or_default();
    let body = if chunked {
        dechunk(body)?
    } else if let Some(n) = length {
        body.get(..n.min(body.len())).unwrap_or_default().to_vec()
    } else {
        body.to_vec()
    };
    Ok(HttpReply { status, body })
}

/// A chunked body's bytes.
fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for _ in 0..10_000 {
        let line_end = b.windows(2).position(|w| w == b"\r\n").ok_or("a chunk was cut short")?;
        let size_text = String::from_utf8_lossy(b.get(..line_end).unwrap_or_default()).to_string();
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16).map_err(|_| "a chunk size isn't hex")?;
        b = b.get(line_end + 2..).unwrap_or_default();
        if size == 0 {
            return Ok(out);
        }
        out.extend(b.get(..size).ok_or("a chunk was cut short")?);
        if out.len() > MAX_HTTP {
            return Err("the answer is too large".into());
        }
        b = b.get(size + 2..).unwrap_or_default();
    }
    Err("too many chunks".into())
}

/// Ask the network who is there (SSDP), listening for `timeout_ms` (200-10000).
pub fn discover(timeout_ms: u64) -> Result<Vec<Found>, String> {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("can't open a UDP port: {e}"))?;
    let _ = sock.set_multicast_ttl_v4(2); // the default (1) works on one network too: failing to raise it is fine
    for t in ssdp::TARGETS {
        sock.send_to(ssdp::m_search(t, 2).as_bytes(), ssdp::MULTICAST).map_err(|e| format!("can't send the search: {e}"))?;
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.clamp(200, 10_000));
    let mut found = Vec::new();
    let mut buf = [0u8; 4096];
    while let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
        sock.set_read_timeout(Some(left)).map_err(|e| e.to_string())?;
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                if let Some(f) = ssdp::parse_response(buf.get(..n).unwrap_or_default()) {
                    ssdp::add(&mut found, f);
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_http() {
        let r = parse_http(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello world").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"hello"[..]));
        let r = parse_http(b"HTTP/1.0 404 Not Found\r\n\r\n").unwrap();
        assert_eq!(r.status, 404);
        let r = parse_http(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n").unwrap();
        assert_eq!(r.body, b"abcde");
    }

    #[test]
    fn refuses_bad_http() {
        assert!(parse_http(b"").is_err());
        assert!(parse_http(b"SSH-2.0\r\n\r\n").is_err());
        assert!(parse_http(b"HTTP/1.1 abc\r\n\r\n").is_err());
        assert!(parse_http(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").is_err());
        assert!(parse_http(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffff\r\nab").is_err());
    }

    #[test]
    fn parses_ips() {
        assert!(parse_ip(" 192.168.54.1 ").is_ok());
        assert!(parse_ip("::1").is_ok());
        assert!(parse_ip("camera.local").is_err());
    }

    /// The transport against a camera served from this machine: a tiny HTTP server on loopback
    /// answering `cam.cgi`, and live view datagrams sent to the transport's port.
    #[test]
    fn talks_to_a_loopback_camera() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let (mut c, _) = server.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = c.read(&mut buf).unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET /cam.cgi?mode=getstate HTTP/1.1\r\n"));
            c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 40\r\n\r\n<camrply><result>ok</result></camrply>\r\n").unwrap();
        });
        let ip: IpAddr = [127, 0, 0, 1].into();
        let mut net = NetTransport::open(ip, 0).unwrap();
        net.http_port = port;
        let r = net.get("/cam.cgi?mode=getstate").unwrap();
        assert!(crate::Reply::parse(&r.body).ok());
        t.join().unwrap();

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(&crate::sim::stand_in_jpeg(1), ("127.0.0.1", net.udp_port())).unwrap();
        let mut buf = vec![0u8; crate::frame::MAX_DATAGRAM];
        let mut got = None;
        for _ in 0..50 {
            if let Some(n) = net.recv(&mut buf, 100).unwrap() {
                got = Some(n);
                break;
            }
        }
        assert_eq!(got, Some(crate::sim::stand_in_jpeg(1).len()));
    }

    #[test]
    fn a_closed_port_is_an_error_not_a_hang() {
        // Port 9 (discard) on loopback is closed on CI machines: refused at once.
        let r = http_get(SocketAddr::new([127, 0, 0, 1].into(), 9), "127.0.0.1", "/");
        assert!(r.is_err());
    }
}
