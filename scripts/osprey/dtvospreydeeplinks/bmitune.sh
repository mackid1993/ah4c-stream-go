#!/bin/bash
# bmitune.sh for osprey/dtvospreydeeplinks
# Spawns a nohup prober.sh (like keep_watching) that polls the encoder
# until real video is flowing, then fires the deep link. Because it's
# nohup'd, it survives stopbmitune — bmitune can be killed off while
# the prober keeps going, and the deep link still lands.
#
# AH4C passes:  $1 = channel (name~id)   $2 = tunerIP

channelID=$(echo "$1" | awk -F~ '{print $2}')
channelName=$(echo "$1" | awk -F~ '{print $1}')
streamerIP="$2"
streamerNoPort="${streamerIP%%:*}"
adbTarget="adb -s $streamerIP"

mkdir -p "$streamerNoPort"
echo $$ > "$streamerNoPort/bmitune_pid"

log() { printf '[bmitune %s] %s\n' "$(date '+%H:%M:%S')" "$*" > /proc/1/fd/1; }

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
    *) exit 1 ;;
  esac
}

# Write a prober script that probes the encoder until real video
# (>500 KB/s) is flowing, then fires the deeplink. nohup it so it
# survives stopbmitune. Same pattern as keep_watching.
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

startKeepAlive() {
  cat > "./$streamerNoPort/keep_watching.sh" <<EOF
#!/bin/bash
echo "[\$(date)] Keep-alive started for $streamerIP (interval: \$KEEP_WATCHING)" > /proc/1/fd/1
while true; do
  sleep \$KEEP_WATCHING
  echo "[\$(date)] Keep-alive sent to $streamerIP" > /proc/1/fd/1
  $adbTarget shell input keyevent KEYCODE_MEDIA_PLAY
done
EOF
  chmod +x "./$streamerNoPort/keep_watching.sh"
  [[ -n "$KEEP_WATCHING" ]] && nohup "./$streamerNoPort/keep_watching.sh" &
}

log "start channel=$channelName/$channelID streamerIP=$streamerIP"
matchEncoderURL
spawnProber
startKeepAlive
log "done"
