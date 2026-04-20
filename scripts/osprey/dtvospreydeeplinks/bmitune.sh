#!/bin/bash
# bmitune.sh for osprey/dtvospreydeeplinks
# Poll the encoder until it's producing bytes — only THEN fire the
# deep link. Stops the deep link from firing onto a silent encoder
# and gives the downstream handoff something to chew on right away.

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

fireDeepLink() {
  log "firing deep link: $channelName/$channelID → $streamerIP"
  $adbTarget shell "am start -a android.intent.action.VIEW -d 'https://deeplink.directvnow.com/tune/live/channel/$channelName/$channelID' com.att.tv.openvideo"
}

# while → probe → if bytes → fire deep link → break
# Threshold is low (>10 KB/s) — we just need proof the encoder is
# producing anything (HDMI locked) before we ask Fire TV to change
# channel. Cap at 30 probes so we don't hang forever.
probeAndTune() {
  [[ -z "$encoderURL" ]] && { log "no encoderURL mapped — firing anyway"; fireDeepLink; return; }
  log "polling encoder $encoderURL for bytes before deeplink"
  local -i i=0
  while (( i < 30 )); do
    (( i++ ))
    local bytes
    bytes=$(timeout 1 curl -sN "$encoderURL" 2>/dev/null | wc -c)
    if (( bytes > 10000 )); then
      log "encoder has data on probe $i ($bytes bytes) — firing deeplink"
      fireDeepLink
      return
    fi
    log "probe $i teeny ($bytes bytes) — sleep 1"
    sleep 1
  done
  log "TIMEOUT after $i probes — firing deeplink anyway"
  fireDeepLink
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
probeAndTune
startKeepAlive
log "done"
