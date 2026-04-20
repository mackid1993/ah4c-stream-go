// ah4c-stream (Go port). AH4C CMD-mode streaming shim.
//
// Loop: http.Get encoder, io.Copy body → stdout, on EOF reconnect.
// Pure passthrough — DVR's PCR-driven playback runs at wall-clock.
//
// Session 1 is probed. If probeBytes arrives in < probeWindow the
// encoder is already delivering tuned content (warm retune) — flush
// the probe and stream session 1 to stdout. Otherwise it's pre-
// deeplink HDMI junk — discard to EOF and let session 2 be the
// first real stream.
//
// Every stdout write carries a writeTimeout deadline. Covers both
// EPIPE (pipe closed) and a stalled reader (pipe buffer fills, Write
// blocks) — either way the write fails and we exit so AH4C can
// release the tuner.
//
// LinkPi closes TCP every ~5 s by design; reconnecting is normal.
// Exit only on stdout write failure or encoder dead for deadBudget.
package main

import (
	"io"
	"log"
	"net/http"
	"os"
	"time"
)

const (
	reconnectPause = 50 * time.Millisecond
	deadBudget     = 30 * time.Second
	probeBytes     = 32 * 1024
	probeWindow    = 500 * time.Millisecond
	writeTimeout   = 10 * time.Second
)

func writeStdout(p []byte) error {
	os.Stdout.SetWriteDeadline(time.Now().Add(writeTimeout))
	_, err := os.Stdout.Write(p)
	return err
}

func copyStdout(src io.Reader) (int64, error) {
	buf := make([]byte, 32*1024)
	var total int64
	for {
		nr, rerr := src.Read(buf)
		if nr > 0 {
			if werr := writeStdout(buf[:nr]); werr != nil {
				return total, werr
			}
			total += int64(nr)
		}
		if rerr != nil {
			if rerr == io.EOF {
				return total, nil
			}
			return total, rerr
		}
	}
}

var nullPacket = func() []byte {
	p := make([]byte, 188)
	p[0] = 0x47 // sync
	p[1] = 0x1F // PID hi
	p[2] = 0xFF // PID lo → 0x1FFF NULL
	p[3] = 0x10 // AF_control=01, CC=0
	for i := 4; i < 188; i++ {
		p[i] = 0xFF
	}
	return p
}()

func main() {
	log.SetFlags(log.Ltime | log.Lmicroseconds)
	log.SetOutput(os.Stderr)

	if len(os.Args) < 2 {
		log.Fatalln("usage: ah4c-stream <url>")
	}
	url := os.Args[1]
	log.Printf("start url=%s", url)

	lastGood := time.Now()
	sessions := 0
	for {
		sessions++
		tGet := time.Now()
		resp, err := http.Get(url)
		if err != nil {
			log.Printf("session=%d get_err=%v", sessions, err)
			if time.Since(lastGood) > deadBudget {
				log.Fatalf("encoder dead for %v — giving up", deadBudget)
			}
			time.Sleep(reconnectPause)
			continue
		}
		if resp.StatusCode != 200 {
			log.Printf("session=%d status=%s", sessions, resp.Status)
			resp.Body.Close()
			if time.Since(lastGood) > deadBudget {
				log.Fatalf("encoder non-200 for %v — giving up", deadBudget)
			}
			time.Sleep(reconnectPause)
			continue
		}
		log.Printf("session=%d connected dt_ms=%d", sessions,
			time.Since(tGet).Milliseconds())
		lastGood = time.Now()

		if sessions == 1 {
			probe := make([]byte, probeBytes)
			t0 := time.Now()
			n, rerr := io.ReadFull(resp.Body, probe)
			dt := time.Since(t0)
			hot := rerr == nil && n == probeBytes && dt < probeWindow
			if !hot {
				io.Copy(io.Discard, resp.Body)
				resp.Body.Close()
				log.Printf("session=1 COLD probe_n=%d dt_ms=%d — drained",
					n, dt.Milliseconds())
				if werr := writeStdout(nullPacket); werr != nil {
					log.Printf("stdout closed — exit")
					return
				}
				continue
			}
			log.Printf("session=1 HOT probe_n=%d dt_ms=%d — passthrough",
				n, dt.Milliseconds())
			if werr := writeStdout(probe[:n]); werr != nil {
				resp.Body.Close()
				return
			}
		}

		n, werr := copyStdout(resp.Body)
		resp.Body.Close()
		log.Printf("session=%d bytes=%d werr=%v", sessions, n, werr)
		if werr != nil {
			log.Printf("stdout closed — exit")
			return
		}
		if werr := writeStdout(nullPacket); werr != nil {
			log.Printf("stdout closed — exit")
			return
		}
	}
}
