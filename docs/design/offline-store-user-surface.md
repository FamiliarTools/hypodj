# The offline store's user-facing surface

Status: active
Created: 2026-09-09
Updated: 2026-09-09

## Context

The offline mirror is a multi-day background job the user cannot watch. Everything
they will ever know about it arrives through three surfaces: a one-line badge on
the TUI's bottom row, the `dj store` view, and `dj store frontier`. This document
covers what those may say and why, which is a separate question from how the
mirror works (see `offline-audio-store.md`).

The governing constraint, stated by Guilherme on 2026-09-09: the user must be
informed of critical information and must not be overwhelmed with information
they cannot act on.

## The dividing principle

**A surface earns a line only if the user can act on it.** Everything explaining
*how* the ranking decided belongs behind `dj store frontier`, which is opt-in.

Applied to the mirror there are exactly two actionable facts:

| Fact | The user's move |
|---|---|
| Some favourites did not fit | Raise the cache limit (`dj store limit 24G`) |
| Some songs will never download | Rescan the library |

Everything else - decile scores, per-group play counts, raw byte shortfalls, the
ranking rule - is evidence for a decision already made. It is forensics, and
forensics belong in `frontier`.

## The badge is a step-down, not a truncation

This is the constraint that governs every wording change, and it is easy to miss.

`render_command` in `crates/hypodj-tui/src/ui.rs` tries the full badge, then the
short form (`<head> (held)`), then draws nothing. The fit test is:

```
full > hint_len + badge_len + MIN_WAVE_CELLS      // MIN_WAVE_CELLS = 16, hint = 8
```

So the badge never gets truncated into a half-sentence. It silently drops *whole
reason clauses* instead. **Every cell the daemon adds to the `X-Store` line is
paid for by a reason vanishing at some width.**

The reference width is **129 columns**, Guilherme's real terminal on bubble-gum.
Measured twice off screenshots, two independent methods agreeing: a 63-character
row spans ~975px at ~15.5px per cell, and the old 96-cell badge left a wave smear
of 24.75% of the row, which is exactly `129 - 96`.

Pinned by `ui.rs::the_real_mirror_line_still_shows_its_reasons_at_the_real_terminal_width`.
Any new clause must budget against 129 or that test fails, which is the point.

A second consequence: `store_badge` drops clause index 1 *positionally*, and that
clause is the size. **The TUI never shows the GiB budget.** Any wording that
refers to the budget must carry its own referent, because the number it points at
is not on screen.

## Fullness is a picture, not a ratio

The cache gauge is `[◉◉◉◉◉◉◉◉◉◎]`: ten fixed cells, filled discs for
used, hollow for free. It answers "is there room", which is a shape read at a
glance, rather than "how many gigabytes", which is two numbers to subtract.

It sits in the HEAD clause and not beside the sizes it describes. That is
forced: `store_badge` drops clause index 1 positionally, so a gauge placed with
the gigabytes would never appear on the TUI - the one surface it is for.

It also repairs the referent problem structurally rather than verbally. The old
clause "did not fit in 16 GiB" had to name a number precisely because the budget
was invisible; with the gauge on screen the verdict can just be "3 songs left
out" and the picture supplies the why. That is why the wording got shorter and
the line did not get longer: the gauge is paid for by the words it replaced.

Two rounding lies are suppressed at the ends, because they are exactly the two
states worth acting on: a mirror holding real music never reads empty, and one
with room left never reads full.

`◉` (U+25C9 fisheye) and `◎` (U+25CE bullseye) both read as a disc with a centre
hole. Character width is a live hazard here - most box and geometric glyphs are
East Asian Ambiguous, and a terminal rendering them double-width would silently
blow the 129-column budget. These are safe on any terminal that draws the
bottom-bar wave correctly, because that wave is already built from Ambiguous
block characters.

## The line must account for every song

The badge said this for months:

```
384 of 446 songs, 3 songs (0.1 GiB) would not fit, 4 songs failed to download
```

That totals 391 against a wanted 446. The 55 unmentioned songs were the ones
actually being downloaded - the only evidence the reconciler was alive at all.
A line that reads as a complete accounting and is not one is worse than a shorter
one, because it is trusted.

The rule now: **held + coming + given-up equals resident, exactly.** Given-up ids
are a subset of pending, so the in-flight count subtracts them; without that a
stalled song is counted once as coming and again as terminal.

```
384 of 446 songs - 62 still coming [◉◉◉◉◉◉◉◉◉◎], 15.9 of 16.0 GiB, 3 songs left out, 4 songs will not download
```

The head is joined with ` - ` and not `, ` because the clients split on `, ` and
the size clause must stay at index 1 for the positional drop.

## Verdicts are phrased as settled, not as pending

"failed to download" describes a transient event and invites waiting for a retry.
By the time the clause appears the backoff has spent every attempt and only a
restart re-arms it, and the underlying cause is usually permanent (see
`../research/offline-download-failures.md`). It reads "will not download", which
is grammatically parallel to "would not fit" because it is the same kind of fact:
a decision that has settled.

## Surfaces reviewed and found good

A full audit on 2026-09-09 covered the TUI menu, TUI transient status messages,
the keymap help, the `dj` top-level help, and the store views. Only the store
surface carries copy debt. The others are the standard to hold:

- **Menu labels** are verb-first and every one is an action: `play now`,
  `add to queue`, `go to album`, `start a radio from here`.
- **Status messages** pair what happened with the next move:
  `the queue moved - reopen the menu`, `reconnected - re-run the phrase`,
  `nothing is playing to favorite`.

## Resolved on 2026-09-09

All four findings from the audit are fixed:

- The `rule:` line moved to `dj store frontier`. The default view no longer
  lectures about a ranking with no knob in it.
- `rank_reason` stays on `frontier`; the default deferred list uses
  `shortfall_reason`, which says how much music is missing and how much more room
  it needed, in MiB/GiB rather than raw bytes. Two formatters exist on purpose so
  the default view cannot drift back into forensics.
- "would not fit" became "left out", a verdict rather than a physical claim,
  with a fullness gauge in the head supplying the referent the words used to
  have to carry. See "Fullness is a picture" above.
- `dj mark` in the top-level help is four lines instead of seven.

## Open questions

None outstanding.
