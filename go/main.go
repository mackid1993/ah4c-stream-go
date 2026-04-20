// ah4c-stream (Go port). Standalone AH4C CMD-mode tuner built on the
// stallTolerantReader from sullrich/ah4c PR #9 (main.go:1333-1529),
// wrapped with a thin main() that takes a URL on argv and writes the
// MPEG-TS body to stdout.
//
// Why this exists: the Rust port repeatedly diverged from PR #9's
// behavior because we had to hand-roll HTTP (keep-alive, chunked
// transfer decoding, timeouts) that Go's net/http gives us for free.
// Using http.Get verbatim from PR #9 keeps us in lockstep with the
// upstream fix the user authored and confirmed fast.
package main

import (
	"context"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"
)

// Matches PR #9 constants exactly.
const (
	stallReadGap         = 500 * time.Millisecond // queue-empty before injecting nulls
	srcStallReconnect    = 5 * time.Second        // source idle before forced reconnect
	srcReconnectBackoff  = 2 * time.Second        // wait between failed reconnect attempts
	maxUnhealthyDuration = 15 * time.Second       // total time without any real bytes before giving up
	chunkSize            = 32 * 1024
	queueDepth           = 64 // ~2 MiB in-flight
)

// Single 188-byte MPEG-TS NULL packet (PID 0x1FFF). TS demuxers drop
// these on demux, so they're safe to inject as a keepalive when the
// encoder stops producing.
var nullTSPacket = func() [188]byte {
	var p [188]byte
	p[0] = 0x47 // sync byte
	p[1] = 0x1F // TEI=0, PUSI=0, TP=0, PID upper 5 bits = 0x1F
	p[2] = 0xFF // PID lower 8 bits (full PID = 0x1FFF)
	p[3] = 0x10 // scrambling=0, AF_control=01 (payload only), CC=0
	for i := 4; i < 188; i++ {
		p[i] = 0xFF
	}
	return p
}()

// Verbatim port of PR #9's stallTolerantReader. Producer goroutine pulls
// from the encoder body into a bounded channel; Read() drains the channel
// and, on stallReadGap empty, fills the caller's buffer with NULL packets
// so the HTTP response keeps making forward progress toward DVR.
type stallTolerantReader struct {
	chunks      chan []byte
	closed      chan struct{}
	closeOnce   sync.Once
	bodyMu      sync.Mutex
	body        io.ReadCloser
	reconnectFn func() (io.ReadCloser, error)
}

func newStallTolerantReader(body io.ReadCloser, reconnectFn func() (io.ReadCloser, error)) *stallTolerantReader {
	s := &stallTolerantReader{
		chunks:      make(chan []byte, queueDepth),
		closed:      make(chan struct{}),
		body:        body,
		reconnectFn: reconnectFn,
	}
	go s.producer()
	return s
}

func (s *stallTolerantReader) producer() {
	chunk := make([]byte, chunkSize)
	lastRealBytes := time.Now()
	giveUp := func(reason string) {
		log.Printf("%s; closing reader so downstream sees EOF", reason)
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
			log.Printf("source idle/error (%v); reconnecting", err)
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
			log.Printf("reconnect failed: %v", rerr)
			select {
			case <-time.After(srcReconnectBackoff):
			case <-s.closed:
				return
			}
		}
		log.Printf("reconnected")
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

func fetch(url string) (io.ReadCloser, error) {
	resp, err := http.Get(url)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode != 200 {
		resp.Body.Close()
		return nil, fmt.Errorf("status %s", resp.Status)
	}
	return resp.Body, nil
}

func main() {
	log.SetFlags(log.Ltime | log.Lmicroseconds)
	log.SetOutput(os.Stderr)

	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: ah4c-stream <url>")
		os.Exit(2)
	}
	url := os.Args[1]

	log.Printf("start pid=%d url=%s", os.Getpid(), url)

	tStart := time.Now()
	body, err := fetch(url)
	if err != nil {
		log.Fatalf("initial fetch failed: %v", err)
	}
	log.Printf("connect ok dt_ms=%d", time.Since(tStart).Milliseconds())

	reader := newStallTolerantReader(body, func() (io.ReadCloser, error) {
		return fetch(url)
	})

	// SIGTERM/INT/HUP → close reader (which Close()s the encoder body,
	// sending FIN) then exit. Matches the Rust port's teardown semantics.
	sigs := make(chan os.Signal, 1)
	signal.Notify(sigs, syscall.SIGTERM, syscall.SIGINT, syscall.SIGHUP)
	go func() {
		sig := <-sigs
		log.Printf("signal=%v; closing", sig)
		reader.Close()
		// Brief grace period so the encoder sees FIN and the producer
		// goroutine exits; then hard-exit to avoid blocking in io.Copy.
		time.Sleep(50 * time.Millisecond)
		os.Exit(0)
	}()

	// Stream the reader out to stdout. io.Copy's default buffer is 32 KiB
	// — same as our chunkSize — so NULL-fill and real-data reads match
	// in granularity.
	_, err = io.Copy(os.Stdout, reader)
	if err != nil {
		log.Printf("copy ended: %v", err)
	}
}
