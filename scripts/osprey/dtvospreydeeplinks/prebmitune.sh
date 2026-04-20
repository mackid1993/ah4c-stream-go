#!/bin/bash
# prebmitune.sh for osprey/dtvospreydeeplinks
# AH4C calls this SYNCHRONOUSLY before http.Get on the encoder, so it's
# the only place we can actually block the DVR handoff. Wake the Fire
# TV, fire the deep link, then curl-probe the encoder until live bytes
# are flowing. No downstream reader exists yet — the probe is uncontested.
#
# AH4C passes:  $1 = tunerIP   $2 = channel (name~id)

streamerIP="$1"
channelArg="$2"
channelID=$(echo "$channelArg" | awk -F~ '{print $2}')
channelName=$(echo "$channelArg" | awk -F~ '{print $1}')
streamerNoPort="${streamerIP%%:*}"
adbTarget="adb -s $streamerIP"

mkdir -p "$streamerNoPort"

log() { printf '[prebmitune %s] %s\n' "$(date '+%H:%M:%S')" "$*" > /proc/1/fd/1; }

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
    *) ;;
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

fireDeepLink() {
  log "firing deep link: $channelName/$channelID → $streamerIP"
  $adbTarget shell "am start -a android.intent.action.VIEW -d 'https://deeplink.directvnow.com/tune/live/channel/$channelName/$channelID' com.att.tv.openvideo"
}

# 1s curl samples against the encoder until >100 KB/s (~800 Kbps) flows
# — that threshold filters encoder heartbeat/null padding and catches a
# locked HDMI signal. Cap at 30 probes so a dead encoder can't hang forever.
waitForEncoder() {
  [[ -z "$encoderURL" ]] && { log "no encoderURL mapped — skipping probe"; return; }
  log "polling encoder $encoderURL for live bytes"
  local -i i=0
  while (( i < 30 )); do
    (( i++ ))
    local bytes
    bytes=$(timeout 1 curl -sN "$encoderURL" 2>/dev/null | wc -c)
    if (( bytes > 100000 )); then
      log "encoder LIVE on probe $i ($bytes bytes in 1s)"
      return
    fi
    log "probe $i teeny ($bytes bytes) — sleep 1"
    sleep 1
  done
  log "TIMEOUT after $i probes — proceeding anyway"
}

log "start streamerIP=$streamerIP channel=$channelName/$channelID"
adbConnect
matchEncoderURL
fireDeepLink
waitForEncoder
log "done"
