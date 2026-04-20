# ah4c-stream

A standalone AH4C CMD-mode tuner that wraps an HDMI encoder's HTTP stream in the same `stallTolerantReader` logic as [AH4C PR #9](https://github.com/sullrich/ah4c/pull/9). Written in Rust.

If AH4C merges PR #9 upstream, you don't need this — AH4C's network-encoder branch will wrap every tune in the reader automatically. Until then, this binary reproduces the behavior from outside AH4C: drop it in your scripts directory, point a `CMD1=` at it, and you get the same reliability on top of the stock `ghcr.io/mackid1993/ah4c:latest` image — no Dockerfile changes, no custom build.

## What it does

When an HDMI encoder hiccups (channel change, reboot, brief disconnect), it stops sending TS data. AH4C's default path passes the encoder stream straight to Channels DVR, so DVR sees the stream die and gives up.

This binary puts a thin buffering layer between the encoder and DVR:

- Reads from the encoder into a small 2-chunk (~64 KiB) queue.
- Forwards to DVR from the queue.
- If the queue is empty for 500 ms, fills DVR's buffer with **MPEG-TS NULL packets** (PID `0x1FFF`) that every TS demuxer drops — so DVR keeps seeing bytes flowing and doesn't time out.
- If the source is idle for 5 s, closes the connection and reconnects behind the scenes. NULL packets continue while reconnecting.
- If no real source bytes arrive for 15 s total, gives up and closes the reader so DVR can react.
- If the **initial** connect fails, exits with code `2` and zero stdout output so AH4C surfaces it as a tune failure.

The warm path has effectively zero added latency vs. a raw `curl` — the NULL injector only kicks in when something actually breaks.

## Usage

In your `ah4c.env`:

```
CMD1="/opt/scripts/ah4c-stream $ENCODER1_URL"
```

(Path depends on where you mount the binary. The `scripts/` dir AH4C already ships with is a convenient place.)

Exit codes:

| Code | Meaning |
|------|---------|
| `0`  | Clean shutdown (15 s budget expired, signal, downstream EOF) |
| `2`  | Bad args, initial connect failure, non-200 upstream, or chunked transfer encoding |

Stderr carries log lines prefixed `[tuner=host:port]` for AH4C's container logs.

## Install (prebuilt binary)

Grab the latest release from the [releases page](https://github.com/mackid1993/ah4c-stream-go/releases) and drop the right arch into your AH4C container:

- `ah4c-stream-amd64` — most x86-64 Docker hosts
- `ah4c-stream-arm64` — Raspberry Pi, Apple Silicon hosts running arm64 containers

Make it executable and point `CMD1=` at it.

## Build from source

Requires Rust 1.70+ (stable). For local development:

```bash
./build.sh          # native (macOS/Linux) — quick sanity check
```

For Linux release binaries from macOS, install rustup + musl targets first:

```bash
brew install rustup filosottile/musl-cross/musl-cross \
             filosottile/musl-cross/musl-cross-aarch64
rustup-init -y
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

./build.sh amd64    # ah4c-stream-amd64 (linux/amd64, static)
./build.sh arm64    # ah4c-stream-arm64 (linux/arm64, static)
./build.sh both
```

Or skip local cross-compile entirely — push a tag and let `.github/workflows/release.yml` build both binaries and attach them to the GitHub release.

Built with `opt-level=3`, LTO, `panic=abort`, stripped. No runtime dependencies — the musl build runs on any Linux base image.

## Behavior constants

All values match PR #9's `stallTolerantReader`:

| Name | Value | Meaning |
|------|-------|---------|
| `STALL_READ_GAP`       | 500 ms | queue-empty window before injecting NULLs |
| `SRC_STALL_RECONNECT`  | 5 s    | source-idle threshold before forced reconnect |
| `SRC_RECONNECT_BACKOFF`| 2 s    | wait between failed reconnect attempts |
| `MAX_UNHEALTHY`        | 15 s   | total no-source-bytes budget before giving up |
| `CHUNK_SIZE`           | 32 KiB | read / forward chunk size |
| `QUEUE_DEPTH`          | 2      | in-flight chunks (~64 KiB). PR #9 uses 64 because it's in-process with no downstream pipe; standalone has an AH4C stdin pipe whose 64 KiB kernel buffer already provides shock absorption, so anything held here is pure added latency |

## Attribution

The `stallTolerantReader` semantics — producer thread, 500 ms stall timer, NULL-packet injection, reconnect-with-backoff, 15 s unhealthy budget — are a direct port of [`main.go` in PR #9](https://github.com/sullrich/ah4c/pull/9/files) of [sullrich/ah4c](https://github.com/sullrich/ah4c), adapted for standalone use: no `io.Reader` facade, an extra stdout-pipe hop to AH4C, and a smaller queue to compensate.
