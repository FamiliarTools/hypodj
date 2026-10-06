# egui entry gate (first DFS parity checklist)

Status: active
Created: 2026-10-06
Proven: 2026-10-06 against a silent probe daemon
Audience: Guilherme (and agents working this tree)
Depends on: `docs/product/stance-2026-10-06.md`

This page freezes the **minimal** listening-parity checklist implied by one real
NL echo-before-arm outcome. It is the **egui entry gate**: no egui pixel work
until the graphical `dj` presentation can pass the same drill on the same kind
of silent daemon. It is not a feature tour.

Product surface names: CLI = `djc`, listening shell = `dj` (TUI fallback /
egui). On-disk binaries today: CLI = `dj` (`hypodj-cli`), TUI = `dj-gui`
(`hypodj-tui`). Commands below name the on-disk binaries; the checklist speaks
product names where it states rules.

## Drill (one shared outcome)

Phrase (rules-backed, already real): **`play something calmer`**

Against a **silent** probe daemon:

1. Seed one current track (so `NlContext.current` is set).
2. Run echo → confirm → arm on the CLI (`djc` / today's `dj`).
3. Observe queue and now-playing after the plan executes.
4. Same phrase on the TUI fallback of `dj` (today's `dj-gui`) must leave the
   **same** queue and now-playing shape when pointed at the same daemon and the
   same translator path (see Translator note below).

Done means: one shared outcome + this written gate. Not done means: a mock UI,
a second plan language, or a shell-local reinterpretation of the echo.

### Evidence (2026-10-06, CLI pass)

Silent probe:

- Binary: deployed `hypodj` with `HYPODJ_AUDIO=null`
- Bind: `127.0.0.1:6610` (live listening stays on `6600`; do not point probes at
  the live store/mirror)
- Config overrides: `[mpris] enable = false`, `[store] enable = false`,
  `[heard] enable = false`, `[tape] enable = false`
- Throwaway `STATE_DIRECTORY` under `~/tmp/hypodj-probe-dfs-2026-10-06/state`
- Live daemon left untouched (`dj --port 6600 now` stayed "nothing playing")

CLI handshake (rules path; pin skips the Claude Code side door):

```
HYPODJ_PORT=6610 HYPODJ_NL_TRANSLATOR=rules dj "play something calmer"
# stdin: y
# equivalent: dj --port 6610 --nl-translator rules "play something calmer"
```

Observed:

| Step | Output / state |
| --- | --- |
| Echo | `(via rules)` then `[1] play 5 calmer tracks NOW (enqueue + start playback) now` |
| Confirm | `confirm? [y/N]` → `y` → `armed (plan 3)` |
| Before | now-playing `Everest` (Klangstof); queue length **1** |
| After (~2s) | now-playing `Heater` (Perila & Ulla); queue length **6**; status field `toward calmer` |
| Queue tail | Everest, Heater, Desert Blue, Misery Goats, This Is A Low, Get Your Snack On |

Artifact log: `~/tmp/hypodj-probe-dfs-2026-10-06/cli-rules-proof.log`

**Pass (CLI + daemon contract):** echo fields present, confirm armed a plan id,
queue and now-playing changed as the echoed plan promised, audio stayed null.

### TUI proof status (manual gate; not headlessly automated)

`dj-gui` is an interactive ratatui shell. There is no sanctioned headless driver
in-tree (no script inject, no non-TTY NL harness). Inventing expect/tmux
ceremony is out of scope for this DFS.

**Manual gate (same silent probe, same phrase):**

1. Reset the probe deck to one current track (same seed as CLI), or use a fresh
   silent probe on another port.
2. Point the TUI at the probe: `HYPODJ_PORT=6610 dj-gui` (same host/port
   resolution as the CLI via `hypodj-client`).
3. For the same translator path as the CLI proof: export
   `HYPODJ_NL_TRANSLATOR=rules` so the TUI uses daemon `nl`, not the Claude Code
   side door (DJ View and colon both honor the pin).
4. Press `:` (command / NL line), type `play something calmer`, Enter.
5. Read the confirm popup (trust + numbered steps from `nl_echo`). Press `y` to
   arm, `n` / Esc to cancel.
6. Pass when queue length, now-playing identity, and any `toward calmer` (or
   equivalent field) match what the CLI left on that daemon for the same armed
   plan shape.

Code contract already shared (no second brain):

- TUI worker uses `hypodj_client::nl::{nl_request, token_from_pairs,
  echo_from_pairs, split_echo, armed_line, map_ack_reason}` and arms with
  `nl confirm <token>` on the **one** command socket (`crates/hypodj-tui/src/worker.rs`).
- Confirm keys: `y`/`Y` arm, `n`/`N`/Esc cancel (`key_confirm` in
  `crates/hypodj-tui/src/state.rs`).

Until someone runs that manual gate on a silent probe and records matching
queue/now-playing, mark **TUI UI proof: pending**. The daemon + shared-client
handshake is proven; the TUI chrome is not yet witnessed end to end in this
drill.

## Checklist (egui may not ship pixels until these hold)

### Silent daemon

- [ ] Probe uses `HYPODJ_AUDIO=null` (never clone live `HYPODJ_AUDIO=device`).
- [ ] Probe does not share the live offline mirror: `[store] enable = false` or a
      throwaway `dir` / `STATE_DIRECTORY`.
- [ ] MPRIS off on the probe (`[mpris] enable = false`) so the desktop is not
      contested.
- [ ] Probe binds an alternate MPD port (this drill used `6610`). Live `6600`
      stays the listening daemon.
- [ ] Tear the probe down after the run.

### Handshake fields (daemon → shell; shells must not invent)

Wire pairs from a successful `nl "<phrase>"` translate:

| Pair | Meaning |
| --- | --- |
| `nl_echo` | Pipe-joined human plan: trust (`via rules` / `via local model`), numbered `[n] ...` steps, optional `NOTE:` |
| `nl_token` | Owner-scoped token (`nl-<hex>`); required before confirm |

Confirm: `nl confirm <token>` → response includes `plan_id` (render as
`armed (plan N)` via `armed_line`). Cancel: `nl cancel <token>`.

Immediate `plan add` arms may also carry a `result` pair (what actually
executed: e.g. `added N`, `added 0 - no matches for X`). Prefer `result` over a
plan-asked count when present (`result_line_from_pairs`).

Shared parsing lives in `hypodj-client` (`nl.rs`). CLI and TUI (and egui) must
call those seams, not re-split echo text ad hoc.

### Result line and deck truth

- [ ] After arm, queue and now-playing come from daemon `status` /
      `currentsong` / `playlistinfo` (or the client's thin wrappers over them).
- [ ] Shells display and request; they do not reinterpret the plan into a
      different selector, count, or play-vs-enqueue choice.
- [ ] Same phrase + same translator path + same daemon ⇒ same queue /
      now-playing outcome on `djc` and on `dj` (TUI or egui).

### Translator note (do not fork the drill) - REQUIRED PIN

The shipped CLI may try Claude Code first when `claude` is on `PATH`, then fall
through to daemon `nl`. In this drill, CC once rewrote the phrase into
`play 1 tracks matching "calmer"` and reported `added 0 - no matches for
"calmer"` (no deck change). The rules path echoed and armed
`play 5 calmer tracks NOW` and the deck moved.

Parity is **same translator path on every surface under test**, not "whatever
the pretty shell picked." **This gate requires an explicit pin** via shared
`hypodj-client::nl::NlTranslatorPin`:

| Pin | How | Effect |
| --- | --- | --- |
| `rules` | `HYPODJ_NL_TRANSLATOR=rules` or `dj --nl-translator rules` | Force daemon `nl` (skip client Claude Code) on CLI and TUI |
| `claude` / `cc` | env or `--nl-translator claude` | Prefer Claude Code on both surfaces |
| `auto` (default) | unset / blank | Today's human split: CLI tries CC then daemon; TUI colon uses daemon `nl`; DJ View uses CC |

For the egui gate, pin `rules` (or pin `claude` everywhere and accept its plan),
and compare outcomes under that pin. Do not claim parity across a rules arm on
one surface and a CC miss on another.

Probe example (same as the CLI evidence run, now without PATH surgery):

```
HYPODJ_PORT=6610 HYPODJ_NL_TRANSLATOR=rules dj "play something calmer"
# stdin: y
```

### Echo before arm

- [ ] No deck mutation from NL until the user/agent confirms.
- [ ] Default-No confirm (`y` / `yes` only on CLI; `y` on TUI).
- [ ] Cancel is best-effort `nl cancel` on the same connection/token.

### No shell-side audio

- [ ] Clients stay pure MPD/TCP (no libmpv in CLI / TUI / egui).
- [ ] Probe proofs stay silent.

## egui bar (when graphical `dj` exists)

egui passes this gate when, against a silent probe as above, the same phrase
with the same pinned translator path yields the same `nl_echo` / confirm /
armed plan and the same queue + now-playing as the CLI proof (and as the TUI
manual gate once recorded). Chromes may differ; the deck may not.

Out of scope for this gate: Launch ceremony, David / RealityMaker, binary
renames (`djc` / `dj`), visualizers, layout polish.

## Stubborn notes

- Matching screenshots is not parity. Matching daemon outcomes is.
- Pedantic routing stays: multi-word phrases go to NL; do not silently turn
  "play something calmer" into a wrong queue play.
- No em dashes in Guilherme-facing copy. No speaker playback in probes.
