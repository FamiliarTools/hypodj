//! The UNDO JOURNAL: what an [`Action`] did, in the daemon's own words, and the
//! world it did it to.
//!
//! ## Why this exists
//!
//! The agent surface (`plan add ... origin mcp:<sess>`) is NOT bounded by
//! permission - the MPD socket is unauthenticated, so a pre-act gate on it is
//! theatre. What IS buyable is RETRACTABILITY: every [`Action`] execution records
//! the world it replaced plus the REAL [`PlanOutcome::render`] text, so the
//! human's decision moves from "may it?" (before, in a box, mid-turn) to "keep
//! it?" (after, in the app he is already looking at, one keypress from undo).
//!
//! This module holds the DATA and the ring; the restoration primitive itself
//! (`HypodjHandler::install_snapshot`) lives with the state it puts back.
//!
//! ## What a no-op must never do
//!
//! The ring of 32 is small on purpose (it is a scrollback, not a database), so an
//! action that changed NOTHING must not consume a slot - and, more importantly,
//! must never become the TOP undoable entry, because `undo` with no argument
//! retracts the top and a no-op sitting there would answer "nothing happened" to a
//! human asking for his queue back. [`changed_anything`] is that test, and it
//! reads the REAL outcome (`added 0` is a no-op no matter what was asked for),
//! backstopped at the call site by a world-key comparison so a PARTIALLY applied
//! failure - a multi-track enqueue that errored after the third append - is still
//! journaled.

use std::collections::VecDeque;
use std::sync::Mutex;

use tokio::time::Instant;

use crate::handler::{PlanOutcome, QueueItem};
use crate::plan::Action;
use crate::player::PlayState;

/// How many entries the ring keeps. A scrollback, not a database: the natural
/// lifetime of an undo is "until something else touches the world" (see
/// [`WorldKey`]), so entries go stale long before they are evicted. Small also
/// bounds the cost of the honest part - each undoable entry owns a CLONE of the
/// whole queue.
pub(crate) const RING_CAP: usize = 32;

/// The world an [`Action`] replaced: enough to put it back row for row.
///
/// The queue is cloned WHOLE (rows carry their stable qids, so a restore is
/// identity-preserving - the ids ncmpcpp and dj-gui hold stay valid) rather than
/// stored as a diff: a diff would have to be INVERTED per action variant, and
/// every new variant would silently get a wrong inverse. A clone cannot be wrong.
#[derive(Clone)]
pub(crate) struct Snapshot {
    /// The queue rows, in order, with their ids.
    pub(crate) queue: Vec<QueueItem>,
    /// `State::next_id` at capture. Restored as a MAX (never rolled back), so an id
    /// minted after the snapshot can never be re-issued to a different song.
    pub(crate) next_qid: u64,
    /// The index of the current row, if any.
    pub(crate) current: Option<usize>,
    /// The playhead position of the current row, off the live elapsed atomic.
    pub(crate) elapsed_ms: u64,
    /// The REPORTED play state (pending-pause aware), not mpv's raw state.
    pub(crate) state: PlayState,
    /// The user-facing baseline volume - what a manual `setvol` sets, not the live
    /// mid-fade gain.
    pub(crate) target_volume: u8,
}

/// The DRIFT DETECTOR: the coarse shape of the world, compared before an undo is
/// allowed to overwrite it. `undo` REFUSES (loudly, with an ACK) when the live key
/// no longer equals the one recorded just after the action, because anything else
/// would clobber whatever moved in between.
///
/// Each component earns its place by naming a drift the others cannot see:
///
/// - `playlist_version` - bumped by EVERY queue mutation (add, delete, clear,
///   move, the spent-row trim), so it catches a human `add` from ncmpcpp, an
///   autofill top-up and a consume eviction. It is the only component that moves
///   on a pure queue edit.
/// - `current_qid` - the stable id of the current row. An EOF advance moves the
///   playhead to the next row WITHOUT touching the queue, so a version-only key
///   would let an undo yank playback back to a track already finished. A qid, not
///   an index, because `State::trim_spent` shifts every index.
/// - `state` - the reported (pending-pause aware) transport. A human pause or stop
///   between act and undo changes neither the version nor the current row.
/// - `target_volume` - assigned by `setvol`, the knob, MPRIS and every fade
///   terminal with NO version bump, so a version-only key would let an undo
///   silently revert the human's own volume drag.
///
/// Deliberately NOT in the key: the ELAPSED position. It drifts every tick while
/// music plays, so including it would expire every undo within milliseconds. The
/// cost is named rather than hidden - undo cannot un-hear the seconds between the
/// act and the retraction, and does not pretend to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WorldKey {
    pub(crate) playlist_version: u64,
    pub(crate) current_qid: Option<u64>,
    pub(crate) state: PlayState,
    pub(crate) target_volume: u8,
}

/// One thing that happened: attributable, and (while its key holds) retractable.
pub(crate) struct JournalEntry {
    /// Monotonic id, minted per push and never reused. `undo <jid>` names it.
    pub(crate) id: u64,
    /// When it happened, on the shared tokio clock - so ages are deterministic
    /// under `#[tokio::test(start_paused)]`, never wall-clock.
    pub(crate) at: Instant,
    /// The plan `origin` string, RECORDED and never tested: a spoofed or missing
    /// origin can never switch journaling off. `mcp:<sess>` for the agent.
    pub(crate) origin: String,
    /// The action's verb, for a compact human line.
    pub(crate) act: &'static str,
    /// The daemon's OWN words: [`PlanOutcome::render`], never `echo::render_dsl`
    /// (a partial, non-injective renderer) and never a caller's claim.
    pub(crate) did: String,
    /// The world to put back, or `None` when this entry is not undoable (a claimed
    /// entry, or the record of an undo).
    pub(crate) snap: Option<Snapshot>,
    /// The world key as it stood immediately AFTER the action, with one deliberate
    /// adjustment: for a fade that commits its baseline only at its terminal, the
    /// volume slot holds the level the envelope is DRIVING to rather than the one
    /// it has not reached yet (see [`JournalEntry::is_live`]).
    pub(crate) post_key: WorldKey,
}

impl JournalEntry {
    /// Is this entry still an honest description of the live world - i.e. may an
    /// undo overwrite it?
    ///
    /// Exact match on version / current row / transport. The VOLUME slot accepts
    /// TWO values, the post level and the snapshot's pre level, because those are
    /// exactly the states in which nobody else touched the knob: a non-committing
    /// fade (`fade to 40 10`) leaves `target_volume` at the PRE level for the whole
    /// ramp and commits the POST level only at its terminal, so a single-value rule
    /// would have to call one of the two halves of the action's own lifetime
    /// "drift". The admitted coincidence is named: a human who happens to set
    /// exactly one of those two levels is INDISTINGUISHABLE from the fade landing
    /// (or from nothing having happened), and undo will restore the pre level over
    /// it - one keypress to put back, against refusing every volume retraction for
    /// the length of its own envelope.
    pub(crate) fn is_live(&self, live: &WorldKey) -> bool {
        let Some(snap) = self.snap.as_ref() else { return false };
        live.playlist_version == self.post_key.playlist_version
            && live.current_qid == self.post_key.current_qid
            && live.state == self.post_key.state
            && (live.target_volume == self.post_key.target_volume
                || live.target_volume == snap.target_volume)
    }
}

/// One journal row as the wire renders it (`jid` / `jage` / `jorigin` / `jact` /
/// `jdid` / `jundoable`). `undoable` is computed against the LIVE key at read
/// time, so a client badge that says "u to undo" is telling the truth NOW rather
/// than at record time.
pub(crate) struct JournalView {
    pub(crate) id: u64,
    pub(crate) age_secs: u64,
    pub(crate) origin: String,
    pub(crate) act: &'static str,
    pub(crate) did: String,
    pub(crate) undoable: bool,
}

/// Why an `undo` refused. Every arm is a LOUD refusal (an ACK), never a silent
/// no-op: a human who pressed undo and saw nothing happen would press it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UndoRefusal {
    /// Nothing in the ring carries a snapshot, or the named id is gone (evicted,
    /// already claimed, or never existed).
    Nothing,
    /// The named id is there and undoable, but it is not the TOP undoable entry.
    /// Undo is a stack, not a random-access editor - retracting an older entry
    /// would silently discard everything stacked on top of it.
    NotTop,
    /// The world moved under it (see [`WorldKey`]).
    Stale,
}

/// What a successful [`Journal::take_undoable`] hands back.
pub(crate) struct Taken {
    pub(crate) id: u64,
    pub(crate) did: String,
    pub(crate) snap: Snapshot,
}

/// The ring. Guarded by a `std::sync::Mutex` that is NEVER held across an
/// `.await`: every method here is synchronous and short, and the restoration it
/// feeds runs after the lock is released.
pub(crate) struct Journal {
    inner: Mutex<Inner>,
}

struct Inner {
    ring: VecDeque<JournalEntry>,
    next_id: u64,
}

impl Journal {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner { ring: VecDeque::with_capacity(RING_CAP), next_id: 1 }),
        }
    }

    /// Record one thing that happened, evicting the oldest entry when full.
    /// Returns the minted id.
    pub(crate) fn push(
        &self,
        at: Instant,
        origin: &str,
        act: &'static str,
        did: String,
        snap: Option<Snapshot>,
        post_key: WorldKey,
    ) -> u64 {
        let mut g = self.inner.lock().unwrap();
        let id = g.next_id;
        g.next_id += 1;
        if g.ring.len() == RING_CAP {
            g.ring.pop_front();
        }
        g.ring.push_back(JournalEntry {
            id,
            at,
            origin: origin.to_string(),
            act,
            did,
            snap,
            post_key,
        });
        id
    }

    /// The ring NEWEST FIRST, so a client reading only the first block is reading
    /// the entry `undo` would retract.
    pub(crate) fn views(&self, now: Instant, live: &WorldKey) -> Vec<JournalView> {
        let g = self.inner.lock().unwrap();
        // The top undoable entry is the ONLY one `undo` will act on, so no other
        // row may advertise itself as undoable - a badge drawn from a lower row
        // would promise a retraction the stack refuses.
        let top = g.ring.iter().rposition(|e| e.snap.is_some());
        g.ring
            .iter()
            .enumerate()
            .rev()
            .map(|(i, e)| JournalView {
                id: e.id,
                age_secs: now.saturating_duration_since(e.at).as_secs(),
                origin: e.origin.clone(),
                act: e.act,
                did: e.did.clone(),
                undoable: top == Some(i) && e.is_live(live),
            })
            .collect()
    }

    /// Claim the snapshot of the TOP undoable entry (or of `want`, which must BE
    /// the top), leaving the entry in the ring but no longer undoable.
    ///
    /// The snapshot is taken OUT under this lock rather than after a successful
    /// restore, so two concurrent undos can never both install the same world. The
    /// cost is that a restore failing mid-way leaves no retryable entry behind -
    /// deliberate: a half-applied restore is exactly the state one must not blindly
    /// re-drive.
    pub(crate) fn take_undoable(
        &self,
        want: Option<u64>,
        live: &WorldKey,
    ) -> Result<Taken, UndoRefusal> {
        let mut g = self.inner.lock().unwrap();
        let Some(idx) = g.ring.iter().rposition(|e| e.snap.is_some()) else {
            return Err(UndoRefusal::Nothing);
        };
        if let Some(want) = want {
            if g.ring[idx].id != want {
                // Distinguish "gone" (evicted / already claimed / never existed)
                // from "there, but not the top".
                return Err(if g.ring.iter().any(|e| e.id == want) {
                    UndoRefusal::NotTop
                } else {
                    UndoRefusal::Nothing
                });
            }
        }
        if !g.ring[idx].is_live(live) {
            return Err(UndoRefusal::Stale);
        }
        let e = &mut g.ring[idx];
        let snap = e.snap.take().expect("rposition only finds entries carrying a snapshot");
        Ok(Taken { id: e.id, did: e.did.clone(), snap })
    }

    /// TEST-ONLY: how many entries the ring holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().unwrap().ring.len()
    }
}

/// The verb for an [`Action`], for the compact `jact` field. Short and stable -
/// the human-readable half is `jdid`, which is the daemon's real outcome text.
pub(crate) fn action_verb(action: &Action) -> &'static str {
    match action {
        Action::Fade(_) => "fade",
        Action::Stop => "stop",
        Action::Pause => "pause",
        Action::SetVolume(_) => "setvol",
        Action::Enqueue { .. } => "enqueue",
        Action::Wake { .. } => "wake",
        Action::Remove { .. } => "remove",
        Action::Move { .. } => "move",
        Action::Clear { .. } => "clear",
        Action::Play { .. } => "play",
        Action::PlayNow { .. } => "playnow",
        Action::Noop => "noop",
        // EXHAUSTIVE on purpose (`Action` is #[non_exhaustive] only outside this
        // crate): a new variant must fail to compile HERE, so nobody adds an action
        // the journal cannot name.
    }
}

/// Did this action change ANYTHING? Read off the REAL outcome, so a selector that
/// resolved to nothing (`added 0`, `removed 0`) is honestly a no-op regardless of
/// what was asked for.
///
/// [`PlanOutcome::Failed`] reads as "changed nothing" HERE and is backstopped at
/// the call site by a world-key comparison: an enqueue that appended three tracks
/// and then errored moved the key, so it is journaled anyway. That split keeps the
/// common case (a fetch that failed before touching anything) out of the ring
/// without ever losing a partially-applied edit.
pub(crate) fn changed_anything(outcome: &PlanOutcome) -> bool {
    match outcome {
        PlanOutcome::Added { n, .. } => *n > 0,
        PlanOutcome::Played { n, .. } => *n > 0,
        PlanOutcome::Removed(n) => *n > 0,
        PlanOutcome::Moved(n) => *n > 0,
        PlanOutcome::Cleared(n) => *n > 0,
        PlanOutcome::Jumped(n) => *n > 0,
        // Every immediate effect (fade / stop / pause / setvol / wake) moved the
        // transport or the level, even when the world key cannot see it yet (a
        // fade commits its baseline only at its terminal).
        PlanOutcome::Effect(_) => true,
        PlanOutcome::Failed(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::QueueEntry;
    use crate::plan::{ClearScope, QueueSelector};

    fn key(version: u64, qid: Option<u64>, state: PlayState, vol: u8) -> WorldKey {
        WorldKey { playlist_version: version, current_qid: qid, state, target_volume: vol }
    }

    fn snap(vol: u8) -> Snapshot {
        Snapshot {
            queue: vec![QueueItem::queued(
                7,
                QueueEntry::Stream { url: "http://x/1".into(), title: "one".into() },
            )],
            next_qid: 8,
            current: Some(0),
            elapsed_ms: 1_000,
            state: PlayState::Playing,
            target_volume: vol,
        }
    }

    // The ring is a scrollback: at 32 the OLDEST entry falls off, ids keep
    // climbing (never reused), and an evicted id is no longer undoable.
    #[tokio::test(start_paused = true)]
    async fn ring_evicts_the_oldest_at_thirty_two() {
        let j = Journal::new();
        let k = key(1, None, PlayState::Stopped, 50);
        for _ in 0..(RING_CAP + 5) {
            j.push(Instant::now(), "mcp:t", "clear", "cleared 1".into(), Some(snap(50)), k);
        }
        assert_eq!(j.len(), RING_CAP, "the ring never grows past its cap");
        let views = j.views(Instant::now(), &k);
        assert_eq!(views.len(), RING_CAP);
        // Newest first; ids 1..=5 were evicted, so the tail is id 6.
        assert_eq!(views[0].id, (RING_CAP + 5) as u64);
        assert_eq!(views[RING_CAP - 1].id, 6);
        // An evicted id cannot be undone: it is GONE, not merely not-top.
        assert_eq!(j.take_undoable(Some(3), &k).err(), Some(UndoRefusal::Nothing));
    }

    // Only the TOP undoable entry may be retracted, and only once.
    #[tokio::test(start_paused = true)]
    async fn undo_is_a_stack_and_each_entry_is_claimed_once() {
        let j = Journal::new();
        let k = key(1, None, PlayState::Stopped, 50);
        let older =
            j.push(Instant::now(), "mcp:t", "remove", "removed 1".into(), Some(snap(50)), k);
        let newer =
            j.push(Instant::now(), "mcp:t", "clear", "cleared 3".into(), Some(snap(50)), k);

        // Naming an older entry refuses rather than silently discarding the newer.
        assert_eq!(j.take_undoable(Some(older), &k).err(), Some(UndoRefusal::NotTop));
        // The top goes back.
        let t = j.take_undoable(None, &k).expect("the top is undoable");
        assert_eq!(t.id, newer);
        assert_eq!(t.did, "cleared 3");
        // Claimed: it is no longer undoable, and the older entry is now the top.
        let t2 = j.take_undoable(None, &k).expect("the older entry is now the top");
        assert_eq!(t2.id, older);
        assert_eq!(j.take_undoable(None, &k).err(), Some(UndoRefusal::Nothing));
    }

    // Every drift class the key claims to detect, refused one at a time.
    #[tokio::test(start_paused = true)]
    async fn each_drift_class_refuses() {
        let post = key(9, Some(4), PlayState::Playing, 40);
        // Pre-volume 55, post 40: the two halves of a fade's own lifetime.
        let mk = || {
            let j = Journal::new();
            j.push(Instant::now(), "mcp:t", "fade", "fading".into(), Some(snap(55)), post);
            j
        };
        // A human `add` (or an autofill top-up): the version moved.
        assert_eq!(
            mk().take_undoable(None, &key(10, Some(4), PlayState::Playing, 40)).err(),
            Some(UndoRefusal::Stale)
        );
        // An EOF advance: the current row moved with the queue untouched.
        assert_eq!(
            mk().take_undoable(None, &key(9, Some(5), PlayState::Playing, 40)).err(),
            Some(UndoRefusal::Stale)
        );
        // A human pause.
        assert_eq!(
            mk().take_undoable(None, &key(9, Some(4), PlayState::Paused, 40)).err(),
            Some(UndoRefusal::Stale)
        );
        // A human volume drag to a THIRD level (neither pre nor post).
        assert_eq!(
            mk().take_undoable(None, &key(9, Some(4), PlayState::Playing, 30)).err(),
            Some(UndoRefusal::Stale)
        );
        // But the fade's own lifetime is not drift: mid-ramp (still at the PRE
        // level) and settled (at the POST level) both undo.
        assert!(mk().take_undoable(None, &key(9, Some(4), PlayState::Playing, 55)).is_ok());
        assert!(mk().take_undoable(None, &key(9, Some(4), PlayState::Playing, 40)).is_ok());
    }

    // A view says "undoable" only for the one entry `undo` would act on, and only
    // while its key holds.
    #[tokio::test(start_paused = true)]
    async fn only_the_live_top_advertises_itself_as_undoable() {
        let j = Journal::new();
        let k = key(1, None, PlayState::Stopped, 50);
        j.push(Instant::now(), "mcp:t", "remove", "removed 1".into(), Some(snap(50)), k);
        j.push(Instant::now(), "mcp:t", "clear", "cleared 3".into(), Some(snap(50)), k);
        let v = j.views(Instant::now(), &k);
        assert!(v[0].undoable, "the top is undoable");
        assert!(!v[1].undoable, "an entry buried under a newer one is not");
        // Drift: nothing advertises itself.
        let drifted = key(2, None, PlayState::Stopped, 50);
        assert!(j.views(Instant::now(), &drifted).iter().all(|v| !v.undoable));
    }

    // The age comes off the shared tokio clock, so it is deterministic under
    // paused time (never wall-clock).
    #[tokio::test(start_paused = true)]
    async fn the_age_comes_off_the_fake_clock() {
        let j = Journal::new();
        let k = key(1, None, PlayState::Stopped, 50);
        j.push(Instant::now(), "mcp:t", "clear", "cleared 3".into(), Some(snap(50)), k);
        tokio::time::advance(std::time::Duration::from_secs(12)).await;
        assert_eq!(j.views(Instant::now(), &k)[0].age_secs, 12);
    }

    // A no-op outcome is a no-op no matter what was ASKED for.
    #[test]
    fn changed_anything_reads_the_real_count() {
        assert!(!changed_anything(&PlanOutcome::Added { n: 0, selector: "\"x\"".into() }));
        assert!(changed_anything(&PlanOutcome::Added { n: 3, selector: "\"x\"".into() }));
        assert!(!changed_anything(&PlanOutcome::Removed(0)));
        assert!(changed_anything(&PlanOutcome::Removed(2)));
        assert!(!changed_anything(&PlanOutcome::Cleared(0)));
        assert!(!changed_anything(&PlanOutcome::Jumped(0)));
        assert!(!changed_anything(&PlanOutcome::Played {
            n: 0,
            title: None,
            selector: "\"x\"".into()
        }));
        assert!(changed_anything(&PlanOutcome::Effect("fading".into())));
        assert!(!changed_anything(&PlanOutcome::Failed("boom".into())));
    }

    #[test]
    fn verbs_name_the_destructive_actions() {
        assert_eq!(action_verb(&Action::Clear { scope: ClearScope::All }), "clear");
        assert_eq!(action_verb(&Action::Remove { sel: QueueSelector::Qid(3) }), "remove");
        assert_eq!(action_verb(&Action::Noop), "noop");
    }
}
