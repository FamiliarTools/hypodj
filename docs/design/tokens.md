# HypoDJ design tokens

Status: proposed (HypoDJ Lead, 2026-11-23). Not yet bound by Guilherme.

## What makes HypoDJ, HypoDJ

HypoDJ is the furnace under the bath. The work happens below and out of sight,
and the person lounges in the warm room. Four things follow from that, and every
token below comes from one of them.

1. **The music is the colour.** HypoDJ owns almost no colour of its own. The
   chrome borrows its colour from what is playing (the cover palette) and from
   the room it sits in (the terminal or OS background). This is already how the
   TUI works (`album_color.rs`, `sigil.rs`); the tokens make it the rule.
2. **Warmth is the one owned colour, and it means "the furnace is working for
   you".** One ember hue, used only when HypoDJ is acting on your behalf: armed,
   on air, kept. Nowhere else.
3. **Never a surprise.** Echo before arm is a visual grammar, not just a flow.
   A plan you have not yet confirmed must look provisional, and nothing changes
   state without a step you can see.
4. **Startle-safe, on screen as in the ear.** The fade constants that keep the
   audio calm also govern motion. No flash, no cut, no alarm red.

The interface carries meaning in words, never in colour alone. That is what keeps
CLI, TUI and GUI at parity, and what keeps `djc` legible to agents.

## Token tiers

Tokens are **roles and policies**, not a fixed palette. Each surface resolves a
role through its own adapter (ratatui, egui, ANSI or plain text).

### 1. Room (detected, never chosen)

| Token | Rule |
|---|---|
| `room.bg` | The detected background: OSC 11, then `COLORFGBG`, then dark default (TUI); OS theme (GUI). HypoDJ never paints its own background. |
| `ink.primary` | Highest-contrast text against `room.bg`, at least 7:1. |
| `ink.quiet` | Secondary text, at least 4.5:1. Under a reported room (OSC 11) it is the fitted colour and not DIM. With no reported room it is DIM and the terminal's own foreground. Used for metadata, hints, and anything not yet real. |
| `rule` | Hairlines and box-drawing, at most 2:1. Structure is felt, not seen. |

### 2. Music (borrowed from the cover)

| Token | Rule |
|---|---|
| `music.info` | Cover swatch fitted to at least 3:1 (WCAG 1.4.11) in OKLCH. Waveform, level meter, progress. Existing `INFO_TARGET = 3.0`. |
| `music.decoration` | Cover swatch capped at 2:1, the distraction ceiling. Sigil, art fallback. Existing `DECO_CEILING = 2.0`. |
| `music.none` | No cover: a neutral palette from the hash only. |

### 3. Ember (the one owned colour)

| Token | Value / rule |
|---|---|
| `ember` | Proposed: warm amber, `oklch(0.74 0.15 60)`, fitted to at least 3:1 against `room.bg` by the same policy as `music.info`. |

Ember appears only for: an **armed** plan, what is **on air** when marked, and a
**kept** or favourited item (it replaces today's red heart). If something is ember,
HypoDJ is holding it for you.

### 4. State: the echo-before-arm grammar

| State | Look | Words |
|---|---|---|
| `echo` (proposed, not armed) | `ink.quiet`, underlined or dotted outline | "Will do: ..." plus how to confirm |
| `armed` (confirmed, scheduled) | `ember` | "Armed: ..." |
| `live` (happening now) | `ink.primary`, bold | what is happening |
| `refused` / `failed` | `ink.primary`, no colour, no alarm | one sentence: what did not happen and why |

There is no red and no error colour. A refusal is calm and says why.

Confirmation stays proportional to consequence (design law 3, Memoria
m:01kxjktj): an immediate, reversible act (pause, skip, volume) goes straight to
`live` with an undo line and no `echo`. Only future triggers, stops, clears and
wall-clock plans pass through `echo` and wait.

These tokens extend the 13 client design laws (m:01kxjktj), in particular law 11
(spend complexity on correctness, not chrome) and law 12 (calm by design).

### 5. Motion (derived from the fade constants)

Visual transitions use the same constants as audio fades (`FadeConfig`), so the
eye and the ear get the same feel.

| Token | Value | Source |
|---|---|---|
| `motion.floor` | 250 ms | `DEFAULT_MIN_SLEW_MS`. Nothing visible changes faster, except the level meter. |
| `motion.skip` | 350 ms | `DEFAULT_SKIP_FADE_SECS` |
| `motion.pause` | 500 ms | `DEFAULT_PAUSE_FADE_SECS` |
| `motion.curve` | perceptual | Interpolate in OKLCH lightness, like dB, not linearly in RGB. |
| `motion.rate` | 4 Hz max | Chrome redraws at the fade tick (250 ms). The level meter is the only fast mover. |

No bounce, no overshoot, no flashing. The volume fader moves in perceptual dB
detents, the same detents as the server.

### 6. Type, grid and shape

| Token | Rule |
|---|---|
| `type.face` | One monospace face. TUI: the terminal's own. GUI: JetBrains Mono. Monospace because HypoDJ is terminal-native and the GUI is the same instrument. |
| `type.weights` | Regular and bold only. Emphasis is bold, reverse or underline, as in the TUI today. |
| `grid.cell` | The terminal cell is the unit. The GUI maps 1 cell to 8 px wide, so layouts transfer. |
| `glyph.sigil` | Truchet diagonals from the album hash, the HypoDJ mark for "no cover". |

### 7. Voice

One sentence that always says what happened, including why when nothing did.
British English. No exclamation marks. No em dashes. Echo in the future tense,
live in the present, done in the past.

## Family

HypoDJ is a Familiar Tools product and stands on its own tokens. It does not
depend on the Abicoca Labs brand system (forest, ochre, EB Garamond, the fox) or
yapp's palette (black and white, the four cubes). If Familiar Tools later grows a
shared surface-neutral base, HypoDJ's room, state and motion roles are a
candidate for it.

## Implementation shape

- `design/tokens.toml` is the single source of truth for values.
- A small `hypodj-style` crate holds the roles, the contrast policies (moved out of
  `hypodj-tui/src/album_color.rs`) and the motion constants (read from `FadeConfig`).
- Adapters: ratatui (TUI), egui (GUI), ANSI or plain (CLI, plain when not a TTY or
  when `NO_COLOR` is set). No surface defines a colour of its own.
- Parity test: every state above renders with its words on every surface, and the
  CLI output is identical with colour on and off apart from escape codes.
