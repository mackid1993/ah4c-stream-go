#!/bin/bash
# bmitune.sh for osprey/dtvospreydeeplinks
# Fire-then-verify: fire the deep link, then probe the encoder to
# confirm real video is flowing. Retry up to 3 times on no-lock.
#
# The old probe-then-fire ordering chicken-and-egg'd: the encoder
# never produces >500 KB/s until the deep link IS fired, so a probe
# waiting for bytes would wait forever.
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

fireDeepLink() {
  log "firing deeplink: $channelName/$channelID → $streamerIP"
  $adbTarget shell "am start -a android.intent.action.VIEW -d 'https://deeplink.directvnow.com/tune/live/channel/$channelName/$channelID' com.att.tv.openvideo"
}

# Fire deeplink, then probe encoder for up to 12s to verify lock.
# Retry up to 3 times if we never see real video flowing.
tuneAndVerify() {
  [[ -z "$encoderURL" ]] && { log "no encoderURL mapped — firing once, no verify"; fireDeepLink; return; }
  local attempt
  for attempt in 1 2 3; do
    log "attempt $attempt: firing deeplink"
    fireDeepLink
    local probe
    for probe in 1 2 3 4 5 6 7 8 9 10 11 12; do
      sleep 1
      local bytes
      bytes=$(timeout 1 curl -sN "$encoderURL" 2>/dev/null | wc -c)
      if (( bytes > 500000 )); then
        log "LOCKED on attempt $attempt probe $probe ($bytes bytes)"
        return 0
      fi
      log "attempt $attempt probe $probe: $bytes bytes"
    done
    log "attempt $attempt: no lock after 12s — re-firing"
  done
  log "gave up after 3 attempts"
  return 1
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
tuneAndVerify
startKeepAlive
log "done"
