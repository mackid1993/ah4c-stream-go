#!/bin/bash
# prebmitune.sh for osprey/dtvospreydeeplinks
# Waits for the HDMI encoder to actually be serving a live stream before
# letting AH4C's tune() return. Prevents the cold-start trickle/FIN cycle.

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
      log "adb wake FAILED after $tries tries — giving up"
      exit 2
    fi
    log "adb wake retry $tries"
  done
}

# Poll the encoder with 2s curl samples. If <500 KB arrives, encoder is
# cold/trickling — wait and retry. When ≥500 KB arrives, encoder is
# serving stable HD-rate data and we can let tune() proceed.
waitForEncoder() {
  matchEncoderURL
  if [[ -z "$encoderURL" ]]; then
    log "no encoderURL mapped for $streamerIP — skipping check"
    return
  fi
  log "waiting for encoder $encoderURL"
  local -i waited=0 probe=0
  while (( waited < 30 )); do
    (( probe++ ))
    local bytes
    bytes=$(timeout 2 curl -sN "$encoderURL" 2>/dev/null | head -c 1000000 | wc -c)
    if (( bytes >= 500000 )); then
      log "encoder READY on probe $probe ($bytes bytes in 2s)"
      return
    fi
    log "probe $probe cold ($bytes bytes in 2s), waiting 1s"
    sleep 1
    (( waited += 3 ))
  done
  log "encoder readiness TIMEOUT after ${waited}s — proceeding anyway"
}

main() {
  log "start streamerIP=$streamerIP"
  adbConnect
  waitForEncoder
  log "done"
}

main
