// ah4c-stream (Go port). AH4C CMD-mode streaming shim.
//
// Loop: http.Get encoder, io.Copy body → stdout, on EOF reconnect.
// Pure passthrough — no NULL injection anywhere. DVR's PCR-driven
// playback runs at wall-clock rate.
//
// First session is DISCARDED to /dev/null. Reason: AH4C spawns this
// binary in parallel with prebmitune.sh, which means for the first
// ~5 s we'd be streaming whatever the encoder's HDMI input was BEFORE
// the deeplink fires (idle screen, previous channel, loading). The
// DVR records that junk, then real post-deeplink content arrives with
// a fresh PCR base, and the DVR catches up by fast-forwarding through
// the junk — the "crazy fast after it finally tuned" symptom. The
// encoder's own ~5 s session timeout matches the deeplink window, so
// dropping session 1 gives a clean handoff with zero coordination.
//
// LinkPi closes TCP every ~5 s by design; reconnecting is normal.
// Exit only when stdout writes fail (AH4C killed us) or the encoder
// has been unreachable for deadBudget.
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
)

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

		// Session 1 is pre-deeplink junk — discard it. Session 2+
		// is the real tuned content; pass through to stdout.
		var dst io.Writer = os.Stdout
		if sessions == 1 {
			dst = io.Discard
			log.Printf("session=1 DISCARD (pre-deeplink warmup)")
		}
		n, werr := io.Copy(dst, resp.Body)
		resp.Body.Close()
		log.Printf("session=%d bytes=%d werr=%v", sessions, n, werr)
		if werr != nil {
			log.Printf("stdout closed — exit")
			return
		}
	}
}
