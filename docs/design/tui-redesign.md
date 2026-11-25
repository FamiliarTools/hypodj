# dj-gui redesign

Status: proposed, 2026-11-25. Design only; no code changed. Implementation waits
for agents/w16-loose and agents/w16-npcard (both edit crates/hypodj-tui) to land.

## The experience it must give (experience first)

HypoDJ is the furnace under the bath (tokens.md). The person at the TUI is mostly
NOT using it: it sits in a pane while they work or lounge. So the screen is judged
by three moments, in order of frequency:

1. **Glance** (most of the time): what is playing, what comes next, is anything
   held for me (timer, armed plan), am I offline. Readable in one look, at 80x24.
2. **Nudge** (a few times an hour): skip, louder, "more like this", "stop after
   this album". One key or one sentence, no navigation first.
3. **Dig** (rarely): browse albums, search, read a song's detail, read the journal.
   Allowed to take a few keys; must never lose the glance while doing it.

## What the current screen does instead (interrogated, with evidence)

Evidence: the journey harness frames in docs/usability/runs/2026-11-24-w14/
(offline-album/blocked, steps 002 and 006, 80x24 and 129x40), and Guilherme's
2026-11-25 screenshot of the info card.

| # | Observation | Evidence | Breaks |
|---|---|---|---|
| 1 | Space is split by fixed percentages, not content. At 129x40 a 1-album list takes 17 rows and "nothing playing / nothing queued" take 2x16 empty rows. | 129x40 step 002: 31 of 36 body rows blank | Glance: the eye hunts |
| 2 | The queue is shown twice: the Queue tab and the Up Next panel. | both visible on F1 | Subtract |
| 3 | Five F-key tabs (Queue, Albums, Playlists, DJ, Search). Asking the DJ is a tab, though a sentence works from the command line anywhere. | tab row in every frame | Nudge needs navigation |
| 4 | Empty states teach navigation ("F5 to search, F4 to ask the DJ") rather than the act ("type what you want"). | 129x40 step 002 | Nudge |
| 5 | Status is scattered: server state at the right end of the tab row, offline badge at the right end of the bottom bar, timers inside the now-playing card. | 80x24 step 006 | Glance |
| 6 | Implementation leaks into rows: `# ` marker and trailing `/` on album names. | `# Held Harbour/` | Words over syntax |
| 7 | Detail is a full-frame popup that covers the glance and the tab row; at narrow widths it truncates the frames behind it into fragments. | screenshot 2026-11-25 (`[F1]Queue [` cut, columns 12 wide) | Dig loses glance |
| 8 | Four modes (Normal, Command, Search, Confirm) plus two overlays plus the card, each with its own close key (`t`, `U`, `i`, Esc). | state.rs Mode, Panel | Learnability |

## The design space (three whole shapes)

### A. Stage and one line

The now-playing instrument owns the screen; everything else is summoned.

```
 Low Tide                                         server online
 Fixture Band - Held Harbour        track 1 of 3   0:04 / 2:00
 ▄▃▃▂▂▂▃▃▄▅▆▆▇▇▇▇▇▆▆▅▄▄▃▃▃▃▄▄▄▅▅▅▅▅▅▄▄▃▂▂▂▂▂▃▄▅▅▆▆▇▇▇▇▆▆▅▄▄▃▃▃

 next   Harbour Lights                              on disk
 then   Moorings                                    on disk
        + 12 more

 held   sleep in 42 min                       (ember)

 > _                       type to play, ask, or find   ? help
```

- One input line, always there: a bare verb runs, a question answers in place
  (the w13-ask route), anything else searches as you type and the results
  replace the next/then block until Esc.
- Albums, playlists, the journal and song detail open as a **lower sheet** that
  takes the block below the wave, never the stage above it.
- No tabs. No F-keys required.

### B. Two columns, one list

Keep a list-centred tool, but one list with a source switch and detail inline.

```
 Queue  Albums  Playlists  Search          server online - 3 on disk
┌───────────────────────────────┐┌─────────────────────────────────┐
│ > Low Tide          on disk   ││ [cover]  Low Tide                │
│   Harbour Lights    on disk   ││          Fixture Band            │
│   Moorings          on disk   ││          Held Harbour, 2024      │
│                               ││          flac, 857 kbps           │
└───────────────────────────────┘└─────────────────────────────────┘
 playing  Low Tide - Fixture Band   0:04 / 2:00   ▄▃▃▂▂▂▃▃▄▅▆▆▇▇▇
```

- The right column always shows the selected row's detail (kills the popup).
- Now playing shrinks to one line at the bottom.
- Still tabs, still navigation before a nudge.

### C. Conversation

A single scrollback where every act and answer is a line, now playing pinned.

```
 Low Tide - Fixture Band                    0:04 / 2:00   online
 ─────────────────────────────────────────────────────────────
 you   whats offline
 dj    62 albums play offline:  Held Harbour (3) ...   [enter to open]
 you   play held harbour
 dj    playing Held Harbour, 3 songs, all on disk
 > _
```

- Maximally learnable (it is djc in a window).
- Poor glance: next/then and timers scroll away; browsing a list in a chat is slow.

## Comparison

| Criterion | A Stage | B Two columns | C Conversation |
|---|---|---|---|
| Glance at 80x24 (moment 1) | best: everything above the fold | now playing reduced to 1 line | next/timers scroll away |
| Nudge without navigation (moment 2) | one line, no mode | needs a tab or `:` | one line |
| Dig (moment 3) | sheet below stage, glance kept | best: inline detail | weak for browsing |
| Subtracts | tabs, F-keys, Up Next dup, popups, DJ tab | popups, Up Next dup | tabs, panes |
| Parity with djc (words, not colour) | high: same sentences, same answers | medium | highest |
| Risk to existing invariants (sixel overlay rules, confirm-before-arm) | low: confirm renders in the input line's echo, as today in DJ | low | low |

## Decision

**A, with B's inline detail inside the sheet.** A is the only shape that serves
the most frequent moment (glance) and the second (nudge) with zero navigation;
the sheet takes B's strongest idea (detail beside the list, never over the
stage). C is kept as the DJ transcript, reachable as one sheet among the others,
not as the frame.

What is removed: the tab strip and F-keys (kept as aliases for one release, like
the djc rename warning), the Up Next panel's duplicate of the queue, the DJ tab,
the full-frame popups, the `# ` and `/` row syntax, and three of the four modes
(Search and Command merge into the one line; Confirm becomes an echo in that line).

## How it will be proven

- The journey harness (usability/journeys) gains a glance check per state: at
  80x24 the title, the next song, server state and any held timer are all on
  screen at once, with no overlay.
- A nudge journey: from a cold start, "more like this", skip, and "stop after this
  album" each take one input with no navigation keys.
- The existing three journeys must stay 0 FAIL.
- Screens are captured with the pump driver at 80x24 and 129x40 and committed
  next to this doc.

## Open

- The live screenshot's wrong album/cover and pixelated card cover are being
  traced on agents/w16-npcard; this redesign does not depend on their cause.
- The GTK app (agents/w16-gtk) should take the same three moments as its brief.
