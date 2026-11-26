# Find ranking: the free tier, measured

Status: shipped (tier A). Tier B: not needed for Find - see the decision below.

## Objective

Find is a search box. The person already knows the answer and is typing it, so the
ranking's job is PRECISION first and habit only as a tiebreak. That is a different
objective from continuation (autofill/radio), which wants discovery; the two may share
signals but must not share a ranking.

## What ships (`crates/hypodj-core/src/find_rank.rs`)

`searchall` keeps its sets exactly (same artists, albums, songs, stations) and only
re-orders artists, albums and songs by `(precision, habit desc, server order)`:

- precision, the PRIMARY key: `Exact` (a name field equals the query, case and
  punctuation folded) < `Words` (every query word is a whole word of the title/name or
  artist) < `Loose` (whatever else the server's full-text index matched). An exact
  title or artist match therefore can never be outranked by any habit signal.
- habit, only from what Navidrome already put on the search3 row: `starred` (+3, a
  deliberate act), `ln(1 + play_count)`, `+1` for a song rated 4 or 5. Artists use
  their star plus the play counts of their songs in the same reply. No extra round
  trip, no durable state in hypodj.
- server order is the final, stable tiebreak: a query with no habit signal comes back
  exactly as Navidrome ordered it.

`Album` gained `play_count` and `starred` (mapped in `subsonic.rs` from
`AlbumId3.play_count` / `starred`), so albums rank on their own counters.

A separate "starts with" band was tried and REMOVED after the first measurement: it put
`Love We Battle` (5 plays) above `Let Love Rumpel` (42 plays) for `love`, the opposite
of what the person typing meant.

## Measurement (2026-11-26, real library, isolated daemon, read-only)

15 fixed queries (`love the night remix life blue minecraft dub you esperanza heart
computer mix sweet rumpel`), `searchall` on a master build vs this build, both against
the live Navidrome with audio null, MPRIS off, store off. The result SETS were
identical for every query (asserted).

- Songs with any habit signal (plays or star) in the top 10, summed over the 15 queries:
  **94 before, 127 after** (509 habit rows exist across all replies).
- Exact matches: `night` had the song titled exactly `Night` at position 7; now 1.
  `minecraft` keeps `Minecraft` at 1. No exact match moved down in any query.
- Representative changes: `esperanza` top 5 went from five 0-1 play tracks to the
  played ones (5 of 10 -> 10 of 10 habit rows); `dub` lifted the played dubs over the
  unplayed `Dub Housing`; `rumpel` keeps `Let Love Rumpel` (42 plays) first;
  `love` puts `Let Love Rumpel` first and `I Love My Computer` (a frequent album) first
  among albums.
- Known soft spot, accepted: inside the `Words` band an ARTIST word counts the same as
  a title word, so `blue` lifts `Pale Blue` tracks (played) above the unplayed
  `Blue and Lonesome`. Precision is still respected (no exact match is involved), and
  it is what the counters say he plays.

## Tier B decision: not needed for Find

Tier B was session/recency weighting, which Subsonic cannot serve as history (there is
no get-my-play-history endpoint), so it would be a new durable artifact in hypodj. The
measurement says Find does not need it:

- the counters that ARE free already move the played/starred rows into the top 10 for
  every query that had any (94 -> 127), and precision still governs the head;
- RECENCY is also already on the wire: every song row carries the server's last-played
  timestamp (`X-LastPlayed`, `Song::played`), so if recency is ever wanted in Find it
  is one more term in `find_rank`, still zero new state;
- what tier B would add beyond that (within-session context) belongs to continuation,
  not to a search box. Reopen only if a real query shows the person's target buried
  under precision-equal rows that recency would have lifted.
