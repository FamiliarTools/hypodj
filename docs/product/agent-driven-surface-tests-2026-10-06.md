# Agent-driven surface tests (TUI / GUI)

Status: design (not implemented)
Created: 2026-10-06
Audience: Guilherme (and agents working this tree)
Depends on: `docs/product/stance-2026-10-06.md`, `docs/product/egui-entry-gate-2026-10-06.md`

This page designs how an agent drives tests that involve the listening shell
(`dj`: TUI today, egui later) without inventing ceremony and without touching
Guilherme's graphical session. It is the missing automation path for the TUI
witness the egui entry gate still marks pending.

Product names: CLI = `djc`, listening shell = `dj`. On-disk today: CLI = `dj`
(`hypodj-cli`), TUI = `dj-gui` (`hypodj-tui`). No egui crate exists yet.

## Hard constraint (Guilherme, 2026-10-06)

Surface drivers are **headless and agent-owned**:

- Do **not** steal Guilherme's display (`DISPLAY`, Wayland session, focused
  windows).
- Do **not** use `computerUse` on his desktop.
- Allowed: agent-owned PTY, ratatui `TestBackend`, scripted key injection into
  the pure state machine, virtual display (Xvfb / headless Wayland) owned by
  the agent process or the agent desktop, silent probe daemons on alternate
  ports.
- Pass/fail is **daemon truth** (queue, now-playing, status fields), not
  screenshots alone. Chrome may be sampled as a soft witness.

Silent audio always (`HYPODJ_AUDIO=null`). No em dashes in Guilherme-facing
copy. Pin `HYPODJ_NL_TRANSLATOR=rules` for the shared drill.

## Survey (what already exists)

### Shared client (skip chrome)

`hypodj-client` already owns the NL handshake seams (`nl_request`,
`token_from_pairs`, `echo_from_pairs`, `split_echo`, `armed_line`,
`NlTranslatorPin`) and pure `status` / `currentsong` / `playlistinfo` parsing.
All of that is unit-tested with canned strings. The CLI proof in the egui entry
gate already shows the same silent-probe recipe leaves a matching deck when
armed under the rules pin.

That path does **not** close the TUI chrome gate by itself. The gate needs the
colon → echo popup → `y` path inside `dj-gui` to leave the same deck.

### TUI pure core + TestBackend (in-tree, patterned)

| Seam | Where | What it already proves |
| --- | --- | --- |
| Key routing | `hypodj-tui/src/state.rs` (`handle_key`, `key_confirm`, colon submit) | `:` opens Command; NL phrase → `Intent::Nl`; Confirm → `y`/`n`/Esc |
| Dispatch | `hypodj-tui/src/main.rs` tests | `Intent::ConfirmArm` → one `Req::Arm`; cancel → `Req::Cancel` |
| Worker NL | `hypodj-tui/src/worker.rs` | Real `MpdConn`: `nl "<phrase>"` → `RespKind::Confirm(Pending)`; arm via `nl confirm <token>` |
| Render smoke | `hypodj-tui/src/ui.rs` | Many tests paint with `ratatui::backend::TestBackend` (no real TTY) |
| Translator pin | `main.rs` + `NlTranslatorPin::from_process_env` | `HYPODJ_NL_TRANSLATOR=rules` skips the Claude Code side door on colon |

There is **no** headless driver for the shipped `dj-gui` binary today: no
scripted stdin inject, no non-TTY event loop, no PTY harness in-tree. The entry
gate correctly refused inventing expect/tmux ceremony for that DFS; this design
is the sanctioned place to pick a thin driver.

### egui

Docs only (`egui-entry-gate`, stance). No GUI crate, no headless GUI driver.
`computerUse` on Guilherme's session is forbidden; any future GUI driver must
sit on an agent-owned virtual display if pixels are required at all.

### harn MCP (optional later surface)

Worktree `.claude/worktrees/harn-mcp-agent` has `hypodj-mcp` (`dj-mcp`): agent
tools over MPD via `hypodj-client`, with a scripted fake daemon in
`tests/stdio.rs`. It is a plan-add tool surface, not the NL echo-before-arm
chrome under test. Treat as an optional later agent surface, not the TUI gate.

## Goal

Close the **TUI UI proof** on the egui entry-gate drill, agent-runnable,
headless:

1. Silent probe (`HYPODJ_AUDIO=null`, store/mpris/heard/tape off, alt port, throwaway state).
2. Seed one current track.
3. Pin `HYPODJ_NL_TRANSLATOR=rules`.
4. Drive the TUI path that maps to `:`, type `play something calmer`, Enter, confirm `y`.
5. Assert queue length, now-playing identity, and any `toward calmer` (or
   equivalent) against the **same** daemon the CLI proof used.
6. Tear the probe down. Live `6600` untouched.

Chrome buffers (TestBackend text, PTY scrollback) are optional soft witnesses.
Daemon mismatch fails the run even if the popup "looked right."

## Approach A (preferred): headless key pump + real worker + daemon truth

**Idea:** stay inside the crate. Do not spawn a real terminal. Reuse the seams
already tested in isolation, wired together against a silent probe.

### Shape

1. Start silent probe (same recipe as `egui-entry-gate-2026-10-06.md`).
2. `Workers::spawn(host, probe_port)` (existing).
3. Build a `TuiState` with `nl_translator = Rules` (same as env pin).
4. **Key pump** (new, tiny, patterned on `state` + `main` tests):
   - Feed `KeyEvent`s: `:` → chars of the phrase → Enter.
   - `dispatch` resulting `Intent`s onto the worker channels.
   - Drain `Inbound` with a short timeout; on `RespKind::Confirm` /
     `CcConfirm`, call `enter_confirm` (same as the live loop).
   - Feed `y`; `dispatch` `ConfirmArm`.
   - Drain refresh / banner inbounds into state.
5. On a **second** `MpdConn` to the probe (or via a final refresh inbound),
   read `status` / `currentsong` / `playlistinfo` (or the client's model
   parsers) and assert deck shape vs the CLI baseline for that seed.
6. Optional soft check: `Terminal::new(TestBackend::new(w, h))` + `ui::render`
   after confirm entry / after arm; assert echo step text or `armed` banner
   appears in the buffer. Never sufficient alone.
7. Stop workers; kill probe.

### Why this fits

- Already patterned: `ch`/`key` helpers, `dispatch` tests, `TestBackend`
  renders, `Workers::spawn`, shared `nl` parsing.
- No PTY crates, no expect, no tmux, no display, no `computerUse`.
- Stubborn DFS: the pass criterion is daemon outcome under the rules pin.
- Smallest code that actually closes the TUI chrome gap (keys → worker →
  daemon), not just another unit test of `key_confirm`.

### What it does not claim

- It does not prove crossterm raw-mode / real TTY behavior.
- It does not prove sixel / OSC / album-color terminal quirks (those stay unit-
  tested and manual).
- It does not ship a public "script mode" flag on `dj-gui` unless a later need
  appears; the harness can live as an integration test or a tiny
  `#[cfg(test)]` / `tests/` pump beside `main`.

## Approach B (backup): agent-owned PTY around the real binary

**Idea:** spawn `dj-gui` under a PTY the agent owns (`portable-pty`, or a Nix-
provided `script`/`expect` only if already available in the devshell). Inject
bytes for the same key sequence. Still assert daemon truth on the probe port.

### When to use

- Need binary-level smoke (argv, env pin, setup_terminal failure modes).
- Approach A is green and something still fails only on a real CrosstermBackend.

### Constraints

- PTY must be agent-local (box or agent desktop). Never attach to Guilherme's
  interactive terminal emulator window.
- Still no audio; still alt-port probe; still rules pin.
- Heavier dependency and flake surface. Do not invent this for the first slice.

## egui later (out of first slice)

When a graphical `dj` exists:

1. Prefer the same **shared-client + daemon truth** drill first (chrome-free
   parity).
2. If pixels must be driven: agent-owned virtual display only (Xvfb, Weston
   headless, or the agent desktop's isolated session). Never Guilherme's
   `DISPLAY`.
3. `computerUse` only against that isolated virtual display, if at all. Prefer
   in-process egui test harnesses / headless backends over screenshot agents.

harn MCP remains an optional agent verb surface; it does not replace the
listening-shell chrome witness.

## Rejected / deferred

| Idea | Why not first |
| --- | --- |
| Manual-only TUI gate forever | Blocks egui; agents cannot re-proof |
| Screenshots as pass/fail | Stance + entry gate forbid it |
| `computerUse` on Guilherme's desktop | Hard constraint |
| Scripted stdin on `dj-gui` as if it were CLI | TUI does not confirm via stdin; it uses crossterm keys |
| Expect/tmux ceremony without an owner | Entry gate already rejected; Approach B is the named, constrained form |
| Closing the gate with CLI + client tests only | Daemon contract proven; TUI chrome not witnessed |
| Implementing harn MCP NL tools to "skip" TUI | Different surface; optional later |

## Recommended first slice

**Implement Approach A as one integration-style test (or a thin test-only
pump) that closes the TUI witness for `play something calmer` on a silent
probe.**

Smallest closing move:

1. Probe bring-up/tear-down helper matching the entry-gate recipe (port not
   6600, null audio, throwaway state, mpris/store/heard/tape off).
2. Headless key pump: KeyEvents → `handle_key` → `dispatch` → drain worker
   inbounds → `enter_confirm` / apply refresh (extract only what `main`'s loop
   already does; avoid a second brain).
3. Assert daemon queue + now-playing (+ status field if present) after `y`.
4. Pin rules via state field / env for the test process.
5. Optional: one `TestBackend` assertion that the confirm steps were painted.

Stop there. Do not add PTY, egui, harn, or binary renames. Do not claim egui
entry until this TUI witness is recorded green the same way the CLI proof was.

### Success record (when implemented)

Mirror the CLI evidence table in `egui-entry-gate-2026-10-06.md`: probe bind,
seed track, echo trust + steps, armed plan id, before/after queue length and
now-playing, log path under `~/tmp/...`. Mark **TUI UI proof: pass** on that
page only after a real silent run.

## Non-goals for this design

- Launch ceremony, David / RealityMaker, `djc`/`dj` binary renames.
- Visualizers, sixel, album-color as gate criteria.
- Speaker playback in probes.
- Replacing unit tests; this sits above them as one DFS witness.
