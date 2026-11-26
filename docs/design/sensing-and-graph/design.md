# Sensing the room, and what is left of the node graph

Design research, 2026-11-26. Two directions ruled in scope for feature-complete:
physical cues (Memoria task c1us19g, spine m:01kxpp3) and the node-graph
re-foundation (task 68h5sg2). Read-only on the code; nothing here is built.

Ground truth this rests on (checked today, not assumed):

- The transition engine landed (555d269): `reconcile.rs` is a declared head plus one
  pure `plan`, every head change crossfades. The "a removal is the declaration of a
  new state" reframe is already literal code.
- The input shore's first cell landed (fe04474, b893cf7): `intelligence.rs` has a
  decaying `Pull` / `PullField`, set by `field <words>`, bias-only at the one
  selection seam (`pull_rerank` in `plan_enqueue`).
- `features.rs` reads measured bpm, key, beats, arousal/valence, voice and a 1280-d
  effnet embedding, written by the `mir` bin. **But `~/.local/state/hypodj/features`
  does not exist on bubble-gum: live coverage is zero.** `reconcile.rs` reads no
  feature at all (no bpm, key or beat reference in the file).
- Both parts below depend on that one missing artifact. It is the real blocker.

# Part 1 - Sensing

## What hardware is actually here

Measured on bubble-gum, not guessed:

| Sensor | Present | Note |
|---|---|---|
| Webcam | yes, `/dev/video0`, "Laptop Webcam Module (2nd Gen)" | hardware privacy switch |
| Internal mics | yes (HDA) | hardware privacy switch |
| Accelerometer | **no**. IIO exposes only `als` (ambient light) | no lid/chassis motion |
| Bluetooth, paired | WH-1000XM3, OB-4, MX keyboard and mouse | no IMU reachable on any |
| Phone | unknown | needs Guilherme |

## Which signal means "the room is moving with the music"

**The discriminator is tempo lock, not motion amount.** Motion amount is confounded
by everything a person at a laptop does (typing, talking, reaching). What only
music explains is periodic vertical head motion at the track's beat rate. hypodj
is in the unusual position of KNOWING that rate (measured `bpm` and `beats` per
track), so the question becomes a narrow one: is there a spectral peak in head-y
at `bpm/60` Hz or its metrical neighbours (0.5x, 2x)?

Primary sources that this is a real, measurable effect:

- Head movement entrains to the beat and its vigour tracks engagement: audiences
  moved their heads faster to live than recorded music, and fans were better
  entrained than neutral listeners (Swarbrick et al. 2019,
  https://www.ncbi.nlm.nih.gov/pmc/articles/PMC6336707/).
- Music-induced movement follows musical features, pulse clarity among them
  (Burger et al. 2013, https://www.ncbi.nlm.nih.gov/pmc/articles/PMC3624091/).
- Even people told to stand still move their heads more with music, most with
  high tempo and pulse clarity (Gonzalez-Sanchez, Zelechowska, Jensenius 2018,
  https://pmc.ncbi.nlm.nih.gov/articles/PMC6090462/). That study is mm-scale
  motion capture; a laptop camera will not see micromotion. This slice targets
  visible bobbing only, and says so.

Lock is phase-free (only frequency is tested, so the variable latency of the
Bluetooth OB-4 is irrelevant) and has a built-in null: the same trace scored
against a wrong tempo must show no lock, or the detector measures motion, not
music.

## Candidates, ranked

| Signal | Reliability | Cost | Verdict |
|---|---|---|---|
| Webcam, head-y tempo lock | good for one seated person within ~1.5 m | one small model at 10-15 fps | **first slice** |
| Webcam, frame-difference periodicity (no person model) | weaker, but works for a room at distance | trivial | fallback mode, same estimator |
| Mic residual after echo cancel | poor as a positive cue | moderate | later, as a NEGATIVE cue only |
| Phone IMU in a pocket | best raw signal | high friction | defer, needs his phone |
| Daemon's own gesture log | free, already labelled | zero | use as the scorer (below) |

**Subtract full pose.** A head bob needs one point. A face detector's box centre
gives head-y; BlazeFace was built for exactly this budget
(https://arxiv.org/abs/1907.05047). The 33-landmark pose model
(https://ai.google.dev/edge/mediapipe/solutions/vision/pose_landmarker,
https://arxiv.org/abs/2006.10204) is only worth it if shoulders or torso prove
necessary. The beat band is 0.5-4 Hz, so 10-15 fps at 320x240 satisfies Nyquist.

**Mic.** It mostly hears hypodj's own output. PipeWire's echo-cancel can subtract
the playback reference (https://docs.pipewire.org/page_module_echo_cancel.html),
but the leak is correlated with the music, so it fakes the very tempo-locked
energy we want. Honest use, later: speech in the residual argues AGAINST louder.

**Phone.** `DeviceMotionEvent` in a browser is secure-context only, needs a
permission call from a gesture, and fires only while the page is visible
(https://www.w3.org/TR/orientation-event/): a TLS endpoint and a screen held
awake. Too much friction for a first proof.

**CPU.** Unmeasured here. Published: BlazePose Lite is about 25 ms a frame on one
Pixel 4 core (https://arxiv.org/abs/2206.11678); face-only is cheaper. Budget:
under 10% of one core, duty-cycled (a 10 s window every 30 s, only while playing,
only when bpm is known). Over budget means drop to frame-difference, not raise it.

## Privacy and perceptibility

- **Off by default.** `[sense] enable = false`. Turning it on is session-scoped
  (`sense on` expires at daemon stop or after a bounded span), never sticky.
- **Nothing leaves the process, let alone the machine.** Frames are never written
  to disk. The only thing that crosses a socket is a scalar a second: lock score,
  matched ratio, confidence. The unit gets `IPAddressDeny=any` plus
  `IPAddressAllow=localhost`.
- **A separate process**, `dj-sense`, under the same rule as `dj-mcp`: it speaks
  MPD and never links `hypodj-core`. The daemon gains no camera code.
- **Visible indicator, twice.** The camera LED, and a sensing mark on the
  now-playing strip of every client. Caveat to verify by eye: Framework documents
  the LED as hardware-driven off sensor power for the original module
  (https://frame.work/blog/1080p-webcam); for the 2nd-gen module the community
  reports the switch cuts only the sensor and the controller then serves blank
  frames (https://community.frame.work/t/fw-13-pro-webcam-switch-is-it-truly-off/85735).
  Consequence for the estimator: a blank frame is "blind", never "nobody moving".
- **The hardware switch is the final authority** and needs no software to honour.

## Sense -> reason -> act, startle-safe

The adapter contract already exists from the latent-field work: an input is
`(kind, direction, strength, half_life, provenance)`. A sensor is one more
adapter, not a new subsystem.

- **Sense.** `dj-sense` emits `lock in [0,1]` with a confidence.
- **Reason.** Sustained lock (two consecutive windows) becomes a `Pull` on the
  energy axis with provenance "from the room", strength capped below the NL
  lexicon's 0.6 so his words always outrank a camera, short half-life. That is
  bias-only on the NEXT selection and never touches the queue. `field` already
  shows it and `field clear` already removes it. (The `field` verb maps words
  through the lexicon today; carrying a provenance and a strength is the one
  small wire extension.)
- **Act on volume**, in three owner-set stages (`act = suggest | confirm | auto`):
  - `suggest`: a line in the readout ("moving with the beat - louder?"). No effect.
  - `confirm`: a held plan he accepts with one key.
  - `auto`: `plan add ... origin sense:cam` with `Action::Fade`. That goes through
    `run_action_outcome`, so it is journaled and undoable, and it is a ramp through
    the one fade slot, never a step.
- **Bounds on `auto`**, all data, none a branch per case:
  - at most +2 dB per nudge (two level JNDs, the 1 dB figure `listen.rs` already
    cites), over a ramp of several seconds;
  - at most +6 dB above the last level a HUMAN set, never above the comfort
    ceiling the wake ramp already respects;
  - a refractory period between nudges;
  - any human volume gesture is a veto: it cancels the fade (the existing atomic
    cancel-and-mutate), becomes the new baseline, and silences the sensor for a
    while.
- **Asymmetric on purpose.** Stillness is not evidence of disengagement (deep
  focus looks identical), so absence of lock never turns anything down.

**The scorer is free.** His own gestures are labels. A manual volume-up within a
minute of a suggestion is a hit; a volume-down or a skip is a miss. A week in
`suggest` mode yields a precision number before `auto` is ever considered.

## Testing without a human

Split the detector at the only seam that matters:

1. **Estimator, pure.** `(t, y)` samples plus a tempo in, lock score out; no wall
   clock, so the repo's fake-clock rule holds for free. Table over synthetic
   traces: sine at the beat plus noise (lock); sine at 0.8x tempo, random walk,
   typing-like bursts, flat line (no lock); blank input (blind).
2. **Vision front, on files.** `dj-sense` takes a video path and a bpm in place of
   the device, so no test opens the real camera. Fixtures are `ffmpeg` moving a
   still face image along `y = A sin(2 pi f t)`: known frequency, checkable.
3. **Reality fixtures, his.** Three 30 s clips, recorded once: bobbing to a known
   120 bpm track, typing with music on, sitting still. Kept outside the repo.

## Recommended first slice

`dj-sense` reading a FILE, face-box head-y, the tempo-lock estimator, printing a
lock score and a suggestion line. No daemon change, no camera, no action.

## Acceptance checks

- Estimator table: every synthetic case above lands on the right side of one
  threshold, and the wrong-tempo control stays under it.
- Synthetic video: recovered frequency within 0.1 Hz of the rendered one.
- His three clips: bobbing locks in at least 8 of 10 windows; typing and still
  lock in at most 1 of 10. **Kill criterion:** if typing-with-music locks more
  often than that, a laptop camera cannot tell work from groove, and the camera
  path stops here.
- CPU under 10% of one core at 15 fps 320x240, measured.
- With the privacy switch off: output is "blind", not "still".

## What needs Guilherme

- The three clips (two minutes of his time; nothing else can supply his face).
- Which phone he carries, only if the camera path is killed.
- Whether "the room" means him at the laptop or people across a room. The first
  is the face path; the second is the frame-difference fallback and a camera
  pointed at the room.
- Later, not now: the nod to move from `suggest` to `auto`.

## Slice 1 as built (2026-11-26)

`crates/hypodj-sense`, binary `dj-sense`, package `.#dj-sense`. Depends on
neither `hypodj-core` nor the daemon; reads a FILE only (ffmpeg decodes it, a
path that is not a regular file is refused).

```
dj-sense VIDEO --bpm N
```

prints one line per 10 s window (every 2 s, so 11 windows over 30 s): verdict
(`lock`, `no-lock`, `still`, `blind`), lock share at the best of 0.5x/1x/2x, the
control share at 0.8x tempo, the strongest frequency, the trace RMS and face
coverage; then `lock: k/n windows, score s`, the CPU line, and one suggestion
line (`suggest: moving with the beat - louder?` only after two consecutive
locked windows; `blind: ...` when most windows see no face).

**Detector: rustface** (SeetaFace funnel cascade, pure Rust, BSD-2-Clause, model
`seeta_fd_frontal_v1.0.bin` 1.2 MB as the fixed-output `.#dj-sense-model`). Picked
for Nix buildability: two small pure-Rust deps, nothing native. ONNX via `ort`
needs the onnxruntime C++ runtime wired through Nix, `tract` is a large compile,
and OpenCV is a C++ build plus bindgen. After the first full-frame hit it searches
only a region around the last face, resampled so the face is about 48 px, with
the pyramid capped near that size; a flat frame (luma std < 3, what the privacy
switch serves) is never searched and counts as no face.

**Estimator.** Gaps interpolated, line detrended, Hann window, power on a 0.02 Hz
grid over 0.4-4.5 Hz. Lock = share of in-band power within 0.12 Hz of
`bpm/60 * {0.5, 1, 2}` >= 0.35, AND >= 3x the best share at the 0.8x control.
RMS under 0.025 face heights is `still` (detector jitter on the synthetic still
clip measured 0.014; a visible bob 0.08-0.09). Face in < 70% of frames is `blind`.

**Analysis rate 10 fps, not 15.** Measured below: at 15 fps the detector alone
was 10-12% of one core, at the budget's edge; at 10 fps Nyquist is still 5 Hz,
covering 2x of 120 bpm. Consequence: above 135 bpm the 2x band leaves the
analysed range and only 0.5x and 1x are tested.

### Measured results (bubble-gum, release build, `taskset -c 3`)

Synthetic fixtures, rendered in `tests/synthetic.rs` (runs in the Nix check
phase, which provides ffmpeg and the model): the public-domain face
(`tests/fixtures/face.pgm`, a downscale of "Official Portrait of President Reagan
1981", US federal government work, Wikimedia Commons) composited on a noisy
640x480 gradient, 30 s each, encoded lossless (ffv1):

| Clip | Result |
|---|---|
| bob, 2 Hz sine, 16 px (0.13 face heights), scored at 120 bpm | lock 11/11, peak 2.00 Hz every window |
| same bob scored at 96 bpm (wrong-tempo control) | lock 0/11 |
| typing (irregular glances down 10-30 px, 0.3-2 s holds, shifts) | lock 0/11 |
| still | lock 0/11, still 11/11 |
| blank (all-black frames) | blind 11/11, suggestion `blind` |

Estimator table (pure, `src/estimator.rs`): beat sine, 0.5x and 2x lock; 0.8x
sine, the 2 Hz sine scored at 96 bpm, random walk, typing bursts do not; flat
is `still`; empty or half-covered input is `blind`.

CPU per 30 s of 640x480 video, from `/proc/self/stat` (own vs reaped ffmpeg
child), machine under load (loadavg about 60 from other work, so these are CPU
times, not wall):

| Input | Rate | Analysis (dj-sense) | Decoder (ffmpeg) |
|---|---|---|---|
| 320x240 raw gray | 15 fps, full-frame each time (first cut) | 7.6 s = 25% | - |
| same | 15 fps, tracked region | 3.0-3.6 s = 10-12% | 1.8 s = 6% |
| same | **10 fps, tracked region** | **1.9 s = 6.2-6.5%** | 0.6-1.2 s = 2-4% |
| 640x480 H.264 30 fps, temporal-noise worst case (8.8 MB/30 s) | 10 fps | 1.8-2.0 s = 5.9-6.7% | 3.4-4.5 s = 11-15% |

So the detector plus estimator is inside the 10% budget at 10 fps. Decoding a
noisy high-bitrate H.264 FILE is not, on its own; that cost belongs to the file
path (a live camera hands over raw frames at 320x240, no H.264), and the
design's duty cycle (10 s every 30 s) divides everything by three.

### Recording the three reality clips (Guilherme runs this, nobody else)

Only he records: it opens `/dev/video0` and his face. Kept outside the repo.
Put on a track whose measured bpm you know (120 is the reference; any is fine,
pass the real number), sit as you normally do at the laptop, then:

```
mkdir -p ~/Videos/hypodj-sense && cd ~/Videos/hypodj-sense
rec() { nix shell nixpkgs#ffmpeg-headless -c ffmpeg -f v4l2 -framerate 30 -video_size 640x480 -i /dev/video0 -t 30 -an -c:v libx264 -preset veryfast -crf 23 "$argv[1].mkv"; }
rec bob      # nod along to the beat the whole 30 s
rec typing   # music on, just work: type, read, glance at the keys
rec still    # music on, sit and listen without moving on purpose
rec blind    # optional: privacy switch OFF, anything
```

(`rec` is written for fish, his shell; in bash use `$1` for `$argv[1]`.)

### Evaluating them

```
cd ~/familiartools/hypodj
for c in bob typing still blind; nix run .#dj-sense -- ~/Videos/hypodj-sense/$c.mkv --bpm 120; end
nix run .#dj-sense -- ~/Videos/hypodj-sense/bob.mkv --bpm 96   # control: must not lock
```

Read each `lock: k/11` line. Pass: bob >= 9/11 (the design's 8 of 10), bob at
96 <= 1/11, typing and still <= 1/11, blind all `blind`. **Kill criterion:**
typing locking in more than 1 of 11 windows means a laptop camera cannot tell
work from groove, and the camera path stops. If bob fails while its `peak`
column sits at the beat, the thresholds are wrong, not the idea: the per-window
columns are there to show which.

# Part 2 - Node graph

## Subtract first: architecture for its own sake

None of these changes what he hears today, so none is in the slice:

- **A graph store or arena of state nodes.** The phase-1 verdict stands: steady
  state is a current-track pointer and a transition is a two-leg crossfade.
- **An activation field over the queue.** Queue order is declared by a human or a
  plan. Emergent ordering of something he arranged is a regression.
- **The owned audio node graph** (cpal, fundsp, symphonia). It buys nothing until
  he asks for an effect mpv cannot do.
- **An ANN index.** A few thousand tracks times 1280 floats is a brute-force dot
  product in about a millisecond.
- **Multi-hop spreading activation as an engine.** At depth one it is a weighted
  sum, which is what the rerank chain already is. Its useful content, several
  seeds with decay plus inhibition, is one formula (below), not a runtime.
- **The 12-axis CLAP frame.** Same `Pull` code later, if two axes ever prove too few.

## What remains that he would actually hear

Three gaps, all visible in the code:

1. **Sequencing is rank-only.** Every selection path ends in `behavior_rerank` or
   `pull_rerank`, which score each candidate against the seed alone. Nothing
   scores candidate against NEIGHBOUR, so a batch can be individually apt and
   jagged as a sequence.
2. **The transition is blind to the pair.** `plan` picks the crossfade shape from
   cause and capabilities. A tempo-matched pair and a clashing pair get the same
   blend.
3. **"What next" is the server's opinion.** Autofill and radio take
   `getSimilarSongs2` and stop honestly when it returns nothing. The local
   embedding could answer where the server cannot, and is unused.

One construct closes all three: **an edge cost between two tracks.**

```
edge(a, b) = w_t * tempo_mismatch(a, b)      // ratio distance, octave-folded
           + w_k * key_distance(a, b)        // Camelot wheel steps
           + w_e * |arousal(a) - arousal(b)|
           + w_s * (1 - cos(emb(a), emb(b)))
```

Pure, total, and every term drops out when its feature is missing, so with no
features the cost is constant and behaviour is byte-identical to today. That is
the same degrade-to-today posture `Pull` has.

Why these terms: a study of 1,557 real DJ mixes and 20,765 transitions tested and
supported the claim that DJs leave tempo and key nearly unchanged across a
transition (Kim et al. 2020, https://arxiv.org/abs/2008.10267), and Spotify
framed playlist sequencing as exactly a traversal over a pairwise-cost graph, with
the crossfade optimised for the same pair (Bittner et al. 2017,
https://archives.ismir.net/ismir2017/paper/000086.pdf).

It is read at two seams and nowhere else:

- **Order.** A resolved batch is emitted as a greedy walk from the current tail,
  each step taking the candidate with the best `rank - edge(prev, cand) + pull`.
  Append-only and count-clamped as before; it only permutes what was already
  going to be appended.
- **Transition.** For `Cause::Natural`, `plan` takes `edge(outgoing, incoming)` as
  one more input and scales the blend: low cost gets the long crossfade, high
  cost a short one. Gestures keep today's behaviour. This is step 3 of the
  reduced design; beat alignment from `timeline.beats` is step 4 and comes after.

**Memoria's part, reduced to what earns its keep.** "What next" as spreading
activation is: seeds are the last K played tracks with recency decay, activation
is similarity to the seed set, inhibition is recently-played and skipped. That is
a weighted multi-seed cosine minus the penalties `behavior_score` already
computes. One matrix-vector product, used as a candidate SOURCE when the wire
call comes back empty. No edges are stored.

**History edges (Hebbian co-play) wait for data, not code.** The heard ledger
already writes an ordered row per library track that sounded (`spent_heard_row`),
but retention is bounded by `keep_sessions` and whether a row tells played-out
from skipped is unchecked. No weights until there are weeks of rows to fit.

## The emergent behaviour the slice must produce

One function, three behaviours, none named in code:

1. **A gradual descent.** With a "calmer" pull active, pull-only ordering jumps
   straight to the calmest candidate. Pull plus edge cost should walk DOWN:
   net arousal falls across the path while each adjacent step stays small. No
   "arc" or "ramp" exists anywhere in the code; it is smoothness times direction.
2. **A bridge.** Tail in one cluster, batch drawn from two far clusters: the walk
   exhausts the near cluster, crosses at the cheapest pair, then continues. No
   genre field is read.
3. **The blend follows the pair.** The same number that ordered the tracks sets
   how long they overlap, so a well-sequenced queue also mixes long, and a human
   forcing a clash gets a short, clean handover.

## Recommended first slice

1. Run the `mir` batch over the mirror (844 files on disk) so features exist at all.
2. `edge(a, b)` as a pure function in `intelligence.rs`, table-tested.
3. The greedy walk at the single existing reweight seam, behind the same
   "no features, no change" identity.

Transition scaling (seam two) is the second slice: it changes audio, so it waits
for his listening witness of the engine as built (task 01m85g3y).

## Acceptance checks

- Identity: with an empty feature store, output order is byte-identical to master.
- Property tests on synthetic features for the descent and the bridge.
- **On the real library, falsifiable:** over 50 sampled seeds, mean adjacent edge
  cost of the emitted order is lower than the wire order's in at least 80%. With
  a "calmer" pull, max adjacent arousal step is smaller than pull-only ordering's
  in at least 80%, with net arousal change still negative. **Kill criterion:**
  if either fails, the edge is vocabulary, not capability, and stops here.
- No new `Action`, no new execution path: the walk sits inside `plan_enqueue`,
  so everything still goes through `run_action_outcome`.
- Second slice only: the planner table gains the edge column, convergence and
  idempotence still hold, and `scripts/transition-listen` stays inside its
  +-1 dB level-continuity bound for both the long and the short blend.
- Whole-workspace build and test, the Nix package build, and feature-less tests
  still skipping cleanly in the sandbox.

## What needs Guilherme

- The two open decisions from `docs/design/audio-features/design.md` that gate
  the batch: accepting the CC BY-NC-SA Essentia models for personal use, and
  running it on bubble-gum over the mirror first (that doc estimated about 2.5 h
  for 422 tracks; the mirror is now roughly twice that), capped at 4 cores.
- His listening witness of the transition engine (01m85g3y) before any change to
  blend lengths.
- One sentence of taste: when he forces a clash, short handover or today's blend.
