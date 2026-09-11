# 0002. The continuation walk matches the shape of what is playing

Status: accepted

## Context

Autofill is the "keep going, keep playing, do not stop playing" engine: when the
queue truly drains, it fetches library tracks similar to the recency seed,
appends them, and continues. It appended SINGLE TRACKS, always.

That is the right unit for someone playing singles, and the wrong one for
someone playing records. Put on an album, let it run out, and the walk compiled
a mixtape out of other people's albums - one track from here, one from there.
The listening mode the user had chosen was silently dropped at the drain edge.

Stated goal (Guilherme, 2026-09-11): if the user is playing full albums, the
mechanism should put full albums instead of compiling intra-album.

The hard part is not appending an album. It is knowing when to - without adding
a mode the user has to set, and without a flag that can drift out of sync with
what is actually on the deck.

## Options considered

- **Derive the shape from the queue itself (chosen).** Take the anchor (current
  track, else the last library song) and measure the contiguous run of its
  `album_id`. A run of 3 or more is someone playing a record. No setting, no
  state to maintain, and it is true by construction at every drain.
- **An explicit `album` continuation mode.** Rejected: a third mode beside
  `radio` and `autofill` that the user has to know exists and remember to flip
  when they change how they are listening. The information is already on the
  deck; asking for it again is the tool serving itself.
- **A sticky flag set when an album is enqueued.** Rejected: a flag is a second
  source of truth about the queue. Every path that edits the queue (a manual
  enqueue, a delete, a clear, a shuffle) becomes a place the flag can go stale,
  and the failure is silent - albums appended to a deck that is no longer a
  record, or singles appended to one that is.
- **Ask the server for a similar ALBUM directly.** Rejected: `getSimilarSongs`
  is the one similarity seam that exists and is already in the refill. Reusing
  it as the suggestion layer and expanding the chosen candidate's album with
  `getAlbum` adds one call, no new backend capability, and keeps working on a
  server with no album-similarity endpoint.

## Decision

Album mode is a property of the queue, read fresh at every refill.

`State::album_context()` returns the album being listened through when the
contiguous run around the anchor is at least `ALBUM_CONTEXT_MIN_RUN` (3). Three
is the smallest run that shuffle and the track-level similarity walk do not
produce by coincidence, and it lets the mode engage a few tracks into the FIRST
record rather than only after a whole one.

When it holds, the refill keeps the same single `similar()` fetch as its
suggestion layer and changes only the UNIT it appends: the first candidate whose
album is neither the record playing nor one of the last ten served gets expanded
via `getAlbum`, sorted by disc then track, and appended whole. At most four
expansion attempts per drain.

Three properties carry the design:

| Property | How |
|---|---|
| Sticky without a flag | An appended record IS a long contiguous run, so the next drain reads album mode again. Record after record, with nothing to keep in sync. |
| Short albums still count | A second clause: the anchor's album equals the most recently served album. An EP of two tracks can never reach a run of 3, and this only ever fires on a record the walk itself chose. |
| No immediate repeat, at album granularity | Album mode does NOT filter the track dedup ring - a record is appended whole or not at all - so the no-repeat invariant moves to a 10-deep album ring instead. |

Every failure leg - no album ids among the candidates, every candidate album
recently served, every `getAlbum` failing - falls THROUGH to the ordinary track
walk rather than stopping. Keeping the music going outranks keeping it
album-shaped.

## Consequences

Easier:

- Listening to records is now a mode the tool follows instead of one it ends.
  Nothing to configure; it engages and disengages by itself as the deck changes.
- The track walk is untouched. A shuffled or loose-track deck never issues a
  `getAlbum` at all, so nothing got slower for the single-track listener.

Harder, and the honest costs:

- **One extra round trip per album-shaped drain** (up to four when candidates
  fail to expand), on a path that previously made exactly one call.
- **Coarser steps.** A whole record is appended at once, so the lookahead jumps
  by an album rather than by `autofill_count`, and the walk changes direction
  once per record instead of once per track. That is the feature, but it does
  mean a wrong turn costs a full album of listening rather than one song.
- **Servers that do not report `album_id` cannot reach album mode at all.** The
  detector returns `None` and the behaviour is exactly what it was before.
- **A deliberate three-in-a-row is read as a record.** Someone who hand-queues
  three tracks off one album gets an album appended. Judged correct: that is
  what "playing a record" looks like from the deck, and the 3-run threshold is
  the tunable if it ever proves wrong in practice.
