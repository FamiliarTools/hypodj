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


use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::Notify;

use super::{
    db_to_mpv_volume, fade_task, mpv_volume_to_db, spent_heard_row, FadeHandle, FadeSlot,
    HypodjHandler, ResolvedPlay, State, Terminal, SKIP_DIP_DB,
};
use crate::clock::TokioClock;
use crate::event::QueueId;
use crate::fade::{run_crossfade, CrossfadeSink, CrossfadeSpec, FadeOutcome, FadeProgress, FadeTarget};
use crate::player::{PlayState, PlayerError, PlayerHandle};

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
    /// `stop`: hold the head, take the decks to silence (a short slewed fade from a
    /// playing deck, never a cut).
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
    /// The fade-out of a `stop` over a playing deck (the pause fade length).
    pub stop_fade: Duration,
    /// The fade-out when the head goes away under a playing track (clear).
    pub quiesce: Duration,
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
            // Silent already, or a fade-out is carrying it to the stop.
            Audible::Silent | Audible::Single { leaving: true, .. } => Step::Settled,
            // A playing deck is faded out, never cut: a stop is a deliberate
            // transition like a pause, and a cut from full level is a click.
            Audible::Single { playing: true, .. } => Step::Quiesce { fade: Some(c.stop_fade) },
            // Paused: nothing audible to smooth.
            Audible::Single { playing: false, .. } => Step::Quiesce { fade: None },
            // Land the overlap first (bounded), then fade the one deck left.
            Audible::Overlap { .. } => Step::FinishThenReplan,
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
                    (true, false) => Step::Quiesce { fade: Some(c.quiesce) },
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

// ── the impure half: the one path that changes what the decks play ─────────

/// The capability to change what a deck plays. Constructible ONLY in this module (its
/// field is private), and every head-changing player primitive (`play_url`,
/// `crossfade_arm`, `crossfade_commit`, `crossfade_abort`) demands one. So a handler
/// path that loads a track without declaring a head does not compile: the reconciler
/// below is the single path by construction, not by convention.
pub(crate) struct HeadKey {
    _private: (),
}

const KEY: HeadKey = HeadKey { _private: () };

/// The key a [`crate::player::ProbeDeck`] uses: a deck no handler owns has no engine
/// to go through. Only the player module's probe wrapper may call this.
pub(crate) fn probe_key() -> &'static HeadKey {
    &KEY
}

/// TEST-ONLY: the player's own tests drive its primitives directly.
#[cfg(test)]
pub(crate) fn test_key() -> &'static HeadKey {
    &KEY
}

/// An overlap in flight: which legs, where its envelope stands, and how it lands.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Overlap {
    /// The leg going away.
    pub out: Leg,
    /// The leg arriving.
    pub inn: Leg,
    /// Reference levels of the two legs (see [`crate::fade::crossfade_pair`]).
    pub out_db: f64,
    pub in_db: f64,
    /// Balance when the running schedule started, and when.
    pub from_t: f64,
    pub started: tokio::time::Instant,
    /// The length of a WHOLE 0-to-1 transition at this overlap's speed.
    pub dur: Duration,
    /// The outgoing track played out (a boundary), so its finish is published.
    pub attribute: bool,
    /// The ACTIVE deck (the one owning identity) is the INCOMING leg: this overlap is
    /// running back to the row it started from, and lands by silencing the spare deck
    /// rather than promoting it.
    pub reversed: bool,
}

impl Overlap {
    /// The envelope's balance now (0 = all outgoing, 1 = all incoming).
    fn position(&self) -> f64 {
        let secs = self.dur.as_secs_f64();
        if secs <= 0.0 {
            return 1.0;
        }
        (self.from_t + self.started.elapsed().as_secs_f64() / secs).clamp(0.0, 1.0)
    }
}

/// The engine's in-flight transition, held in [`State`] (one owner).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Transit {
    Overlap(Overlap),
    /// A single-deck duck-swap-rise is under way.
    Dip,
}

/// The reconciler's own state, owned by the handler.
pub(crate) struct Engine {
    /// Held for one whole reconcile pass: steps execute one at a time, in order.
    run: tokio::sync::Mutex<()>,
    /// The coalescing poke: whoever changes what is audible outside a pass (an overlap
    /// landing, a fade-out the user cut short) wakes the reconciler task through it.
    wake: Arc<Notify>,
}

impl Engine {
    pub(crate) fn new() -> Self {
        Engine { run: tokio::sync::Mutex::new(()), wake: Arc::new(Notify::new()) }
    }
}

/// The bound on one reconcile pass. The planner converges in at most four steps from
/// any snapshot (pinned by `plan_converges_and_is_idempotent`); the margin absorbs
/// declarations that land mid-pass.
const MAX_STEPS: usize = 12;

/// The bounded fast-finish of an overlap a third declaration interrupts.
const SETTLE: Duration = Duration::from_millis(200);

/// The equal-power pair, in deck terms. Forward: the active deck is outgoing. Reversed:
/// the active deck is the one coming back.
struct PairSink {
    player: PlayerHandle,
    reversed: bool,
}

impl CrossfadeSink for PairSink {
    fn set_pair_db(
        &self,
        outgoing_db: f64,
        incoming_db: f64,
    ) -> impl std::future::Future<Output = Result<(), PlayerError>> + Send {
        let (active, spare) =
            if self.reversed { (incoming_db, outgoing_db) } else { (outgoing_db, incoming_db) };
        let player = self.player.clone();
        async move { player.set_deck_pair_db(&KEY, active, spare).await }
    }

    fn settle(&self) -> Duration {
        self.player.gain_settle()
    }
}

impl HypodjHandler {
    /// Spawn the reconciler TASK: it re-runs a pass whenever something outside a pass
    /// changed what is audible (an overlap's own terminal, a fade-out cut short). Needs
    /// the `Arc` (it outlives any one call), so it starts with [`Self::set_self_ref`];
    /// a raw handler in a unit test reconciles inline in every declaring call instead.
    pub(crate) fn spawn_reconciler(weak: Weak<HypodjHandler>, wake: Arc<Notify>) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        rt.spawn(async move {
            loop {
                wake.notified().await;
                let Some(h) = weak.upgrade() else { break };
                let _ = h.reconcile().await;
            }
        });
    }

    pub(crate) fn engine_wake(&self) -> Arc<Notify> {
        self.engine.wake.clone()
    }

    /// Wake the reconciler task (coalescing; never blocks).
    pub(crate) fn poke(&self) {
        self.engine.wake.notify_one();
    }

    /// THE DECLARATION ENTRY POINT. Every head change runs here: `mutate` writes the
    /// declaration (through the `State` declarers) under the state lock, the cause's
    /// hook runs atomically with it, and the reconciler drives the room to it before
    /// this returns - so a `play` reply still means "the load landed".
    ///
    /// The hook, derived from the cause rather than passed as a bool: a gesture takes
    /// the gain back (cancels a running fade, recommits the baseline, retires a pending
    /// pause) under the SAME slot lock as the declaration, unless an overlap owns the
    /// gain - the planner re-aims an overlap itself (reverse, finish), and aborting its
    /// task here would strand the second deck. The live gain is left where the deck
    /// really is, so a crossfade out of a cancelled fade-out starts from that level.
    /// A gesture also arms the EOF fuse at its head.
    pub(super) async fn declare<R>(
        &self,
        cause: Cause,
        mutate: impl FnOnce(&mut State) -> R,
    ) -> Result<R, String> {
        let r = if cause.resyncs_gain() {
            let state = self.state.clone();
            self.fade
                .cancel_unless(
                    || matches!(state.lock().unwrap().transit, Some(Transit::Overlap(_))),
                    || {
                        let mut st = self.state.lock().unwrap();
                        let r = mutate(&mut st);
                        let v = st.target_volume;
                        st.logical_gain_db = mpv_volume_to_db(v as f64);
                        st.baseline_committed = true;
                        st.fading = false;
                        st.pending_pause = false;
                        r
                    },
                )
                .await
        } else {
            let mut st = self.state.lock().unwrap();
            mutate(&mut st)
        };
        if cause.is_gesture() {
            let head = self.state.lock().unwrap().head_qid();
            self.arm_eof_fuse_at(head.map(|q| q.0));
        }
        self.notify_change();
        self.reconcile().await.map(|()| r)
    }

    /// Run the reconciler: snapshot, plan, execute, until the room is what was declared
    /// or an overlap's own terminal owns the rest. Serialized by the engine's run lock,
    /// level-triggered (it re-reads the declaration every step, so a declaration that
    /// lands mid-pass is never lost), and bounded.
    ///
    /// `Err` only when the CURRENT declaration was abandoned at the bottom of the
    /// fallback ladder (its load failed every way): the caller surfaces that as an ACK.
    pub(crate) async fn reconcile(&self) -> Result<(), String> {
        let _pass = self.engine.run.lock().await;
        let mut err = None;
        for _ in 0..MAX_STEPS {
            let (a, d, c) = self.snapshot();
            let step = plan(a, &d, c);
            tracing::trace!(?a, ?d, ?step, "reconcile");
            match step {
                Step::Settled | Step::AwaitOverlap => {
                    if a == Audible::Silent {
                        // A deck carrying nothing carries nothing off the network: the
                        // store's bulk work stops deferring, whatever emptied it.
                        self.set_store_playback_remote(false);
                    }
                    break;
                }
                _ => {
                    if let Err(e) = self.execute(step, &d, &c).await {
                        err = Some(e);
                    }
                }
            }
        }
        let abandoned = {
            let st = self.state.lock().unwrap();
            st.abandoned_rev == Some(st.declared.rev)
        };
        match err {
            Some(e) if abandoned => Err(e),
            _ => Ok(()),
        }
    }

    /// Hold the engine's run lock: no reconcile pass runs until the guard drops. For a
    /// transport act that must not interleave with a load (stop).
    pub(crate) async fn engine_pass(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.engine.run.lock().await
    }

    /// Bring the room to ONE deck playing the declared head, committed. Transport
    /// gestures that act on "the" deck (pause, seek, an undo's reload) call this first so
    /// they act on the room as declared, never on a transition half done.
    ///
    /// A pass alone is not enough: the planner rightly treats an overlap carrying the
    /// room to the head as `AwaitOverlap` and returns, but every transport primitive
    /// addresses the ACTIVE deck, which through a forward overlap is still the OUTGOING
    /// leg. A pause or seek issued then lands on the track being left (lost when it
    /// stops) while the incoming leg plays on, and the head's deferred commit, running
    /// later, would retire the pause intent raised after it. So an overlap in flight is
    /// finished here within the bounded settle, under the run lock, and the pass that
    /// follows commits the head before the gesture runs.
    pub(crate) async fn settle_transitions(&self) {
        {
            let _pass = self.engine.run.lock().await;
            self.finish_overlap(SETTLE).await;
        }
        let _ = self.reconcile().await;
    }

    /// The (Audible, Declared, Caps) the planner sees, read under ONE state lock.
    fn snapshot(&self) -> (Audible, Declared, Caps) {
        let raw = self.player.state();
        let st = self.state.lock().unwrap();
        let mut d = st.declared;
        if d.head.is_some() && st.head_qid().is_none() {
            // The head's row left the queue: nothing that is not queued may play.
            d.head = None;
        }
        let a = match (st.transit, st.deck) {
            (Some(Transit::Overlap(o)), _) => Audible::Overlap { out: o.out, inn: o.inn },
            (_, Some(leg)) if raw != PlayState::Stopped => Audible::Single {
                leg,
                playing: raw == PlayState::Playing,
                leaving: st.fading && st.quiesce_epoch == Some(st.fade_epoch),
                committed: st.deck_committed,
            },
            _ => Audible::Silent,
        };
        let c = Caps {
            can_crossfade: self.player.can_crossfade(),
            crossfade_skip: self.crossfade_skip_dur().unwrap_or(Duration::ZERO),
            crossfade_bound: self.crossfade_bound_dur().unwrap_or(Duration::ZERO),
            dip: self.skip_fade_dur(),
            stop_fade: self.pause_fade_dur(),
            quiesce: self.clear_fade_dur(),
            settle: SETTLE,
            incoming_loads: st.xf_failed_rev != Some(d.rev),
            abandoned: st.abandoned_rev == Some(d.rev),
        };
        (a, d, c)
    }

    async fn execute(&self, step: Step, d: &Declared, c: &Caps) -> Result<(), String> {
        match step {
            Step::Settled | Step::AwaitOverlap => Ok(()),
            Step::Commit => {
                self.commit_head(d.cause).await;
                Ok(())
            }
            Step::Begin { shape, attribute_finish } => self.begin(shape, attribute_finish, d).await,
            Step::Reverse => {
                self.reverse_overlap().await;
                Ok(())
            }
            Step::FinishThenReplan => {
                self.finish_overlap(c.settle).await;
                Ok(())
            }
            Step::Quiesce { fade: Some(dur) } => {
                self.fade_out(dur).await;
                Ok(())
            }
            Step::Quiesce { fade: None } => {
                self.halt_decks().await;
                Ok(())
            }
        }
    }

    /// Resolve the declared head into play arguments, or abandon the declaration.
    fn resolve_head(&self, d: &Declared) -> Result<ResolvedPlay, String> {
        let item = {
            let st = self.state.lock().unwrap();
            d.head.and_then(|q| st.queue.iter().find(|it| it.id == q.0).cloned())
        };
        let resolved = match item {
            Some(item) => self.resolve_play(&item),
            None => Err("Bad song index".to_string()),
        };
        if resolved.is_err() {
            self.state.lock().unwrap().abandoned_rev = Some(d.rev);
        }
        resolved
    }

    async fn begin(&self, shape: Shape, attribute: bool, d: &Declared) -> Result<(), String> {
        let play = self.resolve_head(d)?;
        // The target is about to be current but is not in the store's window yet: pin
        // it against eviction until its commit releases the pin.
        if let Some(id) = play.song_id.clone() {
            self.set_store_skip_pin(Some(id));
        }
        match shape {
            Shape::Xf(dur) => {
                self.begin_overlap(&play, d, dur, attribute).await;
                Ok(())
            }
            Shape::Dip(dur) => self.begin_dip(&play, d, dur).await,
            Shape::Cut => {
                if d.cause.resyncs_gain() {
                    // Over a paused or silent deck: re-assert the baseline before the
                    // load, since mpv's gain persists across loadfile and a completed
                    // pause fade left it near zero.
                    let v = self.state.lock().unwrap().target_volume;
                    let _ = self.player.set_volume(v).await;
                    self.state.lock().unwrap().live_gain_db = mpv_volume_to_db(v as f64);
                }
                self.load(&play, d.rev).await
            }
            Shape::FromSilence => {
                // Something still audible (an undo while another track plays, on a
                // player that cannot overlap) is taken DOWN first, all the way to
                // silence: never cut from an audible level. A duck the user supersedes
                // just re-plans.
                let audible = self.state.lock().unwrap().deck.is_some()
                    && self.player.state() == PlayState::Playing;
                if audible {
                    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                    let dur = self.skip_fade_dur();
                    if self
                        .install_engine_fade(FadeTarget::Silence, dur, Terminal::Trough(tx))
                        .await
                        .is_ok()
                        && (rx.await.is_err() || self.state.lock().unwrap().declared.rev != d.rev)
                    {
                        return Ok(());
                    }
                }
                // Silent BEFORE the first buffer; the caller owns the rise, from the floor.
                let _ = self.player.set_volume(0).await;
                {
                    let mut st = self.state.lock().unwrap();
                    st.live_gain_db = self.fade_cfg.synth_floor_db;
                }
                self.load(&play, d.rev).await
            }
        }
    }

    /// Load `play` on the ACTIVE deck and record it as what the deck plays. A failure
    /// abandons this declaration (the reconciler stops retrying it) and is returned for
    /// the caller to surface.
    async fn load(&self, play: &ResolvedPlay, rev: u64) -> Result<(), String> {
        self.set_store_playback_remote(!play.local);
        self.state.lock().unwrap().loading_rev = Some(rev);
        let loaded = self
            .player
            .play_url(&KEY, play.song_id.clone(), Some(play.qid), &play.url, play.local)
            .await
            .map_err(|e| e.to_string());
        let mut st = self.state.lock().unwrap();
        st.loading_rev = None;
        match loaded {
            Ok(()) => {
                st.deck = Some(Leg { qid: play.qid, rev });
                st.deck_committed = false;
                Ok(())
            }
            Err(e) => {
                st.abandoned_rev = Some(rev);
                Err(e)
            }
        }
    }

    /// Xf: arm the spare deck silent, then hand both gains to one envelope in the fade
    /// slot. The outgoing leg starts from its LIVE gain, so a crossfade that lands
    /// inside a fade-out continues it rather than jumping back up. A refused arm leaves
    /// nothing audible changed and marks the ladder to continue below the crossfade.
    async fn begin_overlap(&self, play: &ResolvedPlay, d: &Declared, dur: Duration, attribute: bool) {
        let start = {
            let mut st = self.state.lock().unwrap();
            match st.head_start {
                Some((q, secs)) if q == play.qid => {
                    st.head_start = None;
                    Some(secs)
                }
                _ => None,
            }
        };
        let armed = self
            .player
            .crossfade_arm(&KEY, play.song_id.clone(), Some(play.qid), &play.url, play.local, start)
            .await;
        if armed.is_err() || self.state.lock().unwrap().declared.rev != d.rev {
            // Refused, or superseded while loading: silence the spare, re-plan.
            let _ = self.player.crossfade_abort(&KEY).await;
            if armed.is_err() {
                self.state.lock().unwrap().xf_failed_rev = Some(d.rev);
            }
            return;
        }
        self.set_store_playback_remote(!play.local);
        // The warm (if any) belongs to the outgoing deck, which is about to stop; a
        // parked entry there could still auto-advance during the overlap.
        self.disarm_continuation_warm().await;
        let mut slot = self.fade.inner.lock().await;
        if let Some(mut h) = slot.take() {
            h.abort.abort();
            if let Some(join) = h.join.take() {
                let _ = join.await;
            }
        }
        let tick = Duration::from_millis(self.fade_cfg.crossfade_tick_ms);
        let installed = {
            let mut st = self.state.lock().unwrap();
            // No outgoing leg to overlap (it ended under the arm): the ladder goes on.
            st.deck.and_then(|out| {
            let out_db = st.live_gain_db;
            // The queue's own motion keeps the level the room is at (a winddown rides
            // through a boundary); a gesture lands on the baseline.
            let in_db = if d.cause.resyncs_gain() {
                mpv_volume_to_db(st.target_volume as f64)
            } else {
                st.live_gain_db
            };
            let o = Overlap {
                out,
                inn: Leg { qid: play.qid, rev: d.rev },
                out_db,
                in_db,
                from_t: 0.0,
                started: tokio::time::Instant::now(),
                dur,
                attribute,
                reversed: false,
            };
            CrossfadeSpec::between(out_db, in_db, 0.0, dur, tick).ok().map(|spec| {
                st.fade_epoch += 1;
                st.fading = true;
                st.baseline_committed = false;
                st.quiesce_epoch = None;
                st.transit = Some(Transit::Overlap(o));
                (spec, o, st.fade_epoch)
            })
            })
        };
        let Some((spec, o, epoch)) = installed else {
            drop(slot);
            let _ = self.player.crossfade_abort(&KEY).await;
            self.state.lock().unwrap().xf_failed_rev = Some(d.rev);
            return;
        };
        *slot = Some(self.spawn_overlap(spec, o, epoch));
        drop(slot);
        self.notify_change();
    }

    fn spawn_overlap(&self, spec: CrossfadeSpec, o: Overlap, epoch: u64) -> FadeHandle {
        let join = tokio::spawn(overlap_task(
            self.player.clone(),
            spec,
            o,
            self.state.clone(),
            self.changed.clone(),
            epoch,
            self.fade.clone(),
            self.engine.wake.clone(),
        ));
        FadeHandle { abort: join.abort_handle(), join: Some(join) }
    }

    /// Take the running overlap out of the slot (abort + join its task) and return it
    /// with its live balance. `None` when no overlap is in flight.
    async fn seize_overlap(
        &self,
    ) -> Option<(tokio::sync::MutexGuard<'_, Option<FadeHandle>>, Overlap, f64)> {
        let mut slot = self.fade.inner.lock().await;
        let o = match self.state.lock().unwrap().transit {
            Some(Transit::Overlap(o)) => o,
            _ => return None,
        };
        if let Some(mut h) = slot.take() {
            h.abort.abort();
            if let Some(join) = h.join.take() {
                let _ = join.await;
            }
        }
        // Bump under the lock so an aborted straggler's last report is already stale.
        self.state.lock().unwrap().fade_epoch += 1;
        let t = o.position();
        Some((slot, o, t))
    }

    /// Reverse: the declared head is the row the overlap is LEAVING. Run the same
    /// envelope back from where it stands - no settle, no step in either gain.
    async fn reverse_overlap(&self) {
        let Some((mut slot, o, t)) = self.seize_overlap().await else { return };
        let tick = Duration::from_millis(self.fade_cfg.crossfade_tick_ms);
        let back_db = mpv_volume_to_db(self.state.lock().unwrap().target_volume as f64);
        let r = Overlap {
            out: o.inn,
            inn: o.out,
            out_db: o.in_db,
            in_db: back_db,
            from_t: 1.0 - t,
            started: tokio::time::Instant::now(),
            dur: o.dur,
            attribute: false,
            reversed: !o.reversed,
        };
        let Ok(spec) = CrossfadeSpec::between(r.out_db, r.in_db, r.from_t, r.dur, tick) else {
            land_overlap(&self.player, &self.state, &o).await;
            return;
        };
        let epoch = {
            let mut st = self.state.lock().unwrap();
            st.transit = Some(Transit::Overlap(r));
            st.fading = true;
            st.fade_epoch
        };
        *slot = Some(self.spawn_overlap(spec, r, epoch));
        drop(slot);
        // The leg coming back never stopped, so it emits no Playing edge of its own: name
        // it, or the director stays latched on the leg being abandoned and drops every
        // tick of the head. The returning leg is the active deck exactly when this run
        // is reversed.
        let _ = self.player.reannounce(&KEY, r.reversed).await;
    }

    /// Finish the running overlap within `within` (scaled by what is left of it), land
    /// it, and leave the next plan to start from the promoted deck. Two decks is the
    /// bound, so a third head is never a third deck: it is this, then a normal
    /// transition. `within == 0` lands at once (a manual volume taking the gain).
    async fn finish_overlap(&self, within: Duration) {
        let Some((slot, o, t)) = self.seize_overlap().await else { return };
        let tick = Duration::from_millis(self.fade_cfg.crossfade_tick_ms);
        if !within.is_zero() && t < 1.0 {
            if let Ok(spec) = CrossfadeSpec::between(o.out_db, o.in_db, t, within, tick) {
                let sink = PairSink { player: self.player.clone(), reversed: o.reversed };
                let mut report = |_p: FadeProgress| {};
                let _ = run_crossfade(&sink, &spec, &TokioClock, &mut report).await;
            }
        }
        land_overlap(&self.player, &self.state, &o).await;
        drop(slot);
        self.changed.notify_waiters();
    }

    /// Promote an in-flight overlap at once, before something else takes the gain (a
    /// manual volume, a stop). A no-op with nothing in flight.
    pub(crate) async fn settle_overlap(&self) {
        self.finish_overlap(Duration::ZERO).await;
    }

    /// Dip: duck the playing deck to the skip floor, load the head at the trough, rise.
    /// The single-deck floor. A manual volume or pause that takes the gain mid-duck
    /// aborts it, and the next plan starts again from wherever the gain was left.
    async fn begin_dip(&self, play: &ResolvedPlay, d: &Declared, dur: Duration) -> Result<(), String> {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let duck = self
            .install_engine_fade(FadeTarget::Db(SKIP_DIP_DB), dur, Terminal::Trough(tx))
            .await;
        if duck.is_err() {
            // An unbuildable duck must not turn the gesture into a no-op: cut.
            return self.load(play, d.rev).await;
        }
        self.state.lock().unwrap().transit = Some(Transit::Dip);
        let reached = rx.await.is_ok();
        let still = self.state.lock().unwrap().declared.rev == d.rev;
        if !(reached && still) {
            self.state.lock().unwrap().transit = None;
            return Ok(());
        }
        let loaded = self.load(play, d.rev).await;
        self.state.lock().unwrap().transit = None;
        // Rise from the floor, loaded or not: a failed load must not leave the room
        // ducked under a track that keeps playing.
        let target = self.state.lock().unwrap().target_volume;
        let _ = self
            .install_engine_fade(
                FadeTarget::Db(mpv_volume_to_db(target as f64)),
                dur,
                Terminal::SetBaseline(target),
            )
            .await;
        loaded
    }

    /// A one-legged fade-out to a stop: the head went away under a playing track
    /// (clear, remove the last row). The engine records its epoch, so a declaration
    /// landing during it plans a crossfade from its live gain.
    async fn fade_out(&self, dur: Duration) {
        let res = self
            .install_engine_fade(FadeTarget::Silence, dur, Terminal::StopRestore { evict: None })
            .await;
        if res.is_err() {
            self.halt_decks().await;
            return;
        }
        {
            let mut st = self.state.lock().unwrap();
            st.quiesce_epoch = Some(st.fade_epoch);
        }
        // Nothing drains next behind a deck fading to a stop.
        self.disarm_continuation_warm().await;
        self.notify_change();
    }

    /// Silence every deck NOW: stop, or nothing playable with a paused deck. Any fade or
    /// overlap dies with it (both decks stop, so no leg is stranded).
    pub(crate) async fn halt_decks(&self) {
        let mut slot = self.fade.inner.lock().await;
        if let Some(mut h) = slot.take() {
            h.abort.abort();
            if let Some(join) = h.join.take() {
                let _ = join.await;
            }
        }
        let v = {
            let mut st = self.state.lock().unwrap();
            st.fade_epoch += 1;
            st.transit = None;
            st.quiesce_epoch = None;
            st.deck = None;
            let v = st.target_volume;
            st.set_manual_volume(v, false);
            st.pending_pause = false;
            v
        };
        let _ = self.player.crossfade_abort(&KEY).await;
        let _ = self.player.stop().await;
        let _ = self.player.set_volume(v).await;
        drop(slot);
        self.set_store_playback_remote(false);
        self.disarm_continuation_warm().await;
        self.notify_change();
    }

    /// Install an engine-owned single-gain fade (a duck, a rise, a fade-out) through
    /// the ONE fade slot, from the live gain, deliberate and clamped up so it always
    /// lands. Validate-before-abort, epoch-bumped under the slot lock.
    async fn install_engine_fade(
        &self,
        target: FadeTarget,
        dur: Duration,
        terminal: Terminal,
    ) -> Result<(), crate::fade::FadeError> {
        let from_db = self.state.lock().unwrap().live_gain_db;
        let spec = self.build_deliberate_spec(from_db, target, dur)?;
        let synth_floor = self.fade_cfg.synth_floor_db;
        let mut slot = self.fade.inner.lock().await;
        if let Some(mut h) = slot.take() {
            h.abort.abort();
            if let Some(join) = h.join.take() {
                let _ = join.await;
            }
        }
        let epoch = {
            let mut st = self.state.lock().unwrap();
            st.fade_epoch += 1;
            st.fading = true;
            st.baseline_committed = false;
            st.fade_epoch
        };
        let join = tokio::spawn(fade_task(
            self.player.clone(),
            spec,
            self.state.clone(),
            self.changed.clone(),
            epoch,
            terminal,
            self.fade.clone(),
            synth_floor,
        ));
        *slot = Some(FadeHandle { abort: join.abort_handle(), join: Some(join) });
        Ok(())
    }

    /// THE ONE COMMIT: the head is audible, so do its bookkeeping. Replaces the four
    /// duplicated tails (the plain load, the gapless landing, the crossfade terminal,
    /// the dip terminal). Runs once per load.
    pub(crate) async fn commit_head(&self, cause: Cause) {
        let (item, trimmed, retired) = {
            let mut st = self.state.lock().unwrap();
            let Some(leg) = st.deck else { return };
            st.deck_committed = true;
            // A track is now current: a pending fresh-enqueue gesture is consumed.
            st.fresh_enqueue_anchor = None;
            // One track moved forward, so retire one track's worth of history.
            let trimmed = st.trim_spent();
            // A gesture's head landing retires a standing pause intent (the wake from
            // the nothing-loaded terminal); the queue's own motion leaves it alone so a
            // pause fade descending across a boundary keeps its intent.
            let retired = cause.is_gesture() && std::mem::replace(&mut st.pending_pause, false);
            (st.queue.iter().find(|it| it.id == leg.qid.0).cloned(), trimmed, retired)
        };
        for it in &trimmed {
            if let Some(row) = spent_heard_row(it) {
                self.append_heard(row);
            }
        }
        // The head is current, so no skip target needs pinning any more.
        self.set_store_skip_pin(None);
        if let Some(play) = item.as_ref().and_then(|it| self.resolve_play(it).ok()) {
            self.set_store_playback_remote(!play.local);
        }
        let _ = retired;
        self.notify_change();
        // Re-arm the warm behind the freshly current entry.
        self.reschedule_continuation_warm().await;
    }

    /// A position tick from the director: if the head is within the boundary crossfade
    /// length of its natural end, DECLARE its successor early as the queue's own motion.
    /// The planner turns that into an attributed crossfade. Almost always a cheap no.
    ///
    /// Same-album consecutive tracks are never declared early: they flow through the
    /// gapless advance as their makers sequenced them.
    pub(crate) async fn on_position(&self, remaining: Duration) {
        let Some(dur) = self.crossfade_bound_dur() else { return };
        if !self.player.can_crossfade() || remaining > dur {
            return;
        }
        let next = {
            let mut st = self.state.lock().unwrap();
            if st.transit.is_some() || st.fading || st.deck.map(|l| l.qid) != st.head_qid() {
                return;
            }
            let Some(cur) = st.current() else { return };
            let Some(next) = st.peek_next_from(cur, false) else { return };
            if next == cur || super::same_album_run(&st, cur, next) {
                return;
            }
            QueueId(st.queue[next].id)
        };
        let _ = self
            .declare(Cause::Natural, |st| {
                st.declare_head(Some(next), Cause::Natural);
            })
            .await;
    }
}

/// Land an overlap: promote the incoming deck (forward) or silence the spare (a
/// reversed run back), re-assert the reached level, and record what the active deck
/// now plays. The head bookkeeping is the reconciler's ([`HypodjHandler::commit_head`]).
async fn land_overlap(player: &PlayerHandle, state: &Mutex<State>, o: &Overlap) {
    if o.reversed {
        let _ = player.crossfade_retire(&KEY).await;
    } else {
        let _ = player.crossfade_commit(&KEY, o.attribute).await;
    }
    let _ = player.set_volume_f64(db_to_mpv_volume(o.in_db)).await;
    let mut st = state.lock().unwrap();
    st.transit = None;
    st.live_gain_db = o.in_db;
    st.fading = false;
    st.baseline_committed = (o.in_db - mpv_volume_to_db(st.target_volume as f64)).abs() < 1e-9;
    if !o.reversed {
        st.deck = Some(o.inn);
        st.deck_committed = false;
    }
}

/// Drive one overlap to its end, then land it - the two-deck counterpart of
/// `fade_task`, sharing its slot, epoch and abort discipline. Its terminal runs UNDER
/// the slot lock and only while its epoch is current, so a re-aim that seized the
/// overlap first (reverse, finish) is the only one that lands it.
#[allow(clippy::too_many_arguments)]
fn overlap_task(
    player: PlayerHandle,
    spec: CrossfadeSpec,
    o: Overlap,
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    epoch: u64,
    fade_slot: Arc<FadeSlot>,
    wake: Arc<Notify>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = FadeOutcome> + Send>> {
    Box::pin(async move {
        let outcome = {
            let state_r = state.clone();
            let mut report = move |p: FadeProgress| {
                let mut st = state_r.lock().unwrap();
                if st.fade_epoch == epoch {
                    // The room's level is the incoming reference throughout.
                    st.live_gain_db = p.gain_db;
                }
            };
            let sink = PairSink { player: player.clone(), reversed: o.reversed };
            run_crossfade(&sink, &spec, &TokioClock, &mut report).await
        };
        let mut slot_guard = fade_slot.inner.lock().await;
        if state.lock().unwrap().fade_epoch == epoch {
            land_overlap(&player, &state, &o).await;
            *slot_guard = None;
            drop(slot_guard);
            changed.notify_waiters();
            // The deck changed outside any pass: the reconciler commits the head.
            wake.notify_one();
        }
        outcome
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKIP: Duration = Duration::from_millis(1500);
    const BOUND: Duration = Duration::from_secs(4);
    const DIP: Duration = Duration::from_millis(350);
    const STOP: Duration = Duration::from_millis(500);
    const CLEAR: Duration = Duration::from_secs(5);

    fn caps() -> Caps {
        Caps {
            can_crossfade: true,
            crossfade_skip: SKIP,
            crossfade_bound: BOUND,
            dip: DIP,
            stop_fade: STOP,
            quiesce: CLEAR,
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
            ("delete the last row fades out", playing(1, 0), decl(None, Cause::Removed, 1), caps(), Step::Quiesce { fade: Some(CLEAR) }),
            // 6. mid-flight with two decks
            ("redeclaring the incoming waits", Audible::Overlap { out: leg(1, 0), inn: leg(2, 1) }, decl(Some(2), Cause::Gesture, 1), caps(), Step::AwaitOverlap),
            ("declaring the outgoing reverses", Audible::Overlap { out: leg(1, 0), inn: leg(2, 1) }, Declared { fresh: 0, ..decl(Some(1), Cause::Removed, 2) }, caps(), Step::Reverse),
            ("a third head finishes then replans", Audible::Overlap { out: leg(1, 0), inn: leg(2, 1) }, decl(Some(3), Cause::Gesture, 2), caps(), Step::FinishThenReplan),
            ("restarting the outgoing row is a third head, not a reverse", Audible::Overlap { out: leg(1, 0), inn: leg(2, 1) }, decl(Some(1), Cause::Gesture, 2), caps(), Step::FinishThenReplan),
            ("clear mid-overlap finishes first", Audible::Overlap { out: leg(1, 0), inn: leg(2, 1) }, decl(None, Cause::Gesture, 2), caps(), Step::FinishThenReplan),
            // 7. single-deck floor
            ("one deck dips", playing(1, 0), decl(Some(2), Cause::Gesture, 1), no_xf, Step::Begin { shape: Shape::Dip(DIP), attribute_finish: false }),
            ("one deck fades out over the clear length too", playing(1, 0), decl(None, Cause::Gesture, 1), no_xf, Step::Quiesce { fade: Some(CLEAR) }),
            // 8. EOF spine: the track already ended, nothing to overlap
            ("eof advance from silence is a cut", Audible::Silent, decl(Some(2), Cause::Natural, 1), caps(), Step::Begin { shape: Shape::Cut, attribute_finish: false }),
            // 9. load failure falls through, never a no-op
            ("failed arm drops to the dip", playing(1, 0), decl(Some(2), Cause::Gesture, 1), arm_failed, Step::Begin { shape: Shape::Dip(DIP), attribute_finish: false }),
            ("exhausted ladder stops retrying", playing(1, 0), decl(Some(2), Cause::Gesture, 1), Caps { abandoned: true, ..caps() }, Step::Settled),
            // the proof case: the queue swap lands inside clear's fade-out
            ("clear while playing fades out", playing(1, 0), decl(None, Cause::Gesture, 1), caps(), Step::Quiesce { fade: Some(CLEAR) }),
            ("play during the fade-out crossfades in", Audible::Single { leg: leg(1, 0), playing: true, leaving: true, committed: true }, decl(Some(9), Cause::Gesture, 2), caps(), xf(SKIP, false)),
            ("a fade-out with no head is left to finish", Audible::Single { leg: leg(1, 0), playing: true, leaving: true, committed: true }, decl(None, Cause::Gesture, 1), caps(), Step::Settled),
            // transport
            ("play N on the playing row restarts it", playing(1, 0), decl(Some(1), Cause::Gesture, 1), caps(), xf(SKIP, false)),
            ("re-stating the head without a fresh floor is settled", playing(1, 0), Declared { fresh: 0, ..decl(Some(1), Cause::Gesture, 3) }, caps(), Step::Settled),
            ("paused deck: a new head is a plain load", Audible::Single { leg: leg(1, 0), playing: false, leaving: false, committed: true }, decl(Some(2), Cause::Gesture, 1), caps(), Step::Begin { shape: Shape::Cut, attribute_finish: false }),
            ("paused deck: clear halts", Audible::Single { leg: leg(1, 0), playing: false, leaving: false, committed: true }, decl(None, Cause::Gesture, 1), caps(), Step::Quiesce { fade: None }),
            ("stop lands an overlap first", Audible::Overlap { out: leg(1, 0), inn: leg(2, 1) }, Declared { intent: Intent::Stop, ..decl(Some(2), Cause::Gesture, 2) }, caps(), Step::FinishThenReplan),
            ("stop fades a playing deck out", playing(1, 0), Declared { intent: Intent::Stop, ..decl(Some(1), Cause::Gesture, 2) }, caps(), Step::Quiesce { fade: Some(STOP) }),
            ("stop halts a paused deck", Audible::Single { leg: leg(1, 0), playing: false, leaving: false, committed: true }, Declared { intent: Intent::Stop, ..decl(Some(1), Cause::Gesture, 2) }, caps(), Step::Quiesce { fade: None }),
            ("a fade-out to a stop is left to land", Audible::Single { leg: leg(1, 0), playing: true, leaving: true, committed: true }, Declared { intent: Intent::Stop, ..decl(Some(1), Cause::Gesture, 2) }, caps(), Step::Settled),
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
