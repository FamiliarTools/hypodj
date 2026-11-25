# dj-gui redesign - sonnet runner

Evidence read: ui.rs (render 47-165, now_playing_h 35, render_tabs 759, render_info_card 445, render_current 860), keymap.rs, main.rs request_art 835-860, pump.rs, w14 frames (offline-album/online 000/002/006 at 80x24 and 129x40, drop-mid-song/blocked last), journeys 01-03, task t:01m867h (arena-decided: journeys are acceptance specs; every row states availability in words; unreachable => header count + one key to show all; no reshuffle under cursor), m:01m8678 (OS owns the room; HypoDJ owns one warm instrument for now-playing). I could not view Guilherme's screenshot; defects 1-4 below are matched to his description and to code.

## 1. Listener's top jobs (ranked)

Evidence: journeys 01-03 are the three jobs the harness already treats as top; keymap groups; empty-state copy; heard/journal history named in the task.
1. Know what is playing, and that it is right (title/artist/album/cover agree; transport word true). Every journey's S0 check is a variant of this ("screen names what is playing", "no stale 'playing'").
2. Pause / skip / volume / star without thinking (keymap: p > < 0 9 s; used from any screen).
3. Put something on: pick an album/song and press Enter (journey 02 min path = 3 steps: F2, j, Enter).
4. Ask for a mood, then undo it (F4 DJ, u / U). The product's differentiator.
5. Know whether I can trust the list: server reachable? on disk vs stream only? (journeys 01, 02, 03; decision b).
6. See what is next / reorder the queue.
7. Look up details (i), heard/tape (t), help (?). Rare, read-only, must never take over the screen.

Implication: jobs 1-2 are constant and glanceable (an instrument that never moves), jobs 3-6 are one working surface, job 7 is transient.

## 2. Defects in the current TUI

1. Info popup swallows the frame. render_info_card is drawn with `f.area()` as region (ui.rs:445, call at ~137), over the now-playing pane and the tabs; art inside it is dithered cell art (ui.rs ~503-515, "Cell rendering, never sixel") capped at ART_ROWS*2 columns, so it reads as a pixelated blob next to the sharp sixel cover. A read-only detail view should not hide the thing it describes.
2. Tab row clips mid-label. render_tabs (ui.rs:759-800) pushes five fixed labels ("[F1]Queue  [F2]Albums  [F3]Playlists  [F4]DJ  [F5]Search" = 55 cols) plus 2-col gaps with no width budget; only the server word is guarded against truncation. Below ~56 cols the Paragraph clips to "[F1]Queue [". Nothing collapses.
3. Narrow, clipped left column. Screen::Dj splits the list region 50/50 (ui.rs:92-96), so at 80 cols the Queue gets ~38 cols, then render_now splits again 52/48 (ui.rs:826-829) so Now Playing gets ~40 minus border and art, leaving ~28 cols for text. Titles clip, and cover art takes 11 columns of that.
4. Cover/title mismatch. request_art deliberately HOLDS the old cover across a track change until the new fetch lands (main.rs:835-860, test comment at ~1384-1388 "the outgoing cover is held"). On a slow or failed fetch, album and title move on while the previous album's cover stays. The invariant "what is drawn belongs to what is named" is traded for no-blink.
5. Vertical budget spent on chrome. now_playing_h = 12 rows at <30 high (ui.rs:35-41); with margins (2), tabs (1), status (1) the list gets 24-16 = 8 rows, minus borders = 6 visible rows (frame 002.80x24: three rows of data, rest empty). At 129x40 now-playing takes 18 rows while the list sits in a mostly empty box (002.129x40). Two stacked bordered boxes plus two blank margin rows.
6. Empty-state Up Next duplicates Now Playing ("F5 to search, F4 to ask the DJ" appears twice, frame 000.80x24), and a whole 12-row pane says "nothing playing".
7. Status bar is a 1-row crowd: spectrum wave + "offline: all 3 songs" + "? help" share one row (frame 000.80x24 last line); status messages borrow the bottom margin (ui.rs:120-130). "offline: all 3 songs" is shown while "server online" in the tab row says the opposite - two connectivity voices in two places.
8. Overlay stack: help, menu, heard, card, confirm are five independent popups with ordering enforced by draw order and an `overlay_drawn` flag whose only job is to stop sixel holes (ui.rs:48-65). Five modals is the structure, not a bug count: each new feature adds one.
9. Navigation is five function-key screens where Queue appears twice (F1, and inside F4). F-keys do not exist on many laptops without Fn; nothing on screen teaches them except one dim line in the empty state.
10. Albums rows hold cursor `#` and a trailing "on disk"/"stream only" in a plain list - the availability fix is correct but it is a suffix, not a column, so it ragged-clips on narrow widths.

## 3. Three structurally different whole-screen shapes

Key constants across all: j/k g/G Enter space o i s u / ? q stay; p > < 0 9 stay global.

### A. "Deck": one persistent now-playing instrument + one body that swaps (subtractive baseline)
Now-playing becomes a 3-row strip pinned at top (cover as a 2-cell-high swatch/sixel thumb, title - artist, album, transport, time). Below it, ONE body view at a time; the tab row is a single hint line. No Up Next pane (it IS the head of the Queue view).
```
 Low Tide - Fixture Band             Held Harbour        > 0:06/2:00 v
 Queue  Albums  Lists  DJ  Find                          server online
 > Low Tide                         Fixture Band                  2:00
   Harbour Lights  on disk          Fixture Band                  3:10
   Moorings        on disk          Fixture Band                  2:41
   Ridge Line      stream only      Fixture Band                  2:30
   (14 more rows ...)
 
 
 
 
 
 
 
 
 
 
 
 
 
 
 
 
 
 : ask   / find   i info   ? help
```
Keys: Tab / Shift-Tab cycle views (F-keys kept as aliases), 1-5 jump. Cost: 3 rows of chrome, 20 rows of list. Cover is tiny at 80x24; at >=120 cols the strip grows left into a 10-row cover block.

### B. "Stage": two panes by role, not by data type (now-playing as the left rail)
Left rail is a fixed 28-col "instrument" (cover on top, title/artist/album, transport, mini-queue of next 3). Right is the working surface with a tab line. At 80x24 the rail is the whole warm object; at <70 cols the rail drops to Deck strip (A) automatically.
```
 Queue  Albums  Lists  DJ  Find           server online
 +----------------------------+----------------------------------------+
 |  [cover 12x6 sixel/cell]   | > Held Harbour          on disk        |
 |                            |   Stream Summit         stream only    |
 |  Low Tide                  |   Smart lists /                        |
 |  Fixture Band              |                                        |
 |  Held Harbour              |                                        |
 |  playing 0:06 / 2:00  v 60 |                                        |
 |  next Harbour Lights       |                                        |
 |  then Moorings             |                                        |
 +----------------------------+----------------------------------------+
 : ask  / find  i info  ? help                      312 of 1,840 shown
```
Keys: as A, plus `h`/Left moves focus to the rail (p/skip/seek local keys). Cost: ~28 cols to rail always, so list gets ~50 cols at 80 wide, clipping long album names. Rail height wastes rows when the list is long.

### C. "Single line": no panes - a command-first spine (DJ-first)
The screen is the DJ conversation or the active list, full width, with one 2-row footer carrying now-playing. `:` / typing starts the ask; lists are reached by short verbs (`:albums`, `:q`) or Tab. Closest to what djc already does; bets that job 4 is the product.
```
 ask> play something calmer
 DJ: queued 8 songs (undo: u)
 > Low Tide - Fixture Band                          0:06  on disk
   Harbour Lights - Fixture Band                    3:10  on disk
   ...
 
 
 --------------------------------------------------------------------
 > Low Tide - Fixture Band  Held Harbour   0:06/2:00  60%  [server online]
 p pause  > skip  : ask  Tab view  ? help
```
Keys: typing a letter on the empty prompt asks; list actions need Tab into list focus. Cost: hides the cover and spectrum (no warm instrument, violates m:01m8678 intent); browsing is less discoverable.

### D (considered, rejected): modal "now-playing screen" vs "library screen" like phone players (full-screen player toggled by Enter/Esc). Fails job 2/3 together (cannot skip while browsing without switching) and re-creates the Info-popup problem as a whole screen.

## 4. Pick: A (Deck), with B's rail as the wide-terminal variant (>= 110 cols), not a separate mode

Why: it directly targets the complaint. The card/tab/column defects (1-3, 5) all come from fixed-size chrome fighting for 24 rows and 80 cols; Deck removes the chrome rather than rearranging it, and the now-playing strip keeps the cover and the title in ONE widget so mismatch (4) is fixable in one place. It keeps the warm instrument the GUI task wants (a permanent cover + title), yet gives the list 20 rows instead of 6. B is chosen only where width pays for it (list rows are short; 80-col album names are not).

What it removes: the bordered Now Playing and Up Next boxes (Up Next = queue tail, already in Queue view); the blank top and bottom margins; the 50/50 DJ split (DJ becomes a full-width view with queue available by Tab); the full-frame Info popup (info becomes a bottom drawer, 6 rows, anchored below the list so cover and transport stay visible; Esc closes); the duplicate empty-state copy; the per-popup sixel-hole bookkeeping where the cover lives in the fixed strip no overlay covers. The five-popup stack reduces to: drawer (info/heard/journal/help share one drawer slot), confirm (inline in the status row), menu (kept as a small popup anchored at the cursor row).

Specific rules:
- Tab line is budgeted: labels degrade full -> "Queue Albums Lists DJ Find" -> first letters "Q A L D F"; the server word is never dropped for a tab (connectivity is job 5). Never clip a label mid-word.
- Cover is keyed to the song it was fetched for. Draw the cover ONLY when `art.key == now.file`; otherwise draw a neutral swatch (album color) instead of the old cover. Blink is accepted over a lie; the neutral swatch keeps layout steady.
- One connectivity voice: the tab-line word ("server online / unreachable / no network"), plus the header count from decision b ("312 of 1,840 shown") on the list; the footer stops repeating it.
- Rows state availability as a right-aligned column, not a suffix.
- Navigation: Tab/Shift-Tab + 1-5; F1-F5 stay as aliases.

Cost named: the cover is small at 80x24 (about 6x3 cells); big cover/spectrum only at >= 110 cols (rail). Info is shorter (6 rows) so long Last.fm bios scroll.

## 5. Falsifiable acceptance checks (dj-gui --pump, journeys TOML, frames at 80x24 and 129x40)

Add as `[[check]]` entries; each is a screen_require / screen_forbid or a step budget over the existing frame dumps.
1. List yield: in offline-album at 80x24, step 2 (Albums), the number of rows between the tab line and the footer that are list rows is >= 16 (today 6). Frame regex: count lines from first data row to footer.
2. No clipped tab: for widths 80 and 60 (add a pump size 60x20), line 2 matches `Queue.*Albums.*Lists.*DJ.*Find|Q A L D F` and contains no trailing `[F\d]` unclosed bracket; line 1 or 2 contains `server (online|unreachable|no network)`.
3. Cover truth: new journey 04 plays track 1 then skips (`key >`) with the cover endpoint blackholed (the blocked state). Check: after the skip, the frame contains the new title AND the old-cover marker is absent (the swatch/placeholder glyph appears instead). Fail on current build because the old cover is held (main.rs:835-860).
4. Info keeps the instrument: journey 05 `key i` on a playing row; frame must still contain the playing title and the transport word (`playing`) in rows 1-3, and info text (e.g. `Album`) in the lower rows. Fails today (card region is f.area()).
5. Step budgets unchanged: journeys 01-03 still pass at their existing min_steps/step_budget (connected 1, offline-album 3 of 6, drop-mid-song 4); Tab view switching adds no required step (F2 alias still works: script `key F2`).
6. Single connectivity voice: with state=blocked, grep over the frame: `unreachable` appears exactly once outside the list header count; the string `offline: all` is absent from the footer.
7. Daemon-truth parity at both sizes: the transport word on screen equals `truth.json status.state` (existing check, must stay PASS in drop-mid-song blocked).
8. 129x40 is not emptier: the 129x40 frame of Queue with 3 songs shows now-playing at <= 10 rows (today 18) and no more than 20% of non-border lines empty.
All eight run headless; 3 and 4 need two small new journeys and one extra pump frame size (60x20).
