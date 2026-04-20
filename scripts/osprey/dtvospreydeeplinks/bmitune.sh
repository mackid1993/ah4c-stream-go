#!/bin/bash
# bmitune.sh for osprey/dtvospreydeeplinks
# Runs async in a goroutine (main.go:216), so it can't gate the
# handoff. The deep link fire is handled by the nohup'd prober spawned
# from prebmitune — it fires the deeplink the instant the encoder is
# producing real video, and survives stopbmitune. All bmitune does
# here is spawn the keep-alive.
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
startKeepAlive
log "done"
