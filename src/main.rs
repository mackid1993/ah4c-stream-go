// ah4c-stream: standalone AH4C CMD-mode tuner. Port of PR #9's
// stallTolerantReader from sullrich/ah4c (main.go:1337-1529), plus MPEG-TS
// packet-level discontinuity_indicator flagging on reconnect.
//
// PR #9 semantics (literal):
//   Producer — encoder socket. 5s per-read timeout (srcStallReconnect).
//              ANY non-data outcome (Ok(0), timeout, error) closes body and
//              reconnects silently with 2s backoff. No NULL emission here.
//   Consumer — forwards channel bytes to stdout. On 500ms channel-empty
//              (stallReadGap) fills with MPEG-TS NULL packets. Timer resets
//              each recv. No STARTED gate, no consecutive-NULL cap; those
//              divergences (v0.2.14–v0.2.16) caused more issues than they
//              solved and aren't part of PR #9.
//   Channel  — sync_channel(64), matching PR #9.
//
// Beyond PR #9 — required because CMD-mode topology (extra kernel pipe +
// Go io.Pipe hop between us and DVR) adds enough serialization that the
// encoder's FIN/reconnect-with-~3s-silence pattern produces a PCR jump the
// in-process reader never caused:
//
//   TsProc  — 188-byte-packet-aware wrapper around the byte stream. On
//             each reconnect, marks `disc_pending`; then flags
//             adaptation_field.discontinuity_indicator=1 on the first
//             AF-bearing packet per PID after reconnect. This is the
//             MPEG-TS primitive for "PCR jumped, rebase STC" — telling
//             DVR's demuxer the post-reconnect clock is a restart, not a
//             stale stream. Fixes audio cut / A/V desync on cold tunes.
//
// Environment details:
//   Pipe    — stdout enlarged via fcntl F_SETPIPE_SZ (cascade 8→4→2→1 MiB;
//             container caps at 1 MiB without CAP_SYS_RESOURCE).
//   Teardown— SIGTERM/INT/HUP handler shutdown()s the encoder socket before
//             _exit so the kernel sends FIN (not RST) to the encoder.

use std::collections::HashSet;
use std::env;
use std::io::{ErrorKind, Read, Write};
use std::mem::ManuallyDrop;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::process::exit;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

static T0: OnceLock<Instant> = OnceLock::new();
static BYTES_READ: AtomicU64 = AtomicU64::new(0);
static BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);
static PROD_BLOCKED_US: AtomicU64 = AtomicU64::new(0);
static CONS_BLOCKED_US: AtomicU64 = AtomicU64::new(0);
static SLOW_EVENT_MS: u64 = 10;

fn us() -> u64 {
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64
}

const STALL_READ_GAP: Duration = Duration::from_millis(500);
const SRC_STALL_RECONNECT: Duration = Duration::from_secs(5);
const SRC_RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
const MAX_UNHEALTHY: Duration = Duration::from_secs(15);
const CONNECT: Duration = Duration::from_secs(5);
const CHUNK: usize = 32 * 1024;
const QUEUE_DEPTH: usize = 64;

static STREAM_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_term(_: libc::c_int) {
    let fd = STREAM_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe { libc::shutdown(fd, libc::SHUT_RDWR); }
    }
    unsafe { libc::_exit(0); }
}

fn install_signal_handler() {
    let h = on_term as *const () as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT,  h);
        libc::signal(libc::SIGHUP,  h);
    }
}

fn main() {
    install_signal_handler();
    let _ = T0.set(Instant::now());
    eprintln!("[us={}] start pid={}", us(), std::process::id());

    let url = env::args().nth(1).unwrap_or_else(|| { eprintln!("usage: ah4c-stream <url>"); exit(2) });
    let t_conn = Instant::now();
    let (stream, leftover) = connect(&url).unwrap_or_else(|e| {
        eprintln!("initial connect failed: {}", e); exit(2)
    });
    eprintln!("[us={}] connect ok dt_ms={} leftover={}", us(), t_conn.elapsed().as_millis(), leftover.len());
    STREAM_FD.store(stream.as_raw_fd(), Ordering::Relaxed);

    let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE_DEPTH);
    let p_url = url.clone();
    thread::spawn(move || producer(p_url, stream, leftover, tx));
    thread::spawn(summary_thread);
    consumer(rx);
}

fn summary_thread() {
    let mut last_r = 0u64;
    let mut last_w = 0u64;
    let mut last_pb = 0u64;
    let mut last_cb = 0u64;
    loop {
        thread::sleep(Duration::from_secs(1));
        let r = BYTES_READ.load(Ordering::Relaxed);
        let w = BYTES_WRITTEN.load(Ordering::Relaxed);
        let pb = PROD_BLOCKED_US.load(Ordering::Relaxed);
        let cb = CONS_BLOCKED_US.load(Ordering::Relaxed);
        eprintln!(
            "[us={}] 1s read={} write={} d_read={} d_write={} prod_blocked_ms={} cons_blocked_ms={}",
            us(), r, w, r - last_r, w - last_w,
            (pb - last_pb) / 1000, (cb - last_cb) / 1000
        );
        last_r = r; last_w = w; last_pb = pb; last_cb = cb;
    }
}

fn consumer(rx: Receiver<Vec<u8>>) {
    let null = make_null();
    let mut out = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(1) });
    unsafe {
        for size in [8 * 1024 * 1024, 4 * 1024 * 1024, 2 * 1024 * 1024, 1024 * 1024].iter() {
            if libc::fcntl(1, 1031, *size as libc::c_int) >= 0 {
                let actual = libc::fcntl(1, 1032);
                eprintln!("[us={}] pipe_sz requested={} actual={}", us(), size, actual);
                break;
            } else {
                let err = std::io::Error::last_os_error();
                eprintln!("[us={}] pipe_sz requested={} DENIED err={}", us(), size, err);
            }
        }
    }
    loop {
        match rx.recv_timeout(STALL_READ_GAP) {
            Ok(data) => {
                let n = data.len();
                let t_w = Instant::now();
                if out.write_all(&data).is_err() { return; }
                let w_us = t_w.elapsed().as_micros() as u64;
                if w_us > SLOW_EVENT_MS * 1000 {
                    eprintln!("[us={}] cons_write_slow n={} dt_ms={}", us(), n, w_us / 1000);
                }
                BYTES_WRITTEN.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(RecvTimeoutError::Timeout) => {
                eprintln!("[us={}] null_inject (500ms stall)", us());
                if out.write_all(&null).is_err() { return; }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

struct TsProc {
    carry: Vec<u8>,
    synced: bool,
    disc_pending: bool,
    flagged_pids: HashSet<u16>,
}

impl TsProc {
    fn new() -> Self {
        TsProc {
            carry: Vec::with_capacity(4096),
            synced: false,
            disc_pending: false,
            flagged_pids: HashSet::new(),
        }
    }

    fn mark_reconnect(&mut self) {
        self.disc_pending = true;
        self.flagged_pids.clear();
    }

    fn process(&mut self, data: &[u8]) -> Vec<u8> {
        self.carry.extend_from_slice(data);

        if !self.synced {
            if let Some(pos) = self.carry.iter().position(|&b| b == 0x47) {
                if pos > 0 { self.carry.drain(..pos); }
                self.synced = true;
            } else {
                self.carry.clear();
                return Vec::new();
            }
        }

        let mut out = Vec::with_capacity(self.carry.len());
        while self.carry.len() >= 188 {
            if self.carry[0] != 0x47 {
                self.synced = false;
                if let Some(pos) = self.carry.iter().position(|&b| b == 0x47) {
                    self.carry.drain(..pos);
                    self.synced = true;
                    continue;
                } else {
                    self.carry.clear();
                    break;
                }
            }
            let mut packet: [u8; 188] = [0; 188];
            packet.copy_from_slice(&self.carry[..188]);
            self.maybe_flag_disc(&mut packet);
            out.extend_from_slice(&packet);
            self.carry.drain(..188);
        }
        out
    }

    fn maybe_flag_disc(&mut self, packet: &mut [u8; 188]) {
        if !self.disc_pending { return; }
        let pid = ((packet[1] as u16 & 0x1F) << 8) | (packet[2] as u16);
        if pid == 0x1FFF { return; }
        if self.flagged_pids.contains(&pid) { return; }
        // adaptation_field_control is byte 3 bits 5..4. 10 = AF only,
        // 11 = AF + payload. Both have an AF we can flag.
        let af_control = (packet[3] >> 4) & 0x3;
        if af_control != 0b10 && af_control != 0b11 { return; }
        let af_length = packet[4];
        if af_length == 0 { return; }
        // Byte 5 = AF flags. Bit 7 (0x80) = discontinuity_indicator.
        packet[5] |= 0x80;
        self.flagged_pids.insert(pid);
        eprintln!("[us={}] disc_flag pid=0x{:04X}", us(), pid);
    }
}

fn producer(url: String, mut stream: TcpStream, leftover: Vec<u8>, tx: SyncSender<Vec<u8>>) {
    let mut tsp = TsProc::new();
    let mut last_real = Instant::now();

    if !leftover.is_empty() {
        let n = leftover.len();
        BYTES_READ.fetch_add(n as u64, Ordering::Relaxed);
        eprintln!("[us={}] prod_leftover n={}", us(), n);
        let processed = tsp.process(&leftover);
        if !processed.is_empty() && tx.send(processed).is_err() { return; }
    }

    let mut buf = vec![0u8; CHUNK];
    stream.set_read_timeout(Some(SRC_STALL_RECONNECT)).ok();
    loop {
        if last_real.elapsed() > MAX_UNHEALTHY { return; }
        let t_r = Instant::now();
        match stream.read(&mut buf) {
            Ok(n) if n > 0 => {
                let r_us = t_r.elapsed().as_micros() as u64;
                if r_us > SLOW_EVENT_MS * 1000 {
                    eprintln!("[us={}] prod_read_slow n={} dt_ms={}", us(), n, r_us / 1000);
                }
                BYTES_READ.fetch_add(n as u64, Ordering::Relaxed);
                last_real = Instant::now();
                let t_s = Instant::now();
                let processed = tsp.process(&buf[..n]);
                if !processed.is_empty() && tx.send(processed).is_err() { return; }
                let s_us = t_s.elapsed().as_micros() as u64;
                PROD_BLOCKED_US.fetch_add(s_us, Ordering::Relaxed);
                if s_us > SLOW_EVENT_MS * 1000 {
                    eprintln!("[us={}] prod_send_slow n={} dt_ms={}", us(), n, s_us / 1000);
                }
            }
            _ => {
                eprintln!("[us={}] source idle/error; reconnecting", us());
                loop {
                    if last_real.elapsed() > MAX_UNHEALTHY { return; }
                    match connect(&url) {
                        Ok((s, left)) => {
                            stream = s;
                            STREAM_FD.store(stream.as_raw_fd(), Ordering::Relaxed);
                            stream.set_read_timeout(Some(SRC_STALL_RECONNECT)).ok();
                            tsp.mark_reconnect();
                            if !left.is_empty() {
                                let processed = tsp.process(&left);
                                if !processed.is_empty() && tx.send(processed).is_err() { return; }
                            }
                            eprintln!("[us={}] reconnected (disc_pending)", us());
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
