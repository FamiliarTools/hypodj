# hypodj-mir: measured audio features

First slice of docs/design/audio-features/design.md. `hypodj-mir` is an offline
batch that analyses the audio files already in the offline mirror and writes one
`<song_id>.json` per song into a features directory. The daemon only READS that
directory (`[features].dir`, default `<state_dir>/features`, a sibling of
`store/`, never inside it) and re-reads it when its mtime moves.

## What it computes

| Field | Source | Notes |
|---|---|---|
| `bpm`, `key`/`scale`, beat positions | Essentia classic `streaming_extractor_music` 2.1_beta2 | always |
| `replay_gain_db`, `danceability_dfa`, `onset_rate`, `dynamic_complexity` | same | 2.1_beta2 has no EBU R128 LUFS; `replay_gain_db` is what it measures |
| `voice` = P(voice) | `voice_instrumental-discogs-effnet-1` | optional models |
| `relaxed`, `danceable` | `mood_relaxed-` / `danceability-discogs-effnet-1` | optional models |
| `arousal`, `valence` (DEAM, 1..9) | `msd-musicnn-1` + `deam-msd-musicnn-2` | optional models |
| embedding (1280-d, mean-pooled) | `discogs-effnet-bs64-1` | optional models; not consumed yet |

Energy, the number the `Calmer` selector ranks by: DEAM arousal mapped to 0..1
when present, otherwise a documented classic proxy (mean of normalised loudness,
danceability and onset density, `features::FeatureFile::energy`). A song with no
valid file keeps the old genre heuristic, and `djc features` says which one it got.

Validity key: song id + the store's fingerprint (size, suffix, created) +
`EXTRACTOR_VERSION` + model set. A run skips valid files, so it is resumable and a
re-run is a no-op. A file made with more models than the current run asks for is
kept (a classic-only run never overwrites learned heads).

## Running it

Classic only (packaged, no model, no network):

```
hypodj-mir --store ~/.local/state/hypodj/store --out ~/.local/state/hypodj/features
```

The store is read-only to it; it never writes there. Each worker is admitted by
measured headroom before every track (`features::workers_for`: free cores from
loadavg, MemAvailable minus a 3 GiB reserve over the measured per-worker peak),
`--jobs` is only the ceiling. It runs at nice 19 and defers on battery unless
`--allow-battery`.

With the learned heads add `--models DIR --tf-python PY --tf-script
tools/mir/mir_tf.py`. The TF helper needs the `essentia-tensorflow` wheel, which
is not in nixpkgs; today it runs from a venv through nix-ld:

```
python3 -m venv /tmp/mir-venv && /tmp/mir-venv/bin/pip install essentia-tensorflow
LD_LIBRARY_PATH=$NIX_LD_LIBRARY_PATH hypodj-mir ... --tf-python /tmp/mir-venv/bin/python3
```

## Models and licence

The model weights are MTG's, **CC BY-NC-SA 4.0** (https://essentia.upf.edu/models):
personal, non-commercial use only. hypodj never ships them. The flake output
`.#mir-models` fetches the six `.pb` files pinned by hash, only when built:

```
nix build .#mir-models   # re-fetch; result/ is the --models DIR
```

Essentia itself (the classic extractor and the wheel) is AGPL-3.0 and runs only
as a child process; nothing of it is linked into hypodj.

## Measured (bubble-gum, 2026-11-25, 7 mirror copies, 1762 s of audio)

| Run | child CPU / track | CPU real-time factor | wall (7 tracks) | peak child RSS |
|---|---|---|---|---|
| classic only, 2 workers | 9.3 s | 0.037 | 35 s | 296 MiB |
| classic + TF heads, 2 cores per worker | 23.9 s | 0.095 | 97 s (load ~5: worker 2 mostly yielded) | 1408 MiB (2.1 GB on one unpinned run) |

Extrapolated to the 422-track mirror: about 3 CPU-hours with the heads.

## Not yet

- No-vocals curation (`voice < 0.35`, design decision 5): the scalar is stored and
  shown, not yet a `CurateFilter`.
- Similar re-rank by the embedding (0ginfsh item 1), PullField voice/bpm axes,
  transition planner reading beats/key.
- A packaged TF environment, the systemd timer, `download` for songs outside the
  mirror.
