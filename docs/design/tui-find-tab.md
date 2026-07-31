# LibSearch: a first-class Find tab for the HypoDJ TUI

Final design, amended after the eighteen-attack pass and the grounding audit. Every file:line below was re-read in the working tree; where the original synthesis or an attack was factually wrong, that is called out in section 8 rather than quietly fixed.

---

## 1. What this is for, in three sentences

The TUI can browse the newest 100 albums and jump the cursor within rows already loaded, but it has no way to ask the library a question, so an artist like C418 is simply unreachable from the interface. This adds a fifth screen, reached with F5, where you type a query on a prompt line, press Enter, and get back a single ranked list of matching songs and albums (and, once the daemon catches up, artists) that you can enqueue, play, star, and drill into with the keys you already use on the Albums tab. It ships in two stages so that stage one works on the daemon Guilherme is running right now, without a rebuild and without a degraded mode to explain.

---

## 2. The screen

### Naming

The screen is `Screen::Find`, its module is `crates/hypodj-tui/src/find.rs`, its prompt is `find> `, and its tab label is `[F5]Find`. It is deliberately **not** called Search: `Mode::Search`, `Group::Search`, `Act::SearchStart/SearchNext/SearchPrev`, `run_search`, `search_jump`, `search_step`, `search_origin`, `last_search` and `highlight_query` are all already taken by vim's `/` cursor jump (state.rs:207, keymap.rs:84, keymap.rs:166-168, state.rs:982-1014). Two things called "search" on one screen would be unreadable in the code and confusing on screen. "Find" is short, distinct, and true. (Whether the tab strip should nonetheless read `[F5]Search` for the user while the code says Find is section 10.)

### Layout, and the arithmetic that drove it

`ui::render` (ui.rs:23-31) is a fixed six-row vertical layout:

```
Length(1)   blank top margin
Length(1)   tab strip
Min(3)      the screen band          <- the only region a screen owns
Length(12)  Now Playing
Length(1)   command / search / status / ambient wave
Length(1)   blank bottom margin
```

In ratatui 0.29 a `Length` constraint outranks a `Min`, so the band is exactly `height - 16`. At the 60x24 the repo's own headless harness hardcodes (ui.rs:898-911), that is **8 rows**. The Find screen splits it:

```rust
Layout::vertical([Constraint::Length(1), Constraint::Min(2)]).split(list_area)
  [0]  the query line, borderless
  [1]  the bordered results list
```

So at 60x24 the results list has 8 - 1 - 2 = **5 content rows**, against the Albums tab's 6. That one-row difference is the whole price of the prompt line, and it is acceptable.

What is **not** acceptable, and what the fatal attack correctly killed, is spending 3 of those 5 rows on mandatory chrome. The original design had a header row per kind (`ARTISTS 3`, `ALBUMS 12`, `SONGS 200+`) and rendered a header even for a kind with zero hits. At 60x24 that leaves two real rows. **Section header rows are gone.**

### How the three kinds share one flat list

One `Vec<FindRow>`, one cursor, one `ListState`, one `scroll_offset` call. Kind is carried two ways, neither of which costs a row:

1. **A three-column gutter** on every row: `<kind sigil><queue mark><space>`. Kind sigils are ASCII, matching the existing rationale for `#`/`~` (state.rs:186-193): `@` artist, `=` album, blank song. The queue mark column is the existing `queue_mark_glyph(album_mark(..))` output (`#` full, `~` partial, blank none), unchanged.
2. **The block title carries the tallies**, including a kind that returned nothing.

Rows are ordered artists, then albums, then songs. Because the cursor starts at row 0 and headers no longer exist, a long song list at the bottom cannot bury the artists at the top, which was the only thing per-kind display caps were protecting against. **The caps and the `+ N more` expander rows are therefore also gone.** The list is exactly as long as the server's answer (at most 20 + 50 + 200 = 270 rows, shorter than a fully expanded album drill). That deletion removes five row kinds down to three, removes the expand verb, and removes the entire "what does `s` do on a `+N more` row" question.

### Mockup, results landed (78 columns; `>` marks the REVERSED cursor row)

```
 [F1]Queue  [F2]Albums  [F3]Playlists  [F4]DJ  [F5]Find
 find> c418                                        tab: results   ^v: history
+- 3 artists / 12 albums / 200 songs (server cap) ---------------------------+
|>@  C418                                                        12 albums   |
| @  C418 & Kubbi                                                 1 album    |
| @  Daniel Rosenfeld                                             4 albums   |
| =# Minecraft - Volume Alpha       C418   2011                  24 tracks   |
| =~ Minecraft - Volume Beta        C418   2013                  30 tracks   |
| =  148                            C418   2020                  12 tracks   |
| =  0x10c                          C418   2012                   8 tracks   |
|  # Sweden                         C418   Volume Alpha              3:03    |
|    Wet Hands                      C418   Volume Alpha              1:30    |
+----------------------------------------------------------------------------+
+- Now Playing --- (12 rows, unchanged) -------------------------------------+
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~ ? help
```

A row is `gutter(3) + label + right-aligned trailer`. The trailer (`12 albums` / `C418  2011  24 tracks` / `C418  <album>  3:03`) is precomputed at parse time so the renderer stays a dumb column fitter that truncates the label first. The query is underlined in every matching label via the existing `match_spans` (ui.rs:176-201) plus one new fallback in `highlight_query` (see section 3).

### Drilled into an artist

```
 find> c418                                             esc / h: back to hits
+- C418 -------------------------------------------------------------------+
|# Minecraft - Volume Alpha/                                    24 tracks   |
|~ Minecraft - Volume Beta/                                     30 tracks   |
```

The drill is the ordinary `render_browse` over an ordinary `Browse`, indistinguishable from the Albums tab, with the query line kept above it so you never lose the question that produced this branch.

---

## 3. The keyboard interaction model

### Focus, not a new Mode

`Find` owns `focus: Focus::{Query, Results}`. `state.mode` stays `Mode::Normal` throughout. A fifth `Mode` would force a new arm into `render_command`'s hardcoded per-mode caret match (ui.rs:1777-1785) and into `handle_key` (state.rs:646-651), for no gain. The precedent is `Screen::Dj`, which owns `dj_input`, intercepts `key_normal` before the keymap (state.rs:687-689), and draws its own caret with `f.set_cursor_position` from inside `render_dj` (ui.rs:1652-1657) while mode is Normal. Find copies that shape exactly, with one added line beside the Dj intercept:

```rust
if self.screen == Screen::Find { return self.key_find(key); }
```

`key_find` swallows keys only while `focus == Query`. In `Results` it falls straight through to `keymap::match_key`, so every global binding behaves precisely as on the other four screens.

### New KEYMAP rows (three)

These go in the single-source table, get help-overlay coverage for free (ui.rs:69 renders from `keymap::grouped()`), and are compiler-forced into `apply_act`'s exhaustive match (state.rs:708-789).

| Key | New `Act` | What that key does today | Conflict check |
|---|---|---|---|
| `F5` | `ScreenFind` | Nothing. `match_key` returns `None` for F5; KEYMAP binds only `Code(KeyCode::F(1..=4))` (keymap.rs:146-149). | `Code` matchers ignore all modifiers (keymap.rs:71), so there is no shift/alt variant to collide with. Digits 1-8 are also free, but `0` and `9` are volume (keymap.rs:163-164), so a digit tab-switcher would be inconsistent. `documented_bindings_all_present` (keymap.rs:283-289) loops `for f in 1..=4u8` and must be bumped to `1..=5` in the same commit. |
| `Tab` | `FocusToggle` | Nothing. No `Code(KeyCode::Tab)` matcher exists anywhere in KEYMAP. | Not consumed by any modal: the help overlay handles only `?`/Esc/`q`/j/k/arrows/PageUp/PageDown/Space and swallows the rest (state.rs:657-683); `Mode::Confirm` handles only y/Y/n/N/Esc (state.rs:1154-1160); the three text inputs handle only Char/Backspace/Enter/Esc. In Query focus the screen intercept consumes Tab before `match_key`, so this row only ever fires in Results focus. |
| `}` and `{` | `KindNext` / `KindPrev` | Nothing. Neither bracket appears in any matcher. | `KeyMatch::Char` requires `!CONTROL` (keymap.rs:66-73). Free per the confirmed free set. Vim-adjacent: paragraph motion, which is what a kind jump is. |

**`i` is dropped** from the original design. Tab returning focus to the query line makes a second focus-the-query key redundant, and five keys pointing at one outcome was itself a finding.

### Existing bindings that gain a `Screen::Find` arm

Every one of these is an existing key. Nothing about them changes on Queue, Albums, Playlists or Dj.

- **`j` `k` `Down` `Up` `C-n` `C-p`** move the cursor. There are no non-selectable rows any more, so this is a plain clamped move.
- **`g` / `G`** first / last row.
- **`Enter`** (`Act::PlaySel` -> `enter_action`, state.rs:868-885): **always plays**, exactly as its own doc comment at state.rs:864-866 promises ("Enter always PLAYS the selection ... Drilling-in moved to `o`"). Song row -> `Intent::Enqueue { play: true }`. Album row -> `Intent::Enqueue { uri: "album/<id>", play: true }`, the existing atomic whole-album push (worker.rs:482-513, handler.rs:7036-7055). Artist row -> sets `status_msg = "o opens this artist"` and does not move. That last case is feedback, not a silent no-op, and it keeps Enter's meaning identical on every screen.
- **`o`** (`Act::Open`) drills: artist -> `lsinfo artist/<id>`, album -> `lsinfo album/<id>`. No-op on a song row, same rule as Albums today.
- **`Space`** (`Act::Enqueue` -> `enqueue_selected`, state.rs:917-930) appends without playing then advances one row. Album and song enqueue. On an artist row it posts the same hint and does not advance. Note `enqueue_selected`'s body currently *starts* with `match self.active_browse()` and returns `None` when that is `None`, so Find needs an explicit early arm or Space silently does nothing.
- **`s`** (`Act::FavSelected` -> `favorite_selected`, state.rs:899-910) stars the cursor row. Today that function reads `self.queue` only and silently no-ops off Queue, so it needs a Find arm reading the row's uri. No daemon change is needed for any of the three kinds: `playlistadd Starred <uri>` routes song, album and artist through `Favorite::from_uri`, whose comment states "The uri PREFIX is the sole routing authority" (handler.rs:8067-8086). Starring an artist works only because Find's artist rows carry a real `artist/<id>`.
- **`h` / `Left` / `Esc`** (`Act::BrowseBack`) pops the drill; see the Esc ladder below.
- **`p` `>` `<` `C-f` `C-b` `0` `9` `C-s` `:` `?` `q`** unchanged.

### Query focus: the text line

Editing is deliberately identical to the three existing input surfaces (state.rs:1037-1104, 1169-1249): `Char(c)` pushes, `Backspace` pops, the caret is pinned at end-of-string computed from `chars().count()`. No left/right, no `C-w`, no `C-u`. A fourth divergent editing dialect is worse than four consistent minimal ones; a shared `TextInput` refactor of all four is a separate change with its own value. This inherits the existing wart verbatim: these handlers match on `key.code` only, so `Ctrl+c` inserts a literal `c`, exactly as it does in Command, Search and Dj today.

Carve-outs, resolved before the intercept swallows anything, mirroring `key_dj` (state.rs:1178-1190):

- **F1 through F5** switch screens outright. An F-key is never part of a query.
- **`?` opens help when, and only when, the query buffer is empty.** This is copied verbatim from `key_dj`'s `Act::HelpToggle if self.dj_input.is_empty()` guard, and it matters because `render_command` draws the dim ` ? help` hint whenever mode is Normal and `status_msg` is None (ui.rs:1799-1832), which is exactly the cold Find state. Advertising a key on screen while the focused surface eats it is the worst kind of small lie.

Then:

- **`Enter`** on a non-empty query submits: `Intent::Find(query)`, push to history, `phase = Loading{query}`, focus moves to Results. On an empty query it just moves focus to Results.
- **`Tab`** moves focus to Results without submitting.
- **`Up` / `Down`** walk a session-local query history: a ring capped at 20, newest first, de-duped on push, with the half-typed line stashed at slot 0 so `Down` returns it. The hint row shows `history 2/7` while you are off slot 0, so nothing appears to vanish without explanation. This is the whole answer to "how do I get back to a previous query": the query line **is** the history, there is no recall UI. In memory only; nothing is written to disk. (This is the one attack I only partially accepted; see section 8 and section 10.)
- **`Esc`** with a buffer that differs from the submitted query reverts the buffer. With an unmodified buffer it **leaves to `Screen::Queue`**, exactly as `key_dj` does at state.rs:1193-1197.
- Everything else is literal text. `/`, `:` and `?`-mid-phrase all type themselves, which is what someone entering a title expects.

### The Esc ladder

Every Esc makes progress; it never toggles.

```
drill open   -> Esc pops the drill back to the hits   (local, no round trip)
hits, no drill -> Esc moves focus to the query line
query line, buffer unmodified -> Esc leaves to Screen::Queue
```

`Tab` is the one focus **toggle**, so the gesture that got you into the results also gets you back out. Section jumping lives on `}` / `{`.

### F5 is idempotent

`switch_screen` (state.rs:792-796) unconditionally calls `self.last_search.clear()`. Pressing F5 while already on Find would therefore silently wipe a standing `/` query and its `n`/`N` target. Fix, and it is an improvement for every screen: guard `switch_screen` with an early `if self.screen == screen { return None; }`. F2 while on Albums becomes genuinely idempotent too, and a standing `/` query survives a redundant tab press. F5 has exactly one meaning: switch to Find.

### `/` `n` `N` are unchanged, and now finally mean the right thing

They stay vim's cursor jump over what is on screen. `find` locates the **set**; `/` navigates **within** it. The two never look alike: the library query lives on a prompt row at the top of the pane and is only editable in Query focus, while `/` lives in the thin bottom bar with its own caret.

Three helpers gain a Find arm, and each must branch on `drilling`, or `/` inside a drill would scan the frozen hits behind it and move an invisible cursor:

- `active_labels` (state.rs:945-960): `if drilling { drill row labels } else { hit row labels }`.
- `active_cursor` (state.rs:963-969) and `set_active_cursor` (state.rs:972-978): same branch.

`highlight_query` (state.rs:1008-1014) gains one fallback: on `Screen::Find` in `Mode::Normal` with an empty `last_search`, return the **submitted** query. So every result row underlines the question that produced it, until a `/` query takes over.

---

## 4. Data flow

### Stage one wire command (works on the currently deployed daemon)

```
search any "<query with \ and \" escaped>"
```

`nl::quote_arg` is already `pub` in `crates/hypodj-client/src/nl.rs:25-31`, escapes `\` then `"` to match the daemon tokenizer (mpd.rs:967), and is already used by the TUI at main.rs:408 and worker.rs:501. Nothing in `hypodj-client` needs to change. (The synthesis's proposal to make `grounding::mpd_escape` public was already correctly dropped.)

Response is a flat, relevance-ordered song list, terminated by a bare `OK`. Per song, in emission order (`browse_song_pairs` handler.rs:9488, `push_song_tags` handler.rs:9498-9545):

```
file: song/<id>
Title: <title>
X-Starred: 1              (only when starred; never a "0" line)
Artist: <artist>
Album: <album>
X-AlbumUri: album/<id>    (the only id-bearing album handle on the row)
Track / Disc / Date / Genre / MUSICBRAINZ_TRACKID / Comment / Format / Time / duration
```

**Songs come straight off `file:`. Album rows are derived from `X-AlbumUri`**, grouped by album id, labelled from `Album` plus `Artist`, trailer `(N matching)`. They carry `song_count: None`, which is exactly right: `album_mark` (state.rs:175-183) documents that an unknown count "degrades to Partial for any queued track, never a false Full", so a derived album row shows `~` when any of its matched tracks are queued and blank otherwise, and can never claim a false `#`.

Stage one has **no artist rows**. There is no `X-ArtistUri` on the wire, so a derived artist would be a display string that cannot drill, cannot be starred, and would merge two same-named artists. Shipping a fake artist row is worse than shipping none.

The stage-one block title reads `12 albums / 47 songs` with no cap claim, because `search any` gives the client no information about server caps.

### Stage two wire command: `searchall`

Non-standard, in the same house style as `knob` / `nl` / `identify` / `continuation`, and deliberately **not** a change to `search`/`find` (which must stay song-only so ncmpcpp keeps working). Not added to the `commands` advertisement at handler.rs:8332-8344 (`knob` and `identify` are absent from it too). `ADVERTISED_MPD_VERSION` untouched.

Grammar, chosen so a future window argument cannot be mistaken for query text:

```
searchall <query>            -- exactly ONE positional argument, quoted if multi-word
```

More than one bare positional argument is an ACK (`ACK [2@0] {searchall} too many arguments`), not a silent join. That leaves room for `searchall "c418" window 0:50` later without a breaking arity change, and means a newer client can never silently search for its own arguments.

Three daemon edits:

1. `crates/hypodj-core/src/mpd.rs:148-166` add `MpdCommand::SearchAll(String)`; `mpd.rs:1123-1127` add the `"searchall"` parse arm with the arity check.
2. `handler.rs:8210-8214` dispatch to `self.search_all(q).await`.
3. `search_all`, beside `search_filtered`: empty query -> `MpdResponse::ok()`; otherwise **one** `self.client.search3(&q)` call (subsonic.rs:553, fixed caps artist 20 / album 50 / song 200), then **the same `tag_matches` post-filter that `collect_matches_capped` applies at handler.rs:9152**, with filters `[("any", q)]`. That last clause is not optional: without it `searchall q` and `search any q` return materially different song sets (the `any` arm at handler.rs:9336-9347 spans exactly ten modeled fields, and Navidrome's full-text index is broader), and the stage-one to stage-two upgrade would silently change the answer. With it, `searchall q` is a bounded prefix of `search any q` and the upgrade is purely additive. Results are query-specific and ephemeral: **never cached**, same rule as handler.rs:9042.

It is explicitly **not** `collect_matches_capped`. That loop is up to 25 sequential Subsonic round trips before a single byte comes back (handler.rs:9059-9130, `PAGE = 200`, break only on `(got as i32) < PAGE || fresh == 0`).

Wire shape, a count preamble then artist, album and song blocks, discriminated by uri prefix:

```
X-Hits: artist 3 20
X-Hits: album 12 50
X-Hits: song 200 200
directory: artist/<id>
Artist: C418
X-AlbumCount: 12
directory: album/<id>
Album: Minecraft - Volume Alpha
Artist: C418
Date: 2011
X-SongCount: 24
file: song/<id>
Title: Sweden
... push_song_tags verbatim, including X-AlbumUri ...
OK
```

The artist and album blocks are byte-identical in shape to what `lsinfo_root` (handler.rs:8801-8817) and `lsinfo artist/<id>` (handler.rs:8587-8596) already emit. Only `X-Hits` and `X-AlbumCount` are new, and `X-SongCount` / `X-AlbumUri` / `X-Starred` already establish the `X-` convention.

`X-Hits: <kind> <returned> <cap>` is load-bearing because the protocol otherwise makes an empty result and an empty filter byte-identical (both are a bare `OK`, mpd.rs:2040-2050): it is the only way a zero-hit kind can be stated explicitly. `returned == cap` is rendered as **`(server cap)`**, not `200+`. `search3` is called with a fixed `song_count = Some(200)` and no over-request (subsonic.rs:556), so hitting the cap is not evidence that more exist. Saying "the server's cap was reached" is true; saying "200+" is a guess. Requesting cap+1 would be more informative but would require a new `subsonic.rs` entry point, and is not worth it for a suffix.

### Client plumbing

- New `Intent::Find(String)` -> `dispatch` (main.rs:275-326) -> `Req::Find { query }`. It is not a mutation, so it must **not** set `sent_mutation` and must not append the trailing `request_refresh` (main.rs:328-330). `coalesce_intents` also needs an arm.
- New `RespKind::Find { query, result: Result<FindHits, FindError> }` folded in `apply_resp` (main.rs:498-552). One variant carrying a `Result`, so an ACK is **targeted at the screen** rather than collapsing into the targetless `RespKind::Banner` that `Req::Browse` produces (worker.rs:531-534) and that the very next keypress wipes (state.rs:644).

### The dedicated find socket, and why

**The search does not run on the FIFO command socket.** A sixth worker owns a short-lived socket per query, in exactly the shape that already exists twice in this file: the art worker opens a dedicated short-lived socket per fetch (art.rs:74-121), and the CC worker opens a throwaway socket for grounding (worker.rs:296-306). It is a `Receiver<String>` plus `MpdConn::connect` plus one `conn.command`, posting `Inbound::Resp` onto the same merged channel. It owns no state.

This is a change from the synthesis, and it is the single most important amendment. It resolves three findings at once:

1. **The command socket is strictly FIFO and serial** (worker.rs:336-370). A search on it head-of-line-blocks every following `Req`, including `Refresh`. Worse, `request_refresh` sets `refresh_in_flight = true` at **send** time and only clears it when a `Snapshot` lands (main.rs:336-357), so the 5s `REFRESH_SAFETY` tick can only set `refresh_dirty` and cannot rescue a search-blocked refresh. Now-playing and the progress bar would freeze for the whole search with no UI signal. On its own socket, none of that happens.
2. **`IO_TIMEOUT` is 5s** (hypodj-client/src/mpd.rs:15) and the daemon writes the whole frame only after `handle` returns, so 5s bounds total server-side latency. On the command socket a timeout returns `Err(_)`, which `worker.rs:535` reads as a transport drop and turns into a reconnect: pressing Enter in a search box would produce `connection lost - reconnecting...`. On the find socket, a timeout kills only that socket. The worker reports `search timed out - try a narrower query` into the pane and reconnects lazily on the next query. Playback and the queue are untouched.
3. It is what makes **stage one shippable**. Stage one's `search any "<q>"` routes into the 25-page `collect_matches_capped` path, which for a broad query ("a", "the", "love") against a large library will keep paging because `search3` keeps returning full 200-song pages. That is unbounded and it is exactly the hazard that got Design 3's bulk-enqueue key cut. Isolated on its own socket with an honest, actionable timeout message, it is a slow query, not a broken connection.

Enqueue, star, load and drill all still go through the **command** socket, because they are mutations or cached lookups that must stay ordered with everything else. Only the read-only find query moves.

### Staleness: query echo, no generation counter

There is no request cancellation anywhere in this codebase, and the connection `epoch` (state.rs:385-387) drops responses only across a reconnect, not across a superseded query. `apply_resp` folds a `RespKind::Find` **only if its echoed `query` equals the in-flight `phase = Loading{query}`**. Same guarantee as a monotonic generation counter, zero new plumbing, and the degenerate case (the same query submitted twice) is harmless because the two answers are identical.

The find worker processes queries one at a time. Submitting three in a row waits for all three; only the last one folds. That is honest and, with one `search3` call in stage two, unnoticeable.

### The drill, and its own staleness gate

The drill is a **separate** `Browse` (`Find.drill`), layered over the frozen hits, not a reuse of the result rows. That makes `browse_for(Screen::Find)` return it and the entire existing `browse_into` / `Req::Browse` / `Browse::apply` / `render_browse` path work unchanged.

Two wrinkles, both real:

- `browse_into` (main.rs:396-413) pushes `(cur_path, cur_sel)` onto the stack, and at drill depth 0 that path is `""`, which `browse_back` would re-fetch as `lsinfo ""` (the whole artist root). So `Find.drilling: bool`: entering from a result row sets it, resets the drill `Browse` with an **empty** stack, and pushes nothing. `browse_back` with `drilling && stack.is_empty()` clears `drilling` locally and sends **nothing**. Backing out to the hits is instant, costs no round trip, and preserves the query by construction because the hits were never overwritten.
- `Req::Browse` carries no request identity, and `Browse::apply` resets `selected` and `offset` and latches `loaded` (state.rs:281-288). So the drill fold at main.rs:542-551 must be **gated on `drilling`**: a drill response that lands after the user has already backed out is dropped, rather than left as stale rows waiting to flash under the next drill's title.
- `Find.drill_loading: bool` is set on `BrowseInto` and cleared on the fold, and drives the same spinner in the drill title. Without it the drill renders an empty bordered box with the artist's name for the whole round trip, which is the one screen state the original inventory omitted.

### `parse_browse` is neither reused nor modified for hits

`parse_browse` (state.rs:100-154) stays exactly as it is. Its `"Artist"` arm fires only when the last row is **not** a dir (state.rs:145-152), so a `directory: artist/<id>` plus `Artist: <name>` pair would render the raw Subsonic id as the label. It also silently drops `X-AlbumUri`, `Date` and anything it does not know. Changing that arm would also change what the Albums tab does with any future dir row, for no gain here.

It **is** still used, unchanged, for the **drill**, where `lsinfo artist/<id>` emits `directory` + `Album` + `X-SongCount` and never `Artist` (handler.rs:8587-8596), so the latent bug is never reached.

The hit parser is new, pure, and TUI-local, in `find.rs`.

### Per-screen helpers: six explicit arms, all gated on `drilling`

This is the least interesting code in the change and the likeliest place to ship a silent bug, because most of these are **not** compiler-forced. `move_selection` (state.rs:1072-1076), `go_top` (819-829) and `go_bottom` (832-842) all route through `active_browse()` with a **queue fallback**, so if `active_browse()` returns `None` for Find they will silently move `self.selected`, the Queue cursor, while the results list sits still. Gating `active_browse` alone is necessary but not sufficient.

Specify all six with an explicit Find arm placed **before** the `active_browse` consultation:

| fn | line | Find behaviour |
|---|---|---|
| `active_browse` | state.rs:810-816 | `Some(&mut find.drill)` only while `drilling`, else `None` |
| `browse_for` | state.rs:801-807 | `Some(&mut find.drill)` only while `drilling`, else `None` (this is the drill staleness gate) |
| `move_selection` | state.rs:1072-1076 | `drilling` -> drill; else the hit cursor. Never `self.selected` |
| `go_top` / `go_bottom` | state.rs:819-842 | same branch |
| `active_cursor` / `set_active_cursor` | state.rs:963-978 | same branch |
| `enter_action` | state.rs:868-885 | compiler-forced; per-kind play semantics above |
| `enqueue_selected` | state.rs:917-930 | explicit Find arm before the `match self.active_browse()` |
| `open_selected` | state.rs:934-942 | explicit Find arm before `let b = self.active_browse()?` |
| `browse_back` | state.rs:888-893 | the Esc ladder |
| `favorite_selected` | state.rs:899-910 | Find arm reading the cursor row's uri |
| `active_labels` | state.rs:945-960 | same `drilling` branch |
| `mark_disconnected` | state.rs:630-632 | add `find.drill.loaded = false`; also fall `phase` back from `Loading` to `Done` so the spinner stops |

### Silent-drift sites the compiler will not catch

`render_tabs` (ui.rs:245-265) is a hardcoded four-element **array**, not a match. A missed edit ships a working tab that is invisible in the tab strip. Also: `ui.rs:54` (confirm popup suppression), `ui.rs:365` (hint ownership), `state.rs:596` (`enter_confirm` inline vs popup), `main.rs:287`, `main.rs:297`, `main.rs:518`.

---

## 5. Every state, and what the user sees

| State | What renders |
|---|---|
| **Cold**, first F5, nothing typed | Empty bordered box titled `find`, focus on the query line showing `find> ` with a live caret, one dim centred hint: `type a query and press enter    ^v recall`. **No request has been sent**: `show_screen` (main.rs:361-391) does nothing for this screen, so entering the tab is instant and never touches a socket. |
| **Typing over previous results** | The query grows after `find> ` with a real caret; the hint row reads `enter: search   ^v: history`. Previous results stay below, rendered DIM, with the old query appended to the block title, so you can see what you are about to replace and can still act on it. Nothing is sent per keystroke: typing returns no Intent at all. |
| **Walking history** | The hint row shows `history 3/7` so a replaced half-typed line is visibly recoverable rather than apparently lost. |
| **First query in flight** | Block title becomes `(spin) searching "c418"`, riding the free-running `spin_secs` (main.rs:136-144), which advances every frame regardless of playback state, so it animates on a stopped deck. The box is otherwise empty. Every playback key still responds and now-playing keeps updating, because the search is on its own socket. |
| **Refine in flight** | Old rows stay, DIM; the title carries the spinner plus the new query. The stale answer stays legible, visibly stale, and still actionable. |
| **Results landed** | Title `3 artists / 12 albums / 200 songs (server cap)`. One flat list, artists then albums then songs, kind sigil and queue mark in the gutter, the submitted query underlined in every matching label, cursor on row 0. Focus is on Results. |
| **A kind returned nothing** | Stated in the **title**, never as a row: `no artists / 12 albums / 47 songs`. Absence is the answer to a question the user asked, and it costs zero rows. |
| **No results at all** | One dim centred line `no results for "kalabrezze"` plus `tab to edit, ^ for the last query`. The query is echoed verbatim because the protocol cannot distinguish an empty result from an empty filter; the client knows what it sent, so it says so. Visually distinct from the cold state. |
| **Server ACK** (Navidrome unreachable) | Inside the pane, not the transient bottom bar: `search failed: <ack reason>` on a dim line, previous results left intact underneath, spinner stopped. |
| **Search timed out** | `search timed out - try a narrower query`, in the pane. The find socket reconnects on the next query. The command socket, playback and now-playing are untouched. |
| **Stage one, artists not available** | Nothing to explain. The title says `12 albums / 47 songs`; there is no artist section and no dim apology, because stage one never claims one. |
| **Connection dropped mid-search** | The existing `connection lost - reconnecting...` banner. The hits **stay** (they are a truthful snapshot and re-running is one Enter away) and `phase` falls back from `Loading` to `Done` so the spinner stops rather than spinning forever. |
| **Drill in flight** | Query line stays; drill title reads `(spin) C418` with the same spinner. |
| **Drilled in** | Query line stays, showing the query that produced this branch, with `esc / h: back to hits` on the right. Below it the standard `render_browse` list with queue markers, titled with the artist or album name. Indistinguishable from the Albums tab. Backing out is instant with the original cursor restored, no refetch. |

---

## 6. Implementation order

Each step ends somewhere green, shippable, and independently useful.

**Step 1: the screen skeleton, no network.**
Add `Screen::Find` and `Act::ScreenFind` plus the F5 KEYMAP row; bump `documented_bindings_all_present` to `1..=5`. Add `find.rs` with `Find` (query buffer, submitted query, focus, hits, cursor, `offset: Cell<usize>`, history ring, phase) and the pure `FindRow`/`FindHits` types. Add the render dispatch arm, `render_find`, the `[F5]Find` tab-strip entry, and all twelve per-screen helper arms from section 4, each returning empty/None for now. Add the `switch_screen` idempotence guard. Green: F5 opens an empty screen you can type into and Esc out of; every other screen behaves exactly as before. **Gate: whole-workspace build and test.**

**Step 2: stage-one search, songs plus derived albums, on the deployed daemon.**
Add the find worker with its short-lived socket, `Req::Find`, `Intent::Find`, `RespKind::Find`, the query-echo staleness gate, and the pure `parse_song_rows -> FindHits` in `find.rs` (songs from `file:`, albums grouped from `X-AlbumUri`). Wire Enter, Space, `s`, `o`, and the loading / empty / no-results / ACK / timeout states. **This is the first shippable feature and it works against the running daemon on 6600 with no rebuild.** Live-prove it there.

**Step 3: the drill.**
`Find.drill` as a second `Browse`, `drilling` and `drill_loading`, the `browse_into` empty-stack special case, the gated fold, the Esc ladder, and the `drilling` branch in `active_labels` / `active_cursor` / `set_active_cursor` / `move_selection` / `go_top` / `go_bottom`. Green: `o` on an album drills into its tracks and `h` returns instantly.

**Step 4: `}` `{`, `Tab`, history, polish.**
The two remaining KEYMAP rows, the history ring with its visible indicator, the `highlight_query` fallback, the `?`-on-empty-buffer carve-out, `mark_disconnected`. Green: the interaction model of section 3 is complete for two kinds.

**Step 5: `searchall` on the daemon.**
`MpdCommand::SearchAll` with the arity check, the dispatch arm, `search_all` with the single `search3` call plus the `tag_matches` post-filter, and the `X-Hits` preamble. Ends green as a daemon-only change with `mpd.rs` parse tests and a `search_all` pair-shape test; nothing in the TUI consumes it yet. **Gate: whole workspace, plus `nix build .#hypodj`.**

**Step 6: artists in the TUI.**
Teach the find parser the `directory: artist/` and `directory: album/` blocks and `X-Hits`, switch the find worker's command line to `searchall`, add the artist `@` sigil, the `(server cap)` suffix, and the artist Enter hint. Album hits now include title-only matches. **Gate: whole workspace, both nix builds, the live proof against an isolated daemon, and the live proof against 6600 confirming stage one still works if Guilherme has not switched.**

There is deliberately no `searchall` fallback path in the client. Steps 2 through 4 use `search`; step 6 switches to `searchall` and lands only after the daemon side is merged. A client that has switched will not run against a daemon that has not, because both come from the same flake input and the same `nixos-rebuild switch`. That removes the entire degraded-mode branch, the `unknown command` retry, and the "quietly less than it claims" failure mode that the original design named as its own weakest point.

---

## 7. Test strategy

### The headless render path already exists and is already used

No new harness is needed. `crates/hypodj-tui/src/ui.rs` has, in its `#[cfg(test)]` module, `render_to_lines(state)` over a `TestBackend::new(60, 24)` (ui.rs:898-911), `render_to_lines_sized(state, w, h)` (ui.rs:935-944), and `render_fg_colors(state, w, h)` (ui.rs:915-933). Existing assertions such as `help_overlay_renders_groups_and_bindings_from_keymap` already drive them. GUI work here is fully checkable without a terminal.

**Every primary-state render assertion must pass at `render_to_lines`'s default 60x24**, not only at a comfortable size. That is the direct guard against the layout finding: a design that needs 40 rows to show its main state will fail its own test.

Render assertions, each from a hand-constructed `TuiState` plus `Find`:

- the tab strip contains `[F5]Find` (the guard against `render_tabs`' hardcoded array)
- the cold hint renders and is distinguishable from the no-results state
- in-flight renders `searching` **and** still shows the previous rows
- a three-kind result set renders artists, then albums, then songs, with the right gutter sigils, **at 60x24**, with at least three real hit rows visible
- an artist row renders its **name**, not its raw Subsonic id (the exact bug `parse_browse` would have produced)
- a kind with zero hits renders `no artists` in the title and contributes zero rows
- `no results for "..."` echoes the query
- `search failed: ...` and `search timed out ...` render in the pane
- the drill renders as a plain browse list, and renders its spinner while `drill_loading`
- at 60x20 nothing panics and the query line is still visible
- the caret column equals `chars().count()` for a multibyte query (the `+5`-style caret math at ui.rs:1654 is exactly where an off-by-one lives)

### Pure unit tests in `find.rs`

No IO, no clock. Parse over literal `Vec<(String, String)>` matching the exact wire bytes:

- **stage one**: song rows to `FindRow`s; albums grouped by `X-AlbumUri` with the right `(N matching)` trailer; `song_count: None` on derived album rows so `album_mark` can never claim Full; a song with no `X-AlbumUri` (a stream) produces no album row and does not panic
- **stage two**: three kinds separated by uri prefix; `X-Hits` cap detection (`song 200 200` capped, `artist 3 20` not); `X-AlbumCount` and `X-SongCount` landing on the right rows; `X-AlbumUri` surviving on song rows so the queue gutter works; a zero-hit kind arriving as an explicit empty section
- the history ring: cap 20, dedupe, stash and restore of a half-typed line, and that walking past the end is a clamp not a wrap

### Fold and focus tests

`apply_resp` is pure, so staleness is provable without threads: drive it with `RespKind::Find { query: "old" }` while `phase == Loading{"new"}` and assert the hits are untouched, then land `"new"` and assert it folds. Same for the drill fold being dropped while `!drilling`, and for `mark_disconnected` clearing `drill.loaded` and stopping the spinner while keeping the hits.

Focus machine, via `TuiState::handle_key` with synthetic `KeyEvent`s:

- typing in Query focus produces **no** `Intent` at all (this is the test that proves there is no per-keystroke request path)
- `j` in Query inserts a literal `j`; `j` in Results moves the hit cursor and **not** `self.selected` (the guard against the queue-cursor fallthrough)
- `j`, `g`, `G` and Space with `drilling == true` move the drill cursor, and with `drilling == false` move the hit cursor, at both ends of both lists
- `/` inside a drill moves the **drill** cursor
- Enter submits from Query, plays from Results; Enter on an artist row returns **no** `Intent::Enqueue` and sets a status (the test that prevents shipping a user-visible `unsupported uri: artist/<id>` ACK, handler.rs:7095)
- Esc reverts a modified buffer, then leaves to `Screen::Queue`
- `?` on an empty query buffer opens help; `?` mid-query types a literal `?`
- F1 through F5 switch screens from Query focus
- F5 while already on Find does not clear `last_search`
- Tab toggles focus both ways; `}` wraps over a kind with zero hits

### Keymap tests carry themselves

`every_matcher_round_trips_to_its_own_act` (keymap.rs:226-244) covers the three new `Act`s automatically and would catch a shadowing duplicate; the exhaustive `apply_act` (state.rs:708-789) means they cannot compile without dispatch. `documented_bindings_all_present` (keymap.rs:276-291) hardcodes F1-F4 and a char set and **must** be extended; treat its failure as the intended reminder, not an obstacle.

### Daemon side

`mpd.rs` tests beside the existing ones at mpd.rs:1288-1313: `searchall c418` and `searchall "volume alpha"` parse to the query; `searchall volume alpha` ACKs on arity. Handler side, a `search_all` pair-shape test using

```rust
let Some((h, _)) = handler_with_null_player() else { return };
```

and **never** `.unwrap()` on that `Option`: `nix/package.nix` runs `doCheck` with `-p hypodj-core` in a certless, network-less sandbox where the helper returns `None`, and an unwrap fails the Nix build while the devshell stays green.

One test that only exists because of the post-filter decision: assert that `searchall q`'s song set is a **subsequence** of `search any q`'s, so the stage-one to stage-two upgrade is provably additive rather than a different answer.

### Live proof, the sanctioned gate

No unit test can prove `search3` actually populates the artist and album blocks against Navidrome.

- Spin an isolated daemon on an alt port from a copy of `/run/user/1000/hypodj/config.toml` with **`HYPODJ_AUDIO=null` and MPRIS off** so it stays silent, and tear it down in the same motion.
- Send `searchall c418` over a raw socket; assert the frame carries `X-Hits: artist`, at least one `directory: artist/`, at least one `directory: album/`, at least one `file: song/`, and terminates `OK`.
- `lsinfo artist/<id>` on a returned id to prove the drill target resolves; `add album/<id>` to prove the enqueue target resolves.
- **Time it.** Record the wall-clock latency of `searchall c418` and of a deliberately broad query. This is the measurement the whole submit-on-Enter decision rests on and it has never been taken. Treat anything over 500ms as the trigger to revisit as-you-type or a debounce, and record the number in Memoria rather than in a file.
- Run `dj-gui` against that daemon for F5 / type / Enter / Tab / `}` / `o` / `h`, which no automated test covers.
- **Then run `dj-gui` against the currently deployed daemon on 6600** and confirm stage one still works there.

### Merge gates, per CLAUDE.md

`nix develop --command cargo build -j4 --workspace` and `cargo test -j4 --workspace` (whole workspace, not `-p`, because `MpdCommand` lives in a shared crate), then `nix build .#hypodj` and `.#hypodj-clients`, then the live proof, then every confirmed critical or high review finding resolved and re-verified. No merge-then-discover.

---

## 8. What the attacks changed, and what was a false positive

### Changed, because the attack was right

1. **Section header rows are gone** (fatal, attack 1). The arithmetic is unarguable: `Length` outranks `Min` in ratatui 0.29, the band is `height - 16`, and at the harness's own 60x24 that leaves 5 content rows, of which the original design spent 3 on mandatory chrome. Kind now lives in a one-character gutter sigil and the tallies live in the block title, at zero row cost. Cascade: with ordering alone preventing songs from burying artists, the per-kind display caps and the `+ N more` expander rows lost their justification and were also deleted, which in turn removed five row kinds down to three and dissolved attack 11 entirely.
2. **Enter is play again, everywhere** (attack 2). state.rs:864-866 states the invariant in the source: "Enter always PLAYS the selection ... Drilling-in moved to `o`." I took the attack's option (b): Enter on an artist row posts `o opens this artist` rather than drilling, and `o` stays the single drill verb across Albums and Find. I did **not** take option (a) (teach `enqueue_uri` the `artist/` prefix), because fanning a whole discography would be an N+1 daemon operation with an unbounded queue push, and because (b) makes stage one and stage two behave identically, so nothing shifts under the user when the daemon upgrades.
3. **`?` opens help on an empty query line** (attack 3), copying `key_dj`'s guard at state.rs:1181-1188 verbatim. `render_command` draws the ` ? help` hint whenever mode is Normal and `status_msg` is None (ui.rs:1799-1832), which is exactly the cold Find state, so the original design advertised a key the focused surface would have eaten.
4. **Six per-screen helpers get explicit Find arms, all gated on `drilling`** (attack 4, plus the audit's deepening). The attack's own fix was insufficient: with `active_browse()` returning `None` off-drill, `move_selection`, `go_top` and `go_bottom` fall through to their **queue** branch and silently move the wrong cursor, and none of those three is compiler-forced. That is a default-wrong behaviour in three functions, not an off-by-one.
5. **Tab has exactly one meaning: toggle focus** (attack 5). Section jumping moved to `}` / `{`, both free. `i` was dropped as redundant, removing one of five paths to the same outcome.
6. **The KEYMAP claim was withdrawn** (attack 6). `match_key` returns the first matching row and `every_matcher_round_trips_to_its_own_act` asserts each matcher resolves to its own `Act`, so a second `Code(KeyCode::Up)` row either fails the test or shadows `Act::Up` globally. Resolution: the three tab-level keys (F5, Tab, `}`/`{`) **are** KEYMAP rows with full help coverage; the Query-focus text-line keys are **not**, exactly as the Dj screen's `ask>` line is not, and the design no longer claims they appear in `?`. They are documented in the screen's own hint row.
7. **Esc leaves the screen** (attack 7), matching `key_dj` at state.rs:1193-1197. The ladder is drill, then hits, then query line, then Queue, and each press makes progress instead of ping-ponging.
8. **F5 is idempotent** (attack 9), via an early-return guard in `switch_screen` that also stops F2-while-on-Albums from silently wiping a standing `/` query.
9. **`/` inside the drill is specified** (attack 10): all three of `active_labels`, `active_cursor` and `set_active_cursor` branch on `drilling`, with a test.
10. **The find query runs on a dedicated socket** (attacks 12 and 18). This is the largest amendment and it resolves three things at once: the head-of-line block of `Refresh` (worsened by `request_refresh` arming its gate at send time, main.rs:336-345, so the 5s safety net can only mark it dirty), the 5s `IO_TIMEOUT` being misread as a transport drop (worker.rs:535) and bouncing the connection, and the unbounded 25-page path in stage one. The synthesis rejected a dedicated socket as too much machinery, but that judgement was formed against Design 2's *as-you-type* proposal with its debounce accumulator and generation counter. A dedicated socket **without** debouncing is one thread, one `Receiver<String>`, one `conn.command`, in exactly the shape the art worker (art.rs:74-121) and the CC grounding socket (worker.rs:296-306) already use twice.
11. **`search_all` gets the `tag_matches` post-filter** (attack 13), one `.filter()` line, so `searchall q` is a bounded prefix of `search any q` rather than a different answer, and the stage-one to stage-two upgrade is provably additive.
12. **`searchall` rejects extra positional arguments** (attack 14). The `args.join(" ")` grammar would have permanently foreclosed a positional window or offset, which matters because `opensubsonic`'s `search3` already accepts `artist_offset` and `album_offset` that `subsonic.rs:556` declines to pass, and because the A-Z album index work is about to grow exactly that vocabulary.
13. **Two stages, and no fallback branch** (attack 15, the most valuable one). `push_song_tags` already emits `X-AlbumUri: album/<id>` on every song row (handler.rs:9515-9517), `add album/<id>` is the atomic whole-album push, `lsinfo album/<id>` drills, and `Favorite::from_uri` stars albums. So songs plus derived albums ships against the daemon Guilherme is running **right now**. The original design presented a false binary between deriving all three kinds client-side and adding a wire verb for all three, and as a consequence made its headline feature unusable on the only machine that matters until a human-gated switch. Splitting the merge removes the degraded mode, the `unknown command` retry, and the dim "needs a newer daemon" line that the design's own weakest-point section admitted it did not trust to carry the UX.
14. **`(server cap)` replaces `200+`** (attack 16). `search3` is called with a fixed `song_count = Some(200)` and no over-request, so `returned == cap` is not evidence that more exist. Requesting cap+1 would be more informative but would touch `subsonic.rs`, contradicting the zero-change claim; saying "the cap was reached" is true and free.
15. **`drill_loading` added and the drill fold gated on `drilling`** (attack 17). The eleven-state inventory had no drill-in-flight state, and `Req::Browse` carries no request identity while `Browse::apply` latches `loaded`.

### False positives, and why

1. **Attack 1's 20-row numbers are wrong**, though the half that carries the finding is right. It claims that at h=20 Find shows zero result rows while Albums shows one. `Layout::vertical([Length(1), Min(2)])` over a 4-row area splits 1/3, and a bordered block of height 3 has inner height 1, so Find shows **one** row and Albums shows **two**. Off by one in both. The 60x24 arithmetic (5 content rows, 3 of them mandatory chrome) is exactly right and is why the finding is fatal.
2. **Attack 6's parenthetical is unsupported.** It asserts that `match_key`'s unused `_screen` argument "was left there for exactly this". The doc comment (keymap.rs:185-187) says only that it is "accepted for scope gating; screen-specific no-ops stay guarded in the dispatch bodies". Nothing suggests it was reserved for focus contexts. The finding stands on the round-trip test alone.
3. **Attack 14's skew scenario is misframed.** It describes an old daemon receiving `searchall c418 200 0` and searching for the literal text. An old daemon does not know `searchall` at all and ACKs `unknown command`. The real skew is a newer client against a daemon that knows `searchall`-without-window. The substance, that `args.join(" ")` permanently forecloses a positional argument, is correct and I acted on it.
4. **Attack 8 is only partially accepted.** It argues Up and Down in Query focus should move the results cursor (the fzf gesture) with history on Ctrl-P/Ctrl-N. I kept **history on the arrows**: because the screen is submit-driven rather than as-you-type, the type-then-arrow-into-results gesture does not arise here (the equivalent gesture is type-then-Enter, which already lands the cursor on row 0), and moving history to Ctrl chords would require modifier filtering that none of the four text surfaces does today. The real concern behind the attack, that typed text appears to vanish, is answered by the slot-0 stash plus a visible `history 3/7` indicator. This is flagged in section 10 as genuinely arguable.

### Factual corrections to the inputs

- **`Screen` is TUI-local, not cross-crate.** The search-plumbing ground truth claims "adding a `Screen` variant is a cross-crate edit ... referenced in `hypodj-client`-adjacent worker plumbing: worker.rs:113". `worker.rs` is `crates/hypodj-tui/src/worker.rs`. A grep for `Screen::` returns five files, all under `crates/hypodj-tui`. The tui-shell ground truth is the correct one.
- **The `MpdCommand` blast radius is one file.** The synthesis's risk 6 called `MpdCommand::SearchAll` "the exact class of change that broke the dj-gui build once". `MpdCommand` is exhaustively matched only in `handler.rs`, inside `hypodj-core`; `echo.rs:316` and `hypodj-nl/src/lib.rs:479` match it non-exhaustively in tests; `hypodj-client` has no `hypodj-core` dependency at all and neither `hypodj-tui` nor `hypodj-cli` imports it. The whole-workspace gate remains correct policy, but the dj-gui breakage was a `Screen`/`Action`-class enum, not this one.
- **The brief's "search3 returns artists, albums AND songs over the MPD `search` command"** is half true in the way that matters: `search3` does return all three (subsonic.rs:553-563, `SearchHits` at subsonic.rs:770-776), but `collect_matches_capped` reads `hits.songs` at handler.rs:9098-9100 and discards the other two on every one of up to 25 pages, and `search_filtered` emits only `browse_song_pairs`. Nothing in the workspace reads `.artists` or `.albums`.

---

## 9. Relationship to the tracked A-Z album index work

Three tasks are already in Memoria under `roadmap:tui-browse`:

- **jc3qee6** (pending, high, phase 1): give the server a real A-Z album index. `album_list` (subsonic.rs:230) hardcodes `offset: None`; its sibling `album_list_by_genre` (subsonic.rs:247) already takes one. Add the offset, then a browse path `albums/all` paging `AlphabeticalByName` at 500/page into `dir_cache`, with a deliberate decision about the unbounded growth the old 100-cap was hiding. Its note records it as blocked in practice until the offline-store work merges, because it touches `subsonic.rs:230` and `handler.rs:8650-8663` and a workflow is mid-flight on `handler.rs`.
- **b8i6jky** (blocked, phase 2): re-seed `Screen::Albums` from `albums/all` instead of `list/newest` (state.rs:534), keeping the smart lists one keystroke away. Depends on jc3qee6.
- **cmh0b01** (pending, phase 3): add a real library search verb distinct from the `/` cursor jump. **This design is cmh0b01.**

### What lands first

**Steps 1 through 4 of this design are unblocked right now and should land first.** They touch `crates/hypodj-tui` only, with zero edits to `subsonic.rs` or `handler.rs`, so they do not contend with the in-flight offline-store work that is blocking jc3qee6. They also deliver the thing that prompted all three tasks, which is that C418 becomes reachable from the interface.

**jc3qee6 then lands, then b8i6jky, then step 5 and step 6 of this design.** Both jc3qee6 and step 5 touch `handler.rs` and `subsonic.rs`, so they should be sequenced rather than run in parallel, and jc3qee6 is the higher-priority one. There is a small shared piece worth noticing: jc3qee6's "add an offset parameter to `album_list`" and `searchall`'s deferred window argument are the same windowing idiom. Doing jc3qee6 first establishes the house style, and `searchall`'s grammar is deliberately left open so it can adopt it.

### Is anything made redundant

**No, and cmh0b01 should be updated rather than closed by this document alone.** The three tasks answer genuinely different questions:

- jc3qee6 answers "show me my library, all of it, in order" (browsing, no query, alphabetical, cacheable).
- This design answers "find the thing I am thinking of" (query, ranked, ephemeral, never cached).

Neither substitutes for the other, and the diagnosis handed in was explicit that a real A-Z index is tracked separately and out of scope here. The one honest overlap is that once a real A-Z index exists, the `/` cursor jump over a fully loaded album list also finds C418, which reduces (but does not remove) the pressure for Find. That is an argument for shipping Find first, not for dropping either.

Two amendments to record in Memoria when this is picked up: cmh0b01's title still describes the pre-analysis plan (`:search <q>` or `S` as a verb, "renders artists/albums/songs as a results browse list") and should be updated to point at this two-stage design with the F5 tab and the `searchall` split. And jc3qee6's "DECIDE DELIBERATELY" note about unbounded `dir_cache` growth now has a sibling precedent: `searchall` results are never cached at all, which is the right rule for query-specific data and the wrong rule for an index.

---

## 10. Open questions that are genuinely Guilherme's

1. **The name.** The code says `Screen::Find`, `find.rs`, `find> ` prompt, `[F5]Find` tab, because `Mode::Search`, `Group::Search`, `Act::Search*`, `run_search`, `search_jump`, `search_origin`, `last_search` and `highlight_query` are all already the `/` cursor jump and two things called "search" would be genuinely unreadable. But you asked for a **search** tab. The tab strip could say `[F5]Search` while the code says Find. My call is that the on-screen word should match the code word so the help overlay and the source agree, but the label is a one-line change either way.

2. **Arrows in the query line: history or results cursor.** I kept Up and Down on query history with a visible `history 3/7` indicator, on the reasoning that a submit-driven screen never has the type-then-arrow-into-results gesture. The counter-argument is real: arrows mean cursor movement everywhere else in this app, and every incremental search UI you have used trains type-then-down. The alternative costs modifier filtering in one handler (breaking the key.code-only convention that all four text surfaces share) to put history on Ctrl-P/Ctrl-N. Genuinely arguable, and one keystroke of daily use will settle it faster than any argument.

3. **Whether stage two is worth its own merge at all.** Stage one gives you songs and albums and works today. Stage two adds artist rows (drillable, starrable, real ids) and albums that match on their **title** rather than only through a matching song, at the cost of a new wire verb with no versioning and a lockstep daemon-plus-client deploy. If "find C418, drill into his albums" is the actual daily use, stage two is the point of the whole thing. If "find a song I half-remember" is the actual daily use, stage one may be the entire feature.

4. **Whether Enter on an artist should enqueue the whole discography.** I made it post `o opens this artist` instead, to keep Enter's meaning identical everywhere and to avoid teaching `enqueue_uri` an N+1 unbounded fan-out. If your reflex is "Enter means play, and playing an artist means play everything", that is a defensible different call and it is a contained change to `enqueue_uri` (handler.rs:7029) plus one arm in `enter_action`.

5. **The latency number, which nobody has.** The entire submit-on-Enter decision rests on "one `search3` round trip is fast enough that Enter feels instant", and that has never been measured against your Navidrome. Step 6's live proof times it. If it comes back at 800ms, Enter-to-search will feel heavy in a TUI you live in daily, and the answer would be a debounce on the find socket, which is now cheap precisely because that socket is dedicated. Let the number decide, not this document.

6. **Query history dies with the process.** A deliberate minimal-footprint call, and the first thing a daily user asks for. Persisting it means a state file in `$XDG_STATE_HOME`, which is a new kind of artifact this TUI does not have today.