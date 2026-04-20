#!/bin/bash
# prebmitune.sh for osprey/dtvospreydeeplinks
# After ADB wake, probe the encoder for 1s. If the bytes coming through
# are teeny tiny, the signal isn't live yet — sleep 1 and retry.
# Exit when packets flow at live-stream rate.

streamerIP="$1"
streamerNoPort="${streamerIP%%:*}"
adbTarget="adb -s $streamerIP"

mkdir -p "$streamerNoPort"

log() { printf '[prebmitune %s] %s\n' "$(date '+%H:%M:%S')" "$*"; }

trap 'log "exit code=$?"' EXIT

matchEncoderURL() {
  case "$streamerIP" in
    "$TUNER1_IP") encoderURL=$ENCODER1_URL ;;
    "$TUNER2_IP") encoderURL=$ENCODER2_URL ;;
    "$TUNER3_IP") encoderURL=$ENCODER3_URL ;;
    "$TUNER4_IP") encoderURL=$ENCODER4_URL ;;
    "$TUNER5_IP") encoderURL=$ENCODER5_URL ;;
    "$TUNER6_IP") encoderURL=$ENCODER6_URL ;;
    "$TUNER7_IP") encoderURL=$ENCODER7_URL ;;
    "$TUNER8_IP") encoderURL=$ENCODER8_URL ;;
    "$TUNER9_IP") encoderURL=$ENCODER9_URL ;;
  esac
}

adbConnect() {
  adb connect "$streamerIP" >/dev/null
  local -i tries=0
  while true; do
    if $adbTarget shell input keyevent KEYCODE_WAKEUP >/dev/null 2>&1; then
      log "adb wake ok ($streamerIP)"
      return
    fi
    if (( tries++ >= 3 )); then
      touch "$streamerNoPort/adbCommunicationFail"
      log "adb wake FAILED after $tries tries"
      exit 2
    fi
  done
}

# Probe encoder for 1s per iteration. If bytes received < threshold,
# signal isn't live yet — sleep 1 and retry. Safety cap at 30 iterations
# so a dead encoder can't hang the tune forever.
waitForEncoder() {
  matchEncoderURL
  [[ -z "$encoderURL" ]] && { log "no encoderURL mapped"; return; }
  local encoderIP="${encoderURL#http://}"
  encoderIP="${encoderIP%%/*}"
  log "waiting for live signal on $encoderIP ($encoderURL)"
  local -i i=0
  while (( i < 30 )); do
    (( i++ ))
    local bytes
    bytes=$(timeout 1 curl -sN "$encoderURL" 2>/dev/null | wc -c)
    if (( bytes > 100000 )); then
      log "signal LIVE on probe $i ($bytes bytes in 1s)"
      return
    fi
    log "probe $i teeny ($bytes bytes) — sleep 1"
    sleep 1
  done
  log "TIMEOUT after $i probes — proceeding"
}

main() {
  log "start streamerIP=$streamerIP"
  adbConnect
  waitForEncoder
  log "done"
}

main
