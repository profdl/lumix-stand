//! Probe a real Lumix camera and record what it does, so `docs/gx85.md` can say
//! what the camera does instead of what community notes guess.
//!
//! ```text
//! cargo run -p lumix-link --example probe -- [--ip 192.168.54.1] [--port 49199] [--out DIR]
//!     [--frames 10] [--seconds 6] [--handshake] [--no-capture] [--no-measure-timeout] [--discover]
//!     [--settings [--no-set] [--try-focus]]
//! ```
//!
//! Without `--ip` it searches the network first (SSDP) and uses the camera it finds, else
//! 192.168.54.1 (the camera's own Wi-Fi).
//!
//! ```text
//! ```
//!
//! Steps: (`--discover`: SSDP search first, saving each answer and device description), `getstate`,
//! the access handshake (when asked for, or when the camera refuses), `recmode`, `startstream`,
//! live view for `--seconds` (the first datagrams' headers hex-dumped, `--frames` frames saved as
//! JPEGs, the time between frames), (`--measure-timeout`: how long the stream lasts with no
//! further command), (`--capture`: fire the shutter), `stopstream`. Everything goes to the terminal
//! and to `DIR/probe.log`.
//!
//! `--settings` explores the camera's settings instead of live view: after `recmode` it saves the
//! `getinfo` answers (`DIR/getinfo_*.xml`), reads each setting with `getsetting`, and tries
//! `setsetting` for shutter speed, aperture, ISO, exposure compensation, white balance and focus
//! mode: first the current value written back unchanged, then a few guessed values in different
//! formats. **Every change is undone at once** (the original value written back and read again),
//! and a summary at the end says what was accepted, refused and restored. `--no-set` only reads.
//! `--try-focus` also moves the focus one step each way (`camctrl`) and back, which can't be undone
//! exactly: use it only when the stand's focus can be set again.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lumix_link::frame::{MAX_DATAGRAM, hex_dump};
use lumix_link::net::{self, NetTransport};
use lumix_link::{Reply, Transport, extract_jpeg};

mod settings;

struct Args {
    ip: String,
    /// `--ip` was given (else the camera is searched for first).
    ip_given: bool,
    port: u16,
    out: PathBuf,
    frames: usize,
    seconds: u64,
    handshake: bool,
    capture: bool,
    measure_timeout: bool,
    discover: bool,
    settings: bool,
    no_set: bool,
    try_focus: bool,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        ip: lumix_link::DEFAULT_IP.into(),
        ip_given: false,
        port: lumix_link::DEFAULT_UDP_PORT,
        // Double-clicking the tool runs it with no options: put the results on the Desktop.
        out: std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Desktop").join("gx85-probe"))
            .unwrap_or_else(|| std::env::temp_dir().join("lumix-probe")),
        frames: 10,
        seconds: 6,
        handshake: false,
        capture: true,
        measure_timeout: true,
        discover: false,
        settings: false,
        no_set: false,
        try_focus: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{k} needs a value"));
        match k.as_str() {
            "--ip" => {
                a.ip = val()?;
                a.ip_given = true;
            }
            "--port" => a.port = val()?.parse().map_err(|_| "--port takes a number")?,
            "--out" => a.out = PathBuf::from(val()?),
            "--frames" => a.frames = val()?.parse().map_err(|_| "--frames takes a number")?,
            "--seconds" => a.seconds = val()?.parse().map_err(|_| "--seconds takes a number")?,
            "--handshake" => a.handshake = true,
            "--capture" => a.capture = true,
            "--no-capture" => a.capture = false,
            "--measure-timeout" => a.measure_timeout = true,
            "--no-measure-timeout" => a.measure_timeout = false,
            "--discover" => a.discover = true,
            "--settings" => a.settings = true,
            "--no-set" => a.no_set = true,
            "--try-focus" => a.try_focus = true,
            "-h" | "--help" => return Err(include_str!("main.rs").lines().take(14).collect::<Vec<_>>().join("\n")),
            _ => return Err(format!("unknown option {k} (--help lists them)")),
        }
    }
    a.frames = a.frames.min(1000);
    a.seconds = a.seconds.clamp(1, 600);
    Ok(a)
}

/// Prints to the terminal and the log file.
struct Log(std::fs::File);

impl Log {
    fn line(&mut self, s: &str) {
        println!("{s}");
        let _ = writeln!(self.0, "{s}"); // the terminal has it even if the log write fails
    }
}

fn main() -> Result<(), String> {
    let mut a = args()?;
    std::fs::create_dir_all(&a.out).map_err(|e| format!("can't make {}: {e}", a.out.display()))?;
    let log_path = a.out.join("probe.log");
    let mut log = Log(std::fs::File::create(&log_path).map_err(|e| format!("can't write {}: {e}", log_path.display()))?);
    log.line(&format!("Lumix probe, camera {} , live view port {}, output {}", a.ip, a.port, a.out.display()));
    // Without --ip, search the network first: on a shared network the camera's address isn't
    // known (and the search saves the device descriptions phase 2 needs).
    if a.discover || !a.ip_given {
        let found = discover(&a, &mut log);
        if !a.ip_given {
            match found {
                Some(ip) => {
                    log.line(&format!("using the camera found at {ip}"));
                    a.ip = ip;
                }
                None => log.line(&format!(
                    "no camera answered the search; trying {} (the camera's own Wi-Fi). On a shared network, find the camera's address in the router's device list and run again with --ip ADDRESS",
                    a.ip
                )),
            }
        }
    }
    let ip = net::parse_ip(&a.ip)?;
    let mut t = NetTransport::open(ip, a.port)?;
    log.line(&format!("live view will come in on UDP port {}", t.udp_port()));

    let state = cmd(&mut t, &mut log, "/cam.cgi?mode=getstate")?;
    if a.handshake || state.wants_handshake() || state.result.as_deref() == Some("err_reject") {
        wait_for_acceptance(&mut t, &mut log)?;
    }
    cmd(&mut t, &mut log, "/cam.cgi?mode=camcmd&value=recmode")?;
    if a.settings {
        settings::explore(&a, &mut t, &mut log);
        log.line(&format!("done: send {} and the getinfo_*.xml files beside it", log_path.display()));
        return Ok(());
    }
    let start = format!("/cam.cgi?mode=startstream&value={}", t.udp_port());
    cmd(&mut t, &mut log, &start)?;
    stream(&a, &mut t, &mut log, &start);
    if a.measure_timeout {
        measure_timeout(&mut t, &mut log, &start);
    }
    if a.capture {
        cmd(&mut t, &mut log, "/cam.cgi?mode=camcmd&value=capture")?;
    }
    cmd(&mut t, &mut log, "/cam.cgi?mode=stopstream")?;
    log.line(&format!("done: send {} and the frames beside it", log_path.display()));
    Ok(())
}

/// The access handshake, then wait (at most 90 s) until the camera takes commands.
///
/// Seen on a GX85 (2026-11-07): `accctrl` answers `ok_under_research,GX85-B3B1DA,remote,open` (not
/// XML), and every command right after it is refused with `err_reject`: the request seems to wait
/// for a person to accept it on the camera. So: ask, then try `getstate` every second, asking again
/// every 5 s, logging every change.
fn wait_for_acceptance(t: &mut NetTransport, log: &mut Log) -> Result<(), String> {
    let path = "/cam.cgi?mode=accctrl&type=req_acc&value=4D454930-0100-1000-8000-PICABIA00001&value2=Picabia";
    log.line("\n*** LOOK AT THE CAMERA'S SCREEN: if it asks whether to connect to \"Picabia\", accept. Waiting up to 90 s. ***");
    let began = Instant::now();
    let mut last_ask = None::<Instant>;
    let mut last_seen = String::new();
    while began.elapsed() < Duration::from_secs(90) {
        if last_ask.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
            last_ask = Some(Instant::now());
            match t.get(path) {
                Ok(r) => note_change(log, &mut last_seen, "accctrl", &String::from_utf8_lossy(&r.body), &began),
                Err(e) => note_change(log, &mut last_seen, "accctrl", &format!("! {e}"), &began),
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
        let r = t.get("/cam.cgi?mode=getstate")?;
        let reply = Reply::parse(&r.body);
        note_change(log, &mut last_seen, "getstate", reply.result.as_deref().unwrap_or("(no result)"), &began);
        if reply.ok() {
            log.line(&format!("accepted after {} s", began.elapsed().as_secs()));
            log.line(&String::from_utf8_lossy(&r.body));
            return Ok(());
        }
    }
    log.line("the camera never accepted within 90 s; carrying on anyway, to log what it says");
    Ok(())
}

/// Log `what`'s answer when it differs from the last one logged.
fn note_change(log: &mut Log, last: &mut String, what: &str, answer: &str, began: &Instant) {
    let line = format!("{what}: {}", answer.trim());
    if line != *last {
        log.line(&format!("[{:>2} s] {line}", began.elapsed().as_secs()));
        *last = line;
    }
}

/// Send one command and log the answer in full.
fn cmd(t: &mut NetTransport, log: &mut Log, path: &str) -> Result<Reply, String> {
    log.line(&format!("\n> GET {path}"));
    let started = Instant::now();
    match t.get(path) {
        Ok(r) => {
            log.line(&format!("< HTTP {} in {} ms, {} bytes", r.status, started.elapsed().as_millis(), r.body.len()));
            log.line(&String::from_utf8_lossy(&r.body));
            Ok(Reply::parse(&r.body))
        }
        Err(e) => {
            log.line(&format!("! {e}"));
            Err(e)
        }
    }
}

/// Live view for `a.seconds`, refreshing every 5 s.
fn stream(a: &Args, t: &mut NetTransport, log: &mut Log, start: &str) {
    log.line(&format!("\nlive view for {} s", a.seconds));
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let began = Instant::now();
    let mut last_refresh = Instant::now();
    let (mut datagrams, mut saved, mut bad, mut last_at) = (0usize, 0usize, 0usize, None::<Instant>);
    let mut gaps = Vec::new();
    while began.elapsed() < Duration::from_secs(a.seconds) {
        if last_refresh.elapsed() > Duration::from_secs(5) {
            let _ = cmd(t, log, "/cam.cgi?mode=getstate"); // logged; a failed refresh shows in the frame count
            let _ = cmd(t, log, start);
            last_refresh = Instant::now();
        }
        let Ok(Some(n)) = t.recv(&mut buf, 200) else { continue };
        let d = buf.get(..n).unwrap_or_default();
        datagrams += 1;
        if let Some(prev) = last_at {
            gaps.push(prev.elapsed().as_millis());
        }
        last_at = Some(Instant::now());
        if datagrams <= 3 {
            log.line(&format!("datagram {datagrams}: {n} bytes\n{}", hex_dump(d, 64)));
        }
        match extract_jpeg(d) {
            Ok(f) => {
                if datagrams <= 3 {
                    log.line(&format!("  header {} bytes, JPEG {} bytes", f.header_len, f.jpeg.len()));
                }
                if saved < a.frames {
                    let p = a.out.join(format!("frame_{:03}.jpg", saved + 1));
                    if std::fs::write(&p, &f.jpeg).is_ok() {
                        saved += 1;
                    }
                }
            }
            Err(e) => {
                bad += 1;
                if bad <= 3 {
                    log.line(&format!("  no frame in datagram {datagrams}: {e}\n{}", hex_dump(d, 96)));
                }
            }
        }
    }
    let avg = if gaps.is_empty() { 0 } else { gaps.iter().sum::<u128>() / gaps.len() as u128 };
    log.line(&format!(
        "live view: {datagrams} datagrams in {} s ({:.1}/s), {saved} frames saved, {bad} without a frame, {avg} ms apart on average, longest gap {} ms",
        a.seconds,
        datagrams as f64 / a.seconds as f64,
        gaps.iter().max().copied().unwrap_or(0)
    ));
    if datagrams == 0 {
        log.line("no datagrams: on macOS, allow the terminal under System Settings › Privacy & Security › Local Network, and check the firewall");
    }
}

/// How long the stream lasts with no further command (at most 60 s).
fn measure_timeout(t: &mut NetTransport, log: &mut Log, start: &str) {
    log.line("\nmeasuring how long live view lasts with no further command (up to 60 s)…");
    let _ = cmd(t, log, start); // a fresh start to time from
    let began = Instant::now();
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut last = Instant::now();
    while began.elapsed() < Duration::from_secs(60) {
        if let Ok(Some(_)) = t.recv(&mut buf, 200) {
            last = Instant::now();
        } else if last.elapsed() > Duration::from_secs(2) {
            break;
        }
    }
    log.line(&format!("live view stopped about {} ms after startstream", last.duration_since(began).as_millis()));
}

/// SSDP search; save each answer's device description. The first likely camera's address.
fn discover(a: &Args, log: &mut Log) -> Option<String> {
    log.line("\nSSDP search (3 s)…");
    match net::discover(3000) {
        Err(e) => {
            log.line(&format!("! {e}"));
            None
        }
        Ok(found) => {
            log.line(&format!("{} answered", found.len()));
            let mut camera = None;
            // A shared network can have many devices: at most 20 descriptions are fetched.
            for (i, f) in found.iter().enumerate().take(20) {
                log.line(&format!("- {} lumix={} server={} st={} usn={} location={}", f.ip, f.likely_lumix, f.server, f.st, f.usn, f.location));
                let mut lumix = f.likely_lumix;
                if let Ok(r) = net::fetch(&f.location) {
                    let p = a.out.join(format!("device_{}.xml", i + 1));
                    let _ = std::fs::write(&p, &r.body); // the log line above already has the location
                    log.line(&format!("  description saved: {}", p.display()));
                    // The description names the maker even when the search answer doesn't.
                    let text = String::from_utf8_lossy(&r.body).to_ascii_lowercase();
                    if text.contains("panasonic") || text.contains("lumix") {
                        log.line("  its description names Panasonic");
                        lumix = true;
                    }
                }
                if lumix && camera.is_none() {
                    camera = Some(f.ip.clone());
                }
            }
            camera
        }
    }
}
