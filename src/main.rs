// ah4c-stream: standalone AH4C CMD-mode tuner. Literal port of PR #9's
// stallTolerantReader from sullrich/ah4c (main.go:1337-1529).
//
//   Producer — encoder socket. 5s per-read timeout (srcStallReconnect).
//              On ANY non-data outcome (Ok(0), read timeout, or other I/O
//              error) the producer closes the body and enters a reconnect
//              loop that retries silently with 2s backoff — no NULL
//              emission, producer never writes NULLs.
//   Consumer — forwards channel bytes to stdout. On 500ms channel-empty
//              (stallReadGap) fills the output with MPEG-TS NULL packets.
//              The 500ms timer is created fresh each recv, so NULLs only
//              fire after 500ms of actual channel starvation.
//   Channel  — sync_channel(2). PR #9 uses 64 because it's in-process with
//              DVR and the channel stays near-empty in steady state. In CMD
//              mode the extra stdout-pipe hop lets AH4C's bursty reads fill
//              whatever buffer we give it — 64 × 32KB accumulates as PCR lag
//              visible to DVR. 2 is enough shock absorption against AH4C's
//              per-read variance without giving depth room to grow.
//   Pipe     — stdout enlarged to 1 MiB via fcntl F_SETPIPE_SZ so AH4C's
//              ~1s io.Copy startup pause doesn't cascade backpressure to
//              the encoder.
//   Teardown — SIGTERM/INT/HUP handler shutdown()s the encoder socket
//              before _exit so the kernel sends FIN (not RST) to the
//              encoder.

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

// ---------- diagnostic instrumentation (v0.2.6) ----------
static T0: OnceLock<Instant> = OnceLock::new();
static BYTES_READ: AtomicU64 = AtomicU64::new(0);
static BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);
static PROD_BLOCKED_US: AtomicU64 = AtomicU64::new(0);
static CONS_BLOCKED_US: AtomicU64 = AtomicU64::new(0);
static SLOW_EVENT_MS: u64 = 10;

fn us() -> u64 {
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64
}
// ---------------------------------------------------------

const STALL_READ_GAP: Duration = Duration::from_millis(250);
const SRC_STALL_RECONNECT: Duration = Duration::from_secs(5);
const SRC_RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
const MAX_UNHEALTHY: Duration = Duration::from_secs(15);
const CONNECT: Duration = Duration::from_secs(5);
const CHUNK: usize = 32 * 1024;
// 256 × 32 KiB = 8 MiB in-process buffer. This is our "fat pipe" since the
// container caps kernel pipe-max-size at 1 MiB. Channel depth this large only
// accumulates depth if the consumer can't drain fast enough (i.e. stdout pipe
// is full) — during cold-start trickle the consumer always drains to empty,
// so the 250 ms NULL timer still fires and pads DVR's rate.
const QUEUE_DEPTH: usize = 256;

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
        // PR #9's Read(): start fresh 500ms timer, wait for chunk, fill
        // with NULL packets on timeout. Timer resets each iteration so
        // NULLs only fire after 500ms of actual channel starvation.
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

fn producer(url: String, mut stream: TcpStream, leftover: Vec<u8>, tx: SyncSender<Vec<u8>>) {
    let mut last_real = Instant::now();
    if !leftover.is_empty() {
        let n = leftover.len();
        BYTES_READ.fetch_add(n as u64, Ordering::Relaxed);
        eprintln!("[us={}] prod_leftover n={}", us(), n);
        if tx.send(leftover).is_err() { return; }
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
                if tx.send(buf[..n].to_vec()).is_err() { return; }
                let s_us = t_s.elapsed().as_micros() as u64;
                PROD_BLOCKED_US.fetch_add(s_us, Ordering::Relaxed);
                if s_us > SLOW_EVENT_MS * 1000 {
                    eprintln!("[us={}] prod_send_slow n={} dt_ms={}", us(), n, s_us / 1000);
                }
            }
            _ => {
                // PR #9: ANY non-data outcome (Ok(0), 5s timeout, real error)
                // → close body, reconnect silently with 2s backoff. Producer
                // never emits NULLs; the consumer's 500ms timer handles
                // DVR keepalive during the channel starvation that results.
                eprintln!("[us={}] source idle/error; reconnecting", us());
                loop {
                    if last_real.elapsed() > MAX_UNHEALTHY { return; }
                    match connect(&url) {
                        Ok((s, left)) => {
                            stream = s;
                            STREAM_FD.store(stream.as_raw_fd(), Ordering::Relaxed);
                            stream.set_read_timeout(Some(SRC_STALL_RECONNECT)).ok();
                            if !left.is_empty() && tx.send(left).is_err() { return; }
                            eprintln!("[us={}] reconnected", us());
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
    // 174 × 188 B = 32712 B (~32 KiB). Large enough that NULL keepalive
    // meaningfully pads DVR's observed byte rate past its "stream is too
    // slow, behind live" threshold during encoder trickle. v0.2.13's 188 B
    // emission didn't move the needle; reverted.
    let mut v = Vec::with_capacity(174 * 188);
    for _ in 0..174 {
        v.extend_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
        v.extend(std::iter::repeat(0xFF).take(184));
    }
    v
}

fn ioerr(s: &str) -> std::io::Error { std::io::Error::new(ErrorKind::Other, s.to_string()) }
