// ah4c-stream: standalone AH4C CMD-mode tuner. Semantic match to the bash
// reference (curl + cat-NULL-on-retry); simpler than PR #9's stallTolerantReader.
//
//   Producer — encoder socket. Read with a 1s poll timeout (to allow the
//              MAX_UNHEALTHY check to run); read-timeouts are non-fatal and
//              the producer just loops back. On EOF or real I/O error the
//              producer enters a reconnect loop that emits one NULL chunk
//              per iteration (matches bash's `cat $NULL_FILE; curl $URL`).
//   Consumer — forwards channel bytes to stdout (raw fd 1, bypassing
//              LineWriter). No in-stream NULL injection — NULLs only flow
//              during the producer's reconnect phase. This avoids the
//              ~10s audio desync caused by NULL floods interleaved with
//              cold-start encoder trickle.
//   Channel  — sync_channel(2). Minimal latency; AH4C's stdin pipe (enlarged
//              to 1 MiB via fcntl F_SETPIPE_SZ) is the real shock absorber
//              against AH4C's ~1s startup pause to DVR.
//   Teardown — SIGTERM/INT/HUP handler shutdown()s the encoder socket before
//              _exit so the kernel sends FIN (not RST) to the encoder.

use std::env;
use std::io::{ErrorKind, Read, Write};
use std::mem::ManuallyDrop;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::process::exit;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
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

const READ_POLL: Duration = Duration::from_secs(1);
const SRC_RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
const MAX_UNHEALTHY: Duration = Duration::from_secs(15);
const CONNECT: Duration = Duration::from_secs(2);
const CHUNK: usize = 32 * 1024;
const QUEUE_DEPTH: usize = 2;

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
    // Bypass std::io::stdout()'s LineWriter — it buffers binary data (no \n)
    // up to 8 KiB, adding read-size-dependent latency. Raw fd 1 goes straight
    // to write(2) per call, same as Perl's syswrite. ManuallyDrop so the
    // File's Drop doesn't close fd 1 on scope exit.
    let mut out = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(1) });
    // Enlarge stdout pipe buffer. AH4C's io.Copy to DVR pauses for ~1s at
    // startup; a default 64 KiB pipe would force our write_all to block, which
    // cascades backpressure to the encoder. 1 MiB absorbs the pause. Kernel
    // caps at /proc/sys/fs/pipe-max-size. F_SETPIPE_SZ = 1031, F_GETPIPE_SZ
    // = 1032. Try descending sizes; take the largest that's accepted.
    unsafe {
        for size in [8 * 1024 * 1024, 4 * 1024 * 1024, 2 * 1024 * 1024, 1024 * 1024].iter() {
            if libc::fcntl(1, 1031, *size as libc::c_int) >= 0 {
                let actual = libc::fcntl(1, 1032);
                eprintln!("[us={}] pipe_sz requested={} actual={}", us(), size, actual);
                break;
            }
        }
    }
    loop {
        match rx.recv() {
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
            Err(_) => return,
        }
    }
}

fn producer(url: String, mut stream: TcpStream, leftover: Vec<u8>, tx: SyncSender<Vec<u8>>) {
    let mut last_real = Instant::now();
    let null = make_null();
    if !leftover.is_empty() {
        let n = leftover.len();
        BYTES_READ.fetch_add(n as u64, Ordering::Relaxed);
        eprintln!("[us={}] prod_leftover n={}", us(), n);
        if tx.send(leftover).is_err() { return; }
    }
    let mut buf = vec![0u8; CHUNK];
    stream.set_read_timeout(Some(READ_POLL)).ok();
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
            Err(ref e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                // Read poll expired — socket alive but no data. Match bash's
                // curl-blocks-forever: just loop back and keep waiting. The
                // MAX_UNHEALTHY check at the top of the loop is the safety net.
                continue;
            }
            _ => {
                eprintln!("[us={}] source EOF/error; reconnecting", us());
                loop {
                    if last_real.elapsed() > MAX_UNHEALTHY { return; }
                    match connect(&url) {
                        Ok((s, left)) => {
                            stream = s;
                            STREAM_FD.store(stream.as_raw_fd(), Ordering::Relaxed);
                            stream.set_read_timeout(Some(READ_POLL)).ok();
                            if !left.is_empty() && tx.send(left).is_err() { return; }
                            eprintln!("[us={}] reconnected", us());
                            break;
                        }
                        Err(_) => {
                            // Emit NULL only after a failed connect attempt — keeps
                            // DVR fed during real outages, but a fast reconnect
                            // (encoder glitched for <1s) injects zero NULL bytes
                            // and the demuxer sees clean continuity.
                            eprintln!("[us={}] prod_null_emit (reconnect retry)", us());
                            if tx.send(null.clone()).is_err() { return; }
                            thread::sleep(SRC_RECONNECT_BACKOFF);
                        }
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
