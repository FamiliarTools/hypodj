# Why songs fail to download into the offline mirror

Date: 2026-09-09
Status: complete

## Summary

Every permanent download failure observed on the live mirror was the Navidrome
index reporting a wrong `size` for the song, not a network, disk, or hypodj fault.
The audio files on the server are complete and download fine; only the indexed
metadata is wrong. hypodj refuses these before a byte lands, burns all eight
backoff attempts on a disagreement that can never change, and reports the result
as "failed to download", which reads as transient. The fix is a full Navidrome
rescan; the code-side gap is that the failure is misclassified and the affected
songs are unnameable.

## How the refusal happens

`fetch_original` (`crates/hypodj-core/src/store.rs`) compares the response
`Content-Length` against the server-reported `song.size` and refuses before
streaming any bytes:

```
download: server declared 30617839 bytes, expected 3145728
```

`expected` is Navidrome's indexed size. The gate is correct and deliberate - it
is what makes a wrong file unable to commit. The problem is upstream of it.

## The evidence

All 13 ids that ever gave up were queried against the live server. Indexed size
versus the bytes the same server then serves:

| Indexed `size` | Real `Content-Length` | Song |
|---|---|---|
| 3145728 (3 MiB) | 30617839 | Sneaker Pimps - Walking Zero |
| 3145728 (3 MiB) | - | Sneaker Pimps - How Do |
| 3145728 (3 MiB) | - | Sneaker Pimps - Becoming X |
| 3145728 (3 MiB) | - | Sneaker Pimps - Spin Spin Sugar |
| 3145728 (3 MiB) | - | Sneaker Pimps - Waterbaby |
| 4194304 (4 MiB) | - | Sneaker Pimps - Tesko Suicide |
| 12582912 (12 MiB) | 43751158 | Superpitcher - Happiness |
| 14680064 (14 MiB) | - | Superpitcher - Voodoo |
| 11534336 (11 MiB) | - | Superpitcher - Grace |
| 13631488 (13 MiB) | - | Superpitcher - I Walk |
| 47841147 | 50720816 | Headache - The Beginning of the End |
| 25562820 | - | Headache - That Thing with the Rabbit |

Two distinct causes, distinguishable by the shape of the number:

- **Exact round MiB values** (3 MiB, 12 MiB, 14 MiB) across whole albums. Real
  FLACs are never exactly a power-of-two multiple. These were scanned while the
  files were still being copied. `bitRate` is derived from `size` and is
  correspondingly absurd - a "90 kbps FLAC".
- **Plausible non-round values that drifted** (the two Headache tracks). Scanned
  correctly, then the file changed on disk, most likely a tag rewrite.

## Why it is worse than a one-off failure

- The disagreement is immutable, but `Backoff` treats it as transient and spends
  `DOWNLOAD_GIVE_UP_AFTER` attempts on it. A restart re-arms the backoff, so the
  whole cycle repeats on every daemon start, forever.
- `StoreStatus::given_up` is a bare `usize`. The ids are discarded, so the only
  way to learn *which* songs are affected is
  `journalctl --user -u hypodj | grep 'giving up'`. Neither the badge nor
  `dj store` can name them.
- The badge said "N songs failed to download", which describes a transient event
  and invites waiting for a retry that will never run. Reworded to
  "N songs will not download" on 2026-09-09.

## Recommendations

1. **Run a full Navidrome rescan.** This is the actual fix and it is server-side.
   The files are intact; only the index is wrong.
2. **Classify a size disagreement separately from a transient failure.** Skip on
   the first mismatch rather than retrying eight times, and give it its own
   status so the badge can say something true about it.
3. **Carry the ids.** `PassReport` and `StoreStatus` should hold the given-up
   `SongId`s so `dj store` can name the songs. A count the user cannot resolve
   into titles is not actionable.
