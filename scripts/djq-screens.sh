#!/usr/bin/env nix-shell
#! nix-shell -i bash -p bash xorg-server xwininfo xdotool dbus gawk coreutils gnugrep
# Headless, SILENT proof screenshots for djq (docs/gui/screens/qt/).
#
# Everything this starts is private and torn down on exit:
# - a probe daemon with audio = null, MPRIS off, the offline store pointed at a
#   throwaway dir capped at ~120 MB, on port 6691 (never 6600), configured from a
#   COPY of the runtime config with those overrides (the copy is deleted; nothing is
#   printed from it);
# - an Xvfb on :94 (refuses :0; Qt is forced to xcb, never Wayland);
# - djq under `env -i` with a private HOME / XDG_RUNTIME_DIR and its own D-Bus.
#
# Usage: scripts/djq-screens.sh <djq-binary> <hypodj-daemon-binary>
# A devshell-built djq needs QML_IMPORT_PATH / QT_PLUGIN_PATH, which are passed
# through from the calling environment when set (the nix package is wrapped).
set -euo pipefail

DJQ=${1:?djq binary}
DAEMON=${2:?hypodj daemon binary}
DISP=:94
PORT=6691
case "$DISP" in :0 | :0.*) echo "refusing the user's display" >&2; exit 1 ;; esac

REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT=$REPO/docs/gui/screens/qt
mkdir -p "$OUT"
TMP=$(mktemp -d /tmp/djq-proof.XXXXXX)
chmod 700 "$TMP"
PIDS=()
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  # Logs only (never the config copy): kept when asked, for debugging a shot.
  if [ -n "${DJQ_LOGDIR:-}" ]; then mkdir -p "$DJQ_LOGDIR"; cp "$TMP"/*.log "$DJQ_LOGDIR"/ 2>/dev/null || true; fi
  rm -rf "$TMP"
}
trap cleanup EXIT
# A signal must still reach the EXIT trap, or the daemons outlive the script.
trap "exit 1" INT TERM

# Probe daemon config: the real [server] block (credentials stay in files), every
# other section replaced. Lines are copied by section, never echoed.
SRC=/run/user/1000/hypodj/config.toml
awk '/^\[/{keep = ($0 == "[server]")} keep' "$SRC" >"$TMP/config.toml"
cat >>"$TMP/config.toml" <<EOC

[mpd]
bind = "127.0.0.1:$PORT"

[mpris]
enable = false

[store]
enable = true
dir = "$TMP/store"
# A few songs at most: enough for real "on disk" labels, bounded traffic.
max_bytes = 120000000
EOC
mkdir -p "$TMP/store" "$TMP/state" "$TMP/home" "$TMP/run"
chmod 700 "$TMP/run"
env -i PATH="$PATH" HOME="$TMP/home" HYPODJ_AUDIO=null STATE_DIRECTORY="$TMP/state" \
  RUST_LOG=warn "$DAEMON" "$TMP/config.toml" >"$TMP/daemon.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 50); do
  (exec 3<>/dev/tcp/127.0.0.1/$PORT) 2>/dev/null && break
  sleep 0.2
done

Xvfb "$DISP" -screen 0 1280x900x24 -nolisten tcp -noreset >"$TMP/xvfb.log" 2>&1 &
PIDS+=($!)
sleep 1

# One MPD exchange over bash's /dev/tcp: send the command, read to OK / ACK.
mpd() {
  exec 3<>/dev/tcp/127.0.0.1/$PORT || return 0
  local line
  read -r -t 5 line <&3 || true
  printf '%s\n' "$1" >&3
  while read -r -t 60 line <&3; do
    [ "$line" = OK ] && break
    case "$line" in ACK*) break ;; esac
    printf '%s\n' "$line"
  done
  exec 3<&-
}
mpc() { mpd "$1" >/dev/null || true; }
# A queue to look at: the first album, paused on track 1, volume set (silent).
mpc 'clear'
FIRST=$(mpd 'lsinfo albums/all' | awk -F': ' '/^directory: album\//&&!f{print $2; f=1}') || true
[ -n "$FIRST" ] && mpc "add \"$FIRST\""
mpc 'play 0'
sleep 3
mpc 'pause 1'
mpc 'setvol 60'

shot() { # name page [extra env...]
  local name=$1 page=$2
  # ONLY="a b": re-take just those shots.
  if [ -n "${ONLY:-}" ] && [[ " $ONLY " != *" $name "* ]]; then return 0; fi
  shift 2
  env -i PATH="$PATH" HOME="$TMP/home" XDG_RUNTIME_DIR="$TMP/run" DISPLAY="$DISP" \
    QT_QPA_PLATFORM=xcb QT_QUICK_BACKEND=software QT_FORCE_STDERR_LOGGING=1 \
    QT_ACCESSIBILITY=0 LANG=C.UTF-8 \
    ${QML_IMPORT_PATH:+QML_IMPORT_PATH="$QML_IMPORT_PATH"} \
    ${QT_PLUGIN_PATH:+QT_PLUGIN_PATH="$QT_PLUGIN_PATH"} \
    XDG_DATA_DIRS="${XDG_DATA_DIRS:-}" \
    HYPODJ_PORT=$PORT DJQ_SCREENSHOT="$OUT/$name.png" DJQ_PAGE="$page" "$@" \
    dbus-run-session -- "$DJQ" >"$TMP/djq-$name.log" 2>&1 &
  local pid=$!
  sleep 3
  DISPLAY=$DISP xwininfo -root -tree | grep -i hypodj | head -1 || echo "no djq window on $DISP" >&2
  # KEYS: xdotool steps typed into the djq window (on the PRIVATE display only).
  if [ -n "${KEYS:-}" ]; then
    local wid
    wid=$(DISPLAY=$DISP xdotool search --name '^HypoDJ$' | head -1)
    DISPLAY=$DISP xdotool windowfocus --sync "$wid"
    local step
    IFS=';' read -ra steps <<<"$KEYS"
    for step in "${steps[@]}"; do
      # shellcheck disable=SC2086
      DISPLAY=$DISP xdotool $step
      sleep 0.4
    done
  fi
  wait "$pid" || true
  grep -a 'djq-focus' "$TMP/djq-$name.log" || true
  echo "$OUT/$name.png"
}

shot playing playing
shot playing-narrow playing DJQ_WIDTH=360
shot albums albums DJQ_SETTLE=20
shot album album DJQ_SETTLE=25
shot search search DJQ_SEARCH=love DJQ_SETTLE=10
KEYS="type lov" shot type-to-search playing DJQ_SETTLE=7
# Online every row is a Tab stop (the offline run below must skip the greyed ones).
KEYS="key Tab;key Tab;key Tab;key Tab;key Tab;key Tab;key Tab;key Tab;key Tab;key Tab" \
  shot queue-keys playing DJQ_SETTLE=12 DJQ_FOCUS_LOG=1
KEYS="type lov;key Escape" shot type-to-search-escape playing DJQ_SETTLE=7
shot ask-question playing "DJQ_ASK=what is downloaded" DJQ_SETTLE=8
shot ask-action playing "DJQ_ASK=play something calmer" DJQ_SETTLE=15
# The chrome is whatever style the host sets: the same window under a dark host
# style, where the instrument follows the room to its dark ground.
shot playing-dark playing QT_QUICK_CONTROLS_STYLE=Material QT_QUICK_CONTROLS_MATERIAL_THEME=Dark

# A second world: the music server unreachable (a dead loopback port, made-up
# credentials - nothing secret is copied), the store a COPY of the first daemon's
# mirror so the on-disk fallback has rows, and a [[stream]] with a very long name
# queued so the long-title layout is on screen. Port 6695 (the first daemon also
# holds 6692, its side channel at port + 1), never 6600.
PORT2=6695
# The offline world needs something held: wait (bounded) for the first mirror to
# finish at least one song before copying it.
for _ in $(seq 120); do
  [ "$(mpd 'playlistinfo' | grep -c '^X-Offline: 1')" -ge 2 ] && break
  sleep 1
done
sleep 2 # let the last finished song's sidecar land before the copy
mkdir -p "$TMP/store2" "$TMP/state2"
cp -a "$TMP/store/." "$TMP/store2/" 2>/dev/null || true
cp -a "$TMP/state/." "$TMP/state2/" 2>/dev/null || true
SLUG=an-extraordinarily-long-stream-path-that-a-queue-row-has-to-shorten-rather-than-overflow-the-window.mp3
LONG="The Extraordinarily Long Name Of A Late Night Community Radio Stream That Never Ends - Live From The Harbour Wall Until Dawn"
cat >"$TMP/config2.toml" <<EOC
[server]
url = "http://127.0.0.1:9"
username = "nobody"
password = "nothing"

[mpd]
bind = "127.0.0.1:$PORT2"

[mpris]
enable = false

[store]
enable = true
dir = "$TMP/store2"
max_bytes = 120000000

[[stream]]
url = "http://127.0.0.1:9/$SLUG"
name = "$LONG"
EOC
env -i PATH="$PATH" HOME="$TMP/home" HYPODJ_AUDIO=null STATE_DIRECTORY="$TMP/state2" \
  RUST_LOG=warn "$DAEMON" "$TMP/config2.toml" >"$TMP/daemon2.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 50); do
  (exec 3<>/dev/tcp/127.0.0.1/$PORT2) 2>/dev/null && break
  sleep 0.2
done
PORT=$PORT2
# First the restored queue (held and unheld songs mixed): Tab through the window;
# the focus log names every row focus landed on, and no disabled row may appear.
echo "offline queue X-Offline: $(mpd 'playlistinfo' | grep -c 'X-Offline: 1') held, $(mpd 'playlistinfo' | grep -c 'X-Offline: 0') not"
KEYS="key Tab;key Tab;key Tab;key Tab;key Tab;key Tab;key Tab;key Tab;key Tab;key Tab;key Tab;key Tab" \
  shot unreachable-queue playing DJQ_SETTLE=14 DJQ_FOCUS_LOG=1
shot unreachable-album album DJQ_SETTLE=16
mpc 'clear'
mpc "add \"http://127.0.0.1:9/$SLUG\""
mpc 'play 0'
shot unreachable-playing playing DJQ_SETTLE=8
shot unreachable-albums albums DJQ_SETTLE=12
shot unreachable-narrow playing DJQ_WIDTH=360 DJQ_SETTLE=8

echo "daemon log tail:"; tail -n 5 "$TMP/daemon.log" | sed 's/password=[^ &]*/password=REDACTED/g'
