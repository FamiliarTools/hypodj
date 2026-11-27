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

## As built (first slice)

- `crates/hypodj-core/src/graph.rs`: `edge(a, b)` (tempo octave-folded,
  saturating at a 12% ratio; Camelot steps over 7; measured-energy difference;
  embedding cosine as `(1 - cos) / 2`), equal weights, every term `0..=1`. A term
  missing on either side reads 0.5, the middle of its range, not 0: a 0 would make
  an unanalysed track the perfect neighbour and pull it to the front of a
  half-analysed batch. The genre-lexicon energy never enters it.
- `graph::walk` orders the batch by `rank - 2 * edge(prev, cand)` from the queue's
  last library song, at the one seam in `plan_enqueue` after `pull_rerank`, for
  exactly the selectors the pull may reorder (mood `Query`, `Calmer`, `Similar`,
  `SimilarToCurrent`, `Radio`). `rank` is the incoming position, which already
  carries similarity, behaviour and the pull. Fewer than two measured candidates:
  the walk returns its input untouched, so with no features every existing test
  is unchanged.
- The kill criterion is `graph::kill_criterion`, run by the `graph-eval` bin.

### Running the kill criterion

On synthetic data (exercises the harness; NOT evidence about the library):

```
nix develop --command cargo run -j4 -p hypodj-daemon --bin graph-eval -- --synthetic
```

On the real library, once `hypodj-mir` has filled the features dir (read-only:
the features dir and `getSong` / `getSimilarSongs2`):

```
nix develop --command cargo run -j4 -p hypodj-daemon --bin graph-eval -- \
  --config /run/user/1000/hypodj/config.toml \
  --features ~/.local/state/hypodj/features --seeds 50 --pool 20
```

Synthetic result (seed `0x5eed`, 50 seeds, pool 20 in the bin, 12 in the
pinned test): the edge leg wins every seed; the calmer leg wins 78% in the test
(76% in the bin),
just under the bar. The synthetic library draws energy independently of tempo,
key and timbre, so the three other terms are pure noise against the descent: the
worst case, not a forecast. The real library decides.

Exit 0 is PASS, exit 2 is the kill criterion firing. Only measured seeds and
candidates count; a seed with fewer than three measured candidates is skipped,
so the printed `seeds evaluated` is the real denominator.

## What needs Guilherme

- The two open decisions from `docs/design/audio-features/design.md` that gate
  the batch: accepting the CC BY-NC-SA Essentia models for personal use, and
  running it on bubble-gum over the mirror first (that doc estimated about 2.5 h
  for 422 tracks; the mirror is now roughly twice that), capped at 4 cores.
- His listening witness of the transition engine (01m85g3y) before any change to
  blend lengths.
- One sentence of taste: when he forces a clash, short handover or today's blend.
