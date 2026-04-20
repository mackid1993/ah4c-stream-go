// ah4c-stream (Go port). AH4C CMD-mode streaming shim.
//
// Loop: http.Get encoder, io.Copy body → stdout, on EOF reconnect.
// Pure passthrough — no NULL injection anywhere. DVR's PCR-driven
// playback runs at wall-clock rate.
//
// Cold vs warm detection via /tmp state file keyed by URL:
//
//   cold  — no recent touch. AH4C spawned us in parallel with
//           prebmitune.sh, so session 1 is pre-deeplink HDMI junk
//           (idle screen, previous channel, loading). Discard it;
//           the encoder's ~5 s TCP cycle hands us a clean session 2
//           with real post-deeplink content.
//
//   warm  — touched in the last warmWindow. AH4C is respawning us
//           for a DVR reconnect on an already-tuned encoder, so
//           session 1 is real content — pass it through.
//
// The file's mtime is refreshed after every stdout-bound session
// end, so continuous streams stay warm.
//
// LinkPi closes TCP every ~5 s by design; reconnecting is normal.
// Exit only when stdout writes fail (AH4C killed us) or the encoder
// has been unreachable for deadBudget.
package main

import (
	"hash/fnv"
	"io"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"time"
)

const (
	reconnectPause = 50 * time.Millisecond
	deadBudget     = 30 * time.Second
	warmWindow     = 10 * time.Second
)

func stateFile(url string) string {
	h := fnv.New64a()
	h.Write([]byte(url))
	return filepath.Join(os.TempDir(), "ah4c-stream-"+strconv.FormatUint(h.Sum64(), 16))
}

func touch(path string) {
	if f, err := os.Create(path); err == nil {
		f.Close()
	}
}

func main() {
	log.SetFlags(log.Ltime | log.Lmicroseconds)
	log.SetOutput(os.Stderr)

	if len(os.Args) < 2 {
		log.Fatalln("usage: ah4c-stream <url>")
	}
	url := os.Args[1]
	sf := stateFile(url)

	warm := false
	if st, err := os.Stat(sf); err == nil {
		age := time.Since(st.ModTime())
		if age < warmWindow {
			warm = true
			log.Printf("start url=%s WARM (last stream %dms ago)", url, age.Milliseconds())
		} else {
			log.Printf("start url=%s cold (last stream %v ago)", url, age)
		}
	} else {
		log.Printf("start url=%s cold (no state file)", url)
	}

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

		// Cold session 1 = pre-deeplink junk → discard. Warm or
		// session 2+ = real content → stdout.
		var dst io.Writer = os.Stdout
		if !warm && sessions == 1 {
			dst = io.Discard
			log.Printf("session=1 DISCARD (cold — pre-deeplink warmup)")
		}
		n, werr := io.Copy(dst, resp.Body)
		resp.Body.Close()
		log.Printf("session=%d bytes=%d werr=%v", sessions, n, werr)
		if werr != nil {
			log.Printf("stdout closed — exit")
			return
		}
		if dst == os.Stdout {
			touch(sf)
		}
	}
}
