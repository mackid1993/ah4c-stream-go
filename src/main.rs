// ah4c-stream: standalone AH4C CMD-mode tuner. Port of PR #9's
// stallTolerantReader semantics, two-threaded:
//
//   Producer — encoder socket; 5s per-read timeout. On timeout or EOF,
//              reconnects with 2s backoff. 15s no-real-bytes budget.
//   Consumer — 500ms channel-empty timer. On data → stdout. On timeout →
//              MPEG-TS NULL block to stdout (DVR keepalive).
//   Channel  — sync_channel(2). PR #9 uses 64 because it's in-process with
//              DVR; our extra stdout-pipe hop to AH4C already provides shock
//              absorption via the 64 KiB kernel pipe buffer, so anything
//              held in this channel is pure added latency.
//   Cold-path — consumer withholds NULL injection until the first real
//              chunk has been forwarded. Leading NULLs before the stream's
//              first PAT/PMT desync the DVR demuxer and delay audio lock-on
//              by ~10s on cold tune.

use std::env;
use std::io::{ErrorKind, Read, Write};
use std::mem::ManuallyDrop;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::io::FromRawFd;
use std::process::exit;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

const STALL_READ_GAP: Duration = Duration::from_millis(500);
const SRC_STALL_RECONNECT: Duration = Duration::from_secs(5);
const SRC_RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
const MAX_UNHEALTHY: Duration = Duration::from_secs(15);
const CONNECT: Duration = Duration::from_secs(5);
const CHUNK: usize = 32 * 1024;
const QUEUE_DEPTH: usize = 2;

fn main() {
    let url = env::args().nth(1).unwrap_or_else(|| { eprintln!("usage: ah4c-stream <url>"); exit(2) });
    let (stream, leftover) = connect(&url).unwrap_or_else(|e| {
        eprintln!("initial connect failed: {}", e); exit(2)
    });

    let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE_DEPTH);
    let p_url = url.clone();
    thread::spawn(move || producer(p_url, stream, leftover, tx));
    consumer(rx);
}

fn consumer(rx: Receiver<Vec<u8>>) {
    let null = make_null();
    // Bypass std::io::stdout()'s LineWriter — it buffers binary data (no \n)
    // up to 8 KiB, adding read-size-dependent latency. Raw fd 1 goes straight
    // to write(2) per call, same as Perl's syswrite. ManuallyDrop so the
    // File's Drop doesn't close fd 1 on scope exit.
    let mut out = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(1) });
    let mut started = false;
    loop {
        match rx.recv_timeout(STALL_READ_GAP) {
            Ok(data) => {
                started = true;
                if out.write_all(&data).is_err() { return; }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !started { continue; }
                if out.write_all(&null).is_err() { return; }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn producer(url: String, mut stream: TcpStream, leftover: Vec<u8>, tx: SyncSender<Vec<u8>>) {
    let mut last_real = Instant::now();
    if !leftover.is_empty() {
        if tx.send(leftover).is_err() { return; }
    }
    let mut buf = vec![0u8; CHUNK];
    loop {
        if last_real.elapsed() > MAX_UNHEALTHY { return; }
        stream.set_read_timeout(Some(SRC_STALL_RECONNECT)).ok();
        match stream.read(&mut buf) {
            Ok(n) if n > 0 => {
                last_real = Instant::now();
                if tx.send(buf[..n].to_vec()).is_err() { return; }
            }
            _ => {
                eprintln!("source idle/error; reconnecting");
                loop {
                    if last_real.elapsed() > MAX_UNHEALTHY { return; }
                    match connect(&url) {
                        Ok((s, left)) => {
                            stream = s;
                            if !left.is_empty() && tx.send(left).is_err() { return; }
                            eprintln!("reconnected");
                            break;
                        }
                        Err(_) => thread::sleep(SRC_RECONNECT_BACKOFF),
                    }
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
    let mut v = Vec::with_capacity(174 * 188);
    for _ in 0..174 {
        v.extend_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
        v.extend(std::iter::repeat(0xFF).take(184));
    }
    v
}

fn ioerr(s: &str) -> std::io::Error { std::io::Error::new(ErrorKind::Other, s.to_string()) }
