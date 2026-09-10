# Attention and detail: the hint and info system

Status: active
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

With one caveat the draft got wrong: a peek is not cheap *yet*. One cover fetch
is currently ~37 client round trips (the 8 KiB default `binary_limit`,
`handler.rs:1256`) and ~37 uncached `getSong` calls, because the daemon
re-resolves the cover id on every chunk (`handler.rs:14038`). Both are fixed
independently - the client sends `binarylimit 1048576` once after the greeting,
the daemon memoises the resolution - and **that fix is a prerequisite, not a
follow-up**.

## Where the data comes from, cheapest first

**1. Already in the row.** The queue/browse rows are parsed from MPD responses
that already carry title, artist, album, duration. Depth 0 and part of depth 1
need no source at all.

**2. The daemon already knows more than it says.** This is the biggest correction
to the original draft. `map_song` (`subsonic.rs`) already keeps `play_count`,
`played`, `user_rating`, `comment`, `size`, `suffix`, `content_type`, `created`,
`composer` and `performer` - and `push_song_tags` (`handler.rs:14929`) simply
never emits about a third of them. **The card is first an emission fix, not a
fetch feature.**

**3. The offline store's sidecars - local, but only for the file half.** Each
mirrored song has `<store>/<id>.toml` with an embedded `Song`. It is written
solely by `commit()` (`store.rs:3167`) and no reconciler ever refreshes it, so
`play_count` / `played` / `starred` / `user_rating` read from a sidecar are
**frozen at mirror time**. File metadata is permanently good; user metadata is a
labelled fallback only, or the card shows a play count that has not moved in
months.

**4. Navidrome, for the FRESH user half.** `getSong` is the only current source
for a play count that has moved since the mirror ran. `getAlbumInfo2` and `getArtistInfo2` carry
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

The art *worker* is reusable for depth 1; its *adoption gate* is not. The
`Inbound::Art` arm accepts a reply only when the key equals `art_want(&state.now)`
(`main.rs:558-565`), which structurally cannot admit a peek. And the peek must
never be written into `state.art`: `request_art` is edge-triggered, so
now-playing would stay wrong until the next track change, and the palette
consumers (the waveform at `ui.rs:3150`, the sigil at `main.rs:785`) would
recolour the whole UI on every rest. The peek is a **separate field**, and the
pane swaps its title to "Preview"; the palette keeps reading `state.art`, so
there is no recolour by construction.

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

`i` opens it; `i` or `Esc` closes it. It is **an overlay field, never a fifth
`Mode`** - `state.rs:548-553` already litigated exactly this for the context
menu, and mutual exclusion holds by construction through `take_screen()`.

Three obligations the codebase has already paid for once, all easy to omit:

- It must join the `overlay_drawn` disjunction (`ui.rs:56-64`) or it punches a
  permanent silhouette out of a sixel cover. That bug has been fixed here before.
- A card pinned to a queue row needs the menu's `queue_len` invalidation guard
  (`main.rs:629-630`), or it keeps describing a row that moved.
- The scroll machinery already exists twice line-for-line (help and heard). The
  card would be the third copy, so extract a `ScrollBox` first and port the other
  two onto it.

The affordance glyph should be **ASCII**, not `ⓘ`. The gutter sigils are
deliberately ASCII (`find.rs:64-77`), the one non-ASCII glyph in that path needed
a U+FE0E variation selector to stop it corrupting the border (`ui.rs:707-718`),
and U+24D8 is East-Asian-Ambiguous width. This reverses the original draft.

Contents, in the order a person asks:

1. Title, artist, album, year, track
2. What the user has done with it: play count, last played, starred, rating
3. What the file is: format, bitrate, size, duration
4. Whether it is offline - and this needs an **index-only** residency check, or a
   400-row listing becomes 400 stats
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

Answered by reading the code (2026-09-10):

- **Dwell needs no timer, tick or poll change.** The loop already free-runs at
  <=50ms (`event::poll(POLL)`, `main.rs:46/213`) with an unconditional draw, so
  dwell is two fields stamped once per frame beside `request_art`. Do not reuse
  `anim_secs` - it freezes when paused. Selection is written in ~15 places across
  three files, which makes the derived-not-evented thesis stronger than argued.
- **The peek goes in the existing pane, as a separate field**, with the title
  swapped to "Preview". See the concurrency section for why never `state.art`.
- **`info` on an unseen uri does a live lookup first**, falls back to the sidecar
  with an explicit `X-InfoSource: store`, and answers `X-Info: unknown` rather
  than inventing.

Still Guilherme's call:

- The rest delay. 350ms is a guess, and whether the peek and the `i` affordance
  share one delay or need two. Wants trying live, not deciding on paper.
- The DJ screen: `cursor_target` returns `None` there, so dwell, peek and card
  would silently never fire even though the queue renders alongside the chat.
  Plausibly right, but it should be a decision rather than an accident.
- Peek scope: the art pane is global chrome on every screen, so the "Preview"
  title swap happens app-wide. Accept, or gate the peek to list screens only?
- `X-Offline` as a per-row pair is visible to every MPD client (ncmpcpp, mpc),
  not only the card. Fine, or should residency stay card-only?
- Whether the notes/lyrics layer is worth building at all: `getAlbumInfo2` and
  `getArtistInfo2` are Last.fm-backed and often empty, and artist notes need a
  model change because `Song` carries no `artist_id`.
