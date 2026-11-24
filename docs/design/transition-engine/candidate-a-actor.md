# Candidate A - the transition engine as an actor

Types and signatures: `sketch.rs` (same directory). New module `crates/hypodj-core/src/engine/` (`mod.rs` public surface, `plan.rs` pure, `actor.rs` + `decks.rs` private).

## Problem

hypodj has five audible head-change shapes (XF, DIP, CUT, SIL, GAPLESS) chosen by *which handler function you happened to call*: three entry gates (`play_index`, `play_index_inner(false)`, `play_index_from_silence`), a separate skip path (`user_skip` -> `skip_with_fade` -> `start_crossfade` / `install_skip_dip`), a director-driven boundary path, and a warm/gapless path, with the head commit duplicated four times. As a result the proof case (clear+add+play) hard CUTs from a stopped deck, MPRIS next diverges from MPD next, and play N cuts even while music plays. Constraints to keep: the std `Mutex<State>` is never held across `.await`; Eof stays lossless; manual volume cancels ramps atomically; never strand a second audible deck; gesture vs autonomous (a winddown survives autofill); finish attribution (skip = no scrobble, boundary = attributed Eof without a second advance); evict atomic with the head commit; a 1-deck floor (DIP); a load failure falls through and is never a no-op; every plan `Action` still goes through `run_action_outcome`.

## Usage (caller's view)

```rust
// Anything that changes what is playing:
engine.retarget(Target::Entry(qid), Cause::Skip).await?;          // MPD next/prev AND MPRIS next/prev
engine.retarget(Target::Entry(qid), Cause::PlayAt).await?;        // play N, playid, plan Play/PlayNow
engine.retarget(Target::Entry(cur), Cause::RemoveCurrent{evict}).await?; // with successor
engine.retarget(Target::Nothing,    Cause::RemoveCurrent{evict}).await?; // last row
engine.retarget(Target::Nothing,    Cause::QueueReplaced).await?; // clear
engine.retarget(Target::Entry(q0),  Cause::QueueReplaced).await?; // play after the swap -> XF
engine.retarget(Target::Entry(qid), Cause::FromSilence{ rise: Some(FadeIntent::WakeTo(..)) }).await?;

// Volume (the engine is the single gain writer):
engine.ramp(FadeIntent::PauseOut).await?;   engine.set_level(40).await;

// Read side (status, MPRIS, director):
let v = engine.view().borrow().clone();   // v.reported, v.phase
```

The proof case as the wire sees it (ncmpcpp "replace queue and play"): `clear` -> `Target::Nothing / QueueReplaced` starts a one-legged fade-out over `skip_xf` (1.5 s) and reports stopped at once; `add` x N change only queue rows; `play 0` -> `Target::Entry(q0) / QueueReplaced` arrives while the phase is `Overlap{incoming: None}`, so the planner returns `Crossfade` with `Reaim::Fresh-into-fadeout`: the spare deck arms q0 silently and the envelope starts from the outgoing leg's *current* gain. Nothing in that path knows it is the proof case. If `play` comes after the fade-out finished, the deck is silent and the planner picks `FromSilence`/`Cut`, which is correct (silence was already heard).

EOF: the deck Eof goes into the actor. On a `Steady` head with a parked warm entry -> `Gapless`, emits `Finished{advanced:true}`. Otherwise it emits `NeedsSuccessor{after}`, and the handler's existing `advance_on_eof` (repeat/single/consume/autofill policy) calls `retarget(Entry(next), NaturalEnd)` or `retarget(Nothing, NaturalEnd)`.

## Shape

**Data first.** `Phase` (actor-private) is the only transition state in the process:
`Idle | Steady{head} | Overlap{out, incoming: Option<Leg>, env, finish} | Dip{head, trough_target, env}`. A `Leg` is `(DeckIx, QueueId)`. Deck *identity* lives in `Phase`, so `PlayerHandle::active: AtomicUsize`, `crossfade_commit`'s flip, and the state flags `crossfading`, `pending_skip`, `pending_crossfade_evict`, `pending_crossfade_finish` are all derived from `Phase` (or deleted), not synced. `Envelope` is a value (start instant, duration, from/to dB per leg). The actor writes gains on a tick, so a re-aim reads the live position directly instead of asking a task that might be mid-write.

**Flow.** `retarget` -> `Msg::Retarget` (awaited send, lossless) -> `on_retarget`:
1. Build a `Snapshot` (one brief std lock for queue facts like same-album run and the winddown flag, plus the actor's own `Phase`).
2. Pure `plan_transition(&cause, &target, &snap) -> TransitionSpec {shape, reaim, resync, attribute_finish, evict}`.
3. If `resync` (= `cause.is_gesture()`): cancel the `Level` ramp, set `pending_pause=false`, arm the EOF fuse. This happens in one place, so autonomous causes cannot kill a winddown.
4. Apply `reaim`.
5. Walk `shape.degrade()` until a load succeeds (`Crossfade -> Dip -> Cut -> Err(Unplayable)`; on one deck the planner starts at `Dip`).
6. Publish `HeadView{reported: target}` right away (replaces `pending_skip` as the reported target).
At envelope end, `commit_head` is the ONE commit: under one std lock it sets `current = position_of(qid)`, evicts the row, commits the baseline, and releases the store pin. If `finish.attribute`, it first emits `Finished` (crossfaded Eof semantics).

**Planner table** (pure, unit-testable without mpv):

| condition | shape |
|---|---|
| target is head/incoming | `Nothing` (`AlreadyThere`) |
| `FromSilence` cause, or `!audible` | `FromSilence` / `Cut` |
| `NaturalEnd` + warm parked == target | `Gapless` |
| `NaturalEnd` cold | `Cut` (the head already ended, nothing to overlap) |
| `Boundary` + `same_album_run` | not emitted (engine skips the boundary check) |
| `Boundary` | `Crossfade{boundary_xf}`, attribute |
| gesture or `Autofill` while audible, `can_overlap`, `skip_xf>0` | `Crossfade{skip_xf}` |
| same, 1 deck or `skip_xf==0` | `Dip` |
| `Target::Nothing` while audible | `FadeOut{skip_xf}` |

**Mid-flight with two decks (`Reaim`).** This is explicit and total over `Phase`:
- `Overlap`, target == incoming -> `Keep` (idempotent).
- `Overlap`, target == out -> `Reverse`: the same envelope runs backwards from its current position with no settle and no step (replaces delete-case-0 `settle_crossfade(false)` + rise).
- `Overlap`, third target -> `DropQuieter`: keep the louder leg as the new outgoing at its live dB and stop the quieter leg. On an equal-power curve the quieter leg is at most -3 dB below the louder, and usually far lower once one of them dominates. Arm the new target on the freed deck and start a fresh envelope from (live dB, -inf). Ties keep the incoming, because it is already the reported head. A deck is never stranded: the stop and the reuse are the same deck in one actor step.
- `Overlap{incoming: None}` (fade-out) -> arm on the spare as a normal `Crossfade`, from the live gain. This is the proof case.
- `Dip` in descent or trough -> `RedirectTrough`: change what the trough loads, nothing audible. `Dip` in rise -> treat the risen track as head (`Steady`) and plan afresh.
- Load failure of the new incoming during a re-aim: the survivor leg rises back to baseline along an envelope, then the ladder continues to `Dip`/`Cut` on the target.

**Concurrency.** Being an actor *is* the epoch guard. Only one task writes gains and phase, so `fade_epoch`, `FadeSlot` abort+join, the slot-lock terminals, and `settle_crossfade`-before-gain-taking are replaced by the actor's message ordering. The `Mutex<State>` is only ever locked synchronously inside `snapshot`/`commit_head`, never across an await (per separate-before-serializing-shared-state: the engine owns transition state, the handler owns rows, and the merge is `commit_head`). Retargets that pile up during a slow load are coalesced: the actor drains the mailbox and runs the last `Retarget`, and earlier ones reply `Started{shape}` of the superseding transition (they were superseded, not failed).

**Encoded invariants.** The write half of `PlayerHandle` moves into `Decks` (private module), so a handler path that loads a track without the engine does not compile (per encode-lessons-in-structure). `HeadView.reported` is the single source for "status current". `Cause` derives gesture-ness and attribution, so no call site passes a bare `resync: bool`. Engine time uses `clock.rs`, and tests are `#[tokio::test(start_paused)]`.

**Interface depth.** There are 5 public methods (`retarget`, `ramp`, `set_level`, `transport`, `view`) plus 4 enums. Hidden behind them: deck allocation, arming order (silent first), envelope math, the fallback ladder, mid-flight re-aim, warm parking, phantom-EOF suppression, attribution, and the commit. Callers never name a deck or a shape.

**What it does not do.** It does not choose the successor (queue policy stays in the handler, and the engine only consumes a `watch<Option<QueueId>>` for prewarm and the boundary check). It does not journal; `run_action_outcome` wraps the `retarget` call as today. It does not make retargets atomic with queue edits: a qid deleted between send and execution gives `UnknownEntry`.

## Migration of every call site

| Today | Becomes |
|---|---|
| MPD next/previous -> `user_skip` -> `skip_with_fade` | `retarget(Entry(succ/pred qid), Skip)` (paused/stopped: planner picks Cut, transport kept) |
| delete playing row, successor (case 3) | `retarget(Entry(next), RemoveCurrent{evict})`; the row is evicted in `commit_head` |
| delete playing row, no successor (`FadeIntent::OutEvicting`) | `retarget(Nothing, RemoveCurrent{evict})` |
| delete the row a skip is heading to (case 0) | `retarget(Entry(out_qid), Skip)` -> `Reaim::Reverse` |
| play N / playid | `retarget(Entry(qid), PlayAt)` (now XF while playing) |
| clear | `retarget(Nothing, QueueReplaced)` (fade-out, not a deck stop) |
| clear+add+play | as above, then `retarget(Entry(q0), QueueReplaced)` -> XF |
| plan Play / PlayNow | `retarget(Entry, PlayAt)` inside `run_action_outcome` |
| radio start / autofill gesture | `retarget(Entry, QueueReplaced)` |
| autofill walk, continuation station | `retarget(Entry, Autofill)` (no resync; winddown survives) |
| EOF cold (`advance_on_eof` -> `play_index_inner(false)`) | engine `NeedsSuccessor` -> `retarget(Entry(next), NaturalEnd)` |
| EOF warm (`commit_landed_advance`) | engine-internal `Gapless`; `Finished{advanced:true}` |
| boundary crossfade (director tick -> `maybe_boundary_crossfade`) | engine-internal on TimePos + successor watch; `Cause::Boundary` |
| wake / resume restore / undo `install_snapshot` | `retarget(Entry, FromSilence{rise})` |
| MPRIS next/previous | `retarget(Entry, Skip)` (divergence gone) |
| `stop` / pause / resume | `engine.transport(..)` |
| setvol / MPRIS volume / `cancel_with` users | `engine.set_level(v)` |
| `fade` verbs, winddown, PauseOut, WakeTo | `engine.ramp(intent)` |

## Code deleted

From handler.rs: `user_skip`, `skip_with_fade`, `install_skip_dip`, `play_index`, `play_index_inner`, `play_index_from_silence`, `play_index_evicting`, `commit_landed_advance`, `start_crossfade`, `settle_crossfade`, `maybe_boundary_crossfade`, `crossfade_skip_dur`, `disarm_continuation_warm` (folded into the actor's warm handling), `Terminal::SkipLoad`, the crossfade_task terminal, the `FadeSlot` struct (`cancel_with`/`supersede` move into the actor as `Level` handling), and the `State` fields `crossfading`, `pending_skip`, `pending_crossfade_evict`, `pending_crossfade_finish`, `fade_epoch`.

From player.rs: `PlayerHandle::active`, `crossfade_arm`/`crossfade_commit`/`crossfade_abort` (become `Decks` primitives), `switch_warmed` and `WarmKind::Skip` along with the phantom-EOF guard for skip warms (a skip never parks a warm; it arms the spare deck).

From director.rs: the boundary-crossfade call on tick.

fade.rs keeps the gain math (`crossfade_gains`, ramp curves). `CrossfadeSpec::new` gains a `from_db: (f64, f64)` start.

## Tradeoffs accepted

- We accept that every gain write is serialized through one actor in exchange for deleting the epoch/abort/join/settle machinery. A slow mpv `loadfile` blocks other engine messages (including `set_level`) for its duration. This is mitigated by coalescing, but a volume knob press can lag one load (around 100 ms local, longer over the network).
- We accept that `clear` no longer silences instantly (a 1.5 s fade-out) in exchange for the proof case falling out of the general path. Status reports stopped at once.
- We accept that `play N` while playing becomes a crossfade, not a cut. That is a behavior change, and it is the stated goal.
- We accept that `DropQuieter` can audibly truncate a leg that is up to -3 dB under the louder one, at the exact midpoint of an envelope, in exchange for a total rule with two decks. A third deck would remove this and is not worth a third libmpv instance.
- We accept that EOF successor choice crosses back to the handler (`NeedsSuccessor`) in exchange for queue policy staying in one place.

## Alternatives considered

- **Engine as a mutex-guarded struct called from handler tasks** (keep FadeSlot, add a `retarget` method). Its interface is the same size, but every invariant becomes "hold the slot lock, check the epoch", and a reader must know those internals. That is the complexity this design removes. It lost on hidden-complexity-per-surface.
- **Declarative desired-state reconciler** (handler writes `desired_head`, engine converges). It leaks nothing, but cause, attribution and evict would have to ride along as state that goes stale between writes. Sequential intent (skip, then reverse) maps onto commands more naturally than onto a converging value.
- **Keep the gates and add a `Shape` parameter to `play_index`**. That gives two ways to change the head that are still selected by call site, which is exactly today's failure.

## Open questions and risks

- Should `clear` alone (no following play) fade out over `skip_xf`, or keep a shorter dedicated `clear_fade_secs`? Does any client rely on clear being instantly silent?
- During a `FadeOut` window, status says stopped while audio is still fading. Is that acceptable for ncmpcpp, or should it report "playing" until the fade ends?
- Should a manual `set_level` during an overlap rescale the envelope's reference level (proposed) or settle the overlap first?
- Unverified from the grounding, and it needs a live-libmpv `#[ignore]` test: an outgoing EOF inside a 1.5 s skip window (proposed: early commit), and End-before-Start ordering across decks. The actor drops events from a deck that has no `Leg`.
- Coalescing replies `Started` to superseded callers. Is that right for the journal's `PlanOutcome::render`, or should they get a distinct `Superseded`?

## Next implementation step

Write `engine/plan.rs` (`plan_transition`, `Shape::degrade`, `Reaim` selection) with table tests over `Snapshot`, since it is pure and pins the policy before the actor exists.
