# Candidate B: declarative head, one reconciler

## Problem
Every head change in hypodj is a bespoke command path (skip_with_fade, play_index x3, SkipLoad terminal, warm
switch, commit_landed_advance, crossfade terminal), each re-deciding transition shape, gain resync and state
commit. The proof case (swap the queue including the playing track, crossfade into the new first) fails because
Clear stops the deck and play_index cuts. Constraints carried from grounding: two decks max; Eof on a lossless
channel; the fade slot's epoch-guarded terminals; std Mutex never held across await; gesture vs autonomous
semantics; finish attribution (skip = no scrobble); evict atomicity; single-deck floor; load failure never a no-op.

Guilherme's reframe: removing a song is not a removal, it is the declaration of a new state. So call sites stop
saying how and say only what: the queue, the head, and whether to play.

## Usage (caller's view)
Call sites never touch the player for a head change. They write the declaration and return.

```rust
// MPD next / MPRIS next / plan Skip - identical:
let qid = st.successor_of_head();
st.declare_head(qid, Intent::Play, Cause::Gesture);

// play N / playid / plan Play / PlayNow / radio start / autofill gesture:
st.declare_head(Some(qid_n), Intent::Play, Cause::Gesture);

// clear + add + play, or the PROOF CASE (swap whole queue incl. playing track):
st.declare_queue(new_items, first_qid, Intent::Play, Cause::Gesture);   // audible qid not in new queue => Xf

// delete the playing row:
st.remove_row(qid);        // head becomes successor, cause Removed; no evict plumbing

// EOF (director, sync, lossless):  on_eof(&mut st, ev);   // declares head := successor, Cause::Natural

// wake / restore / undo install_snapshot:
st.declare_queue(snap.items, snap.head, Intent::Play, Cause::FromSilence);
```
Each declaring mutator ends with `notify_change()` which already is the universal commit choke; it now also does
`reconciler.poke()`. Nobody awaits a transition. Callers that need "it landed" (tests, `play` reply) await
`handler.settled(rev)`, a watch on the reconciler's committed rev.

## Shape
**State.** `State` gains `declared: Declared` (head by QueueId, intent, cause, rev). It REPLACES `pending_skip`,
`pending_crossfade_evict`, `pending_crossfade_finish`, and the index-valued `current` becomes derived
(`queue.position(committed_qid)`). Status/MPRIS report `declared.head` immediately - what pending_skip faked.
`pending_pause` collapses into `intent == Pause`. One owner per fact (split ownership red flag avoided).

**Audible** is derived, not stored: `Silent | Single{qid,warm} | Overlap{out,inn}` from PlayerHandle (active deck,
parked warm) plus the single in-flight overlap record the reconciler holds. The old `crossfading` flag is just
`Overlap`.

**plan(audible, declared, caps) -> Step** is the only place shape policy exists. Table:
- head == audible qid (or Single.warm matches natural cause with same_album_run/continuation) -> Settled / Gapless.
- audible Silent, or cause FromSilence -> Begin{FromSilence}.
- intent Pause/Stop or head None -> Quiesce.
- cause Gesture/Natural/Removed with caps.can_crossfade && skip>0 && incoming_loads -> Xf(skip); Natural at a
  boundary uses crossfade_bound and attribute_finish=true; Removed/Gesture attribute false (no scrobble).
- else Dip if incoming_loads else Cut (the ladder; never a no-op).
- Overlap: head==inn -> AwaitOverlap; head==out -> Reverse (today's delete_at case 0); other -> FinishThenReplan.

**Reconciler.** One task. `poke()` = `Notify::notify_one` (a coalescing permit). Loop: lock, snapshot
(audible, declared, rev), unlock; `plan`; execute the step OUTSIDE the lock; relock and commit only if `rev` is
unchanged, else discard (a loaded-but-stale incoming deck is aborted; crossfade_arm never flips `active`, so abort
is safe) and loop. Terminates on Settled. Because it re-reads the level each iteration, a poke during execution is
never lost, and running it twice with no new declaration plans Settled (idempotent by construction, and
property-tested on `plan`).

**Single commit tail.** The 4 duplicated commits become `commit_head(qid)` inside the reconciler: set committed
qid, clear fresh_enqueue_anchor, trim_spent + heard rows, set_store_skip_pin(None), notify, reschedule warm. The
Cause hook `on_declared` (gesture: cancel fade + baseline + clear pending_pause + arm EOF fuse; Natural: none;
FromSilence: vol 0, retire pending_pause after landing) runs once, in the declarer, under the same lock as the
declaration, so the atomic "cancel + mutate under one slot lock" invariant holds.

**Lossless EOF.** Eof stays on the blocking_send mpsc into the director. The director applies `on_eof` as a pure
state write (declare successor, Cause::Natural; or, if Eof carries `crossfaded=true`, `mark_advanced` only - the
overlap commit already declared nothing new). Writing a level is lossless even if pokes coalesce: the declaration
persists until converged; only the Notify is lossy, and it is a hint. A premature Eof for the out deck inside a
skip window finds `declared.rev` newer than the Eof's `qid`-tagged identity and is ignored as stale (closes the
unverified grounding gap with a rule, not a timing assumption). Load failure feeds back as a declaration
(`head := next playable`, fuse-bounded), never a silent drop.

**Two decks bound it.** `Audible::Overlap` is the maximum. A third declaration can never need a third deck
because plan maps it to `Reverse` or `FinishThenReplan` (bounded fast-finish <= caps.settle, then a normal
Single->new transition). The bound is a type fact: there is no Audible with three tracks. Single-deck floor:
`can_crossfade=false` makes Xf unreachable in `plan`; ladder lands on Dip.

**Retractability.** Plan actions already run through run_action_outcome; they now end in a declaration, and the
journal snapshots (queue, head qid, intent). Undo = declare the snapshot with Cause::FromSilence/Gesture; the
reconciler makes it audible. No Action bypass is possible since the player is not reachable from Action code.

**Interface depth.** Public surface: 3 declarers + `on_eof` + `settled(rev)`. Hidden: shape selection, deck
choreography, warm parking, evict, fade-slot settlement, attribution, commit, fallback ladder. The deleted
`settle_crossfade`-before-X rule becomes unrepresentable: only the reconciler takes the gain on head changes.
(per boundary-discipline, make-operations-idempotent, separate state owner.)

### Module map
- `reconcile.rs` (new): Declared, Audible, Caps, Shape, Step, `plan` (pure), Reconciler.
- `handler.rs`: State loses 3 pending_* fields; keeps volume/fade owners; declarers + notify_change poke.
- `player.rs`: unchanged API (crossfade_arm/commit/play_url/prefetch_warm). Warm = only Gapless/continuation.
- `fade.rs`: unchanged (run_crossfade, ramps). `director.rs`: Eof -> `on_eof`; boundary tick becomes a
  declaration (`Natural`, shape decided by plan) rather than maybe_boundary_crossfade.

## Call-site migration (grounding table)
| Trigger | New |
|---|---|
| MPD next/prev, user_skip | declare_head(succ/pred, Play, Gesture) |
| MPRIS next/prev | same call (divergence gone) |
| delete playing | remove_row -> Removed |
| delete row a skip heads to | remove_row; head moved to successor; plan sees Overlap with head!=inn -> FinishThenReplan or Reverse |
| play N / playid / plan Play / PlayNow | declare_head(.., Gesture) |
| clear+add+play, queue swap | declare_queue(.., Gesture) -> Xf (proof) |
| radio start / autofill gesture | declare_head(.., Gesture) |
| autofill walk / continuation | declare_head(.., Natural) (no resync: ramp survives) |
| EOF cold | on_eof -> Natural -> plan: Cut/Xf as caps allow (see tradeoffs) |
| EOF warm | on_eof, plan sees Single.warm==head -> Gapless |
| boundary crossfade | director tick declares Natural early (head=next) once remaining<=dur and crossfade_bound>0 and !same_album_run; plan picks Xf(bound, attribute) |
| wake / restore / undo | declare_queue(.., FromSilence) |

## Code deleted
skip_with_fade, install_skip_dip, Terminal::SkipLoad (and its dip-specific commit), play_index_evicting,
user_skip, play_index/play_index_inner/play_index_from_silence (replaced by 3 declarers + executor),
commit_landed_advance + crossfade terminal commit + SkipLoad commit + inner tail (-> `commit_head`),
settle_crossfade call sites (kept as one internal fn in the reconciler), pending_skip, pending_crossfade_evict,
pending_crossfade_finish, FadeIntent::OutEvicting, WarmKind::Skip + its phantom-EOF guard + switch_warmed (Skip
warm no longer exists; plan decides Xf early instead), maybe_boundary_crossfade (as a handler method),
`Clear` stopping the deck.

## Synthesis decision
(Filled by arena.) Candidate B is the declarative base; graft from imperative candidates their `Transit` future
handle only if `settled(rev)` proves too coarse for tests.

## Tradeoffs accepted
- We accept one task and one Notify (new concurrency actor) in exchange for deleting ~10 command paths.
- We accept that a call's reply returns before audio changes (use `settled(rev)` when needed) in exchange for no
  caller coordinating with the player.
- We accept FinishThenReplan (a <=200ms truncated overlap) on a third declaration in exchange for the 2-deck bound
  needing no special-casing and no stranded deck.
- We accept that EOF cold with Natural may crossfade-or-cut by caps; a track that already ended has no outgoing
  audio, so plan returns Cut when `Audible` is Silent-after-EOF: indistinguishable and correct.
- We accept that status reports the declared head ahead of audio (as pending_skip did) in exchange for removing
  the index/qid dual bookkeeping.

## Alternatives considered
- Imperative `retarget(target, cause)` entry point: a deep call, but callers still pick Cause AND must await/sequence
  with the queue mutation; mutation and transition can disagree (queue swapped, retarget forgotten) - two ways to do
  one task, and "remove current" must pass an evict token. Declarative makes the disagreement unrepresentable.
- Per-trigger shape table in each handler (status quo, tidied): shallow, leaks policy into 12 sites.
- Event-sourced command queue into the engine: loses coalescing; mid-flight redeclare would need cancellation
  semantics that a level gives free.

## Open questions and risks
- Should `Intent::Pause` ride the same reconciler (fade-to-pause as Quiesce) or stay in the transport fade path for
  this wave?
- Does mpv premature-Ok on failed loadfile (noted in play_index) mean the reconciler can only detect load failure
  via the Eof fuse, so a failed Xf incoming is discovered late?
- Eof tagging by qid requires player events already carry qid (they do per play_url Some(qid)); verify End-before-Start
  ordering across the overlap before relying on the stale-Eof rule.
- Risk: reconcile loop livelock if a declarer re-declares on every commit; guard with rev-equality, not content.
- Risk: test ergonomics - null player must still resolve `settled(rev)`; plan is pure so most coverage is unit-level.

## Next implementation step
Write `reconcile.rs` with `plan` plus its table-driven and idempotence property tests against the grounding's nine
semantics, before touching handler.rs.
