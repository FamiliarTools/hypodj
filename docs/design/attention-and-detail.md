# Attention and detail: the hint and info system

Status: draft
Created: 2026-09-10
Updated: 2026-09-10

## Context

The TUI shows a list of names. Everything else it knows - the cover, the file's
metadata, what Navidrome knows about the user's relationship with a song - is
unreachable without leaving the app.

Two ways in, asked for on 2026-09-09:

- **Passive.** The cursor rests on a row for a moment; the cover appears.
- **Active.** `i` on the selection opens a detail card: file metadata plus the
  server's user-specific and generated data.

Constraints: performance, and an architecture worth living in.

## The one idea

**Attention has a depth, and depth is derived, not evented.**

Not "on cursor move, cancel a timer; on timer fire, dispatch a fetch; on key,
dispatch another". That is three mechanisms, three cancellation bugs, and three
places to rate-limit. Instead:

```
depth(selection, resting_for, asked) -> 0 | 1 | 2
```

A pure function, recomputed every frame. One reconciler diffs what that depth
wants against what is already held, and issues at most one request. Everything
else - scroll bursts, reconnects, cold start, a key pressed during a fetch - is
the same code path: the next frame.

This is not a new pattern in this codebase. It is the one already in
`request_art` (`main.rs`), which computes `want` from `state.now`, compares it
against `art_req_key`, and fires once on change. And it is the offline store's
reconciler in miniature: a desired set, diffed against held truth, one owner,
every edge case collapsing into "the next pass". The info system should be the
third instance of that shape, not a fourth idea.

## The ladder

| Depth | Trigger | Shows | Costs |
|---|---|---|---|
| 0 | row is on screen | name, artist | nothing |
| 1 | cursor rests ~350ms | cover peek, and whatever metadata is already local | usually nothing |
| 2 | `i` pressed | full card: file metadata + server user data | one request |

Two properties make this cheap rather than merely staged:

**Monotone.** Depth only rises with evidence of attention and falls only when the
selection changes. Nothing is ever fetched twice because the user hesitated.

**Prefix.** Depth 1's data is a strict prefix of depth 2's. Pressing `i` after a
peek appends; it never re-fetches or invalidates what the peek already showed, so
the card opens instantly with the cover already in it and fills in the rest.

## Dwell is the signal, not a debounce

The temptation is to treat the rest-delay as a rate limiter bolted onto a
feature. It is the other way round: **the thing that tells you the user wants
more is the same thing that protects the server.**

A user scrolling through 400 rows is not interested in any of them, and they
generate zero requests - not because a debounce swallowed them, but because
nobody asked. There is no separate throttle to tune, and no state where the
throttle and the intent disagree.

Consequence worth stating plainly: **cursor movement costs no network by
construction.** That is the performance constraint discharged at the design
level rather than defended with a cache.

## Where the data comes from, cheapest first

**1. Already in the row.** The queue/browse rows are parsed from MPD responses
that already carry title, artist, album, duration. Depth 0 and part of depth 1
need no source at all.

**2. The offline store's sidecars - local, no network.** Each mirrored song has
`<store>/<id>.toml` holding an embedded full `Song` table (see
`offline-audio-store.md`). The songs a user lingers on are overwhelmingly the
ones they starred, which are exactly the ones the mirror holds. So for the common
case, depth 1 and much of depth 2 are a disk read of a small TOML file.

This synergy is real and worth building for rather than stumbling into: the
offline mirror is already a local metadata cache, and nothing currently reads it
as one.

**3. Navidrome, for what only it knows.** `getSong` carries the user-specific
half - `playCount`, `played`, `starred`, `userRating` - which is generated
server-side and cannot come off disk. `getAlbumInfo2` and `getArtistInfo2` carry
the written notes. These are depth 2 only.

The daemon should expose this as one verb rather than the client learning
Subsonic: `info <uri>` answering flat pairs, in the shape `store` and `heard`
already use. `subsonic.rs` stays the only file touching wire types - a one-file
blast radius is an existing invariant, not a preference.

## Concurrency: a dedicated socket, for the reason already established

Info requests must not ride the command socket. The `Find` variant in `worker.rs`
documents exactly why, and every word of it applies here: the command socket is
strictly FIFO, so a slow info fetch head-of-line-blocks every following `Refresh`
and freezes now-playing with no UI signal; and a 5s timeout there is read as a
transport drop, so a slow query prints "connection lost".

So: an **info worker** with its own connection, beside the existing art and find
workers. Same shape, same reasons, no new argument needed.

The art worker is reused as-is for depth 1. It already exists, already decodes,
already echoes its key.

## Staleness: echo the key, drop the late

Also already solved here. `Inbound::Art` carries the key back so the render
thread can reject a response for a since-changed selection, and responses are
tagged with the connection epoch. The info reply does the same: it carries the
uri it answers for, and the render thread drops it unless it still matches the
current selection.

**No cancellation.** A superseded request is not cancelled; it completes and its
result is either cached (the user may come back) or dropped by the key check.
Cancellation across a socket is where this kind of system usually rots, and the
reconciler shape means it is never needed.

## Caching

A small bounded map, keyed by uri, holding depth-1 and depth-2 payloads.

Hand-rolled rather than a dependency: no `lru` crate is in the workspace today
(it appears only transitively), and this is roughly thirty lines against a new
supply-chain edge. Bound it by entry count, not bytes, for everything except
covers - covers are already decoded images and should stay in the art path, which
owns that memory.

The point of the cache is not the server round trip. It is that walking back up a
list you just walked down must be instant, because that is what a person actually
does when comparing two songs.

## The card

`i` opens it; `i` or `Esc` closes it. The indicator is `ⓘ` (U+24D8), shown on the
selected row when depth 2 is available for it - so the affordance is discovered
by resting, which is the same gesture the system is already built around.

Contents, in the order a person asks:

1. Title, artist, album, year, track
2. What the user has done with it: play count, last played, starred, rating
3. What the file is: format, bitrate, sample rate, size, duration
4. Whether it is offline (this is the mirror's own answer, and free)
5. The written notes, if any

Width: it is a panel, not the bottom bar, so it is not under the 129-column
constraint that governs the badge (`offline-store-user-surface.md`). It should
still degrade by dropping whole rows rather than truncating values.

## Rejected

- **Fetch on selection change.** The obvious implementation, and it turns a
  held arrow key into a request storm. Dwell removes the storm rather than
  absorbing it.
- **Prefetch neighbours of the cursor.** Tempting and wrong at this size: it
  triples traffic to save a delay the user already accepted by resting. Revisit
  only with evidence.
- **A background sweep populating the cache.** That is the offline store, which
  exists. Do not build a second one.
- **Teaching the client Subsonic directly.** Breaks the one-file blast radius.

## Open questions

- The rest delay. 350ms is a guess; it wants trying, and it may differ between a
  cover peek and the `ⓘ` affordance appearing.
- Whether depth 1 should peek the cover in the row's own space or in the existing
  art pane, which currently belongs to now-playing. Showing the selection there
  means the pane stops meaning "what is playing", which is a real cost.
- Whether `info` should answer for a uri the daemon has never seen (a browse row
  for an unmirrored song) with a live lookup or an honest "not known yet".
