// ah4c-stream: standalone AH4C CMD-mode tuner. Rust port of the behavior
// in AH4C PR #9's stallTolerantReader, adapted for standalone use (stdin
// pipe to AH4C, no in-process io.Reader facade).
//
// Architecture:
//   Producer thread  — owns the encoder socket. Per-read deadline of
//                      SRC_STALL_RECONNECT (5s); on timeout/EOF, closes
//                      the socket and enters reconnect-with-backoff.
//                      Pushes each non-empty read onto `tx`.
//   Consumer (main)  — pulls from `rx` with a STALL_READ_GAP (500ms)
//                      recv_timeout. On timeout, writes a 32 KiB block of
//                      MPEG-TS NULL packets (PID 0x1FFF) to stdout so the
//                      HTTP response to DVR keeps making forward progress.
//                      On real data, writes it through.
//   Channel          — sync_channel(2). See QUEUE_DEPTH note below.
//
// Timeouts / retry budget (match PR #9):
//   STALL_READ_GAP       = 500ms — consumer NULL injection trigger
//   SRC_STALL_RECONNECT  = 5s    — producer per-read deadline
//   SRC_RECONNECT_BACKOFF= 2s    — wait between failed reconnect attempts
//   MAX_UNHEALTHY        = 15s   — total no-real-bytes time before give-up
//
// Exit codes:
//   0  clean shutdown (budget expired, stdout EPIPE)
//   2  bad args, initial connect fail (fails-fast, no leading NULLs),
//      non-200, Transfer-Encoding: chunked

use std::env;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::exit;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

const STALL_READ_GAP: Duration = Duration::from_millis(500);
const SRC_STALL_RECONNECT: Duration = Duration::from_secs(5);
const SRC_RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
const MAX_UNHEALTHY: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CHUNK_SIZE: usize = 32 * 1024;

// Standalone uses a smaller queue than PR #9's 64. In standalone mode there's
// an extra stdout→AH4C pipe carrying its own 64 KiB of buffering; a 2 MiB
// channel on top becomes pure latency between encoder and DVR. 2 is the
// minimum that still lets the producer fill a new chunk while the consumer
// is writing the previous one.
const QUEUE_DEPTH: usize = 2;

fn make_null_chunk() -> Vec<u8> {
    // 174 × 188-byte TS NULL packets = 32,712 bytes (≤ 32 KiB).
    // sync=0x47, PID=0x1FFF, adaptation=01 (payload only), CC=0, payload=0xFF.
    let mut v = Vec::with_capacity(174 * 188);
    for _ in 0..174 {
        v.extend_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
        v.extend(std::iter::repeat(0xFF).take(184));
    }
    v
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: ah4c-stream <encoder_url>");
        exit(2);
    }
    let url = args[1].clone();
    let label = parse_label(&url);

    // Fail-fast on initial connect — matches AH4C tune() returning
    // "device(s) not available" (no leading NULL bytes before EOF).
    let (stream, leftover) = match connect(&url) {
        Ok(p) => p,
        Err(e) => {
            logf(&label, &format!("initial connect failed: {}", e));
            exit(2);
        }
    };

    let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE_DEPTH);

    let url_producer = url.clone();
    let label_producer = label.clone();
    thread::spawn(move || {
        producer(&url_producer, stream, leftover, &tx, &label_producer);
    });

    consumer(rx);
}

fn parse_label(url: &str) -> String {
    let stripped = url.strip_prefix("http://").unwrap_or(url);
    let host = stripped.split('/').next().unwrap_or(url);
    format!("tuner={}", host)
}

fn logf(label: &str, msg: &str) {
    eprintln!("[{}] {}", label, msg);
}

fn consumer(rx: Receiver<Vec<u8>>) {
    let null_chunk = make_null_chunk();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    loop {
        match rx.recv_timeout(STALL_READ_GAP) {
            Ok(data) => {
                if out.write_all(&data).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if out.write_all(&null_chunk).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn producer(
    url: &str,
    initial_stream: TcpStream,
    initial_leftover: Vec<u8>,
    tx: &SyncSender<Vec<u8>>,
    label: &str,
) {
    let mut last_real = Instant::now();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut stream: Option<TcpStream> = Some(initial_stream);

    // Any body bytes consumed with the initial response headers count as
    // the first real read; push them through before reading from the socket.
    if !initial_leftover.is_empty() {
        if tx.send(initial_leftover).is_err() {
            return;
        }
    }

    loop {
        if last_real.elapsed() >= MAX_UNHEALTHY {
            logf(
                label,
                &format!(
                    "no source bytes for {:?}; closing reader so DVR sees EOF",
                    MAX_UNHEALTHY
                ),
            );
            return;
        }

        if stream.is_none() {
            match connect(url) {
                Ok((s, leftover)) => {
                    logf(label, "reconnected");
                    if !leftover.is_empty() {
                        if tx.send(leftover).is_err() {
                            return;
                        }
                        last_real = Instant::now();
                    }
                    stream = Some(s);
                }
                Err(e) => {
                    logf(label, &format!("reconnect failed: {}", e));
                    thread::sleep(SRC_RECONNECT_BACKOFF);
                    continue;
                }
            }
        }

        let s = stream.as_mut().unwrap();
        let _ = s.set_read_timeout(Some(SRC_STALL_RECONNECT));

        match s.read(&mut buf) {
            Ok(0) => {
                logf(label, "source EOF; reconnecting");
                stream = None;
            }
            Ok(n) => {
                let data = buf[..n].to_vec();
                if tx.send(data).is_err() {
                    return;
                }
                last_real = Instant::now();
            }
            Err(e) if is_timeout(&e) => {
                logf(
                    label,
                    &format!("source idle {:?}; reconnecting", SRC_STALL_RECONNECT),
                );
                stream = None;
            }
            Err(e) => {
                logf(label, &format!("source error ({}); reconnecting", e));
                stream = None;
            }
        }
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
}

// connect opens a TCP socket, sends an HTTP/1.0 GET, reads + validates the
// response headers, and returns (socket, leftover_body_bytes). HTTP/1.0 +
// Connection: close avoids chunked transfer encoding and keep-alive; body
// length is implicitly "until EOF."
fn connect(url: &str) -> std::io::Result<(TcpStream, Vec<u8>)> {
    let stripped = url
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "only http:// scheme"))?;

    let (host_port, path) = match stripped.find('/') {
        Some(i) => (&stripped[..i], &stripped[i..]),
        None => (stripped, "/"),
    };
    let host_port_norm = if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{}:80", host_port)
    };

    let addr = host_port_norm
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(ErrorKind::NotFound, "no addresses resolved"))?;

    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    stream.set_nodelay(true)?;

    let req = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: ah4c-stream\r\nConnection: close\r\n\r\n",
        path, host_port
    );
    stream.write_all(req.as_bytes())?;

    // Read until CRLFCRLF (8 KiB cap).
    let mut hdr = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "connection closed during headers",
            ));
        }
        hdr.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_crlf_crlf(&hdr) {
            let leftover = hdr.split_off(pos + 4);
            let headers = hdr;

            let first_line = headers
                .split(|&b| b == b'\n')
                .next()
                .unwrap_or(&[]);
            let first_line_str = std::str::from_utf8(first_line).unwrap_or("");
            if !first_line_str.starts_with("HTTP/") || !first_line_str.contains(" 200 ") {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    format!("non-200 status: {}", first_line_str.trim()),
                ));
            }

            let lower = headers.to_ascii_lowercase();
            if contains_subslice(&lower, b"transfer-encoding:")
                && contains_subslice(&lower, b"chunked")
            {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "chunked transfer encoding not supported",
                ));
            }

            // Clear connect-phase deadlines; producer sets its own per-read.
            stream.set_read_timeout(None)?;
            stream.set_write_timeout(None)?;
            return Ok((stream, leftover));
        }
        if hdr.len() > 8192 {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "headers too large",
            ));
        }
    }
}

fn find_crlf_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}
