"""hypodj usability journey harness (first slice).

Runs INSIDE a private user + network namespace (see run.sh). Builds a world per
(journey, connectivity state), drives the shipped dj-gui through `dj-gui --pump`,
and judges each run on daemon truth plus the words on screen.

The world:
  * a REAL Navidrome on 10.77.0.1:4533 (a dummy link in the namespace) serving a
    generated fixture library: two albums, one starred (so the store mirrors it);
  * a silent hypodj daemon: empty environment, private HOME / XDG_RUNTIME_DIR /
    state dir, HYPODJ_AUDIO=null, [mpris] off, [recognize] off, the store pointed at
    a throwaway copy of a seed the daemon itself mirrored while online;
  * connectivity states, all real kernel behaviour, no fakes:
      online      the link is up and nothing filters it
      blocked     nft drops every packet to the server: connects HANG, an open
                  stream stalls - the captive-portal / dead-VPN failure
      no_network  the server address is removed: connects fail at once with
                  ENETUNREACH - the laptop-with-wifi-off failure

usage (inside the namespace): harness.py <target-dir> [journey-id ...]
       (anywhere, no world):  harness.py --rejudge docs/usability/runs/<date>
"""

import datetime
import hashlib
import json
import os
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import time
import tomllib
import urllib.parse
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
SERVER_IP = "10.77.0.1"
SERVER_PORT = 4533
SERVER_URL = f"http://{SERVER_IP}:{SERVER_PORT}"
MPD_PORT = 6610  # inside the namespace; the host's 6600 is not reachable from here
USER, PASSWORD = "fixture", "fixture-pass"
STATES = ("online", "blocked", "no_network")

FIXTURE = {
    # album: (artist, starred, [titles])
    "Held Harbour": ("Fixture Band", True, ["Low Tide", "Harbour Lights", "Moorings"]),
    "Stream Summit": ("Fixture Band", False, ["Ascent", "Thin Air", "Ridge Line"]),
}
TRACK_SECS = 120
# Albums with an embedded cover (a generated test card), so a journey can tell a
# drawn cover from the swatch: the cover-truth journey plays this one, then skips
# into an album whose cover cannot be fetched.
COVERED = {"Held Harbour"}
# Filler albums AFTER the named ones in name order (so every journey's `j` counts
# still land where they did), one short track each: enough rows that an 80x24 Albums
# view has to show how many it can fit.
FILLER_SECS = 20
for _n in range(1, 17):
    FIXTURE[f"Tidewater {_n:02d}"] = ("Filler Band", False, [f"Slack Water {_n:02d}"])


def log(*a):
    print("harness:", *a, file=sys.stderr, flush=True)


def sh(*cmd, check=True, **kw):
    return subprocess.run(list(cmd), check=check, **kw)


# ---------------------------------------------------------------- network states

NFT_BLOCK = f"""table inet hjblock {{
  chain out {{ type filter hook output priority 0; ip daddr {SERVER_IP} tcp dport {SERVER_PORT} drop; }}
}}
"""


def net_cmd(state, work):
    """A shell command that moves the namespace to `state` and records it in the
    world file the driver stamps into every truth.json."""
    world = work / "world"
    rules = work / "block.nft"
    rules.write_text(NFT_BLOCK)
    reset = (
        "nft delete table inet hjblock 2>/dev/null; "
        f"ip addr replace {SERVER_IP}/24 dev srv0 2>/dev/null; "
    )
    if state == "online":
        body = reset
    elif state == "blocked":
        body = reset + f"nft -f {rules}; "
    elif state == "no_network":
        body = reset + f"ip addr del {SERVER_IP}/24 dev srv0; "
    else:
        raise ValueError(state)
    return body + f"echo {state} > {world}"


def set_net(state, work):
    sh("sh", "-c", net_cmd(state, work))


# ---------------------------------------------------------------- fixture + server

def make_fixture(music):
    cover = music / "cover-card.png"
    if not cover.exists():
        music.mkdir(parents=True, exist_ok=True)
        sh("ffmpeg", "-nostdin", "-loglevel", "error", "-f", "lavfi", "-i",
           "testsrc2=size=256x256:duration=1", "-frames:v", "1", str(cover))
    for album, (artist, _, titles) in FIXTURE.items():
        d = music / artist / album
        d.mkdir(parents=True, exist_ok=True)
        secs = FILLER_SECS if album.startswith("Tidewater") else TRACK_SECS
        for i, title in enumerate(titles, 1):
            out = d / f"{i:02d} {title}.mp3"
            if out.exists():
                continue
            freq = 220 + 40 * i + (0 if FIXTURE[album][1] else 300)
            art = (["-i", str(cover), "-map", "0:a", "-map", "1:v", "-c:v", "png",
                    "-disposition:v:0", "attached_pic", "-id3v2_version", "3"]
                   if album in COVERED else [])
            sh(
                "ffmpeg", "-nostdin", "-loglevel", "error", "-f", "lavfi",
                "-i", f"sine=frequency={freq}:duration={secs}", *art,
                "-b:a", "48k", "-metadata", f"title={title}", "-metadata", f"artist={artist}",
                "-metadata", f"album_artist={artist}", "-metadata", f"album={album}",
                "-metadata", f"track={i}", "-metadata", "date=2026", str(out),
            )


def api(method, **params):
    salt = secrets.token_hex(6)
    q = dict(u=USER, t=hashlib.md5((PASSWORD + salt).encode()).hexdigest(), s=salt,
             v="1.16.1", c="usability-harness", f="json", **params)
    url = f"{SERVER_URL}/rest/{method}?{urllib.parse.urlencode(q, doseq=True)}"
    with urllib.request.urlopen(url, timeout=10) as r:
        body = json.load(r)["subsonic-response"]
    if body.get("status") != "ok":
        raise RuntimeError(f"{method}: {body}")
    return body


def start_navidrome(work, music):
    """Start the server; on any setup failure it is killed here, never orphaned."""
    try:
        proc = _start_navidrome(work, music)
        return proc
    except BaseException:
        stop(_NAV[0] if _NAV else None)
        raise


_NAV = []


def _start_navidrome(work, music):
    data = work / "navidrome"
    data.mkdir(exist_ok=True)
    env = {
        "PATH": os.environ["PATH"], "HOME": str(work / "home"),
        "ND_MUSICFOLDER": str(music), "ND_DATAFOLDER": str(data),
        "ND_ADDRESS": SERVER_IP, "ND_PORT": str(SERVER_PORT), "ND_LOGLEVEL": "warn",
        "ND_SCANNER_SCHEDULE": "0", "ND_ENABLEINSIGHTSCOLLECTOR": "false",
        "ND_LASTFM_ENABLED": "false", "ND_SPOTIFY_ID": "", "ND_ENABLEEXTERNALSERVICES": "false",
    }
    proc = subprocess.Popen(["navidrome"], env=env, stdout=open(work / "navidrome.log", "w"),
                            stderr=subprocess.STDOUT)
    _NAV[:] = [proc]
    deadline = time.time() + 60
    while time.time() < deadline:
        try:
            req = urllib.request.Request(
                f"{SERVER_URL}/auth/createAdmin", method="POST",
                data=json.dumps({"username": USER, "password": PASSWORD}).encode(),
                headers={"Content-Type": "application/json"})
            urllib.request.urlopen(req, timeout=3).read()
            break
        except urllib.error.HTTPError:
            break  # admin already exists
        except Exception:
            time.sleep(0.5)
    else:
        raise RuntimeError("navidrome did not come up")
    try:
        api("startScan", fullScan="true")
    except Exception as e:
        log("startScan:", e)
    deadline = time.time() + 120
    while time.time() < deadline:
        albums = api("getAlbumList2", type="alphabeticalByName", size=50)["albumList2"].get("album", [])
        if len(albums) >= len(FIXTURE) and not api("getScanStatus")["scanStatus"].get("scanning"):
            break
        time.sleep(1)
    else:
        raise RuntimeError("navidrome scan did not finish")
    for a in albums:
        if FIXTURE.get(a["name"], (None, False))[1]:
            songs = api("getAlbum", id=a["id"])["album"]["song"]
            api("star", id=[s["id"] for s in songs])
    log("navidrome up:", ", ".join(a["name"] for a in albums))
    return proc


# ---------------------------------------------------------------- daemon

def mpd(cmd, timeout=10):
    with socket.create_connection(("127.0.0.1", MPD_PORT), timeout=timeout) as s:
        f = s.makefile("rwb")
        f.readline()
        f.write((cmd + "\n").encode())
        f.flush()
        pairs = []
        for raw in f:
            line = raw.decode().rstrip("\n")
            if line == "OK" or line.startswith("ACK"):
                break
            k, _, v = line.partition(": ")
            pairs.append((k, v))
        return dict(pairs)


def start_daemon(bins, run, store, queue_ahead=None):
    home, xdg, state = run / "home", run / "xdg", run / "state"
    for d in (home, xdg, state):
        d.mkdir(parents=True, exist_ok=True)
    os.chmod(xdg, 0o700)
    cfg = run / "hypodj.toml"
    cfg.write_text(f"""[server]
url = "{SERVER_URL}"
username = "{USER}"
password = "{PASSWORD}"
client_name = "usability-harness"

[mpd]
bind = "127.0.0.1:{MPD_PORT}"

[mpris]
enable = false

[recognize]
auto = false

[restart]
state_dir = "{state}"

[store]
enable = true
dir = "{store}"
""" + (f"queue_ahead = {queue_ahead}\n" if queue_ahead is not None else ""))
    env = {"HOME": str(home), "XDG_RUNTIME_DIR": str(xdg), "HYPODJ_AUDIO": "null",
           "RUST_LOG": "info", "PATH": os.environ["PATH"]}
    proc = subprocess.Popen([str(bins / "hypodj"), str(cfg)], env=env, cwd=run,
                            stdout=open(run / "daemon.log", "w"), stderr=subprocess.STDOUT)
    deadline = time.time() + 90
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"daemon exited {proc.returncode}, see {run / 'daemon.log'}")
        try:
            mpd("ping", timeout=2)
            return proc
        except OSError:
            time.sleep(0.3)
    stop(proc)
    raise RuntimeError("daemon never answered")


def stop(proc):
    if proc and proc.poll() is None:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


def seed_store(bins, work):
    """Let a real daemon mirror the starred album while online; that directory is the
    seed every run copies. Never the user's mirror: it lives under the work dir."""
    seed = work / "seed-store"
    if seed.exists():
        return seed
    run = work / "seed-run"
    set_net("online", work)
    proc = start_daemon(bins, run, seed)
    want = sum(len(t) for _, s, t in FIXTURE.values() if s)
    try:
        # Disk is the truth here: the `store` verb's counters update per reconcile
        # pass (15 min), so they read "pending" after the files have landed.
        deadline = time.time() + 180
        while time.time() < deadline:
            have = len([p for p in seed.glob("*.toml")])
            if have >= want:
                time.sleep(1)
                log("seed store on disk:", have, "songs; store verb says", mpd("store").get("X-Store"))
                break
            time.sleep(1)
        else:
            raise RuntimeError("store never mirrored the starred album")
    finally:
        stop(proc)
    return seed


# ---------------------------------------------------------------- journeys

MPDSEND = """import socket, sys
s = socket.create_connection(("127.0.0.1", %d), timeout=20)
f = s.makefile("rwb"); f.readline()
for cmd in sys.argv[1].split(" ; "):
    f.write((cmd + "\\n").encode()); f.flush()
    for raw in f:
        if raw in (b"OK\\n",) or raw.startswith(b"ACK"):
            sys.stderr.write(cmd + ": " + raw.decode()); break
""" % MPD_PORT


def render_script(spec, state, work):
    out = []
    for line in spec["script"]:
        line = line.replace("{state}", state)
        if line.startswith("net "):
            out.append("exec " + net_cmd(line.split(None, 1)[1].strip(), work))
        elif line.startswith("mpd "):
            # The world before the listener touches the UI (music already on): sent on
            # the daemon's socket, not through dj-gui, and not counted as a step.
            helper = work / "mpdsend.py"
            helper.write_text(MPDSEND)
            arg = line.split(None, 1)[1].replace("'", "'\\''")
            out.append(f"exec python3 -I {helper} '{arg}'")
        else:
            out.append(line)
    return "\n".join(out) + "\n"


def truth_get(truth, path):
    # `playlistinfo[Title=Thin Air].X-Offline` picks one queue row (rows start at
    # `file`) by a field, then a key inside it; plain `status.state` is a flat lookup.
    m = re.fullmatch(r"(\w+)\[([^=\]]+)=([^\]]*)\]\.(.+)", path)
    src, _, key = path.partition(".")
    if m:
        src, fk, fv, key = m.groups()
    block = truth.get(src) or {}
    if not block.get("ok"):
        return None
    pairs = block.get("pairs", [])
    if m:
        rows, row = [], None
        for k, v in pairs:
            if k == "file":
                row = {}
                rows.append(row)
            if row is not None:
                row.setdefault(k, v)
        pick = [r for r in rows if r.get(fk) == fv]
        return pick[0].get(key) if pick else None
    for k, v in pairs:
        if k == key:
            return v
    return None


def eval_truth(expr, truth):
    m = re.fullmatch(r"\s*(\S+(?:\[[^\]]*\]\.\S+)?)\s*(==|!=|~|exists|absent)\s*(.*?)\s*", expr)
    if not m:
        return False, f"bad expr {expr!r}"
    path, op, want = m.groups()
    got = truth_get(truth, path)
    ok = {
        "==": got == want, "!=": got != want, "~": got is not None and want.lower() in got.lower(),
        "exists": got is not None, "absent": got is None,
    }[op]
    return ok, f"{path}={got!r}"


def judge(spec, state, outdir, steps):
    results = []
    last = steps[-1]["n"]

    def frames(n, sizes):
        # Every size the driver rendered (older runs carry two, newer three), or only
        # the ones a check names.
        found = {p.name.split(".")[1]: p for p in sorted(outdir.glob(f"{n:03d}.*x*.txt"))}
        return {sz: p.read_text() for sz, p in found.items() if not sizes or sz in sizes}

    for chk in spec.get("check", []):
        if state not in chk.get("when", STATES):
            continue
        n = last if chk.get("at", "last") == "last" else int(chk["at"])
        fr = frames(n, chk.get("sizes"))
        truth = json.loads((outdir / f"{n:03d}.truth.json").read_text())
        notes, ok = [], True
        conds = [eval_truth(e, truth) for e in chk.get("if_truth", [])]
        if not all(c[0] for c in conds):
            results.append({"check": chk["name"], "step": n, "pass": True, "na": True, "severity": chk.get("severity", ""),
                            "notes": ["n/a: precondition false (" + "; ".join(c[1] for c in conds) + ")"]})
            continue
        if chk.get("transport_matches"):
            # The transport word the screen shows must be the daemon's own state.
            word = {"play": "playing", "pause": "paused", "stop": None}.get(truth_get(truth, "status.state"))
            for sz, text in fr.items():
                shown = [w for w in ("playing", "paused") if f"{w} \u00b7" in text]
                if word and word not in shown:
                    ok = False
                    notes.append(f"{sz}: daemon is {word} but screen shows {shown or 'no transport word'}")
                if not word and shown:
                    ok = False
                    notes.append(f"{sz}: daemon is stopped but screen shows {shown}")
        for sz, text in fr.items():
            low = text.lower()
            req_any = chk.get("screen_require_any", [])
            if req_any and not any(w.lower() in low for w in req_any):
                ok = False
                notes.append(f"{sz}: none of {req_any} on screen")
            for rx in chk.get("screen_require_re", []):
                if not re.search(rx, text, re.I | re.S):
                    ok = False
                    notes.append(f"{sz}: no match for /{rx}/ on screen")
            for w in chk.get("screen_forbid", []):
                if w.lower() in low:
                    ok = False
                    notes.append(f"{sz}: forbidden {w!r} on screen")
            for rx in chk.get("screen_forbid_re", []):
                m = re.search(rx, text, re.I | re.S)
                if m:
                    ok = False
                    notes.append(f"{sz}: forbidden /{rx}/ matched {m.group(0)[:40]!r}")
            if "count_re" in chk:
                # [regex, min]: at least `min` LINES of the frame match.
                rx, want = chk["count_re"]
                got = sum(1 for line in text.splitlines() if re.search(rx, line, re.I))
                if got < want:
                    ok = False
                    notes.append(f"{sz}: {got} lines match /{rx}/, want >= {want}")
        for expr in chk.get("truth", []):
            good, why = eval_truth(expr, truth)
            ok &= good
            if not good:
                notes.append(f"truth {expr!r} failed ({why})")
        results.append({"check": chk["name"], "step": n, "pass": ok, "notes": notes,
                        "severity": chk.get("severity", "")})
    # A `say` is one sentence on the `:` line: one input, as a listener counts it.
    scripted = sum(1 for s in steps if s["step"].startswith(("key", "type", "say")))
    results.append({"check": f"steps {scripted} within budget {spec['step_budget']}",
                    "step": last, "pass": scripted <= spec["step_budget"], "notes": [], "severity": "S2"})
    hung = [s["n"] for s in steps if not s["settled"]]
    results.append({"check": "every step settled (sign of life within 12s)", "step": last,
                    "pass": not hung, "notes": [f"unsettled steps {hung}"] if hung else [],
                    "severity": "S2"})
    return results


def run_journey(bins, spec, state, work, seed, outroot):
    tag = f"{spec['id']}/{state}"
    run = work / "runs" / spec["id"] / state
    if run.exists():
        shutil.rmtree(run)
    run.mkdir(parents=True)
    store = run / "store"
    shutil.copytree(seed, store)
    outdir = outroot / spec["id"] / state
    if outdir.exists():
        shutil.rmtree(outdir)
    outdir.mkdir(parents=True)
    start = spec.get("start", "{state}").replace("{state}", state)
    set_net(start, work)
    daemon = start_daemon(bins, run, store, spec.get("store_queue_ahead"))
    try:
        script = outdir / "script.pump"
        script.write_text(render_script(spec, state, work))
        env = {"HOME": str(run / "home"), "XDG_RUNTIME_DIR": str(run / "xdg"),
               "HYPODJ_HOST": "127.0.0.1", "HYPODJ_PORT": str(MPD_PORT),
               "HYPODJ_NL_TRANSLATOR": "rules", "PATH": os.environ["PATH"], "TERM": "dumb"}
        log(f"{tag}: driving dj-gui")
        sh(str(bins / "dj-gui"), "--pump", str(script), str(outdir), str(work / "world"),
           env=env, stdin=subprocess.DEVNULL, timeout=900)
    finally:
        stop(daemon)
        set_net("online", work)
    shutil.copy(run / "daemon.log", outdir / "daemon.log")
    steps = [json.loads(l) for l in (outdir / "steps.jsonl").read_text().splitlines()]
    return judge(spec, state, outdir, steps)


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    # HYPODJ_RUN_NAME names the run folder (e.g. 2026-11-25-w18), so a new run never
    # overwrites a dated run already committed for the same day.
    date = os.environ.get("HYPODJ_RUN_NAME") or datetime.date.today().isoformat()
    outroot = REPO / "docs" / "usability" / "runs" / date
    if sys.argv[1] == "--rejudge":
        # Re-apply the current specs' checks to recorded frames + truth; no world.
        outroot = Path(sys.argv[2]).resolve()
        table = []
        for path in sorted((HERE / "journeys").glob("*.toml")):
            spec = tomllib.loads(path.read_text())
            for state in spec.get("states", STATES):
                d = outroot / spec["id"] / state
                if (d / "steps.jsonl").exists():
                    steps = [json.loads(l) for l in (d / "steps.jsonl").read_text().splitlines()]
                    table.append({"journey": spec["id"], "goal": spec["goal"], "state": state,
                                  "results": judge(spec, state, d, steps)})
        (outroot / "results.json").unlink(missing_ok=True)
        write_report(outroot, outroot.name, table)
        return
    bins = Path(sys.argv[1]).resolve()
    only = set(sys.argv[2:])
    work = Path(f"/tmp/hypodj-usability-{os.getpid()}")
    work.mkdir()
    (work / "home").mkdir()
    sh("ip", "link", "set", "lo", "up")
    sh("ip", "link", "add", "srv0", "type", "dummy")
    sh("ip", "addr", "add", f"{SERVER_IP}/24", "dev", "srv0")
    sh("ip", "link", "set", "srv0", "up")
    (work / "world").write_text("online\n")
    music = work / "music"
    make_fixture(music)
    nav = None
    table = []
    try:
        nav = start_navidrome(work, music)
        seed = seed_store(bins, work)
        for path in sorted((HERE / "journeys").glob("*.toml")):
            spec = tomllib.loads(path.read_text())
            if only and spec["id"] not in only:
                continue
            for state in spec.get("states", STATES):
                try:
                    res = run_journey(bins, spec, state, work, seed, outroot)
                except Exception as e:
                    res = [{"check": "run completed", "step": -1, "pass": False,
                            "notes": [repr(e)], "severity": "harness"}]
                for r in res:
                    log(f"{spec['id']}/{state}: {'PASS' if r['pass'] else 'FAIL'} {r['check']} {r['notes']}")
                table.append({"journey": spec["id"], "goal": spec["goal"], "state": state, "results": res})
    finally:
        stop(nav)
        shutil.rmtree(work, ignore_errors=True)
    write_report(outroot, date, table)


def write_report(outroot, date, table):
    outroot.mkdir(parents=True, exist_ok=True)
    prev = outroot / "results.json"
    if prev.exists():
        # A partial rerun (journey ids given) replaces only its own rows.
        ran = {r["journey"] for r in table}
        table = [r for r in json.loads(prev.read_text()) if r["journey"] not in ran] + table
    (outroot / "results.json").write_text(json.dumps(table, indent=1) + "\n")
    lines = [f"# usability journeys - {date}", "",
             "World: real Navidrome + silent hypodj in a rootless user+net namespace (see usability/).", "",
             "| journey | state | check | result | notes |", "|---|---|---|---|---|"]
    for row in table:
        for r in row["results"]:
            notes = "; ".join(r["notes"]).replace("|", "/")
            lines.append(f"| {row['journey']} | {row['state']} | {r['check']} | "
                         f"{'n/a' if r.get('na') else 'PASS' if r['pass'] else 'FAIL'} | {notes} |")
    (outroot / "results.md").write_text("\n".join(lines) + "\n")
    log("wrote", outroot / "results.md")


if __name__ == "__main__":
    main()
