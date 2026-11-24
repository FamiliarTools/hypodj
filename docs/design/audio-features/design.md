# Per-track audio features: how hypodj learns what a track sounds like

Status: design, 2026-11-24. No production code yet. Feeds Memoria tasks iwwy04x
(vocal/mood/energy/texture), 0ginfsh (P4 embedding re-rank), and node-graph step 4
(feature-aware ranking + phrase alignment).

## The problem in one line

Curation can only filter on what Navidrome exposes (artist, title, genre), so "no
voices" fell back to guessing instrumental ARTISTS (4xwqweb). Every richer axis -
vocal presence, energy, mood, tempo/key, texture, similarity, phrase boundaries -
has to be DERIVED from the audio we own (Guilherme's 2026-07-17 correction:
local derivation is primary, external corpora optional at most).

## What the server already gives (measured, live Navidrome, 2026-11-24)

Read-only `getScanStatus` + a full `search3` walk:

| Field | Coverage of 1950 songs |
|---|---|
| songs / folders | 1950 / 208, 181.0 h of audio |
| `bpm` > 0 | 74 (3.8%) |
| `replayGain` non-empty | 370 (19%) |
| `moods` non-empty | 0 |
| `musicBrainzId` | 453 (23%) |
| OpenSubsonic extensions | transcodeOffset, formPost, songLyrics, indexBasedQueue, transcoding, playbackReport, topSongsByArtistId - **no sonicSimilarity** (answers 0ginfsh item 4: it is not advertised) |

Conclusion: tags are too sparse to carry anything, and MBID coverage (23%) bounds
any AcousticBrainz/ListenBrainz lookup to under a quarter of the library even
before AcousticBrainz's 2022 freeze. Server data is a fallback, not a source.

The offline mirror (`~/.local/state/hypodj/store`) holds 422 of 1950 tracks
(413 flac, 16 GB, ~38 MB/track).

## Measurements (bubble-gum, Framework 13 AMD, 6 mirror files, 1652 s audio, read-only)

Run under `nice -n19`, pinned to 4 cores (`taskset -c 0-3`), files read in place.

| Extractor | Wall per track (mean) | Real-time factor | Peak RSS | Threads |
|---|---|---|---|---|
| `essentia-extractor` 2.1_beta2 (nixpkgs; AcousticBrainz `streaming_extractor_music`): bpm, beats, key, danceability, loudness, spectral stats | 15.3 s (3.7-23.2) | 0.055 | 120-295 MB | 1 |
| `essentia-tensorflow` 2.1b6 (PyPI wheel): discogs-effnet embedding (1280-d per ~1 s patch) + heads voice_instrumental, mood_relaxed, danceability, approachability, engagement + MusiCNN + DEAM arousal/valence | 18.8 s (7.4-38.6), model load 0.2 s | 0.068 | 1.37 GB (whole process, all graphs) | 4 intra-op, 131% avg CPU used |

Extrapolated to 1950 tracks / 181 h:

- TF heads + embedding: 181 h x 0.068 = **~12 h** on 4 cores at nice 19; it
  barely uses the 4 cores, so 2 single-thread workers would likely do it in
  similar time. Incremental cost after that: ~19 s per new track.
- Classic extractor: ~10 CPU-hours single-thread, ~2.5 h with 4 workers.
- Both in one pass share the decode; budget ~12-15 h once, overnight.

Sanity of the voice head on the sample (P(instrumental), P(voice)):
Esperanza Spalding "Dancing the Animal" 0.28/0.72; Thievery Corporation
"Encounter in Bahia" 0.79/0.21; perila "ways" 0.97/0.03; C418 "Biome Fest"
0.92/0.08; Bill Callahan "I Love You" 0.36/0.64; Tosca "Springer Ambient (Live)"
0.94/0.06. 6/6 on the right side of 0.5, the folk-vocal one least confident -
consistent with the model card's 0.96 5-fold accuracy on 1000 MTG excerpts. The
classic extractor's beat tracker returned plausible tempi (100-145 bpm) and a full
`beats_position` array, but no downbeats or sections.

Not measured (flagged, not guessed): CLAP, beat_this, allin1, Demucs runtimes.

## Options

hypodj is FSL-1.1-MIT. Every model below runs in a SEPARATE process that writes
data files; nothing is linked into the daemon, so AGPL code (Essentia) and
non-commercial weights never become part of the hypodj binary. They do bind how
hypodj may DISTRIBUTE them: model weights are fetched by the user, never vendored.

| Option | Gives | Quality evidence | License | Runtime here | RAM | Nix |
|---|---|---|---|---|---|---|
| A. Navidrome tags (bpm, replayGain, moods) | bpm, loudness | 3.8% / 19% / 0% coverage | n/a | free | 0 | n/a |
| B. AcousticBrainz / ListenBrainz dumps by MBID | AB high-level (voice, mood) | frozen 2022, <=23% of library by MBID | CC0 data | download | small | trivial |
| C. `essentia-extractor` (classic) | bpm, beat grid, key/scale, danceability, loudness, spectral | standard MIR baselines | AGPL-3.0 binary | 0.055 RTF, 1 core | <300 MB | **in nixpkgs** |
| D. Essentia TF models: discogs-effnet + heads | embedding 1280-d; voice/instrumental; mood_* (relaxed, happy, sad, aggressive...); danceability; arousal/valence (DEAM, MusiCNN); approachability/engagement | per-model cards, voice 0.96 CV acc | library AGPL-3.0; **MTG models CC BY-NC-SA 4.0** (essentia.upf.edu/models) | 0.068 RTF, 4 cores | 1.4 GB | wheel not in nixpkgs; `buildPythonPackage` from PyPI wheel + autoPatchelf is feasible (ran here via nix-ld) |
| E. CLAP (LAION-CLAP CC0 code; MS-CLAP MIT) | text-queryable embedding ("warm late night") | zero-shot; known weak on negation ("no voices") | permissive code; check per-checkpoint weights | not measured; transformer on 10 s windows, expect slower than D | ~2 GB expected | PyTorch in nixpkgs, workable |
| F. librosa / aubio | onset, beat, chroma, RMS | baselines, no learned heads | ISC / GPL-3.0 | not measured | small | in nixpkgs |
| G. beat_this (CPJKU, MIT) / allin1 (MIT, needs Demucs) | downbeats; allin1 adds sections (intro/verse/chorus) | ISMIR 2024 / 2023 SOTA | MIT code | not measured; allin1 heavy (source separation) | GB-scale | PyTorch, workable |

## Recommendation

**D + C in one offline batch, behind the existing `FeatureStore` seam; G later and
only for the planner; E deferred; A/B ignored.**

- D answers the concrete asks with real models: "no voices" = voice head,
  "more motion" = arousal + danceability + bpm, mood axes = DEAM valence/arousal
  plus the mood_* heads, and discogs-effnet is the embedding 0ginfsh needs for the
  Similar re-rank. Measured: one overnight pass for the whole library.
- C is already packaged, costs little on top, and supplies bpm, key (Camelot) and
  the beat grid for transitions.
- CC BY-NC-SA is fine for Guilherme's personal library (non-commercial use of
  derived data). The rule that keeps it fine: hypodj ships the CODE that fetches
  and runs a model, never the weights, and the README states the model license.
  If FamiliarTools ever commercialises this, D needs MTG's proprietary licence or a
  swap to E (permissive). Decision below.
- E's selling point is open-vocabulary text queries, but the measured asks are a
  fixed small set and negation is exactly where CLAP is weakest. Add it when a
  query the fixed axes cannot express actually shows up.
- G is what phrase alignment needs (downbeats, sections); it is node-graph step 4
  material and should not block curation.

## Data model and storage

Sidecar lives at `$STATE_DIRECTORY/features/`, a sibling of `store/`, **never
inside it**: the store reconciler deletes every file it does not recognise as a
valid audio+sidecar pair (`store.rs::scan_dir`).

One file per song, `features/<song_id>.json`, written atomically
(`resume::atomic_write_bytes` pattern: temp + rename):

```json
{
  "schema": 1,
  "song_id": "00JCKlZLuR...",
  "source": { "size": 41234567, "suffix": "flac", "created": "2024-..." },
  "extractor": { "name": "hypodj-mir", "version": "1", "models": ["discogs-effnet-bs64-1", "voice_instrumental-discogs-effnet-1", "..."] },
  "scalars": {
    "voice": 0.72, "danceability": 0.72, "relaxed": 0.94,
    "arousal": 5.02, "valence": 5.26,
    "bpm": 126.7, "key": "F", "scale": "major", "loudness_lufs": -9.1
  },
  "embedding_f16_b64": "...",          // mean-pooled discogs-effnet, 1280-d
  "timeline": { "beats": [ ... ], "downbeats": null, "sections": null }
}
```

- `source` is the same `Fingerprint` triple the store already uses (size, suffix,
  created), taken from the Subsonic `Child`, so staleness is decided without
  hashing audio.
- Validity key = (song_id, source, extractor.version). Mismatch in any = re-run.
  Idempotent: a re-run on a valid entry is a no-op.
- Size: 1280 x f16 = 2.5 KB embedding + beats ~5 KB; ~10 KB/song, ~20 MB total.
  The daemon loads the whole directory at start into a `HashMap<SongId, ...>`
  (1950 x 2.5 KB embeddings = 5 MB RAM); brute-force cosine over 1950 is
  microseconds, so no ANN index until the library is ~100x larger.

Rust side (implementation sketch, not code):

- `TrackFeatures` grows optional measured fields: `voice: Option<f32>`,
  `bpm: Option<f32>`, `key: Option<Key>`, `arousal/valence` measured. Keep `energy`
  and `valence` totals: a `FileFeatureStore` fills them from measured
  arousal/valence when present, else delegates to `MetadataStore`. So every caller
  keeps working on a song never analysed.
- `timeline` is NOT on `TrackFeatures` (hot path stays small); a separate
  `fn timeline(&self, id) -> Option<Timeline>` on the store for the planner.
- The daemon only READS this directory. It never runs a model.

## Pipeline

A separate batch tool `hypodj-mir` (Python, Essentia), packaged in the flake as
a `writeShellApplication`-style wrapper with its own runtimeInputs; model weights
fetched by a fixed-output derivation pinned by hash (user-side download, not
redistributed).

When: a `hypodj-mir.service` oneshot + timer (nightly) plus a run triggered when
the library scan time (`getScanStatus.lastScan`) moves. Always incremental.

Input, in order of preference:
1. a file in the offline mirror (read-only, no network) - 422 tracks today;
2. otherwise Subsonic `download` into a private temp dir, analyse, delete.
   Full-library cost is ~74 GB transferred once (38 MB x 1950).
3. Better if the Navidrome host can run it: analyse next to the files, ship only
   the ~20 MB of JSON back. Decision below.

Resumable and idempotent: per-song, the work unit is "validity key missing or
stale -> analyse -> atomic write". Killed mid-run = that song is simply not
written; the next run picks up there. No global state, no journal.

Yields to headroom (measured, not constant):
- Worker count = min(2, cores idle by `/proc/pressure/cpu` and loadavg) before
  each track; 0 = sleep and re-check. TF intra-op threads = 2 per worker (measured
  ~1.3 cores used at 4, so 4 buys little).
- Skip when on battery (`/sys/class/power_supply/*/online`) or when available RAM
  < 3 GB (process peaks at 1.4 GB).
- systemd: `Nice=19`, `CPUSchedulingPolicy=idle`, `IOSchedulingClass=idle`,
  `MemoryHigh=2G`, `CPUQuota=200%`.
- Playback itself is light (mpv decode), so the real conflict is latency on the
  laptop, not audio dropouts; the idle scheduling class is what protects mpv.
  Cost named: on a busy day the batch simply does not progress.

## Consumers

**Curation ("no voices", "more motion").** The NL translator (hypodj-nl) gains
predicates `vocal`/`instrumental` and `energy up/down`, compiled to a filter in
the Plan IR (`voice < 0.35` for "no voices"; uncertain 0.35-0.65 kept at the end,
not dropped, so a sparse library does not empty the queue). Songs with no
features: excluded from a hard "no voices" only when enough analysed candidates
exist; otherwise kept and the reply says how many were unanalysed (honest
coverage, the heard-ledger posture). This goes through `run_action_outcome` like
every other `Action`, so it stays retractable.

**Selection.** `Calmer`/energy already read `FeatureStore` energy; they switch to
measured arousal automatically. `Similar` (0ginfsh item 1): re-rank the wire
candidates by cosine over the effnet embedding when both seed and candidate have
one, else keep wire order. `behavior_score` stays the second key. The PullField
axes (`feature_axes`) gain voice/bpm when present.

**Transition planner (node-graph step 4).** Reads `timeline` + bpm + key:
Camelot distance and tempo ratio weight the edge; with beats only, align the
crossfade to a beat; with downbeats/sections (option G, later) wait for a phrase
boundary. Absent timeline = step 2's plain equal-power crossfade. Loudness LUFS
also feeds the startle-risk input of step 3 (covers the 81% with no replayGain).

## Decisions for Guilherme (recommended default first)

1. **Model license**: accept CC BY-NC-SA Essentia models for personal use, weights
   fetched not shipped. Default: yes. Alternative: CLAP-only (permissive, weaker on
   "no voices", unmeasured).
2. **Where the batch runs**: Default: bubble-gum over the mirror first (422 tracks,
   zero network, ~2.5 h), then `download` for the rest overnight. Alternative: run
   on the Navidrome host next to the files (no 74 GB transfer; needs a deploy
   there).
3. **Full library or mirror only**: Default: full library, mirror first.
4. **Phrase/downbeat analysis (G)**: Default: defer until the transition engine's
   step 2 (second voice) exists; beats from C are enough before then.
5. **"No voices" threshold**: Default `voice < 0.35`, uncertain kept at the end.

## Sources

- Essentia models + license: https://essentia.upf.edu/models.html ("All the models
  created by the MTG are licensed under CC BY-NC-SA 4.0").
- Essentia library: AGPL-3.0 (github.com/MTG/essentia COPYING.txt).
- voice_instrumental-discogs-effnet-1 model card (fetched JSON): classes
  instrumental/voice, 1000 excerpts, 0.96 5-fold normalized accuracy.
- LAION-CLAP LICENSE: CC0-1.0; microsoft/CLAP LICENSE: MIT; librosa: ISC.
- nixpkgs `essentia-extractor` 2.1_beta2, meta.license AGPL-3.0-or-later.
- Live Navidrome: getScanStatus, getOpenSubsonicExtensions, search3 (2026-11-24).
