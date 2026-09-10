# Implementation plan: the hint and info system

Status: active
Created: 2026-09-10
Updated: 2026-09-10

## Context

The sequenced plan for `attention-and-detail.md`, produced by reading the real code
rather than from the design alone. Steps are ordered so each one ships on its own and
leaves the app working.

**Smallest slice that delivers value:** Steps 1-3: the `i` detail card rendered entirely from pairs the client already receives once push_song_tags stops dropping them. One daemon emission fix, one extracted scroll helper, one heard-shaped overlay + KEYMAP row. Zero new sockets, zero new fetches, zero dwell machinery - and the user already gets title/artist/album/year, play count, last played, starred, rating, format/bitrate/size, and offline residency for any row. Everything after it (dwell, verb, worker, peek) layers on without reworking it.

## Steps

### 1. Make push_song_tags whole + per-song X-Offline (daemon emission fix, no new verb)

Extend push_song_tags (handler.rs:14929) with the fields already in the in-memory Song and never emitted, following its emit-only-when-Some rule: X-Rating (user_rating), Composer, Performer, X-Size, X-Suffix, X-Type (content_type), X-Added (created), X-CoverArt (cover_art - this is what later lets a selected row be peeked). Add X-Offline: 1 via an INDEX-ONLY residency check on the store (add e.g. Store::resident(&SongId) -> bool consulting the index without a stat; keep the full lookup() verdict for the card path) so a 400-row listing does not become 400 stats. hypodj-core is a shared crate: whole-workspace build+test per CLAUDE.md.

**Proof it landed:** Against the live daemon: printf 'playlistinfo\nclose\n' | nc 127.0.0.1 6600 shows X-Rating, X-CoverArt and X-Offline: 1 on a mirrored, rated song and omits them where absent. New unit tests beside push_song_tags_emits_x_starred_only_when_starred (handler.rs:18831) assert the never-a-fabricated-zero rule for each new pair.

Files: `crates/hypodj-core/src/handler.rs`, `crates/hypodj-core/src/store.rs`

### 2. Extract ScrollBox from the help/heard duplicated scroll machinery (zero behaviour change)

The measure-in-render / clamp-in-keyhandler pattern exists twice, line-for-line (help: ui.rs:400-422 + state.rs:879-906; heard: ui.rs:300-319 + state.rs:911-933). The card would make a third copy, so extract first: a small ScrollBox { scroll: u16, max: Cell<u16> } with scroll_key(code) and a render-side measure helper, then port help and heard onto it. Pure refactor, no visible change.

**Proof it landed:** ui::tests::help_overlay_renders_groups_and_bindings_from_keymap (ui.rs:1617) and help_overlay_fits_and_scrolls_on_a_short_terminal (ui.rs:1639) stay green, and a live frame: open help on a short terminal, hold j past the end, one k immediately scrolls back (no phantom offset).

Files: `crates/hypodj-tui/src/ui.rs`, `crates/hypodj-tui/src/state.rs`

### 3. The i detail card as a fourth overlay, rendering only pairs the client already receives

Clone the heard shape, not a Mode: card: Option<InfoCard> + ScrollBox on TuiState beside heard's fields (state.rs:568-585); open_card() calls take_screen() first (state.rs:837-853); render_info_card builds Vec<Line> through centered_popup with f.area() as region, titled 'Info  i closes'; add state.card.is_some() to the overlay_drawn disjunction (ui.rs:56-64) or it punches holes in a sixel cover; a key_normal intercept (i/Esc/q close, j/k scroll) after the heard block; invalidate the card when a refresh changes queue_len, copying the menu guard (main.rs:629-630). Bind via KEYMAP row Char('i') + Act::Info (keymap.rs:163-212) with an apply_act arm - the exhaustive match forces dispatch. Contents: sections 1-4 of the doc's list (identity, user stats, file facts, offline) straight from row pairs including step 1's X-Rating/X-Offline; degrade by dropping absent rows.

**Proof it landed:** Live TUI (isolated daemon, audio=null) screenshot: i on a queue row opens the card showing play count, rating, offline: yes, format; i closes it; help overlay now lists the i row without any help-code change. keymap::tests::no_two_rows_claim_the_same_matcher and documented_bindings_all_present stay green.

Files: `crates/hypodj-tui/src/keymap.rs`, `crates/hypodj-tui/src/state.rs`, `crates/hypodj-tui/src/ui.rs`, `crates/hypodj-tui/src/main.rs`

### 4. Dwell substrate: derived selection stamp + depth(), the third request_art-shaped reconciler input

Make cursor_target pub(crate) (state.rs:1540). Add sel_key: Option<menu::Target> and sel_since: Option<Instant> beside art_req_key (state.rs:514-519). Add note_selection(state, now: Instant) in main.rs modeled on request_art, called from the reconcile block at main.rs:256 with the already-computed frame_now - after dispatch and apply_inbound so it sees the post-key, post-response cursor; a key-drain burst collapses to one comparison per frame. Take the clock as a parameter (the enqueue_selected_at pattern, state.rs:1458-1464) so tests never sleep. Expose resting_for(now) and a pure depth(sel_key, resting_for, asked) -> 0|1|2 that returns 0 while any overlay is open (menu/help/heard/card) so a peek can never pop under a popup. Do NOT touch POLL and do NOT reuse anim_secs (freezes when paused) or spin_secs. No visible behaviour yet; this step is the doc's core mechanism landing intact.

**Proof it landed:** Named tests with injected Instants: a burst of move_selection calls within one frame yields exactly one stamp; switch_screen and a Find drill enter/exit re-stamp despite writing no index; depth stays 0 under an open menu; depth reaches 1 only at rest_delay with an unchanged sel_key. (These are the derived-not-evented claims made executable.)

Files: `crates/hypodj-tui/src/state.rs`, `crates/hypodj-tui/src/main.rs`

### 5. Daemon `info <uri>` verb: flat pairs, live-first with honest store fallback

MpdCommand::Info(String) beside Heard/Store (mpd.rs:286-303). Resolution copies handler.rs:11895-11915: strip song/; client.song(&id) for the fresh user half (the only network cost); on transient error fall back to store.cached_song(&id) (store.rs:3069) and add X-InfoSource: store; unknown uri answers X-Info: unknown, never invents. Emit via push_song_tags plus the card-only extras (full lookup() offline verdict). subsonic.rs is untouched at this layer - song() already exists. New MpdCommand variant breaks exhaustive matches in other crates: whole-workspace build+test, then nix build .#hypodj (the sandbox doCheck trap).

**Proof it landed:** Live daemon via nc: 'info song/<id>' returns pairs whose X-Plays matches Navidrome's current count (play the track once, re-ask, count moved - proving it is not the frozen sidecar copy); a bogus uri returns X-Info: unknown; with Navidrome stopped, a mirrored id still answers with X-InfoSource: store.

Files: `crates/hypodj-core/src/mpd.rs`, `crates/hypodj-core/src/handler.rs`

### 6. TUI info worker (own socket) + card fills the fresh user half, with the bounded uri cache

An info worker beside the art and find workers with its own connection - the Find variant's head-of-line-blocking rationale applies verbatim, so info never rides the FIFO command socket. Inbound::Info echoes the uri; the render thread drops it unless it still matches the open card (the Inbound::Art staleness pattern). No cancellation: superseded replies are cached or dropped by the key check. Add the doc's hand-rolled bounded map keyed by uri (~30 lines, entry-count bound, no lru crate) so walking back up a list is instant. Card renders store-sourced user stats with a visible staleness label when X-InfoSource: store is present.

**Proof it landed:** Live TUI: open the card while a Refresh is in flight - now-playing keeps ticking (no freeze, no 'connection lost'); play a track, reopen the card, play count is current; close the card, reopen on the same row - fills instantly from the cache with no wire traffic (observe daemon log).

Files: `crates/hypodj-tui/src/worker.rs`, `crates/hypodj-tui/src/main.rs`, `crates/hypodj-tui/src/state.rs`, `crates/hypodj-tui/src/ui.rs`

### 7. Fix the cover fetch amplification (prerequisite the doc assumed away)

Two independent fixes. Client: the art fetch (art.rs:121-168) sends 'binarylimit 1048576' once after the OK MPD greeting (the daemon already accepts it, handler.rs:13461), collapsing ~37 chunk round trips to ~1. Daemon: memoise the cover-id resolution in the albumart chunk loop so one cover is one getSong, not one per chunk (handler.rs:14029-14051 currently calls client.song() on every chunk before the cached cover_bytes lookup). This makes 'depth 1 usually costs nothing' true before anything relies on it.

**Proof it landed:** A handler unit test asserting the chunk loop resolves the song id exactly once per cover; live: daemon tracing shows a single getSong per cover fetch and the fetch of a large cover completing in one binary exchange (log timestamps collapse from ~37 exchanges to 1-2).

Files: `crates/hypodj-tui/src/art.rs`, `crates/hypodj-core/src/handler.rs`

### 8. Depth-1 cover peek in the art pane, as a separate field, plus the rested-row affordance

Add peek: Option<(PeekKey, AlbumArt)> - never write state.art. The per-frame reconciler: depth(..) >= 1 and the selected row is a song/<id> with a cover reference (step 1's X-CoverArt) -> want that cover; diff against held peek key, request through the existing art worker with a generalised key so the adoption gate (main.rs:558-565) can accept peek replies; drop late replies by key. render_current prefers the peek image with pane title 'Preview'; waveform and sigil keep reading state.art (no recolour by construction); one sixel surface only, so sixel_held/sixel_gen are untouched and bug m2zhfld is not widened. Add a small count-bounded decoded-cover LRU (4-8 entries, ~0.9 MB each) in the art path so back-walking is instant. Show the depth-2 affordance on the rested selected row in each list's EXISTING gutter using an ASCII glyph by default (find.rs convention) - queue (ui.rs:2749), browse (ui.rs:2803), Find (find.rs:64) - respecting each column budget. Prefix property holds: pressing i after a peek opens the card with the cover already held.

**Proof it landed:** Live TUI screenshots: rest the cursor 350ms on a NON-playing queue row - the pane shows that row's cover titled 'Preview' and the bottom-bar waveform colour does not change; move the cursor - the pane reverts to now-playing next frame; hold j through 100 rows - daemon log shows zero albumart requests (dwell-as-signal made observable); walk back up to a peeked row - cover appears instantly with no wire traffic.

Files: `crates/hypodj-tui/src/main.rs`, `crates/hypodj-tui/src/state.rs`, `crates/hypodj-tui/src/ui.rs`, `crates/hypodj-tui/src/art.rs`

### 9. Layer 2 (separable): notes, lyrics, and the missing file facts via subsonic.rs wrappers + Song fields

Three thin wrappers in subsonic.rs (the only file allowed to touch wire types): album_info -> get_album_info2, artist_info -> get_artist_info2, lyrics -> get_lyrics_by_song_id, each map_request_error'd and degrading to absent on parse error (the pinned =0.3.0 crate's albumInfo-key parsing can fail against nonconforming servers - never fail the whole info answer). Add artist_id, sampling_rate, bit_depth, channel_count, bpm to Song/map_song - five one-line mappings that complete the doc's 'what the file is' row and land in every future sidecar for free. CRITICAL: every new Song field carries #[serde(default)] or every existing sidecar on disk fails sidecar_from_toml and the whole mirror re-downloads; prove with a pre-change TOML fixture test. The info verb appends X-AlbumNotes etc.; these ride the info worker, never the command socket (they are Last.fm-backed and slow). Cross-crate model change: whole-workspace build+test + nix build.

**Proof it landed:** A fixture test parsing a sidecar TOML written before the field additions (mirror survives the upgrade); live: 'info song/<id>' for an album with Last.fm notes includes X-AlbumNotes, the card renders section 5, and an album without notes simply omits the section; probe bin against live Navidrome shows sampling_rate/bit_depth populated on a FLAC.

Files: `crates/hypodj-core/src/subsonic.rs`, `crates/hypodj-core/src/model.rs`, `crates/hypodj-core/src/handler.rs`, `crates/hypodj-tui/src/ui.rs`

## Decisions still Guilherme's

- The rest delay: 350ms is the doc's own admitted guess, and whether the cover peek and the i affordance share one delay or use two. Wants trying live, not deciding on paper.
- The affordance glyph: ASCII (the find.rs gutter convention argues for it) vs U+24D8 pinned with U+FE0E like the heart. Pure taste; the plan defaults to ASCII.
- The DJ screen: cursor_target returns None for Screen::Dj, so dwell/peek/card never fire there even though the queue renders alongside the chat. Plausibly intended (the ask line owns printable keys), but it is a silent behavioural decision that should be his, not the code's.
- Peek scope: the art pane is global chrome on every screen, so the 'Preview' title-swap happens app-wide. Accept that, or gate the peek to the list screens (Queue/Find/Albums/Playlists) only?
- Whether Layer 2 (step 9) is worth building now at all: getAlbumInfo2/getArtistInfo2 are Last.fm-backed, often empty, and the artist hop needs a model change - versus stopping at step 8 with a complete card minus written notes.
- X-Offline as a per-row pair is visible to every MPD client (ncmpcpp, mpc), not just the card - fine, or should the row path stay lean and residency stay card-only?
