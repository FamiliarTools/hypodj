#!/usr/bin/env nix-shell
#! nix-shell -i python3 -p python3 curl
"""The falsifiable acceptance checks of docs/design/discovery/design.md section 4,
runnable against ISOLATED, SILENT daemons only.

usage: docs/design/discovery/acceptance.py <hypodj-daemon-binary>

Everything it starts is private and torn down on exit:
- probe daemons on ports 6691/6695 (never 6600; each also binds port+1 for its
  side channel), configured from a COPY of the runtime
  config's [server] block only (credentials stay in files; nothing is printed from it),
  with audio = null (HYPODJ_AUDIO=null), MPRIS off, the offline store OFF, and the heard
  ledger + wants file in a throwaway dir - never ~/.local/state/hypodj/heard;
- every process starts from `env -i` with a private HOME.

Live network reads of NTS and Radio Browser happen (a handful of GETs per check). The
"two weeks" half of check 8 is his listening and cannot run here; this proves the
report SEPARATES the origins, which is the half the slice can fail on.

Writes to Navidrome: none. The owned-mark check marks a song that is ALREADY starred,
so the star it sends changes nothing.
"""

import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

DAEMON = sys.argv[1] if len(sys.argv) > 1 else sys.exit(__doc__)
SRC = "/run/user/1000/hypodj/config.toml"
BLACKHOLE = "https://192.0.2.1"  # TEST-NET-1: routes nowhere, so a connect times out

results = []
procs = []
tmp = tempfile.mkdtemp(prefix="hypodj-disc-")
os.chmod(tmp, 0o700)


def check(n, name, ok, detail):
    results.append((n, name, "PASS" if ok else "FAIL", detail))
    print(f"[{'PASS' if ok else 'FAIL'}] {n}. {name}: {detail}", flush=True)


def server_block():
    keep, out = False, []
    with open(SRC) as f:
        for line in f:
            if line.startswith("["):
                keep = line.strip() == "[server]"
            if keep:
                out.append(line)
    return "".join(out)


def start(port, extra=""):
    d = os.path.join(tmp, str(port))
    for sub in ("home", "state", "heard"):
        os.makedirs(os.path.join(d, sub), exist_ok=True)
    cfg = os.path.join(d, "config.toml")
    with open(cfg, "w") as f:
        os.fchmod(f.fileno(), 0o600)
        f.write(server_block())
        f.write(f"""
[mpd]
bind = "127.0.0.1:{port}"

[mpris]
enable = false

[store]
enable = false

[heard]
dir = "{d}/heard"
{extra}
""")
    env = {"PATH": os.environ["PATH"], "HOME": f"{d}/home", "HYPODJ_AUDIO": "null",
           "STATE_DIRECTORY": f"{d}/state", "RUST_LOG": "warn"}
    for q in (port, port + 1):
        try:
            socket.create_connection(("127.0.0.1", q), 0.2).close()
            raise SystemExit(f"port {q} is already taken; refusing to talk to a daemon that is not ours")
        except OSError:
            pass
    log = open(os.path.join(d, "daemon.log"), "w")
    p = subprocess.Popen([DAEMON, cfg], env=env, stdout=log, stderr=log, stdin=subprocess.DEVNULL)
    procs.append(p)
    for _ in range(100):
        if p.poll() is not None:
            raise SystemExit(f"daemon on {port} exited; see its log")
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            return d
        except OSError:
            time.sleep(0.2)
    raise SystemExit(f"daemon on {port} never listened")


def mpd(port, line, timeout=90):
    """One command; returns (pairs, ack or None)."""
    s = socket.create_connection(("127.0.0.1", port), 5)
    s.settimeout(timeout)
    f = s.makefile("rwb")
    f.readline()
    f.write((line + "\n").encode())
    f.flush()
    pairs, ack = [], None
    while True:
        raw = f.readline()
        if not raw:
            break
        l = raw.decode(errors="replace").rstrip("\n")
        if l == "OK":
            break
        if l.startswith("ACK"):
            ack = l
            break
        k, _, v = l.partition(": ")
        pairs.append((k, v))
    s.close()
    return pairs, ack


def get(pairs, k):
    return next((v for kk, v in pairs if kk == k), None)


def rows(pairs):
    out = []
    for k, v in pairs:
        if k == "file":
            out.append({"file": v})
        elif out and k in ("Title", "X-Source", "X-Lane"):
            out[-1][k] = v
    return out


def content_type(url):
    r = subprocess.run(["curl", "-s", "-L", "-m", "6", "-o", "/dev/null", "-r", "0-1023",
                        "-A", "hypodj acceptance", "-w", "%{http_code} %{content_type}", url],
                       capture_output=True, text=True)
    return r.stdout.strip()


def saved_urls(port):
    p, _ = mpd(port, 'lsinfo "Stations"')
    return [v for k, v in p if k == "file"]


def ledger_rows(d):
    out = []
    hd = os.path.join(d, "heard")
    for name in sorted(os.listdir(hd)):
        if name.endswith(".jsonl"):
            with open(os.path.join(hd, name)) as f:
                out += [json.loads(l) for l in f if l.strip()]
    return out


def wants_count(d):
    p = os.path.join(d, "heard", "wants.ndjson")
    return sum(1 for l in open(p) if l.strip()) if os.path.exists(p) else 0


def song_for(port, artist):
    p, _ = mpd(port, f'search artist "{artist}"')
    songs, cur = [], None
    for k, v in p:
        if k == "file":
            cur = {"file": v}
            songs.append(cur)
        elif cur is not None:
            cur[k] = v
    return next((s for s in songs if s.get("Genre")), songs[0] if songs else None)


def wait_state(port, want, secs=20):
    for _ in range(secs * 4):
        st, _ = mpd(port, "status")
        if get(st, "state") == want:
            return True
        time.sleep(0.25)
    return False


try:
    A = 6691
    da = start(A)
    saved_before = saved_urls(A)

    # 1. suggest ambient: >= 5 rows, none saved, every url audio NOW.
    p, ack = mpd(A, "stations suggest ambient")
    rs = rows(p)
    norm = lambda u: u.split("://", 1)[-1].rstrip("/").lower()
    saved_keys = {norm(u) for u in saved_before}
    types = [(r["file"], content_type(r["file"])) for r in rs]
    bad = [t for t in types if not (t[1].split(" ", 1)[0].startswith("2") and
                                    any(x in t[1] for x in ("audio/", "ogg", "mpegurl")))]
    dup = [r["file"] for r in rs if norm(r["file"]) in saved_keys]
    check(1, "suggest ambient", ack is None and len(rs) >= 5 and not bad and not dup,
          f"{len(rs)} rows, {len(dup)} already saved, non-audio at re-fetch: {bad or 'none'}")
    time.sleep(1)

    # 2. lane follows what plays: Tosca -> downtempo/trip-hop; Derrick May -> other rows.
    tosca = song_for(A, "Tosca")
    may = song_for(A, "Derrick May")
    if not tosca or not may:
        check(2, "lane from playing", False, f"library search found tosca={bool(tosca)} may={bool(may)}")
    else:
        mpd(A, "clear")
        mpd(A, f'add "{tosca["file"]}"')
        mpd(A, "play 0")
        wait_state(A, "play")
        pt, ackt = mpd(A, "stations suggest")
        lane_t = get(pt, "lane") or str(ackt)
        rt = {r["file"] for r in rows(pt)}
        time.sleep(1)
        mpd(A, "clear")
        mpd(A, f'add "{may["file"]}"')
        mpd(A, "play 0")
        wait_state(A, "play")
        pm, ackm = mpd(A, "stations suggest")
        lane_m = get(pm, "lane") or str(ackm)
        rm = {r["file"] for r in rows(pm)}
        mpd(A, "stop")
        mpd(A, "clear")
        # "tagged so": every row carries one of the lane's own tags (printed in `lane:`).
        lane_words = lane_t.split("[", 1)[-1].split("]", 1)[0].split(", ")
        tagged = all(any(t in r.get("X-Lane", "") for t in lane_words) for r in rows(pt))
        ok = ("downtempo" in lane_t or "trip-hop" in lane_t) and rt and tagged and rt != rm
        check(2, "lane from playing", ok,
              f"Tosca ({tosca.get('Genre')}) -> {lane_t!r}, {len(rt)} rows all lane-tagged={tagged}; "
              f"Derrick May ({may.get('Genre')}) -> {lane_m!r}, {len(rm)} rows; overlap {len(rt & rm)}")

    # 3. sources down: RB blackholed -> NTS only + a missing line, bounded; both down -> ACK.
    B = 6695
    start(B, f'\n[discovery]\nradio_browser = "{BLACKHOLE}"\n')
    t0 = time.time()
    p, ack = mpd(B, "stations suggest jazz")
    dt = time.time() - t0
    srcs = {r.get("X-Source") for r in rows(p)}
    miss = [v for k, v in p if k == "missing"]
    ok_rb = ack is None and srcs == {"nts"} and any("Radio Browser" in m for m in miss) and dt < 30
    procs[-1].terminate(); procs[-1].wait()
    start(B, f'\n[discovery]\nradio_browser = "{BLACKHOLE}"\nnts_mixtapes = "{BLACKHOLE}/api/v2/mixtapes"\n')
    t1 = time.time()
    p2, ack2 = mpd(B, "stations suggest ambient")
    dt2 = time.time() - t1
    ok_both = ack2 is not None and dt2 < 30
    check(3, "degrade without hanging", ok_rb and ok_both,
          f"RB down: {dt:.1f}s, sources={sorted(s for s in srcs if s)}, missing={miss}; "
          f"both down: {dt2:.1f}s, {ack2!r}")
    procs[-1].terminate(); procs[-1].wait()

    # 4. radio beyond: deck Playing a Stream, tune start origin suggested; leave -> stop row > 0 s.
    p, ack = mpd(A, "radio beyond ambient")
    playing = wait_state(A, "play")
    cur, _ = mpd(A, "currentsong")
    f = get(cur, "file") or ""
    time.sleep(8)
    mpd(A, "stop")
    time.sleep(7)
    lr = ledger_rows(da)
    starts = [r for r in lr if r.get("ev") == "tune" and r.get("kind") == "start" and r.get("origin") == "suggested"]
    stops = [r for r in lr if r.get("ev") == "tune" and r.get("kind") == "stop" and r.get("origin") == "suggested"]
    ok = ack is None and playing and f.startswith("http") and starts and stops and stops[-1].get("dwell_secs", 0) > 0
    check(4, "radio beyond + tune rows", bool(ok),
          f"reply={get(p, 'radio') or ack!r}, playing={playing}, file={f[:60]}, "
          f"suggested start rows={len(starts)}, stop dwell={stops[-1].get('dwell_secs') if stops else None}s")

    # 5. Navidrome untouched.
    saved_after = saved_urls(A)
    check(5, "no Navidrome write", saved_before == saved_after,
          f"saved stations before={len(saved_before)} after={len(saved_after)}")

    # 6. marks: unowned recognized track -> +1 want; owned track -> +0 (already starred).
    w0 = wants_count(da)
    mpd(A, "clear")
    mpd(A, 'add "https://ice4.somafm.com/groovesalad-128-mp3"')
    mpd(A, "play 0")
    wait_state(A, "play")
    # Wait for an ICY title the library does NOT hold, so the mark can never star (write)
    # anything: an owned title is skipped and the next one awaited.
    title = None
    for _ in range(240):
        cs, _ = mpd(A, "currentsong")
        t = get(cs, "Title")
        if t and " - " in t and not t.startswith("http"):
            artist, _, song = t.partition(" - ")
            hits, _ = mpd(A, f'search title "{song.replace(chr(34), "")}"')
            if not any(k == "file" for k, _ in hits):
                title = t
                break
        time.sleep(1)
    pm1, _ = mpd(A, "mark this") if title else ([], None)
    w1 = wants_count(da)
    mpd(A, "stop")
    mpd(A, "clear")
    st, _ = mpd(A, "listplaylistinfo Starred")
    starred = next((v for k, v in st if k == "file"), None)
    pm2 = []
    if starred:
        mpd(A, f'add "{starred}"')
        mpd(A, "play 0")
        wait_state(A, "play")
        pm2, _ = mpd(A, "mark this")
        mpd(A, "stop")
        mpd(A, "clear")
    w2 = wants_count(da)
    owned1 = get(pm1, "mark_starred") == "1"
    ok = title and (w1 - w0 == (0 if owned1 else 1)) and starred and (w2 - w1 == 0) and get(pm2, "mark_starred") == "1"
    check(6, "marks become wants", bool(ok),
          f"unowned mark on {title!r}: {get(pm1, 'mark_result')!r}, wants {w0}->{w1}; "
          f"owned mark: {get(pm2, 'mark_result')!r}, wants {w1}->{w2}")

    # 7 is the build gates (workspace test, nix build) - run by the caller, not here.

    # 8. the report separates origins. One saved tune-in so both arms exist.
    if saved_before:
        mpd(A, f'add "{saved_before[0]}"')
        mpd(A, "play 0")
        wait_state(A, "play")
        time.sleep(8)
        mpd(A, "stop")
        time.sleep(7)
    rp, rack = mpd(A, "heard report")
    lines = [v for k, v in rp if k == "heard"]
    has = lambda o: any(l.startswith(f"{o}: ") for l in lines)
    dec = [l for l in lines if l.startswith("decision:")]
    check(8, "report splits origins", rack is None and has("saved") and has("suggested") and len(dec) == 1,
          " | ".join(l for l in lines if l.split(":")[0] in ("saved", "suggested", "decision", "wants")))
finally:
    for p in procs:
        if p.poll() is None:
            p.terminate()
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                p.kill()
    keep = os.environ.get("DISC_KEEP")
    if keep:
        os.makedirs(keep, exist_ok=True)
        for port in os.listdir(tmp):
            src = os.path.join(tmp, port)
            if os.path.isdir(src):
                shutil.copytree(os.path.join(src, "heard"), os.path.join(keep, port, "heard"), dirs_exist_ok=True)
                shutil.copy(os.path.join(src, "daemon.log"), os.path.join(keep, port, "daemon.log"))
    shutil.rmtree(tmp, ignore_errors=True)

print()
print("| # | check | result | observed |")
print("|---|---|---|---|")
for n, name, r, d in results:
    print(f"| {n} | {name} | {r} | {d.replace('|', '/')} |")
sys.exit(0 if all(r == "PASS" for _, _, r, _ in results) else 1)
