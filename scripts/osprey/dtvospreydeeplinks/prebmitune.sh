#!/bin/bash
# prebmitune.sh for osprey/dtvospreydeeplinks
# Waits for the HDMI encoder to actually be serving a live stream before
# letting AH4C's tune() return. Prevents the cold-start trickle/FIN cycle.
set -x

streamerIP="$1"
streamerNoPort="${streamerIP%%:*}"
adbTarget="adb -s $streamerIP"

mkdir -p "$streamerNoPort"

trap 'echo "prebmitune.sh exiting for $streamerIP with exit code $?"' EXIT

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
  adb connect "$streamerIP"
  local -i tries=0
  while true; do
    $adbTarget shell input keyevent KEYCODE_WAKEUP && break
    (( tries++ >= 3 )) && { touch "$streamerNoPort/adbCommunicationFail"; exit 2; }
  done
}

# Sample 2s of the encoder. If it delivers <500 KB, it's trickling (cold).
# Retry until stable or timeout. The encoder stays hot across brief client
# cycles because the Fire TV is holding the HDMI signal, so our probe
# doesn't destabilize it.
waitForEncoder() {
  matchEncoderURL
  [[ -z "$encoderURL" ]] && return
  local -i waited=0
  while (( waited < 30 )); do
    local bytes
    bytes=$(timeout 2 curl -sN "$encoderURL" 2>/dev/null | head -c 1000000 | wc -c)
    (( bytes >= 500000 )) && { echo "Encoder ready ($bytes bytes in 2s)"; return; }
    echo "Encoder cold ($bytes bytes in 2s), retrying"
    sleep 1
    (( waited += 3 ))
  done
  echo "Encoder readiness timeout; proceeding"
}

main() {
  adbConnect
  waitForEncoder
}

main
