#!/bin/bash
# prebmitune.sh for osprey/dtvospreydeeplinks
# Wakes the streamer, then spawns a nohup prober (like keep_watching)
# that probes the encoder until real bytes are flowing and fires the
# deep link at that moment. Prebmitune returns immediately; the
# prober survives stopbmitune so the deep link still lands on DVR's
# retry even if the first handoff attempt closed early.
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

spawnProber() {
  [[ -z "$encoderURL" ]] && { log "no encoderURL mapped — skipping prober"; return; }
  cat > "./$streamerNoPort/prober.sh" <<EOF
#!/bin/bash
lg() { echo "[prober \$(date '+%H:%M:%S')] \$*" > /proc/1/fd/1; }
lg "start probing $encoderURL for live video (>500 KB/s)"
i=0
while (( i < 60 )); do
  (( i++ ))
  bytes=\$(timeout 1 curl -sN "$encoderURL" 2>/dev/null | wc -c)
  if (( bytes > 500000 )); then
    lg "encoder LIVE on probe \$i (\$bytes bytes) — firing deeplink"
    $adbTarget shell "am start -a android.intent.action.VIEW -d 'https://deeplink.directvnow.com/tune/live/channel/$channelName/$channelID' com.att.tv.openvideo"
    lg "deeplink fired — exiting prober"
    exit 0
  fi
  lg "probe \$i teeny (\$bytes bytes) — sleep 1"
  sleep 1
done
lg "TIMEOUT after \$i probes — giving up"
EOF
  chmod +x "./$streamerNoPort/prober.sh"
  nohup "./$streamerNoPort/prober.sh" > /dev/null 2>&1 &
  echo $! > "./$streamerNoPort/prober_pid"
  log "prober spawned PID $(cat ./$streamerNoPort/prober_pid)"
}

log "start streamerIP=$streamerIP channel=$channelName/$channelID"
adbConnect
matchEncoderURL
spawnProber
log "done"
