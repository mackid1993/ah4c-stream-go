#!/bin/bash
# bmitune.sh for osprey/dtvospreydeeplinks
# Fires the DirectTV Now deep link, then waits for the HDMI encoder to
# actually be streaming a live signal before returning. Keep-alive spawned
# at the end as before.

channelID=$(echo "$1" | awk -F~ '{print $2}')
channelName=$(echo "$1" | awk -F~ '{print $1}')
streamerIP="$2"
streamerNoPort="${streamerIP%%:*}"
adbTarget="adb -s $streamerIP"
[[ -z "$SPEED_MODE" ]] && speedMode="true" || speedMode="$SPEED_MODE"

mkdir -p "$streamerNoPort"
echo $$ > "$streamerNoPort/bmitune_pid"

log() { printf '[bmitune %s] %s\n' "$(date '+%H:%M:%S')" "$*"; }

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

fireDeepLink() {
  log "firing deep link: $channelName/$channelID → $streamerIP"
  $adbTarget shell "am start -a android.intent.action.VIEW -d 'https://deeplink.directvnow.com/tune/live/channel/$channelName/$channelID' com.att.tv.openvideo"
}

# Poll the encoder with 1s curl samples after the deep link fires.
# If bytes are teeny (<100 KB in 1s), signal isn't live — sleep 1, retry.
# Cap at 30 probes (~60s) so a dead encoder doesn't hang bmitune forever.
waitForEncoder() {
  [[ -z "$encoderURL" ]] && { log "no encoderURL mapped"; return; }
  local encoderIP="${encoderURL#http://}"
  encoderIP="${encoderIP%%/*}"
  log "waiting for live signal on $encoderIP"
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
fireDeepLink
waitForEncoder
startKeepAlive
log "done"
