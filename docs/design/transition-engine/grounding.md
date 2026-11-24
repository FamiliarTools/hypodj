# Grounding: hypodj head changes today (master + wave 7, handler.rs line refs approx)

Goal (Guilherme, task 68h5sg2, design doc ~/Projects/hypodj-nodegraph-wt/docs/design/node-graph-architecture.md "Decisive verdict"):
ONE always-running transition engine; EVERY head change (skip, EOF, remove current, full queue swap, play N, radio/autofill start, MPRIS next) RETARGETS it, never a bespoke branch. Proof case: replacing the whole queue including the playing track crossfades seamlessly into the new first track. Seamlessness must emerge from one path, not special-casing.

## Shapes today
XF = equal-power two-deck crossfade; DIP = duck-swap-rise (Terminal::SkipLoad); CUT = hard loadfile at baseline; SIL = start from silence; GAPLESS = mpv advance into parked warm entry.

| Trigger | Path | Shape |
|---|---|---|
| MPD next/previous (playing) | user_skip 13030 -> skip_with_fade 13226 | XF if crossfade_skip_dur() (default 1.5s) and incoming loads, else DIP; paused/stopped CUT |
| delete playing row | delete_at case 3 -> skip_with_fade(idx, Some(evict)) | XF/DIP; no successor: FadeIntent::OutEvicting |
| delete row a skip is heading to | delete_at case 0 | settle_crossfade(false)/cancel dip, rise back on old track |
| play N / playid | play_index (resync=true): settle_crossfade(true), cancel fade, set baseline, play_url | CUT even while playing |
| clear+add+play (queue swap) | Clear stops deck, then play_index | CUT from stopped (proof case FAILS) |
| plan Play / PlayNow | play_index | CUT |
| radio start / autofill gesture | play_index | CUT |
| autofill walk, continuation station | play_index_inner(false) (no resync: winddown ramp survives) | CUT |
| EOF advance cold | advance_on_eof -> play_index_inner(false) | CUT |
| EOF advance warm | commit_landed_advance | GAPLESS |
| boundary crossfade | director tick -> maybe_boundary_crossfade 12809 -> start_crossfade(..,true) | XF (crossfade_secs default 0 = off); skips same_album_run |
| wake / resume restore / undo install_snapshot | play_index_from_silence | SIL + WakeTo ramp |
| MPRIS next/previous | play_index directly | CUT (diverges from MPD next) |

Three entry gates: play_index (gesture: resync + arm_eof_fuse_for_gesture), play_index_inner(false) (autonomous), play_index_from_silence (inner(false) + retire pending_pause + arm EOF fuse). State commit tail duplicated 4x: play_index_inner tail, commit_landed_advance, crossfade_task terminal, SkipLoad terminal.

## Player decks (player.rs)
spawn_decks: one libmpv actor thread per deck, shared lossless evt_tx; falls back to 1 deck. PlayerHandle{decks, active}; can_crossfade() = decks>=2 (false for null player / encode probe).
Crossfade API: crossfade_arm (spare at vol 0, PlayUrl, does NOT flip active - outgoing owns identity until commit, abort safe); start_crossfade bumps fade_epoch, sets fading+crossfading, pending_skip=Some(idx) (status reports target at once), pending_crossfade_evict/finish, baseline_committed=false; crossfade_task runs fade.rs run_crossfade (set_pair_db writes incoming first; crossfade_gains sin/cos); terminal under slot lock: crossfade_commit(attribute_finish) = EmitCrossfadedEof (if attributed) -> active.fetch_add -> Stop outgoing; then set_volume(baseline) + state commit, current re-derived from QueueId. settle_crossfade(promote) finishes early (commit, or abort only when incoming deleted); idempotent; called before any other gain-taking path.
Warm slot: PrefetchWarm parks target as 2nd mpv playlist entry; kinds Skip / Continuation; phantom-EOF guard swallows natural EOF while a Skip warm is parked; near-EOF guard current_can_warm; continuation warm = gapless handoff (commit_landed_advance).
EOF spine: Eof via blocking_send (lossless), carries crossfaded: bool. During overlap the outgoing deck owns identity; the crossfade COMMIT is the advance; crossfaded=true Eof -> director TrackEnd + mark_advanced, no advance_on_eof. Skip XF emits no Eof (no scrobble). Unverified: outgoing EOF within a 1.5s skip window; End-before-Start ordering across the overlap.

## Fade slot invariants (CLAUDE.md)
One FadeSlot; every installer locks, aborts+joins old handle, bumps fade_epoch; terminals epoch-guarded under slot lock. std Mutex<State> never held across .await. Manual volume change cancels in-flight fade atomically under one slot lock. Crossfade is a non-committing envelope (live_gain_db stays baseline). Aborting a crossfade alone strands a second audible deck, hence settle-first. Eof/queue-advance stays on a lossless channel. Retractability: every plan Action runs through run_action_outcome (journal snapshot); undo install_snapshot.

## Decision points
crossfade_skip_dur(): can_crossfade && crossfade_skip_secs>0 (1.5s) -> XF else DIP (skip_fade_secs 0.35). maybe_boundary_crossfade gates: crossfade_secs>0 (default 0), remaining<=dur, not already crossfading/fading/pending_skip, next != cur, !same_album_run (same album_id and track==prev+1 stays gapless). Fallbacks: arm failure -> DIP -> CUT; never a no-op.

## What collapses under retarget(target, cause)
skip_with_fade, install_skip_dip, Terminal::SkipLoad, play_index_evicting, user_skip, skip-warm path (prefetch_warm/switch_warmed/WarmKind::Skip guard), the 4 duplicate commits; divergences (MPRIS, play/playid, plan Play, radio, autofill) fixed for free.

## Semantics that must survive (engine inputs)
1. from-silence when nothing audible (stopped, wake, restore, undo install) - mpv vol 0 before first buffer; ramp owned by caller/intent.
2. gesture vs autonomous: gesture resyncs (cancel fade, reset target volume, clear pending_pause, arm EOF fuse); autonomous must not kill a winddown/sleep ramp.
3. finish attribution: skip/evict = no scrobble; natural boundary = attributed Eof with crossfaded suppression.
4. gapless same-album and continuation warm (zero-length transition / mpv advance).
5. evict atomicity: delete-playing commits evict with the head change.
6. retarget mid-flight with only 2 decks: a third target while overlapping must promote or re-aim without stranding a deck.
7. single-deck floor: can_crossfade false -> DIP.
8. EOF spine: crossfaded Eof + mark_advanced, pending_skip as reported target, EOF fuse, fade_epoch.
9. load failure falls through, never a no-op.
