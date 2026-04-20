// ah4c-stream: standalone AH4C CMD-mode tuner that wraps an HDMI encoder's
// HTTP stream in the same stallTolerantReader logic as AH4C PR #9.
//
// Usage (in ah4c.env):
//   CMD1="./scripts/osprey/dtvospreydeeplinks/ah4c-stream $ENCODER1_URL"
//
// Exit codes:
//   0  - clean shutdown (budget expired, signal, downstream EOF)
//   2  - bad args, initial connect failure, or non-200 upstream — matches
//        Go tune() returning "device(s) not available"
//
// Build for AH4C container (static, any Linux arch):
//   GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -o ah4c-stream .
//   GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o ah4c-stream .
package main

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"sync"
	"time"
)

// =========================================================================
// stallTolerantReader — lifted verbatim from AH4C main.go PR #9.
// =========================================================================

// nullTSPacket is a single 188-byte MPEG-TS NULL packet (PID 0x1FFF). TS
// demuxers including Channels DVR drop these on demux, so they're safe to
// inject as a keepalive when the upstream encoder briefly stops producing.
var nullTSPacket = func() [188]byte {
	var p [188]byte
	p[0] = 0x47 // sync byte
	p[1] = 0x1F // TEI=0, PUSI=0, transport_priority=0, PID upper 5 bits = 0x1F
	p[2] = 0xFF // PID lower 8 bits (full PID = 0x1FFF)
	p[3] = 0x10 // scrambling=0, adaptation_field_control=01 (payload only), CC=0
	for i := 4; i < 188; i++ {
		p[i] = 0xFF
	}
	return p
}()

type stallTolerantReader struct {
	chunks      chan []byte
	closed      chan struct{}
	closeOnce   sync.Once
	bodyMu      sync.Mutex
	body        io.ReadCloser
	reconnectFn func() (io.ReadCloser, error)
	label       string
}

const (
	stallReadGap         = 500 * time.Millisecond
	srcStallReconnect    = 5 * time.Second
	srcReconnectBackoff  = 2 * time.Second
	maxUnhealthyDuration = 15 * time.Second
	chunkSize            = 32 * 1024
	queueDepth           = 64
)

func newStallTolerantReader(body io.ReadCloser, reconnectFn func() (io.ReadCloser, error), label string) *stallTolerantReader {
	s := &stallTolerantReader{
		chunks:      make(chan []byte, queueDepth),
		closed:      make(chan struct{}),
		body:        body,
		reconnectFn: reconnectFn,
		label:       label,
	}
	go s.producer()
	return s
}

func (s *stallTolerantReader) producer() {
	chunk := make([]byte, chunkSize)
	lastRealBytes := time.Now()
	giveUp := func(reason string) {
		logger("[%s] %s; closing reader so DVR sees EOF", s.label, reason)
		s.closeOnce.Do(func() { close(s.closed) })
	}
	for {
		select {
		case <-s.closed:
			return
		default:
		}
		if time.Since(lastRealBytes) > maxUnhealthyDuration {
			giveUp(fmt.Sprintf("no source bytes for %v", maxUnhealthyDuration))
			return
		}
		s.bodyMu.Lock()
		body := s.body
		s.bodyMu.Unlock()
		ctx, cancel := context.WithTimeout(context.Background(), srcStallReconnect)
		n, err := readWithDeadline(ctx, body, chunk)
		cancel()
		if n > 0 {
			lastRealBytes = time.Now()
			data := make([]byte, n)
			copy(data, chunk[:n])
			select {
			case s.chunks <- data:
			case <-s.closed:
				return
			}
			if err == nil {
				continue
			}
		}
		if err != nil {
			logger("[%s] source idle/error (%v); reconnecting", s.label, err)
		}
		body.Close()
		if s.reconnectFn == nil {
			s.closeOnce.Do(func() { close(s.closed) })
			return
		}
		var newBody io.ReadCloser
		for {
			select {
			case <-s.closed:
				return
			default:
			}
			if time.Since(lastRealBytes) > maxUnhealthyDuration {
				giveUp(fmt.Sprintf("no source bytes for %v during reconnect", maxUnhealthyDuration))
				return
			}
			nb, rerr := s.reconnectFn()
			if rerr == nil {
				newBody = nb
				break
			}
			logger("[%s] reconnect failed: %v", s.label, rerr)
			select {
			case <-time.After(srcReconnectBackoff):
			case <-s.closed:
				return
			}
		}
		logger("[%s] reconnected", s.label)
		s.bodyMu.Lock()
		s.body = newBody
		s.bodyMu.Unlock()
	}
}

func (s *stallTolerantReader) Read(p []byte) (int, error) {
	timer := time.NewTimer(stallReadGap)
	defer timer.Stop()
	select {
	case <-s.closed:
		return 0, io.EOF
	case data := <-s.chunks:
		return copy(p, data), nil
	case <-timer.C:
		n := 0
		for n+188 <= len(p) {
			copy(p[n:n+188], nullTSPacket[:])
			n += 188
		}
		if n == 0 {
			return copy(p, nullTSPacket[:]), nil
		}
		return n, nil
	}
}

func (s *stallTolerantReader) Close() error {
	s.closeOnce.Do(func() { close(s.closed) })
	s.bodyMu.Lock()
	body := s.body
	s.bodyMu.Unlock()
	if body != nil {
		return body.Close()
	}
	return nil
}

func readWithDeadline(ctx context.Context, r io.Reader, buf []byte) (int, error) {
	type result struct {
		n   int
		err error
	}
	ch := make(chan result, 1)
	go func() {
		n, err := r.Read(buf)
		ch <- result{n, err}
	}()
	select {
	case res := <-ch:
		return res.n, res.err
	case <-ctx.Done():
		return 0, ctx.Err()
	}
}

// =========================================================================
// Standalone driver: wraps the reader in a tiny main() with the same
// contract AH4C's network-encoder branch expects.
// =========================================================================

func logger(format string, args ...interface{}) {
	fmt.Fprintf(os.Stderr, format+"\n", args...)
}

// fetch mirrors what tune() does in main.go before wrapping the body in
// stallTolerantReader: plain http.Get, 200-or-error. http.DefaultClient
// has no timeouts, which matches PR #9 exactly — the only bound on a
// slow-recovering encoder is the producer's 15 s maxUnhealthyDuration
// budget. Previously a custom client with 5 s dial and 5 s response-
// header timeouts was causing reconnects to fail before an encoder mid-
// reboot could finish responding, blowing the budget and EOFing the
// stream prematurely.
func fetch(u string) (io.ReadCloser, error) {
	resp, err := http.Get(u)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode != 200 {
		resp.Body.Close()
		return nil, fmt.Errorf("status %s", resp.Status)
	}
	return resp.Body, nil
}

func labelFor(u string) string {
	parsed, err := url.Parse(u)
	if err != nil || parsed.Host == "" {
		return "tuner=" + u
	}
	return "tuner=" + parsed.Host
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: ah4c-stream <encoder_url>")
		os.Exit(2)
	}
	encoderURL := os.Args[1]
	label := labelFor(encoderURL)

	// Initial fetch — match tune()'s fail-fast semantics. A dead encoder
	// must surface as exit 2 with zero stdout bytes so AH4C reports the
	// tune as failed rather than streaming NULLs for 15 s and then EOF.
	body, err := fetch(encoderURL)
	if err != nil {
		logger("[%s] initial fetch failed: %v", label, err)
		os.Exit(2)
	}

	reconnectFn := func() (io.ReadCloser, error) {
		return fetch(encoderURL)
	}

	reader := newStallTolerantReader(body, reconnectFn, label)
	defer reader.Close()

	// Pump to stdout. Returns when the reader closes (budget expired) or
	// stdout errors (downstream / AH4C closed the pipe).
	_, _ = io.Copy(os.Stdout, reader)
}
