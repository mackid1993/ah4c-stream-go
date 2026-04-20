// ah4c-stream: standalone AH4C CMD-mode tuner. Reads an HDMI encoder's
// HTTP/1.0 stream and forwards it to stdout (which AH4C pipes to Channels
// DVR). If the source goes silent for 500 ms, injects MPEG-TS NULL packets
// so the HTTP response keeps making forward progress. If silent for 5 s,
// reconnects. If silent for 15 s total, exits so DVR sees EOF and reacts.

use std::env;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::exit;
use std::thread::sleep;
use std::time::{Duration, Instant};

const STALL: Duration = Duration::from_millis(500);
const CONNECT: Duration = Duration::from_secs(5);
const BACKOFF: Duration = Duration::from_secs(2);
const BUDGET: Duration = Duration::from_secs(15);
const IDLE_TICKS: u32 = 10; // 10 × 500 ms = 5 s source-idle → reconnect
const CHUNK: usize = 32 * 1024;

fn main() {
    let url = env::args().nth(1).unwrap_or_else(|| { eprintln!("usage: ah4c-stream <url>"); exit(2) });
    let null = make_null();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let (mut stream, leftover) = connect(&url).unwrap_or_else(|e| {
        eprintln!("initial connect failed: {}", e); exit(2)
    });
    if out.write_all(&leftover).is_err() { return; }

    let mut buf = [0u8; CHUNK];
    let mut last_real = Instant::now();
    let mut stalls = 0u32;

    loop {
        if last_real.elapsed() >= BUDGET { return; }
        stream.set_read_timeout(Some(STALL)).ok();
        match stream.read(&mut buf) {
            Ok(n) if n > 0 => {
                if out.write_all(&buf[..n]).is_err() { return; }
                last_real = Instant::now();
                stalls = 0;
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                if out.write_all(&null).is_err() { return; }
                stalls += 1;
                if stalls >= IDLE_TICKS {
                    stream = match reconnect(&url, &null, &mut out, &mut last_real) {
                        Some(s) => { stalls = 0; s }
                        None => return,
                    };
                }
            }
            _ => {
                stream = match reconnect(&url, &null, &mut out, &mut last_real) {
                    Some(s) => { stalls = 0; s }
                    None => return,
                };
            }
        }
    }
}

fn reconnect<W: Write>(url: &str, null: &[u8], out: &mut W, last_real: &mut Instant) -> Option<TcpStream> {
    loop {
        if last_real.elapsed() >= BUDGET { return None; }
        match connect(url) {
            Ok((s, leftover)) => {
                if out.write_all(&leftover).is_err() { return None; }
                *last_real = Instant::now();
                eprintln!("reconnected");
                return Some(s);
            }
            Err(_) => {
                let deadline = Instant::now() + BACKOFF;
                while Instant::now() < deadline {
                    if last_real.elapsed() >= BUDGET { return None; }
                    if out.write_all(null).is_err() { return None; }
                    sleep(STALL);
                }
            }
        }
    }
}

fn connect(url: &str) -> std::io::Result<(TcpStream, Vec<u8>)> {
    let s = url.strip_prefix("http://").ok_or_else(|| ioerr("http:// only"))?;
    let (hp, path) = s.split_once('/').map(|(a, b)| (a.to_string(), format!("/{}", b)))
        .unwrap_or_else(|| (s.to_string(), "/".into()));
    let hp_full = if hp.contains(':') { hp.clone() } else { format!("{}:80", hp) };
    let addr = hp_full.to_socket_addrs()?.next().ok_or_else(|| ioerr("dns"))?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(CONNECT))?;
    stream.write_all(format!("GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n", path, hp).as_bytes())?;

    let mut hdr = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 { return Err(ioerr("eof during headers")); }
        hdr.extend_from_slice(&tmp[..n]);
        if let Some(p) = hdr.windows(4).position(|w| w == b"\r\n\r\n") { break p; }
        if hdr.len() > 8192 { return Err(ioerr("headers too large")); }
    };
    let leftover = hdr.split_off(end + 4);
    let first = std::str::from_utf8(&hdr).unwrap_or("").lines().next().unwrap_or("");
    if !first.contains(" 200 ") { return Err(ioerr(first.trim())); }
    stream.set_read_timeout(None)?;
    Ok((stream, leftover))
}

fn make_null() -> Vec<u8> {
    // 174 × 188-byte TS NULL packets = 32,712 bytes (≤ 32 KiB).
    // sync=0x47, PID=0x1FFF, adaptation=01, CC=0, payload=0xFF.
    let mut v = Vec::with_capacity(174 * 188);
    for _ in 0..174 {
        v.extend_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
        v.extend(std::iter::repeat(0xFF).take(184));
    }
    v
}

fn ioerr(s: &str) -> std::io::Error { std::io::Error::new(ErrorKind::Other, s.to_string()) }
