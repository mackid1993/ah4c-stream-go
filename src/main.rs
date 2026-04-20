// ah4c-stream: standalone AH4C CMD-mode tuner. Port of PR #9's
// stallTolerantReader from sullrich/ah4c (main.go:1337-1529), plus MPEG-TS
// packet-level discontinuity_indicator flagging on reconnect.
//
// PR #9 semantics (literal):
//   Producer — encoder socket. 5s per-read timeout (srcStallReconnect).
//              ANY non-data outcome (Ok(0), timeout, error) closes body and
//              reconnects silently with 2s backoff. No NULL emission here.
//   Consumer — forwards channel bytes to stdout. On 500ms channel-empty
//              (stallReadGap), fills with MPEG-TS NULL packets ONLY when
//              LAST_REAL_DATA_US is older than STALE_DATA_MS (1.5 s).
//              VBR/low-activity encoders can leave the channel empty for
//              >500 ms between chunks while healthy; padding those gaps
//              adds PCR-less bytes to DVR's buffer, pushes the live edge
//              backward, and corrupts audio PTS alignment. The time-based
//              gate also covers the pre-reconnect silence window (producer
//              blocked in stream.read up to SRC_STALL_RECONNECT=5s), where
//              an in-reconnect-branch flag would leave DVR dark.
//              Diverges from PR #9 because CMD-mode topology inherits
//              upstream VBR jitter the in-process reader never saw.
//   Channel  — sync_channel(64), matching PR #9.
//
// Beyond PR #9 — required because CMD-mode topology (extra kernel pipe +
// Go io.Pipe hop between us and DVR) adds enough serialization that the
// encoder's FIN/reconnect-with-~3s-silence pattern produces a PCR jump the
// in-process reader never caused:
//
//   TsProc  — 188-byte-packet-aware wrapper around the byte stream.
//             Triggers discontinuity on either (a) TCP reconnect, or
//             (b) in-stream PCR jump detected per PID. HDMI encoders
//             keep the HTTP session open across channel changes / HDMI
//             renegotiates, so TCP reconnect alone misses those events.
//             On each trigger, emits a synthetic AF-only packet with
//             discontinuity_indicator=1 ahead of the first real packet
//             per PID — the MPEG-TS primitive for "PCR restart, rebase
//             STC". Fixes audio cut / A/V desync on cold tunes AND on
//             mid-session channel changes.
//
// Environment details:
//   HTTP    — GET HTTP/1.1 with keep-alive / identity-encoding preference
//             (matches Go's http.Get). Many HDMI encoders (LinkPi observed)
//             fast-path HTTP/1.1 requests and only enter a full channel-
//             lock cycle on HTTP/1.0 + Connection: close. Encoder may still
//             choose chunked transfer-encoding for the streaming body;
//             connect() detects this and wraps the socket in a Chunked
//             decoder transparent to the producer. Go's stdlib does this
//             in net/http for free.
//   Pipe    — stdout kept at Linux default (~64 KiB). Earlier versions
//             enlarged via F_SETPIPE_SZ to absorb AH4C's ~1s io.Copy
//             startup pause, but that 1 MiB fill becomes a fixed startup
//             prebuffer DVR sees as end-to-end latency. PR #9's in-process
//             io.Pipe is a rendezvous (~0 buffer); removing the cascade
//             brings us closer to that profile. Producer still has
//             sync_channel(64) = 2 MiB of shock absorption; if AH4C's
//             startup pause exceeds the channel, producer backpressures
//             the encoder's TCP send buffer rather than filling the pipe.
//   Teardown— SIGTERM/INT/HUP handler shutdown()s the encoder socket before
//             _exit so the kernel sends FIN (not RST) to the encoder.

use std::collections::{HashMap, HashSet};
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
// PCR is 27 MHz. A jump forward > 500 ms (13.5M ticks) or any backward
// motion on the same PID is treated as an in-stream discontinuity —
// independent of whether the TCP connection dropped. HDMI encoders keep
// the HTTP session open across channel changes and HDMI renegotiates,
// so this is the only signal that catches those events.
const PCR_JUMP_FORWARD_TICKS: u64 = 13_500_000;
// How long the source may be silent before we start padding DVR's pipe
// with NULL packets. Chosen to tolerate VBR burstiness (even at 50 KB/s
// the gap between chunks is ~640 ms) while reacting long before DVR's
// demuxer gives up. Covers BOTH the pre-reconnect silence window (up to
// SRC_STALL_RECONNECT = 5 s the producer is blocked in stream.read) AND
// the reconnect + post-reconnect-pre-first-byte window. Single threshold
// replaces the previous "producer in reconnect branch" flag, which left
// the pre-reconnect 5 s silent — that silence was the remaining cause
// of audio desync / falling behind live.
const STALE_DATA_MS: u64 = 1500;

static STREAM_FD: AtomicI32 = AtomicI32::new(-1);

// Wall-clock us() of the last successful source read. Consumer reads
// this on each 500 ms timeout tick and injects NULLs only when the gap
// exceeds STALE_DATA_MS. Bursty traffic never reaches the threshold;
// genuine stalls (encoder frozen, HDMI renegotiating, TCP reconnecting)
// always do.
static LAST_REAL_DATA_US: AtomicU64 = AtomicU64::new(0);

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
    let (reader, fd) = connect(&url).unwrap_or_else(|e| {
        eprintln!("initial connect failed: {}", e); exit(2)
    });
    eprintln!("[us={}] connect ok dt_ms={}", us(), t_conn.elapsed().as_millis());
    STREAM_FD.store(fd, Ordering::Relaxed);

    let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE_DEPTH);
    let p_url = url.clone();
    thread::spawn(move || producer(p_url, reader, tx));
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
    let actual = unsafe { libc::fcntl(1, 1032) };
    eprintln!("[us={}] pipe_sz default={}", us(), actual);
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
                // Inject NULLs only when the source has actually been silent
                // longer than STALE_DATA_MS. Bursty VBR gaps are well under
                // that; real stalls (encoder frozen, HDMI renegotiating, TCP
                // reconnecting) always cross it within ~1.5 s of going quiet.
                let now = us();
                let last = LAST_REAL_DATA_US.load(Ordering::Relaxed);
                let gap_ms = now.saturating_sub(last) / 1000;
                if gap_ms > STALE_DATA_MS {
                    eprintln!("[us={}] null_inject gap_ms={}", now, gap_ms);
                    if out.write_all(&null).is_err() { return; }
                }
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
    last_pcr: HashMap<u16, u64>,
}

impl TsProc {
    fn new() -> Self {
        TsProc {
            carry: Vec::with_capacity(4096),
            synced: false,
            disc_pending: false,
            flagged_pids: HashSet::new(),
            last_pcr: HashMap::new(),
        }
    }

    fn mark_discontinuity(&mut self, reason: &str) {
        eprintln!("[us={}] disc_trigger reason={}", us(), reason);
        self.disc_pending = true;
        self.flagged_pids.clear();
        self.last_pcr.clear();
    }

    // Parse PCR from a 188-byte TS packet, if present.
    // AF present when adaptation_field_control bits (packet[3] >> 4) & 0x03
    // == 0b10 or 0b11. AF length in packet[4]; PCR_flag is bit 0x10 of
    // packet[5]; PCR base+ext occupies packet[6..12] (33-bit base at 90 kHz
    // + 9-bit ext at 27 MHz). Result is in 27 MHz ticks.
    fn extract_pcr(p: &[u8]) -> Option<u64> {
        if p.len() < 12 { return None; }
        let af_ctrl = (p[3] >> 4) & 0x03;
        if af_ctrl != 0b10 && af_ctrl != 0b11 { return None; }
        if p[4] == 0 { return None; }
        if (p[5] & 0x10) == 0 { return None; }
        let base: u64 = ((p[6] as u64) << 25)
            | ((p[7] as u64) << 17)
            | ((p[8] as u64) << 9)
            | ((p[9] as u64) << 1)
            | ((p[10] as u64) >> 7);
        let ext: u64 = (((p[10] as u64) & 0x01) << 8) | (p[11] as u64);
        Some(base * 300 + ext)
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

        let mut out = Vec::with_capacity(self.carry.len() + 376);
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
            let pid = ((self.carry[1] as u16 & 0x1F) << 8) | (self.carry[2] as u16);

            // Detect in-stream PCR jumps (channel change / HDMI renegotiate
            // without TCP reconnect). Must be evaluated BEFORE maybe_inject_disc
            // so the synthetic packet lands ahead of the jumped real packet.
            if let Some(pcr) = Self::extract_pcr(&self.carry[..188]) {
                if let Some(&prev) = self.last_pcr.get(&pid) {
                    let jumped = pcr < prev || pcr - prev > PCR_JUMP_FORWARD_TICKS;
                    if jumped {
                        self.mark_discontinuity(&format!(
                            "pcr_jump pid=0x{:04X} prev={} new={}",
                            pid, prev, pcr
                        ));
                    }
                }
                self.last_pcr.insert(pid, pcr);
            }

            self.maybe_inject_disc(pid, &mut out);
            out.extend_from_slice(&self.carry[..188]);
            self.carry.drain(..188);
        }
        out
    }

    // On each discontinuity trigger, insert a synthetic AF-only packet
    // with discontinuity_indicator=1 immediately before each PID's first
    // real post-trigger packet. This is the MPEG-TS primitive for "PCR
    // restarted / stream spliced" — the demuxer rebases STC on the NEXT
    // packet of this PID (which is the real packet right after). Doesn't
    // modify real packets, so zero risk of corrupting PES headers or ES
    // data; broadcast encoders use this shape at splice points.
    fn maybe_inject_disc(&mut self, pid: u16, out: &mut Vec<u8>) {
        if !self.disc_pending { return; }
        if pid == 0x1FFF { return; }       // NULL packets
        if pid == 0x0000 { return; }       // PAT — has its own version_number mechanism
        if self.flagged_pids.contains(&pid) { return; }
        out.extend_from_slice(&make_disc_packet(pid));
        self.flagged_pids.insert(pid);
        eprintln!("[us={}] disc_inject pid=0x{:04X}", us(), pid);
    }
}

fn make_disc_packet(pid: u16) -> [u8; 188] {
    let mut p = [0xFFu8; 188];
    p[0] = 0x47;                                 // sync
    p[1] = ((pid >> 8) & 0x1F) as u8;            // TEI=0, PUSI=0, TP=0, PID hi 5 bits
    p[2] = (pid & 0xFF) as u8;                   // PID lo 8 bits
    p[3] = 0x20;                                 // scrambling=00, af_control=10 (AF only), CC=0
    p[4] = 183;                                  // AF length: fills bytes 5..188
    p[5] = 0x80;                                 // AF flags: discontinuity_indicator=1, all else 0
    // bytes 6..188 remain 0xFF (AF stuffing)
    p
}

fn producer(url: String, mut reader: Box<dyn Read + Send>, tx: SyncSender<Vec<u8>>) {
    let mut tsp = TsProc::new();
    let mut last_real = Instant::now();
    let mut buf = vec![0u8; CHUNK];
    loop {
        if last_real.elapsed() > MAX_UNHEALTHY { return; }
        let t_r = Instant::now();
        match reader.read(&mut buf) {
            Ok(n) if n > 0 => {
                let r_us = t_r.elapsed().as_micros() as u64;
                if r_us > SLOW_EVENT_MS * 1000 {
                    eprintln!("[us={}] prod_read_slow n={} dt_ms={}", us(), n, r_us / 1000);
                }
                BYTES_READ.fetch_add(n as u64, Ordering::Relaxed);
                last_real = Instant::now();
                LAST_REAL_DATA_US.store(us(), Ordering::Relaxed);
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
                        Ok((r, fd)) => {
                            reader = r;
                            STREAM_FD.store(fd, Ordering::Relaxed);
                            tsp.mark_discontinuity("tcp_reconnect");
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

// Prepended leftover + raw TcpStream. Leftover from the HTTP header parse
// is served first, then reads fall through to the socket. Non-chunked path.
struct RawReader {
    prefix: Vec<u8>,
    prefix_pos: usize,
    stream: TcpStream,
}

impl Read for RawReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.prefix_pos < self.prefix.len() {
            let n = (self.prefix.len() - self.prefix_pos).min(buf.len());
            buf[..n].copy_from_slice(&self.prefix[self.prefix_pos..self.prefix_pos + n]);
            self.prefix_pos += n;
            return Ok(n);
        }
        self.stream.read(buf)
    }
}

// Minimal HTTP/1.1 chunked transfer-encoding decoder (RFC 7230 §4.1).
// Wire format:
//   <hex-size>[;chunk-ext]\r\n <size bytes data> \r\n  ... 0\r\n [trailers] \r\n
// Streaming encoders typically never emit the 0-size terminator (stream
// is infinite); if they do, we return Ok(0) and the producer reconnects.
// Leftover from the HTTP header parse may contain the first chunk-size
// line and/or first chunk's data — folded in via `prefix`.
struct ChunkedReader {
    prefix: Vec<u8>,
    prefix_pos: usize,
    stream: TcpStream,
    raw: Vec<u8>,       // bytes pulled from prefix/socket, not yet consumed
    raw_pos: usize,
    remaining: usize,   // data bytes left in the current chunk
    need_trailing_crlf: bool, // after chunk data, two bytes \r\n precede next size
}

impl ChunkedReader {
    fn new(stream: TcpStream, leftover: Vec<u8>) -> Self {
        ChunkedReader {
            prefix: leftover,
            prefix_pos: 0,
            stream,
            raw: Vec::with_capacity(32 * 1024),
            raw_pos: 0,
            remaining: 0,
            need_trailing_crlf: false,
        }
    }

    fn refill(&mut self) -> std::io::Result<()> {
        self.raw.clear();
        self.raw_pos = 0;
        if self.prefix_pos < self.prefix.len() {
            self.raw.extend_from_slice(&self.prefix[self.prefix_pos..]);
            self.prefix_pos = self.prefix.len();
            return Ok(());
        }
        let mut tmp = [0u8; 32 * 1024];
        let n = self.stream.read(&mut tmp)?;
        if n == 0 { return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "eof in chunked body")); }
        self.raw.extend_from_slice(&tmp[..n]);
        Ok(())
    }

    fn next_byte(&mut self) -> std::io::Result<u8> {
        if self.raw_pos >= self.raw.len() { self.refill()?; }
        let b = self.raw[self.raw_pos];
        self.raw_pos += 1;
        Ok(b)
    }

    fn read_size_line(&mut self) -> std::io::Result<usize> {
        let mut line = Vec::with_capacity(16);
        loop {
            let b = self.next_byte()?;
            if b == b'\n' {
                if line.last() == Some(&b'\r') { line.pop(); }
                break;
            }
            line.push(b);
            if line.len() > 256 { return Err(ioerr("chunk size line too long")); }
        }
        let text = std::str::from_utf8(&line).map_err(|_| ioerr("chunk size not utf8"))?;
        let hex = text.split(';').next().unwrap_or("").trim();
        usize::from_str_radix(hex, 16).map_err(|_| ioerr("chunk size not hex"))
    }
}

impl Read for ChunkedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            if self.need_trailing_crlf {
                let _ = self.next_byte()?;
                let _ = self.next_byte()?;
                self.need_trailing_crlf = false;
            }
            let size = self.read_size_line()?;
            if size == 0 { return Ok(0); }
            self.remaining = size;
        }
        if self.raw_pos >= self.raw.len() { self.refill()?; }
        let available = self.raw.len() - self.raw_pos;
        let n = available.min(self.remaining).min(buf.len());
        buf[..n].copy_from_slice(&self.raw[self.raw_pos..self.raw_pos + n]);
        self.raw_pos += n;
        self.remaining -= n;
        if self.remaining == 0 { self.need_trailing_crlf = true; }
        Ok(n)
    }
}

fn connect(url: &str) -> std::io::Result<(Box<dyn Read + Send>, i32)> {
    let s = url.strip_prefix("http://").ok_or_else(|| ioerr("http:// only"))?;
    let (hp, path) = s.split_once('/').map(|(a, b)| (a.to_string(), format!("/{}", b)))
        .unwrap_or_else(|| (s.to_string(), "/".into()));
    let hp_full = if hp.contains(':') { hp.clone() } else { format!("{}:80", hp) };
    let addr = hp_full.to_socket_addrs()?.next().ok_or_else(|| ioerr("dns"))?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(CONNECT))?;
    // HTTP/1.1 + keep-alive shape, matching Go's net/http defaults. PR #9
    // uses http.Get which sends exactly this shape; HTTP/1.0 +
    // Connection: close triggered a full channel-lock cycle on every
    // cold GET (LinkPi observed: ~5s silence). If the encoder chooses
    // Transfer-Encoding: chunked for the body, we decode it below.
    stream.write_all(format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: Go-http-client/1.1\r\nAccept-Encoding: identity\r\n\r\n",
        path, hp
    ).as_bytes())?;

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
    let hdr_text = std::str::from_utf8(&hdr).unwrap_or("");
    let first = hdr_text.lines().next().unwrap_or("");
    eprintln!("[us={}] resp_status={}", us(), first.trim());
    let mut chunked = false;
    for line in hdr_text.lines().skip(1) {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("transfer-encoding:") || lower.starts_with("content-length:")
            || lower.starts_with("connection:") || lower.starts_with("content-type:") {
            eprintln!("[us={}] resp_header={}", us(), line.trim());
        }
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }
    if !first.contains(" 200 ") { return Err(ioerr(first.trim())); }
    stream.set_read_timeout(Some(SRC_STALL_RECONNECT))?;
    let fd = stream.as_raw_fd();
    let reader: Box<dyn Read + Send> = if chunked {
        eprintln!("[us={}] body=chunked leftover={}", us(), leftover.len());
        Box::new(ChunkedReader::new(stream, leftover))
    } else {
        eprintln!("[us={}] body=raw leftover={}", us(), leftover.len());
        Box::new(RawReader { prefix: leftover, prefix_pos: 0, stream })
    };
    Ok((reader, fd))
}

// 174 × 188 B = 32,712 B ≈ 32 KiB. Matches PR #9 exactly. v0.2.22
// shrank this to one packet to cut timeline drift but broke cold-start
// and audio — AH4C and/or DVR evidently needs a meaningful byte volume
// per tick to treat the stream as live. Restoring the PR #9 size.
fn make_null() -> Vec<u8> {
    let mut v = Vec::with_capacity(174 * 188);
    for _ in 0..174 {
        v.extend_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
        v.extend(std::iter::repeat(0xFF).take(184));
    }
    v
}

fn ioerr(s: &str) -> std::io::Error { std::io::Error::new(ErrorKind::Other, s.to_string()) }
