#!/bin/bash
# prebmitune.sh for osprey/dtvospreydeeplinks — just wakes the streamer.

streamerIP="$1"
streamerNoPort="${streamerIP%%:*}"
adbTarget="adb -s $streamerIP"

mkdir -p "$streamerNoPort"

log() { printf '[prebmitune %s] %s\n' "$(date '+%H:%M:%S')" "$*"; }

trap 'log "exit code=$?"' EXIT

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

log "start streamerIP=$streamerIP"
adbConnect
log "done"
