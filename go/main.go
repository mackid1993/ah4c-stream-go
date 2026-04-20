// ah4c-stream (Go port). AH4C CMD-mode streaming shim.
//
// Loop: http.Get encoder, io.Copy body → stdout, on EOF reconnect.
// Pure passthrough — no NULL injection anywhere. DVR's PCR-driven
// playback runs at wall-clock rate.
//
// Cold vs hot detection by intrinsic bitrate. Until one session
// commits hot, each session starts with a probeBytes read: if it
// fills in < probeWindow the encoder is delivering tuned content
// (warm retune or fresh post-deeplink) — flush the probe to stdout
// and commit to passthrough. Otherwise (slow fill or early EOF) the
// session is pre-deeplink HDMI junk (idle screen, previous channel,
// loading) — drain it to /dev/null and try the next session. Once
// committed, every session flows straight to stdout. No state file,
// no /tmp pollution, no multi-tuner collision.
//
// This skips pre-deeplink junk on a cold tune (prevents the DVR's
// catch-up fast-play) while preserving the full first session on a
// warm retune.
//
// Teardown: client Timeout, dial Timeout, and ResponseHeaderTimeout
// guarantee a hung encoder fails fast so this process exits and
// AH4C releases the tuner.
//
// LinkPi closes TCP every ~5 s by design; reconnecting is normal.
// Exit only on unrecoverable: stdout write failure or encoder dead
// for deadBudget.
package main

import (
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"time"
)

const (
	reconnectPause = 50 * time.Millisecond
	deadBudget     = 30 * time.Second
	probeBytes     = 32 * 1024
	probeWindow    = 500 * time.Millisecond
)

var client = &http.Client{
	Timeout: 60 * time.Second,
	Transport: &http.Transport{
		DialContext: (&net.Dialer{
			Timeout: 3 * time.Second,
		}).DialContext,
		ResponseHeaderTimeout: 10 * time.Second,
	},
}

func main() {
	log.SetFlags(log.Ltime | log.Lmicroseconds)
	log.SetOutput(os.Stderr)

	if len(os.Args) < 2 {
		log.Fatalln("usage: ah4c-stream <url>")
	}
	url := os.Args[1]
	log.Printf("start url=%s", url)

	lastGood := time.Now()
	committed := false
	sessions := 0
	for {
		sessions++
		tGet := time.Now()
		resp, err := client.Get(url)
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

		if !committed {
			probe := make([]byte, probeBytes)
			t0 := time.Now()
			n, rerr := io.ReadFull(resp.Body, probe)
			dt := time.Since(t0)
			hot := rerr == nil && n == probeBytes && dt < probeWindow
			if !hot {
				io.Copy(io.Discard, resp.Body)
				resp.Body.Close()
				log.Printf("session=%d COLD probe_n=%d dt_ms=%d — drained",
					sessions, n, dt.Milliseconds())
				continue
			}
			committed = true
			log.Printf("session=%d HOT probe_n=%d dt_ms=%d — committed",
				sessions, n, dt.Milliseconds())
			if _, werr := os.Stdout.Write(probe[:n]); werr != nil {
				resp.Body.Close()
				return
			}
		}

		n, werr := io.Copy(os.Stdout, resp.Body)
		resp.Body.Close()
		log.Printf("session=%d bytes=%d werr=%v", sessions, n, werr)
		if werr != nil {
			log.Printf("io.Copy err — exit")
			return
		}
	}
}
