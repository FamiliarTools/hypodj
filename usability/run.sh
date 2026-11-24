#!/usr/bin/env nix-shell
#! nix-shell -i bash -p python3 navidrome ffmpeg-headless nftables iproute2 util-linux
# Usability journey harness entry point. Runs the whole world inside a private
# user + network namespace (`unshare -rn`, rootless): a real Navidrome, a silent
# hypodj daemon (audio null, MPRIS off, throwaway store) and the headless dj-gui
# driver (`dj-gui --pump`). Nothing in here can reach the host's port 6600, its
# display, its session bus or its audio device: the namespace has only its own
# loopback and a dummy link, and every process starts from an empty environment.
#
# usage: usability/run.sh <target-dir> [journey-id ...]
#   <target-dir> holds the built `hypodj` and `dj-gui` binaries
#   (e.g. target/debug after `cargo build -p hypodj-daemon -p dj-gui`).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
exec unshare -rn python3 -I "$here/harness.py" "$@"
