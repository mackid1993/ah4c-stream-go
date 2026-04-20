# ah4c-stream-go

A standalone AH4C CMD-mode tuner that wraps an HDMI encoder's HTTP stream in the same `stallTolerantReader` logic as [AH4C PR #9](https://github.com/sullrich/ah4c/pull/9).

If AH4C merges PR #9 upstream, you don't need this — AH4C's network-encoder branch will wrap every tune in the reader automatically. Until then, this binary reproduces the behavior from outside AH4C: drop it in your scripts directory, point a `CMD1=` at it, and you get the same reliability on top of the stock `ghcr.io/mackid1993/ah4c:latest` image — no Dockerfile changes, no custom build.

## What it does

When an HDMI encoder hiccups (channel change, reboot, brief disconnect), it stops sending TS data. AH4C's default path passes the encoder stream straight to Channels DVR, so DVR sees the stream die and gives up.

This binary puts a buffering layer between the encoder and DVR:

- Reads from the encoder into a 64-chunk (~2 MB) queue.
- Forwards to DVR from the queue.
- If the queue is empty for 500 ms, fills DVR's buffer with **MPEG-TS NULL packets** (PID `0x1FFF`) that every TS demuxer drops — so DVR keeps seeing bytes flowing and doesn't time out.
- If the source is idle for 5 s, closes the connection and reconnects behind the scenes. NULL packets continue while reconnecting.
- If no real source bytes arrive for 15 s total, gives up and closes the reader so DVR can react.
- If the **initial** connect fails, exits with code `2` and zero stdout output so AH4C surfaces it as a tune failure (matches Go's `tune()` returning "device(s) not available").

The warm path has zero added latency vs. a raw `curl` — the reader only kicks in when something actually breaks.

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
| `2`  | Bad args, initial connect failure, or non-200 upstream |

Stderr carries log lines prefixed `[tuner=host:port]` for AH4C's container logs.

## Install (prebuilt binary)

Grab the latest release from the [releases page](https://github.com/mackid1993/ah4c-stream-go/releases) and drop the right arch into your AH4C container:

- `ah4c-stream-amd64` — most x86-64 Docker hosts
- `ah4c-stream-arm64` — Raspberry Pi, Apple Silicon hosts running arm64 containers

Make it executable and point `CMD1=` at it.

## Build from source

Requires Go 1.21+. Cross-compile from any OS:

```bash
./build.sh          # linux/amd64 (default)
./build.sh arm64    # linux/arm64
./build.sh both     # both, named ah4c-stream-amd64 / ah4c-stream-arm64
```

The binary is statically linked (`CGO_ENABLED=0`), stripped (`-ldflags="-s -w"`), and path-trimmed (`-trimpath`). Runs on any Linux base image — no glibc dependency.

## Behavior constants

All values match PR #9's `stallTolerantReader`:

| Name | Value | Meaning |
|------|-------|---------|
| `stallReadGap`         | 500 ms | queue-empty window before injecting NULLs |
| `srcStallReconnect`    | 5 s    | source-idle threshold before forced reconnect |
| `srcReconnectBackoff`  | 2 s    | wait between failed reconnect attempts |
| `maxUnhealthyDuration` | 15 s   | total no-source-bytes budget before giving up |
| `chunkSize`            | 32 KiB | read / forward chunk size |
| `queueDepth`           | 64     | in-flight chunks (~2 MB max buffer) |

## Attribution

The `stallTolerantReader` type, producer goroutine, `Read`, `Close`, and `readWithDeadline` helpers are lifted verbatim from [`main.go` in PR #9](https://github.com/sullrich/ah4c/pull/9/files) of [sullrich/ah4c](https://github.com/sullrich/ah4c). This repo wraps them in a small `main()` so the same behavior is available to AH4C's CMD-mode tuners without requiring the PR to be merged upstream.
