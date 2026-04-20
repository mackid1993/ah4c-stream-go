// ah4c-stream: standalone AH4C CMD-mode tuner that behaviorally mirrors
// AH4C PR #9's stallTolerantReader, but without wrapping anything in an
// io.Reader facade. A single pump loop reads from the encoder TCP socket
// with a 500ms per-read deadline and writes directly to stdout; when the
// socket is idle it writes a 32 KiB block of MPEG-TS NULL packets (PID
// 0x1FFF) instead. TS demuxers including Channels DVR drop NULL packets
// on demux, so they're safe to inject as a keep-alive while the upstream
// encoder is between bytes.
//
// Timeouts / retry budget (match PR #9):
//   stallReadGap         = 500ms  — per-read deadline; on timeout, write NULLs
//   srcStallReconnect    = 5s     — consecutive no-byte time before reconnect
//   srcReconnectBackoff  = 2s     — wait between failed reconnect attempts
//   maxUnhealthyDuration = 15s    — total no-real-bytes time before give-up
//
// Usage (in ah4c.env):
//   CMD1="./scripts/osprey/dtvospreydeeplinks/ah4c-stream $ENCODER1_URL"
//
// Exit codes:
//   0  - 15s no-source-bytes budget expired, downstream closed, or source EOF
//   2  - bad args, initial connect failed (fails-fast, no leading NULLs),
//        chunked transfer encoding (HDMI encoders don't use it), non-200
//
// Build:
//   GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -o ah4c-stream-amd64 .
//   GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o ah4c-stream-arm64 .
package main

import (
	"bufio"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"
)

const (
	stallReadGap         = 500 * time.Millisecond
	srcStallReconnect    = 5 * time.Second
	srcReconnectBackoff  = 2 * time.Second
	maxUnhealthyDuration = 15 * time.Second
	connectTimeout       = 5 * time.Second
	chunkSize            = 32 * 1024
)

// 174 × 188-byte TS NULL packets = 32,712 bytes (≤ 32 KiB).
// sync=0x47, PID=0x1FFF, adaptation_field_control=01, CC=0, payload=0xFF.
var nullChunk = func() []byte {
	const count = 174
	buf := make([]byte, count*188)
	for i := 0; i < count; i++ {
		off := i * 188
		buf[off+0] = 0x47
		buf[off+1] = 0x1F
		buf[off+2] = 0xFF
		buf[off+3] = 0x10
		for j := 4; j < 188; j++ {
			buf[off+j] = 0xFF
		}
	}
	return buf
}()

var label string

func logf(format string, args ...interface{}) {
	fmt.Fprintf(os.Stderr, "["+label+"] "+format+"\n", args...)
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: ah4c-stream <encoder_url>")
		os.Exit(2)
	}
	rawURL := os.Args[1]
	if u, err := url.Parse(rawURL); err == nil && u.Host != "" {
		label = "tuner=" + u.Host
	} else {
		label = "tuner=" + rawURL
	}

	// Fail-fast on initial connect — matches AH4C tune() returning
	// "device(s) not available" (no leading NULL bytes before EOF).
	conn, br, err := connect(rawURL)
	if err != nil {
		logf("initial connect failed: %v", err)
		os.Exit(2)
	}
	pump(rawURL, conn, br)
}

// pump is the single read/write loop. It owns the connection, the last-real-
// bytes clock, and the reconnect trigger. No goroutines, no channels.
func pump(rawURL string, conn net.Conn, br *bufio.Reader) {
	defer func() {
		if conn != nil {
			conn.Close()
		}
	}()

	lastReal := time.Now()
	buf := make([]byte, chunkSize)

	for {
		if time.Since(lastReal) >= maxUnhealthyDuration {
			logf("no source bytes for %v; exiting so DVR sees EOF", maxUnhealthyDuration)
			return
		}

		_ = conn.SetReadDeadline(time.Now().Add(stallReadGap))
		n, rerr := br.Read(buf)

		if n > 0 {
			if _, werr := os.Stdout.Write(buf[:n]); werr != nil {
				return
			}
			lastReal = time.Now()
		}
		if rerr == nil {
			continue
		}

		if ne, ok := rerr.(net.Error); ok && ne.Timeout() {
			// 500ms passed with no socket bytes — inject NULL keep-alive.
			if _, werr := os.Stdout.Write(nullChunk); werr != nil {
				return
			}
			if time.Since(lastReal) < srcStallReconnect {
				continue
			}
			logf("source idle %v; reconnecting", srcStallReconnect)
		} else {
			logf("source error (%v); reconnecting", rerr)
		}

		conn.Close()
		conn, br = nil, nil
		newConn, newBr, cerr := reconnectLoop(rawURL, &lastReal)
		if cerr != nil {
			logf("%v", cerr)
			return
		}
		conn, br = newConn, newBr
	}
}

// reconnectLoop keeps trying to reopen the encoder. NULL packets continue
// flowing to DVR during each backoff so the HTTP response stays alive. It
// respects the 15s no-real-bytes budget: once exhausted, pump exits and
// DVR sees EOF.
func reconnectLoop(rawURL string, lastReal *time.Time) (net.Conn, *bufio.Reader, error) {
	for {
		if time.Since(*lastReal) >= maxUnhealthyDuration {
			return nil, nil, fmt.Errorf("no source bytes for %v during reconnect; exiting", maxUnhealthyDuration)
		}
		conn, br, err := connect(rawURL)
		if err == nil {
			logf("reconnected")
			return conn, br, nil
		}
		logf("reconnect failed: %v", err)
		deadline := time.Now().Add(srcReconnectBackoff)
		for time.Now().Before(deadline) {
			if _, werr := os.Stdout.Write(nullChunk); werr != nil {
				return nil, nil, fmt.Errorf("downstream closed")
			}
			if time.Since(*lastReal) >= maxUnhealthyDuration {
				return nil, nil, fmt.Errorf("no source bytes for %v during backoff; exiting", maxUnhealthyDuration)
			}
			time.Sleep(stallReadGap)
		}
	}
}

// connect opens a TCP socket, sends an HTTP/1.0 GET, parses the response
// headers, and returns the socket + bufio.Reader positioned at the first
// body byte. HTTP/1.0 + Connection: close avoids chunked transfer encoding
// and keep-alive behavior; body length is implicitly "until EOF."
func connect(rawURL string) (net.Conn, *bufio.Reader, error) {
	u, err := url.Parse(rawURL)
	if err != nil {
		return nil, nil, err
	}
	if u.Scheme != "http" {
		return nil, nil, fmt.Errorf("unsupported scheme %q (only http)", u.Scheme)
	}
	host := u.Host
	if _, _, splitErr := net.SplitHostPort(host); splitErr != nil {
		host = host + ":80"
	}
	conn, err := net.DialTimeout("tcp", host, connectTimeout)
	if err != nil {
		return nil, nil, err
	}
	path := u.RequestURI()
	if path == "" {
		path = "/"
	}
	req := fmt.Sprintf(
		"GET %s HTTP/1.0\r\nHost: %s\r\nUser-Agent: ah4c-stream\r\nConnection: close\r\n\r\n",
		path, u.Host,
	)
	_ = conn.SetDeadline(time.Now().Add(connectTimeout))
	if _, err := io.WriteString(conn, req); err != nil {
		conn.Close()
		return nil, nil, err
	}
	br := bufio.NewReaderSize(conn, chunkSize)
	resp, err := http.ReadResponse(br, nil)
	if err != nil {
		conn.Close()
		return nil, nil, err
	}
	if resp.StatusCode != 200 {
		conn.Close()
		return nil, nil, fmt.Errorf("status %s", resp.Status)
	}
	for _, te := range resp.TransferEncoding {
		if strings.EqualFold(te, "chunked") {
			conn.Close()
			return nil, nil, fmt.Errorf("chunked transfer encoding not supported")
		}
	}
	// Clear the connect-phase deadline; pump manages per-read deadlines.
	_ = conn.SetDeadline(time.Time{})
	return conn, br, nil
}
