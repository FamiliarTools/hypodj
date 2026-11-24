//! THE TRANSITION ENGINE: a declarative head and one reconciler.
//!
//! Every head change in hypodj - a skip, a natural end, removing the playing row, a
//! whole-queue swap, play N, radio or autofill starting, MPRIS next, wake, undo - is
//! the same act: a call site DECLARES what should be playing ([`Declared`]) and
//! returns. It never says how. The reconciler compares that declaration with what the
//! decks are actually doing ([`Audible`]) and the pure [`plan`] picks the one next
//! [`Step`] that moves the room toward it. Executing a step changes what is audible,
//! so the loop re-plans until [`Step::Settled`].
//!
//! That is why seamlessness is not special-cased anywhere: replacing the whole queue
//! including the playing track is "the audible row is not the declared head", which is
//! the same question a skip asks, and gets the same crossfade.
//!
//! The pure half (this section) owns ALL shape policy. It is total over its inputs and
//! pinned by the table and convergence tests at the bottom of the file.
//!
//! No em dashes, no double hyphens.

// The executor that consumes the planner lands in the next commit.
#![allow(dead_code)]

use std::time::Duration;

use crate::event::QueueId;

// ── declared state ─────────────────────────────────────────────────────────

/// Why the head was last declared. Written by the DECLARER (the only party that knows),
/// read by [`plan`] and by the declaration hook. One field replaces the old
/// gesture/autonomous bool, the play_index / inner / from_silence trio, the evicting
/// fade intent and the skip warm kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Cause {
    /// A human asked: next/prev/play N/playid/MPRIS/plan Play/radio start/queue swap.
    Gesture,
    /// The queue's own motion: EOF advance, a boundary crossfade, the autofill walk, a
    /// continuation station. Must not disturb a winddown or sleep ramp.
    Natural,
    /// The playing row was removed; the declared head is its successor. The outgoing
    /// track is abandoned, not finished.
    Removed,
    /// Nothing was audible by design: wake, resume restore, undo. The caller owns the
    /// rise; the engine guarantees the deck is silent before its first buffer.
    FromSilence,
}

impl Cause {
    /// A human asked for this head. Arms the EOF fuse for the walk that may follow and
    /// retires a standing pause intent once the head is audible.
    pub(crate) fn is_gesture(self) -> bool {
        !matches!(self, Cause::Natural)
    }

    /// Takes the gain back from whatever fade is running and re-asserts the baseline.
    /// A wake/undo owns its own rise from silence, and the queue's own motion must let
    /// a winddown ride across the boundary, so neither resyncs.
    pub(crate) fn resyncs_gain(self) -> bool {
        matches!(self, Cause::Gesture | Cause::Removed)
    }

    /// The outgoing track PLAYED OUT, so its finish is published and scrobbled. Only the
    /// queue's own motion finishes a track; a skip or a removal abandons it.
    pub(crate) fn attributes_finish(self) -> bool {
        matches!(self, Cause::Natural)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Intent {
    Play,
    /// `stop`: hold the head, silence the decks at once.
    Stop,
}

/// The ONLY thing call sites write about the head. `head` is a [`QueueId`], never an
/// index, so a whole-queue swap is "head changed" and a removed row is "the audible
/// row is not the head", never a special case.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Declared {
    pub head: Option<QueueId>,
    pub intent: Intent,
    pub cause: Cause,
    /// Bumped by every declaration. The executor commits a step's result only if this
    /// still matches (the same epoch discipline as the fade slot).
    pub rev: u64,
    /// The oldest load of `head` that satisfies this declaration. A gesture sets it to
    /// its own `rev`, so `play N` on the row already playing RESTARTS it (a fresh load,
    /// crossfaded in from the top) instead of being a no-op; a declaration that only
    /// re-states the head leaves it alone.
    pub fresh: u64,
}

impl Declared {
    pub(crate) fn idle() -> Self {
        Declared { head: None, intent: Intent::Play, cause: Cause::Natural, rev: 0, fresh: 0 }
    }
}

// ── audible state ──────────────────────────────────────────────────────────

/// One deck's identity: the row it plays and the declaration rev it was loaded under.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Leg {
    pub qid: QueueId,
    pub rev: u64,
}

/// What the decks are actually doing, derived for each plan from the player and the
/// engine's own deck record. Two decks is the maximum, and that is a type fact: there
/// is no variant with three tracks, so a third declaration can never need a third deck.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Audible {
    /// Nothing loaded, or the loaded track already ended.
    Silent,
    Single {
        leg: Leg,
        /// The deck is advancing audio (false: paused, which is the user's pause).
        playing: bool,
        /// A one-legged fade-out is in flight on it (clear, remove-last).
        leaving: bool,
        /// The head bookkeeping for this load has run (`commit_head`).
        committed: bool,
    },
    /// Two decks audible. `out` owns the identity until the overlap commits.
    Overlap { out: Leg, inn: Leg },
}

// ── the pure core ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub(crate) struct Caps {
    /// The player has a second deck.
    pub can_crossfade: bool,
    /// The gesture crossfade length; zero turns it off.
    pub crossfade_skip: Duration,
    /// The natural-boundary crossfade length; zero turns it off.
    pub crossfade_bound: Duration,
    /// The single-deck duck-swap-rise length.
    pub dip: Duration,
    /// The bound on fast-finishing an overlap a third declaration interrupts.
    pub settle: Duration,
    /// False once arming the second deck failed for this declaration: the ladder
    /// continues below the crossfade.
    pub incoming_loads: bool,
    /// This exact declaration already failed at the bottom of the ladder. Nothing is
    /// retried until something declares again.
    pub abandoned: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Shape {
    /// Equal-power two-deck overlap, starting from the outgoing leg's LIVE gain.
    Xf(Duration),
    /// Duck, swap at the trough, rise: the single-deck floor.
    Dip(Duration),
    /// A hard load at the baseline: nothing audible to transition from.
    Cut,
    /// Silent first, then load: the caller owns the rise.
    FromSilence,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Step {
    /// The room is what was declared.
    Settled,
    /// The head is audible but its bookkeeping has not run: the ONE commit.
    Commit,
    /// Load and transit to the head.
    Begin { shape: Shape, attribute_finish: bool },
    /// An overlap is carrying the room to the head: its own terminal finishes it.
    AwaitOverlap,
    /// An overlap is heading AWAY from the declared head: run its envelope back.
    Reverse,
    /// An overlap is heading to neither leg's row: finish it within `settle`, re-plan.
    FinishThenReplan,
    /// Nothing should play: fade out over the given length, or halt at once.
    Quiesce { fade: Option<Duration> },
}

/// PURE. All shape policy lives here and nowhere else.
///
/// `d.head` must already be resolved against the queue (a head whose row is gone is
/// `None`). Total; never returns a shape the caps forbid; the ladder Xf, Dip, Cut is
/// monotone in the caps; and a snapshot that reflects this step's effect plans
/// `Settled` within a bounded number of steps (pinned by the convergence test).
pub(crate) fn plan(a: Audible, d: &Declared, c: Caps) -> Step {
    let is_head = |leg: Leg| d.head == Some(leg.qid) && leg.rev >= d.fresh;
    if d.intent == Intent::Stop {
        return match a {
            Audible::Silent => Step::Settled,
            _ => Step::Quiesce { fade: None },
        };
    }
    match a {
        Audible::Overlap { out, inn } => {
            if is_head(inn) {
                Step::AwaitOverlap
            } else if is_head(out) {
                Step::Reverse
            } else {
                Step::FinishThenReplan
            }
        }
        Audible::Single { leg, playing, leaving, committed } => {
            if is_head(leg) && !leaving {
                return if committed { Step::Settled } else { Step::Commit };
            }
            if d.head.is_none() {
                return match (playing, leaving) {
                    // The fade-out is already carrying it to silence.
                    (_, true) => Step::Settled,
                    (true, false) => Step::Quiesce { fade: Some(quiesce_fade(c)) },
                    // Paused: nothing audible to smooth.
                    (false, false) => Step::Quiesce { fade: None },
                };
            }
            if c.abandoned {
                return Step::Settled;
            }
            if d.cause == Cause::FromSilence {
                return begin(Shape::FromSilence, false);
            }
            if !playing {
                return begin(Shape::Cut, false);
            }
            ladder(d.cause, c)
        }
        Audible::Silent => {
            if d.head.is_none() || c.abandoned {
                return Step::Settled;
            }
            let shape = if d.cause == Cause::FromSilence { Shape::FromSilence } else { Shape::Cut };
            begin(shape, false)
        }
    }
}

fn begin(shape: Shape, attribute_finish: bool) -> Step {
    Step::Begin { shape, attribute_finish }
}

/// From one audible, playing leg to a different head.
fn ladder(cause: Cause, c: Caps) -> Step {
    let xf_len = if cause.attributes_finish() { c.crossfade_bound } else { c.crossfade_skip };
    if c.can_crossfade && c.incoming_loads && !xf_len.is_zero() {
        return begin(Shape::Xf(xf_len), cause.attributes_finish());
    }
    // The queue's own motion with nothing to overlap (boundary crossfade off) never
    // ducks a track: a natural advance is a plain load, exactly as without the engine.
    if cause.attributes_finish() || c.dip.is_zero() {
        return begin(Shape::Cut, cause.attributes_finish());
    }
    begin(Shape::Dip(c.dip), false)
}

/// The fade-out length when the head goes away under a playing track: the gesture
/// crossfade length, so a `clear; add; play` that lands inside it overlaps exactly as a
/// skip would, or the dip length on a deck that cannot overlap.
fn quiesce_fade(c: Caps) -> Duration {
    if c.can_crossfade && !c.crossfade_skip.is_zero() {
        c.crossfade_skip
    } else {
        c.dip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKIP: Duration = Duration::from_millis(1500);
    const BOUND: Duration = Duration::from_secs(4);
    const DIP: Duration = Duration::from_millis(350);

    fn caps() -> Caps {
        Caps {
            can_crossfade: true,
            crossfade_skip: SKIP,
            crossfade_bound: BOUND,
            dip: DIP,
            settle: Duration::from_millis(200),
            incoming_loads: true,
            abandoned: false,
        }
    }

    fn leg(q: u64, rev: u64) -> Leg {
        Leg { qid: QueueId(q), rev }
    }

    fn playing(q: u64, rev: u64) -> Audible {
        Audible::Single { leg: leg(q, rev), playing: true, leaving: false, committed: true }
    }

    fn decl(head: Option<u64>, cause: Cause, rev: u64) -> Declared {
        Declared { head: head.map(QueueId), intent: Intent::Play, cause, rev, fresh: rev }
    }

    fn xf(len: Duration, attribute: bool) -> Step {
        Step::Begin { shape: Shape::Xf(len), attribute_finish: attribute }
    }

    // The grounding's nine semantics, one row (or a few) each, plus the behaviours the
    // engine exists for. Each row: (name, audible, declared, caps, expected step).
    #[test]
    fn plan_table() {
        let no_xf = Caps { can_crossfade: false, ..caps() };
        let arm_failed = Caps { incoming_loads: false, ..caps() };
        let rows: Vec<(&str, Audible, Declared, Caps, Step)> = vec![
            // 1. from silence
            ("stopped + play is a plain load", Audible::Silent, decl(Some(1), Cause::Gesture, 1), caps(), Step::Begin { shape: Shape::Cut, attribute_finish: false }),
            ("wake/undo while silent", Audible::Silent, decl(Some(1), Cause::FromSilence, 1), caps(), Step::Begin { shape: Shape::FromSilence, attribute_finish: false }),
            ("undo while another track plays still starts from silence", playing(2, 0), decl(Some(1), Cause::FromSilence, 1), caps(), Step::Begin { shape: Shape::FromSilence, attribute_finish: false }),
            // 2 + 3. gesture vs natural, and finish attribution
            ("next while playing crossfades, unattributed", playing(1, 0), decl(Some(2), Cause::Gesture, 1), caps(), xf(SKIP, false)),
            ("play N while playing crossfades", playing(1, 0), decl(Some(3), Cause::Gesture, 1), caps(), xf(SKIP, false)),
            ("boundary crossfade is attributed", playing(1, 0), decl(Some(2), Cause::Natural, 1), caps(), xf(BOUND, true)),
            ("boundary off: natural motion is a plain load, never a duck", playing(1, 0), decl(Some(2), Cause::Natural, 1), Caps { crossfade_bound: Duration::ZERO, ..caps() }, Step::Begin { shape: Shape::Cut, attribute_finish: true }),
            // 4. gapless: mpv already advanced into the head
            ("landed advance only commits", Audible::Single { leg: leg(2, 1), playing: true, leaving: false, committed: false }, decl(Some(2), Cause::Natural, 1), caps(), Step::Commit),
            ("committed head is settled", playing(2, 1), decl(Some(2), Cause::Natural, 1), caps(), Step::Settled),
            // 5. remove the playing row: the successor is declared, the outgoing abandoned
            ("delete playing row crossfades to the successor", playing(1, 0), decl(Some(2), Cause::Removed, 1), caps(), xf(SKIP, false)),
            ("delete the last row fades out", playing(1, 0), decl(None, Cause::Removed, 1), caps(), Step::Quiesce { fade: Some(SKIP) }),
            // 6. mid-flight with two decks
            ("redeclaring the incoming waits", Audible::Overlap { out: leg(1, 0), inn: leg(2, 1) }, decl(Some(2), Cause::Gesture, 1), caps(), Step::AwaitOverlap),
            ("declaring the outgoing reverses", Audible::Overlap { out: leg(1, 0), inn: leg(2, 1) }, Declared { fresh: 0, ..decl(Some(1), Cause::Removed, 2) }, caps(), Step::Reverse),
            ("a third head finishes then replans", Audible::Overlap { out: leg(1, 0), inn: leg(2, 1) }, decl(Some(3), Cause::Gesture, 2), caps(), Step::FinishThenReplan),
            ("restarting the outgoing row is a third head, not a reverse", Audible::Overlap { out: leg(1, 0), inn: leg(2, 1) }, decl(Some(1), Cause::Gesture, 2), caps(), Step::FinishThenReplan),
            ("clear mid-overlap finishes first", Audible::Overlap { out: leg(1, 0), inn: leg(2, 1) }, decl(None, Cause::Gesture, 2), caps(), Step::FinishThenReplan),
            // 7. single-deck floor
            ("one deck dips", playing(1, 0), decl(Some(2), Cause::Gesture, 1), no_xf, Step::Begin { shape: Shape::Dip(DIP), attribute_finish: false }),
            ("one deck fades out over the dip length", playing(1, 0), decl(None, Cause::Gesture, 1), no_xf, Step::Quiesce { fade: Some(DIP) }),
            // 8. EOF spine: the track already ended, nothing to overlap
            ("eof advance from silence is a cut", Audible::Silent, decl(Some(2), Cause::Natural, 1), caps(), Step::Begin { shape: Shape::Cut, attribute_finish: false }),
            // 9. load failure falls through, never a no-op
            ("failed arm drops to the dip", playing(1, 0), decl(Some(2), Cause::Gesture, 1), arm_failed, Step::Begin { shape: Shape::Dip(DIP), attribute_finish: false }),
            ("exhausted ladder stops retrying", playing(1, 0), decl(Some(2), Cause::Gesture, 1), Caps { abandoned: true, ..caps() }, Step::Settled),
            // the proof case: the queue swap lands inside clear's fade-out
            ("clear while playing fades out", playing(1, 0), decl(None, Cause::Gesture, 1), caps(), Step::Quiesce { fade: Some(SKIP) }),
            ("play during the fade-out crossfades in", Audible::Single { leg: leg(1, 0), playing: true, leaving: true, committed: true }, decl(Some(9), Cause::Gesture, 2), caps(), xf(SKIP, false)),
            ("a fade-out with no head is left to finish", Audible::Single { leg: leg(1, 0), playing: true, leaving: true, committed: true }, decl(None, Cause::Gesture, 1), caps(), Step::Settled),
            // transport
            ("play N on the playing row restarts it", playing(1, 0), decl(Some(1), Cause::Gesture, 1), caps(), xf(SKIP, false)),
            ("re-stating the head without a fresh floor is settled", playing(1, 0), Declared { fresh: 0, ..decl(Some(1), Cause::Gesture, 3) }, caps(), Step::Settled),
            ("paused deck: a new head is a plain load", Audible::Single { leg: leg(1, 0), playing: false, leaving: false, committed: true }, decl(Some(2), Cause::Gesture, 1), caps(), Step::Begin { shape: Shape::Cut, attribute_finish: false }),
            ("paused deck: clear halts", Audible::Single { leg: leg(1, 0), playing: false, leaving: false, committed: true }, decl(None, Cause::Gesture, 1), caps(), Step::Quiesce { fade: None }),
            ("stop halts whatever plays", Audible::Overlap { out: leg(1, 0), inn: leg(2, 1) }, Declared { intent: Intent::Stop, ..decl(Some(2), Cause::Gesture, 2) }, caps(), Step::Quiesce { fade: None }),
            ("stop when silent is settled", Audible::Silent, Declared { intent: Intent::Stop, ..decl(Some(2), Cause::Gesture, 2) }, caps(), Step::Settled),
        ];
        for (name, a, d, c, want) in rows {
            assert_eq!(plan(a, &d, c), want, "{name}");
        }
    }

    #[test]
    fn cause_derivations() {
        use Cause::*;
        assert!(Gesture.is_gesture() && Removed.is_gesture() && FromSilence.is_gesture());
        assert!(!Natural.is_gesture());
        // A wake/undo owns its own rise; the queue's motion must not kill a winddown.
        assert!(Gesture.resyncs_gain() && Removed.resyncs_gain());
        assert!(!Natural.resyncs_gain() && !FromSilence.resyncs_gain());
        // Skip and remove never scrobble the outgoing track.
        assert!(Natural.attributes_finish());
        assert!(!Gesture.attributes_finish() && !Removed.attributes_finish());
        assert!(!FromSilence.attributes_finish());
    }

    /// A model of what each step DOES to the snapshot, so the property below can walk
    /// the plan forward. It encodes the executor's contract, not its code.
    fn apply(a: Audible, d: &Declared, step: Step) -> Audible {
        let head_leg = || Leg { qid: d.head.expect("a begin has a head"), rev: d.rev };
        match step {
            Step::Settled => a,
            Step::Commit => match a {
                Audible::Single { leg, playing, leaving, .. } => {
                    Audible::Single { leg, playing, leaving, committed: true }
                }
                other => panic!("commit planned on {other:?}"),
            },
            Step::Begin { shape: Shape::Xf(_), .. } => match a {
                Audible::Single { leg, .. } => Audible::Overlap { out: leg, inn: head_leg() },
                other => panic!("crossfade planned on {other:?}"),
            },
            Step::Begin { .. } => {
                Audible::Single { leg: head_leg(), playing: true, leaving: false, committed: false }
            }
            // The overlap's terminal promotes the incoming deck.
            Step::AwaitOverlap | Step::FinishThenReplan => match a {
                Audible::Overlap { inn, .. } => {
                    Audible::Single { leg: inn, playing: true, leaving: false, committed: false }
                }
                other => panic!("overlap step on {other:?}"),
            },
            Step::Reverse => match a {
                Audible::Overlap { out, .. } => {
                    Audible::Single { leg: out, playing: true, leaving: false, committed: true }
                }
                other => panic!("reverse on {other:?}"),
            },
            Step::Quiesce { fade: Some(_) } => match a {
                Audible::Single { leg, committed, .. } => {
                    Audible::Single { leg, playing: true, leaving: true, committed }
                }
                other => panic!("fade-out planned on {other:?}"),
            },
            Step::Quiesce { fade: None } => Audible::Silent,
        }
    }

    fn all_audibles() -> Vec<Audible> {
        let mut out = vec![Audible::Silent];
        for q in 1..=3 {
            for rev in [0, 5] {
                for playing in [true, false] {
                    for leaving in [true, false] {
                        for committed in [true, false] {
                            out.push(Audible::Single { leg: leg(q, rev), playing, leaving, committed });
                        }
                    }
                }
            }
        }
        for (o, i) in [(1, 2), (2, 1), (1, 3)] {
            out.push(Audible::Overlap { out: leg(o, 0), inn: leg(i, 5) });
        }
        out
    }

    /// Total, convergent and idempotent over every small snapshot: walking the plan
    /// forward reaches `Settled` (or a fade-out the plan leaves running) within four
    /// steps, never crossfades on one deck, and a settled snapshot plans `Settled`
    /// again - running the reconciler twice with no new declaration does nothing.
    #[test]
    fn plan_converges_and_is_idempotent() {
        let causes = [Cause::Gesture, Cause::Natural, Cause::Removed, Cause::FromSilence];
        let cap_sets = [
            caps(),
            Caps { can_crossfade: false, ..caps() },
            Caps { incoming_loads: false, ..caps() },
            Caps { crossfade_skip: Duration::ZERO, crossfade_bound: Duration::ZERO, ..caps() },
        ];
        let mut checked = 0;
        for a0 in all_audibles() {
            for head in [None, Some(1), Some(2), Some(3)] {
                for cause in causes {
                    for intent in [Intent::Play, Intent::Stop] {
                        for fresh in [0, 7] {
                            for c in cap_sets {
                                let d = Declared { head: head.map(QueueId), intent, cause, rev: 7, fresh };
                                let mut a = a0;
                                let mut steps = 0;
                                loop {
                                    let s = plan(a, &d, c);
                                    if let Step::Begin { shape: Shape::Xf(_), .. } = s {
                                        assert!(c.can_crossfade && c.incoming_loads, "{a0:?} {d:?} {c:?}");
                                    }
                                    if s == Step::Settled {
                                        assert_eq!(plan(a, &d, c), Step::Settled, "idempotent");
                                        break;
                                    }
                                    a = apply(a, &d, s);
                                    steps += 1;
                                    assert!(steps <= 4, "no convergence from {a0:?} under {d:?} {c:?}");
                                }
                                checked += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(checked > 5000, "the sweep actually ran ({checked})");
    }
}
