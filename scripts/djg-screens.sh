#!/usr/bin/env nix-shell
#! nix-shell -i bash -p bash xorg-server xwininfo xdotool xwd imagemagick dbus gawk coreutils gnugrep
# Headless, SILENT proof screenshots for djg (docs/gui/screens/).
#
# Everything this starts is private and torn down on exit:
# - a probe daemon with audio = null, MPRIS off, the offline store pointed at a
#   throwaway dir capped at ~120 MB, on port 6690 (never 6600), configured from a COPY of the runtime
#   config with those overrides (the copy is deleted; nothing is printed from it);
# - an Xvfb on :93 (refuses :0 and any Wayland socket);
# - djg under `env -i` with a private HOME / XDG_RUNTIME_DIR and its own D-Bus.
#
# Usage: scripts/djg-screens.sh <djg-binary> <hypodj-daemon-binary>
set -euo pipefail

DJG=${1:?djg binary}
DAEMON=${2:?hypodj daemon binary}
DISP=:93
PORT=6690
case "$DISP" in :0 | :0.*) echo "refusing the user's display" >&2; exit 1 ;; esac

REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT=$REPO/docs/gui/screens
mkdir -p "$OUT"
TMP=$(mktemp -d /tmp/djg-proof.XXXXXX)
chmod 700 "$TMP"
PIDS=()
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  # Logs only (never the config copy): kept when asked, for debugging a shot.
  if [ -n "${DJG_LOGDIR:-}" ]; then mkdir -p "$DJG_LOGDIR"; cp "$TMP"/*.log "$DJG_LOGDIR"/ 2>/dev/null || true; fi
  rm -rf "$TMP"
}
trap cleanup EXIT

# Probe daemon config: the real [server] block (credentials stay in files), every
# other section replaced. Lines are copied by section, never echoed.
SRC=/run/user/1000/hypodj/config.toml
awk '/^\[/{keep = ($0 == "[server]")} keep' "$SRC" >"$TMP/config.toml"
cat >>"$TMP/config.toml" <<EOF

[mpd]
bind = "127.0.0.1:$PORT"

[mpris]
enable = false

[store]
enable = true
dir = "$TMP/store"
# A few songs at most: enough for real "on disk" labels, bounded traffic.
max_bytes = 120000000
EOF
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

# One MPD exchange over bash's /dev/tcp (no nc flavour differences): send the
# command, then read until the frame's OK / ACK.
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
# A queue to look at: the first album, stopped on track 1, volume set (silent).
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
    GDK_BACKEND=x11 GSK_RENDERER=cairo NO_AT_BRIDGE=1 GTK_A11Y=none \
    XDG_DATA_DIRS="${XDG_DATA_DIRS:-}" \
    HYPODJ_PORT=$PORT DJG_SCREENSHOT="$OUT/$name.png" DJG_PAGE="$page" "$@" \
    dbus-run-session -- "$DJG" >"$TMP/djg-$name.log" 2>&1 &
  local pid=$!
  sleep 3
  DISPLAY=$DISP xwininfo -root -tree | grep -i hypodj | head -1 || echo "no djg window on $DISP" >&2
  # KEYS: xdotool steps typed into the djg window (on the PRIVATE display only).
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
  # ROOT: also capture the app's secondary toplevels (the shortcuts window),
  # which are not in the main window's own render.
  if [ -n "${ROOT:-}" ]; then
    sleep 1
    # No window manager stacks new toplevels, so each visible secondary window
    # (the shortcuts window) is captured by id as well.
    local w
    for w in $(DISPLAY=$DISP xdotool search --onlyvisible --name '^djg$' || true); do
      DISPLAY=$DISP xwd -id "$w" -silent | magick xwd:- "$OUT/$name-window.png" || true
    done
  fi
  wait "$pid" || true
  grep '^djg-focus' "$TMP/djg-$name.log" || true
  echo "$OUT/$name.png"
}

shot playing playing
shot playing-narrow playing DJG_WIDTH=360
shot albums albums DJG_SETTLE=20
shot album album DJG_SETTLE=25
shot search search DJG_SEARCH=love
shot ask-question playing "DJG_ASK=what is downloaded" DJG_SETTLE=8
shot ask-action playing "DJG_ASK=play something calmer" DJG_SETTLE=15
KEYS="type lov" shot type-to-search playing DJG_SETTLE=7
KEYS="type lov;key Escape" shot type-to-search-escape playing DJG_SETTLE=7
shot about playing DJG_ACTION=app.about DJG_SETTLE=5
ROOT=1 KEYS="key ctrl+question" shot shortcuts playing DJG_SETTLE=6
shot playing-light playing ADW_DEBUG_COLOR_SCHEME=prefer-light
shot playing-dark playing ADW_DEBUG_COLOR_SCHEME=prefer-dark

# Offline: a second daemon with a dead server URL (made-up credentials, nothing
# secret copied), a COPY of the first daemon's mirror and state, so the restored
# queue mixes held and unheld songs. Port 6697, never 6600.
PORT2=6697
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
EOC
env -i PATH="$PATH" HOME="$TMP/home" HYPODJ_AUDIO=null STATE_DIRECTORY="$TMP/state2" \
  RUST_LOG=warn "$DAEMON" "$TMP/config2.toml" >"$TMP/daemon2.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 50); do
  (exec 3<>/dev/tcp/127.0.0.1/$PORT2) 2>/dev/null && break
  sleep 0.2
done
PORT=$PORT2
# The restored queue is what the first daemon had; make sure it is there.
[ -n "$(mpd 'playlistinfo')" ] || { [ -n "$FIRST" ] && mpc "add \"$FIRST\""; }
echo "offline queue X-Offline: $(mpd 'playlistinfo' | grep -c 'X-Offline: 1') held, $(mpd 'playlistinfo' | grep -c 'X-Offline: 0') not"
# Tab through the window, then arrow through Up Next: the focus log names every
# row focus landed on, and no insensitive row may appear in it.
KEYS="key Tab;key Tab;key Tab;key Tab;key Tab;key Tab;key Tab;key Down;key Down;key Down;key Down;key Down;key Down" \
  shot unreachable-playing playing DJG_SETTLE=14 DJG_FOCUS_LOG=1
shot unreachable-albums albums DJG_SETTLE=12
shot unreachable-album album DJG_SETTLE=16
shot unreachable-narrow playing DJG_WIDTH=360 DJG_SETTLE=8
echo "daemon log tail:"; tail -n 5 "$TMP/daemon.log" | sed 's/password=[^ &]*/password=REDACTED/g'
