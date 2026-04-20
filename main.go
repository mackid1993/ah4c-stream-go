// ah4c-stream: standalone AH4C CMD-mode tuner that mirrors AH4C PR #9's
// stallTolerantReader behavior. Because this is standalone (not wrapped by
// AH4C's HTTP handler), there's no io.Reader facade to expose: the consumer
// simply writes os.Stdout directly, and the producer feeds it through a
// buffered channel — same two-layer split as stallTolerantReader in
// main.go, without the extra Reader wrapper that would be redundant here.
//
// Architecture (maps 1:1 to stallTolerantReader in ah4c/main.go):
//
//   Producer goroutine   — owns the encoder socket. Per-read deadline of
//                          srcStallReconnect (5 s); on timeout/EOF, closes
//                          the body and enters reconnect-with-backoff.
//                          Pushes each non-empty read into `chunks`.
//   Consumer (main)      — pulls from `chunks` with a stallReadGap (500 ms)
//                          timer. On timer fire, writes a 32 KiB block of
//                          MPEG-TS NULL packets (PID 0x1FFF) to os.Stdout
//                          so the HTTP response to DVR keeps making forward
//                          progress. On real data, writes it through.
//   chunks channel       — queueDepth = 64 (~2 MiB), shock-absorbs encoder
//                          bursts so the consumer's 500 ms timer only fires
//                          during an actual source stall, not on benign
//                          frame-pacing gaps.
//
// Timeouts / retry budget (match PR #9):
//   stallReadGap         = 500ms  — consumer-side NULL injection trigger
//   srcStallReconnect    = 5s     — producer-side per-read deadline
//   srcReconnectBackoff  = 2s     — wait between failed reconnect attempts
//   maxUnhealthyDuration = 15s    — total no-real-bytes time before give-up
//
// Usage (in ah4c.env):
//   CMD1="./scripts/osprey/dtvospreydeeplinks/ah4c-stream $ENCODER1_URL"
//
// Exit codes:
//   0  - 15s no-source-bytes budget expired, or downstream closed its pipe
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
	queueDepth           = 64 // ~2 MiB in-flight, matches PR #9
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

	chunks := make(chan []byte, queueDepth)
	done := make(chan struct{})

	go producer(rawURL, conn, br, chunks, done)
	consumer(chunks, done)
}

// producer owns the encoder socket and the reconnect loop. It pushes
// non-empty reads into chunks. On a 5 s per-read timeout or any other read
// error, it closes the body and reconnects; during the reconnect window the
// consumer keeps DVR fed with NULL packets. When the 15 s no-real-bytes
// budget expires (or reconnect is exhausted), it returns; the deferred
// close(chunks) signals the consumer to exit, which closes done in turn.
func producer(rawURL string, conn net.Conn, br *bufio.Reader, chunks chan<- []byte, done chan struct{}) {
	defer close(chunks)
	defer func() {
		if conn != nil {
			conn.Close()
		}
	}()

	lastReal := time.Now()
	buf := make([]byte, chunkSize)

	for {
		select {
		case <-done:
			return
		default:
		}
		if time.Since(lastReal) >= maxUnhealthyDuration {
			logf("no source bytes for %v; closing reader so DVR sees EOF", maxUnhealthyDuration)
			return
		}

		_ = conn.SetReadDeadline(time.Now().Add(srcStallReconnect))
		n, rerr := br.Read(buf)

		if n > 0 {
			data := make([]byte, n)
			copy(data, buf[:n])
			select {
			case chunks <- data:
			case <-done:
				return
			}
			lastReal = time.Now()
			if rerr == nil {
				continue
			}
		}

		// n == 0 OR err != nil after a partial read — close and reconnect.
		if rerr != nil {
			if ne, ok := rerr.(net.Error); ok && ne.Timeout() {
				logf("source idle %v; reconnecting", srcStallReconnect)
			} else {
				logf("source error (%v); reconnecting", rerr)
			}
		}
		conn.Close()
		conn = nil

		for {
			select {
			case <-done:
				return
			default:
			}
			if time.Since(lastReal) >= maxUnhealthyDuration {
				logf("no source bytes for %v during reconnect; closing reader so DVR sees EOF", maxUnhealthyDuration)
				return
			}
			newConn, newBr, cerr := connect(rawURL)
			if cerr == nil {
				logf("reconnected")
				conn, br = newConn, newBr
				break
			}
			logf("reconnect failed: %v", cerr)
			select {
			case <-time.After(srcReconnectBackoff):
			case <-done:
				return
			}
		}
	}
}

// consumer pulls chunks from the producer with a 500 ms stall-read timer.
// Real data → write to os.Stdout. Timer fires (channel empty 500 ms) →
// write a NULL-packet block so DVR's HTTP response keeps progressing.
// Producer-closed channel or stdout EPIPE → exit.
func consumer(chunks <-chan []byte, done chan struct{}) {
	defer close(done)
	for {
		timer := time.NewTimer(stallReadGap)
		select {
		case data, ok := <-chunks:
			if !timer.Stop() {
				<-timer.C
			}
			if !ok {
				return
			}
			if _, werr := os.Stdout.Write(data); werr != nil {
				return
			}
		case <-timer.C:
			if _, werr := os.Stdout.Write(nullChunk); werr != nil {
				return
			}
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
	// Clear the connect-phase deadline; producer manages per-read deadlines.
	_ = conn.SetDeadline(time.Time{})
	return conn, br, nil
}
