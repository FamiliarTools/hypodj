//! hypodj-nl: the OPTIONAL natural-language -> validated P2 Plan IR translator.
//!
//! OFF THE TRUSTED PATH. This crate only ever EMITS a [`hypodj_core::plan::RawPlan`];
//! it cannot arm. Every plan it produces crosses the existing P2
//! [`hypodj_core::plan::validate`] + a human echo + an explicit confirm before the
//! handler's `plan_add`. The model never touches a QueueId/AlbumId/Instant - those
//! stay symbolic in the raw trigger and are resolved by `validate()` at confirm
//! time against the LIVE snapshot.
//!
//! Layering: the [`hypodj_core::nl::Translator`] SEAM (trait + context + error)
//! lives in hypodj-core so the core keeps ZERO model deps; this crate implements
//! it. The deterministic [`RulesTranslator`] + the [`HybridTranslator`] plumbing +
//! the LlmRawPlan <-> RawPlan conversion + the output parse are ALL model-free and
//! always compiled. Only the constrained-decode BACKEND is behind `feature = "llm"`.

#[cfg(feature = "cc")]
pub mod cc;
#[cfg(feature = "llm")]
pub mod gbnf;
#[cfg(feature = "llm-llama")]
pub mod llama;
pub mod llm;
pub mod rules;

// Echo rendering lives in hypodj-core (the handler renders the echo there and core
// cannot depend on this crate); re-exported for the round-trip test + callers.
pub use hypodj_core::echo::{describe_batch, describe_plan, render_dsl};
pub use llm::{parse_llm_output, LlmActionKind, LlmRawPlan, LlmWhen};
pub use rules::RulesTranslator;

#[cfg(feature = "llm")]
pub use llm::{LlmBackend, LlmTranslator};

use hypodj_core::nl::{NlContext, NlError, NlHit, Translator};

/// Tries the deterministic RULES fast-path first; on a plain NotUnderstood miss
/// (and only then) falls through to the optional local model. Ambiguous /
/// Unresolvable are REAL, specific fails and propagate as-is (never fall through -
/// they mean the rules understood the shape but the request is under-determined /
/// unactionable). An Ok result is returned directly.
pub struct HybridTranslator {
    rules: RulesTranslator,
    /// `Some` only under `feature = "llm"` with a loaded model; otherwise `None`,
    /// so the crate degrades to Rules-only + a loud NotUnderstood (the offline /
    /// optional north star).
    llm: Option<Box<dyn Translator>>,
}

impl HybridTranslator {
    /// Build with an optional model translator (the daemon passes `None` when the
    /// feature is off or no model file is present).
    pub fn new(llm: Option<Box<dyn Translator>>) -> Self {
        Self { rules: RulesTranslator, llm }
    }

    /// The deterministic, model-free translator (the default the daemon wires when
    /// no model is configured).
    pub fn rules_only() -> Self {
        Self::new(None)
    }
}

impl Translator for HybridTranslator {
    fn translate(&self, utterance: &str, ctx: &NlContext) -> Result<NlHit, NlError> {
        match self.rules.translate(utterance, ctx) {
            Err(NlError::NotUnderstood) => match &self.llm {
                Some(l) => l.translate(utterance, ctx),
                None => Err(NlError::NotUnderstood),
            },
            other => other,
        }
    }

    /// Questions are rules-only: the read grammar is closed and deterministic.
    fn query(&self, utterance: &str) -> Option<hypodj_core::nl::NlQuery> {
        self.rules.query(utterance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone, Utc};
    use hypodj_core::model::SongId;
    use hypodj_core::nl::{NlSource, Translator};
    use hypodj_core::plan::{
        Action, FadeIntentIr, PlayMode, PosBase, RawPlan, RawTrigger, Selector, TrackSel,
    };

    /// A fixed civil clock (2026-07-15 05:30 UTC) + a UTC-relative +01:00 zone, so
    /// every wake / wall-clock assertion is deterministic (never wall-clock).
    fn ctx(current: Option<&str>, queue_len: usize) -> NlContext {
        NlContext {
            current: current.map(|s| SongId(s.into())),
            now: tokio::time::Instant::now(),
            now_civil: Utc.with_ymd_and_hms(2026, 7, 15, 5, 30, 0).unwrap(),
            tz: FixedOffset::east_opt(0).unwrap(), // UTC for simple local==UTC math
            queue_len,
        }
    }

    fn tr(u: &str, c: &NlContext) -> Result<Vec<RawPlan>, NlError> {
        rules::translate_rules(u, c)
    }

    fn j(p: &RawPlan) -> serde_json::Value {
        serde_json::to_value(p).unwrap()
    }

    fn raw(trigger: RawTrigger, action: Action) -> RawPlan {
        RawPlan { version: 1, trigger, action, once: true, origin: String::new() }
    }

    // ── CORPUS-AS-TABLE: every phrasing -> exact RawPlan(s) or exact NlError ──

    #[test]
    fn corpus_fade_immediate_and_trigger_gate() {
        let c = ctx(Some("s1"), 5);
        // Bare fade out -> Immediate + Fade(Out{default}).
        assert_eq!(
            j(&tr("fade out", &c).unwrap()[0]),
            j(&raw(RawTrigger::Immediate, Action::Fade(FadeIntentIr::Out { secs: 30.0 })))
        );
        // "fade in" -> Immediate + Fade(In{default}).
        assert_eq!(
            j(&tr("fade in", &c).unwrap()[0]),
            j(&raw(RawTrigger::Immediate, Action::Fade(FadeIntentIr::In { secs: 30.0 })))
        );
        // DISAMBIGUATION: "in <dur>" binds the TRIGGER (fade keeps its default).
        assert_eq!(
            j(&tr("fade out in 20 minutes", &c).unwrap()[0]),
            j(&raw(
                RawTrigger::SpanElapsed { secs: 1200.0 },
                Action::Fade(FadeIntentIr::Out { secs: 30.0 })
            ))
        );
        // DISAMBIGUATION: "over <dur>" binds the FADE secs (trigger stays Immediate).
        assert_eq!(
            j(&tr("fade out over 20 minutes", &c).unwrap()[0]),
            j(&raw(RawTrigger::Immediate, Action::Fade(FadeIntentIr::Out { secs: 1200.0 })))
        );
        // A bare trailing duration after "fade out" binds the FADE secs.
        assert_eq!(
            j(&tr("fade out 20s", &c).unwrap()[0]),
            j(&raw(RawTrigger::Immediate, Action::Fade(FadeIntentIr::Out { secs: 20.0 })))
        );
    }

    #[test]
    fn corpus_fade_track_base_default() {
        let c = ctx(Some("s1"), 5);
        // A bare "the 3rd track" -> Absolute (matches how the queue is displayed).
        assert_eq!(
            j(&tr("fade the 3rd track", &c).unwrap()[0]),
            j(&raw(
                RawTrigger::QueuePosition { n: 3, base: PosBase::Absolute },
                Action::Fade(FadeIntentIr::Out { secs: 30.0 })
            ))
        );
        // "counting current as 1st" -> CurrentIsOne. This is the worked example
        // and MUST equal the P2 fixture RawPlan (QueuePosition{3,CurrentIsOne} +
        // Fade(Out{30})).
        assert_eq!(
            j(&tr("fade the 3rd track counting current as 1st", &c).unwrap()[0]),
            j(&raw(
                RawTrigger::QueuePosition { n: 3, base: PosBase::CurrentIsOne },
                Action::Fade(FadeIntentIr::Out { secs: 30.0 })
            ))
        );
        // Word ordinal + "song" works too.
        assert_eq!(
            j(&tr("fade the fifth song", &c).unwrap()[0]),
            j(&raw(
                RawTrigger::QueuePosition { n: 5, base: PosBase::Absolute },
                Action::Fade(FadeIntentIr::Out { secs: 30.0 })
            ))
        );
        // Outside the closed ordinal set -> NotUnderstood (adapter/model may punt).
        assert_eq!(tr("fade the last track", &c).unwrap_err(), NlError::NotUnderstood);
        assert_eq!(tr("fade the twenty-third track", &c).unwrap_err(), NlError::NotUnderstood);
    }

    #[test]
    fn corpus_unsupported_time_unit_punts() {
        // REGRESSION: an unsupported time unit ("days"/"weeks"/...) must NOT
        // collapse to bare seconds and emit a confident WRONG plan (in 2 days
        // -> pause in 2s). It must punt to NotUnderstood.
        let c = ctx(Some("s1"), 5);
        assert_eq!(tr("pause in 2 days", &c).unwrap_err(), NlError::NotUnderstood);
        assert_eq!(tr("stop in 3 weeks", &c).unwrap_err(), NlError::NotUnderstood);
        assert_eq!(tr("fade out in 5 days", &c).unwrap_err(), NlError::NotUnderstood);
        // Supported units still parse correctly (guards against over-punting).
        assert_eq!(
            j(&tr("fade out in 20 minutes", &c).unwrap()[0]),
            j(&raw(
                RawTrigger::SpanElapsed { secs: 1200.0 },
                Action::Fade(FadeIntentIr::Out { secs: 30.0 })
            ))
        );
    }

    #[test]
    fn corpus_stop_pause_and_boundary() {
        let c = ctx(Some("s1"), 5);
        assert_eq!(
            j(&tr("stop", &c).unwrap()[0]),
            j(&raw(RawTrigger::Immediate, Action::Stop))
        );
        assert_eq!(
            j(&tr("pause", &c).unwrap()[0]),
            j(&raw(RawTrigger::Immediate, Action::Pause))
        );
        // "after this album" -> AlbumBoundary{Current}.
        assert_eq!(
            j(&tr("stop after this album", &c).unwrap()[0]),
            j(&raw(
                RawTrigger::AlbumBoundary { track: TrackSel::Current },
                Action::Stop
            ))
        );
        // "after this track" / bare "after this" -> TrackAfterCurrent (a missed
        // "album" keyword must NOT downgrade silently).
        assert_eq!(
            j(&tr("stop after this track", &c).unwrap()[0]),
            j(&raw(RawTrigger::TrackAfterCurrent, Action::Stop))
        );
        assert_eq!(
            j(&tr("stop after this", &c).unwrap()[0]),
            j(&raw(RawTrigger::TrackAfterCurrent, Action::Stop))
        );
    }

    #[test]
    fn unparseable_in_or_at_tail_punts_never_immediate() {
        let c = ctx(Some("s1"), 5);
        // A misheard "in <x>" tail must PUNT (NotUnderstood), NEVER silently
        // downgrade to an Immediate stop.
        assert_eq!(tr("stop in a bit", &c).unwrap_err(), NlError::NotUnderstood);
        // A misheard "at <x>" tail likewise.
        assert_eq!(tr("stop at noonish", &c).unwrap_err(), NlError::NotUnderstood);
        assert_eq!(tr("pause at whenever", &c).unwrap_err(), NlError::NotUnderstood);
        // Sanity: a plain command with no in/at tail is still Immediate, and a
        // WELL-FORMED tail still parses.
        assert_eq!(
            j(&tr("stop", &c).unwrap()[0]),
            j(&raw(RawTrigger::Immediate, Action::Stop))
        );
        assert_eq!(
            j(&tr("stop in 10 minutes", &c).unwrap()[0]),
            j(&raw(RawTrigger::SpanElapsed { secs: 600.0 }, Action::Stop))
        );
    }

    #[test]
    fn unresolved_deferral_punts_never_stops_now() {
        let c = ctx(Some("s1"), 5);
        // A deferral conjunction we could not parse into a known boundary is a
        // deferred intent - PUNT, never a confident Immediate stop-now.
        for u in [
            "stop after the album ends",
            "stop when this song finishes",
            "pause once the track is over",
        ] {
            assert_eq!(tr(u, &c).unwrap_err(), NlError::NotUnderstood, "{u}");
        }
        // Filler after the verb is still Immediate.
        assert_eq!(
            j(&tr("stop now", &c).unwrap()[0]),
            j(&raw(RawTrigger::Immediate, Action::Stop))
        );
    }

    #[test]
    fn with_selector_handles_multibyte_before_with() {
        let c = ctx(Some("s1"), 5);
        // A multibyte char before " with " that changes byte length under
        // lowercasing must NOT garble the selector or slice mid-codepoint (panic).
        // The genre is still resolved correctly from the ORIGINAL text.
        match &tr("wake me at 7 \u{0130} with jazz", &c).unwrap()[0].action {
            Action::Wake { selector: Some(selector), .. } => {
                assert_eq!(format!("{selector:?}"), format!("{:?}", Selector::Genre("jazz".into())));
            }
            other => panic!("expected Wake with a selector, got {other:?}"),
        }
    }

    #[test]
    fn corpus_with_selector_class() {
        let c = ctx(Some("s1"), 5);
        // "with jazz" (in the genre lexicon) -> Genre; via a wake so "with" is set.
        match &tr("wake me at 7 with jazz", &c).unwrap()[0].action {
            Action::Wake { selector: Some(Selector::Genre(g)), .. } => assert_eq!(g, "jazz"),
            other => panic!("expected Wake(Genre), got {other:?}"),
        }
        // "with Bon Iver" (not a genre) -> Query, preserving the original case.
        match &tr("wake me at 7 with Bon Iver", &c).unwrap()[0].action {
            Action::Wake { selector: Some(Selector::Query(q)), .. } => assert_eq!(q, "Bon Iver"),
            other => panic!("expected Wake(Query), got {other:?}"),
        }
    }

    // F6: a trailing politeness filler ("please" / "thanks" / "thank you") must not
    // defeat the closed-genre match - "with jazz please" is the genre "jazz", not the
    // free-text Query "jazz please".
    #[test]
    fn corpus_with_selector_trims_trailing_filler() {
        let c = ctx(Some("s1"), 5);
        for u in [
            "wake me at 7 with jazz please",
            "wake me at 7 with jazz please.",
            "wake me at 7 with jazz thanks",
            "wake me at 7 with jazz thank you",
        ] {
            match &tr(u, &c).unwrap()[0].action {
                Action::Wake { selector: Some(Selector::Genre(g)), .. } => {
                    assert_eq!(g, "jazz", "{u}")
                }
                other => panic!("expected Wake(Genre jazz) for {u:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn corpus_calmer_similar_and_unresolvable() {
        let c = ctx(Some("song-42"), 5);
        // A bare "something calmer" (no start verb) stays append-only after current.
        let calmer = &tr("something calmer", &c).unwrap()[0];
        match &calmer.action {
            Action::Enqueue { selector: Selector::Calmer(id), .. } => {
                assert_eq!(id, &SongId("song-42".into()))
            }
            other => panic!("got {other:?}"),
        }
        assert!(matches!(calmer.trigger, RawTrigger::TrackAfterCurrent), "append-only");
        // A "play" ask STARTS playback: "play something calmer" -> immediate PlayNow,
        // never a silent append (the play_now correction). Matches "play something
        // calm" (no -er), which reaches the model and play_now-starts.
        let play_calmer = &tr("play something calmer", &c).unwrap()[0];
        match &play_calmer.action {
            Action::PlayNow { selector: Selector::Calmer(id), .. } => {
                assert_eq!(id, &SongId("song-42".into()))
            }
            other => panic!("got {other:?}"),
        }
        assert!(matches!(play_calmer.trigger, RawTrigger::Immediate), "starts now");
        // "more like this" -> Enqueue{Similar(current)}.
        match &tr("more like this", &c).unwrap()[0].action {
            Action::Enqueue { selector: Selector::Similar(id), .. } => {
                assert_eq!(id, &SongId("song-42".into()))
            }
            other => panic!("got {other:?}"),
        }
        // Nothing playing -> Unresolvable (NOT NotUnderstood, NOT a degenerate plan).
        let stopped = ctx(None, 0);
        assert_eq!(
            tr("more like this", &stopped).unwrap_err(),
            NlError::Unresolvable("nothing playing to match".into())
        );
    }

    // F5: nl wake emits ONE Action::Wake (enqueue -> start-from-silence -> play ->
    // ramp), so it actually STARTS playback from a stopped deck - not a silent
    // Enqueue + Fade(In) pair (append-only + volume-only) that never begins playing.
    #[test]
    fn corpus_wake_is_one_wake_plan() {
        let c = ctx(Some("s1"), 5);
        let plans = tr("wake me at 7 with jazz", &c).unwrap();
        assert_eq!(plans.len(), 1, "wake is a single atomic Wake plan");
        // now_civil is 05:30 UTC; "7" (morning) is 07:00 UTC today (tz = UTC).
        match plans[0].trigger {
            RawTrigger::WallClock { at } => {
                assert_eq!(at, Utc.with_ymd_and_hms(2026, 7, 15, 7, 0, 0).unwrap())
            }
            ref other => panic!("expected a WallClock trigger, got {other:?}"),
        }
        match &plans[0].action {
            Action::Wake { selector: Some(Selector::Genre(g)), count } => {
                assert_eq!(g, "jazz");
                assert_eq!(*count, rules::DEFAULT_ENQUEUE_COUNT);
            }
            other => panic!("expected Wake(Genre jazz), got {other:?}"),
        }
    }

    // F1: "wake me at 7" is a LOCAL-time promise resolved against ctx.tz (the system
    // local zone the handler now supplies), not UTC. With a +01:00 local zone, "at 7"
    // local is 06:00 UTC - proving the resolution honors the local offset.
    #[test]
    fn corpus_wake_resolves_local_zone() {
        // 05:30 UTC = 06:30 local at +01:00; "at 7" (07:00 local) is still ahead today.
        let mut c = ctx(Some("s1"), 5);
        c.tz = FixedOffset::east_opt(3600).unwrap(); // +01:00
        let plans = tr("wake me at 7 with jazz", &c).unwrap();
        match plans[0].trigger {
            RawTrigger::WallClock { at } => {
                // 07:00 at +01:00 -> 06:00 UTC (a UTC-only resolver would give 07:00Z).
                assert_eq!(at, Utc.with_ymd_and_hms(2026, 7, 15, 6, 0, 0).unwrap())
            }
            ref other => panic!("expected a WallClock trigger, got {other:?}"),
        }
    }

    // RECURRING phrasings arm a Daily trigger (local time-of-day + day set), not a
    // one-shot instant; a recurrence with no time of day is Ambiguous, never a
    // silent one-shot.
    #[test]
    fn corpus_recurring_schedules() {
        use hypodj_core::plan::DaySet;
        let c = ctx(Some("s1"), 5);
        let daily = |p: &RawPlan| match p.trigger {
            RawTrigger::Daily { hour, minute, days } => (hour, minute, days),
            ref other => panic!("expected Daily, got {other:?}"),
        };
        let p = tr("wake me every weekday at 7", &c).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(daily(&p[0]), (7, 0, DaySet::WEEKDAYS));
        assert!(matches!(p[0].action, Action::Wake { selector: None, .. }));

        let p = tr("wake me at 7:30 every day with jazz", &c).unwrap();
        assert_eq!(daily(&p[0]), (7, 30, DaySet::EVERY));
        assert!(matches!(&p[0].action, Action::Wake { selector: Some(Selector::Genre(g)), .. } if g == "jazz"));

        let p = tr("every day at 23:00 wind down", &c).unwrap();
        assert_eq!(daily(&p[0]), (23, 0, DaySet::EVERY));
        assert!(matches!(p[0].action, Action::Fade(FadeIntentIr::ToFloor { .. })));

        let p = tr("stop every night at 11", &c).unwrap();
        assert_eq!(daily(&p[0]), (23, 0, DaySet::EVERY));
        assert!(matches!(p[0].action, Action::Stop));

        let p = tr("fade out at 22:30 on weekends", &c).unwrap();
        assert_eq!(daily(&p[0]), (22, 30, DaySet::WEEKENDS));

        let p = tr("wake me at 8 every monday and friday", &c).unwrap();
        assert_eq!(daily(&p[0]), (8, 0, DaySet(0b0010001)));

        // A day name alone is a one-shot, never a recurrence.
        let p = tr("wake me at 7", &c).unwrap();
        assert!(matches!(p[0].trigger, RawTrigger::WallClock { .. }));

        assert!(matches!(tr("wind down every day", &c), Err(NlError::Ambiguous(_))));
        assert!(matches!(tr("pause every day in 10 minutes", &c), Err(NlError::Ambiguous(_))));
    }

    #[test]
    fn corpus_wake_pm_meridian_rolls_to_next_day() {
        let c = ctx(Some("s1"), 5);
        // "at 7 tonight" forces PM -> 19:00 today.
        let plans = tr("wake me at 7 tonight with jazz", &c).unwrap();
        match plans[0].trigger {
            RawTrigger::WallClock { at } => {
                assert_eq!(at, Utc.with_ymd_and_hms(2026, 7, 15, 19, 0, 0).unwrap())
            }
            ref other => panic!("got {other:?}"),
        }
        // now is 05:30; "at 5" (morning) has already passed -> tomorrow 05:00.
        let plans = tr("wake me at 5 with jazz", &c).unwrap();
        match plans[0].trigger {
            RawTrigger::WallClock { at } => {
                assert_eq!(at, Utc.with_ymd_and_hms(2026, 7, 16, 5, 0, 0).unwrap())
            }
            ref other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn empty_and_unknown_fall_through() {
        let c = ctx(Some("s1"), 5);
        assert_eq!(tr("", &c).unwrap_err(), NlError::NotUnderstood);
        assert_eq!(tr("do a barrel roll", &c).unwrap_err(), NlError::NotUnderstood);
    }

    fn mode(mode: PlayMode, on: bool) -> RawPlan {
        raw(RawTrigger::Immediate, Action::SetMode { mode, on })
    }

    #[test]
    fn corpus_playback_modes() {
        let c = ctx(Some("s1"), 5);
        let one = |u: &str| tr(u, &c).unwrap().iter().map(j).collect::<Vec<_>>();
        for (u, m, on) in [
            ("shuffle on", PlayMode::Random, true),
            ("shuffle", PlayMode::Random, true),
            ("turn off random", PlayMode::Random, false),
            ("stop shuffling", PlayMode::Random, false),
            ("random off", PlayMode::Random, false),
            ("repeat on", PlayMode::Repeat, true),
            ("turn off repeat", PlayMode::Repeat, false),
            ("loop the queue", PlayMode::Repeat, true),
            ("single mode", PlayMode::Single, true),
            ("single off", PlayMode::Single, false),
            ("consume", PlayMode::Consume, true),
            ("turn consume mode off", PlayMode::Consume, false),
        ] {
            assert_eq!(one(u), vec![j(&mode(m, on))], "{u}");
        }
        // "repeat this" is repeat-ONE: repeat ON + single ON, in that order.
        assert_eq!(
            one("repeat this song"),
            vec![j(&mode(PlayMode::Repeat, true)), j(&mode(PlayMode::Single, true))]
        );
        // Two modes in one breath is ambiguous, never a guess.
        assert!(matches!(tr("repeat and shuffle", &c), Err(NlError::Ambiguous(_))));
        // A mode word inside a content request is NOT a mode toggle.
        assert_eq!(tr("play some random jazz", &c).unwrap_err(), NlError::NotUnderstood);
    }

    #[test]
    fn corpus_favorites_actions() {
        let c = ctx(Some("s1"), 5);
        let starred_now = raw(
            RawTrigger::Immediate,
            Action::PlayNow { selector: Selector::Starred, count: rules::DEFAULT_FAVORITES_COUNT },
        );
        assert_eq!(j(&tr("play my favorites", &c).unwrap()[0]), j(&starred_now));
        assert_eq!(j(&tr("play my favourites please", &c).unwrap()[0]), j(&starred_now));
        assert_eq!(
            j(&tr("queue my favorites", &c).unwrap()[0]),
            j(&raw(
                RawTrigger::Immediate,
                Action::Enqueue { selector: Selector::Starred, count: rules::DEFAULT_FAVORITES_COUNT }
            ))
        );
        assert_eq!(
            j(&tr("play 5 favorites", &c).unwrap()[0]),
            j(&raw(RawTrigger::Immediate, Action::PlayNow { selector: Selector::Starred, count: 5 }))
        );
        // Shuffle my favorites: random ON, then start them.
        let plans: Vec<_> = tr("shuffle my favorites", &c).unwrap().iter().map(j).collect();
        assert_eq!(plans, vec![j(&mode(PlayMode::Random, true)), j(&starred_now)]);
        // A favorites WRITE ("star this") has no Action yet: punt, never guess.
        assert_eq!(tr("favorite this", &c).unwrap_err(), NlError::NotUnderstood);
    }

    #[test]
    fn corpus_sessions_winddown_and_sleep() {
        let c = ctx(Some("s1"), 5);
        let floor = Action::Fade(FadeIntentIr::ToFloor {
            secs: hypodj_core::config::DEFAULT_WINDDOWN_FADE_SECS as f64,
        });
        assert_eq!(j(&tr("wind down", &c).unwrap()[0]), j(&raw(RawTrigger::Immediate, floor.clone())));
        assert_eq!(
            j(&tr("start a wind-down session", &c).unwrap()[0]),
            j(&raw(RawTrigger::Immediate, floor.clone()))
        );
        assert_eq!(
            j(&tr("wind down in 10 minutes", &c).unwrap()[0]),
            j(&raw(RawTrigger::SpanElapsed { secs: 600.0 }, floor))
        );
        let out = Action::Fade(FadeIntentIr::Out {
            secs: hypodj_core::config::DEFAULT_SLEEP_FADE_SECS as f64,
        });
        for u in ["sleep in 30 minutes", "sleep timer 30 minutes", "go to sleep in 30 minutes"] {
            assert_eq!(
                j(&tr(u, &c).unwrap()[0]),
                j(&raw(RawTrigger::SpanElapsed { secs: 1800.0 }, out.clone())),
                "{u}"
            );
        }
        // No duration: ambiguous, never an invented timer.
        assert!(matches!(tr("sleep timer", &c), Err(NlError::Ambiguous(_))));
    }

    #[test]
    fn query_class_favorites_questions() {
        use hypodj_core::nl::NlQuery;
        let t = HybridTranslator::rules_only();
        for u in ["what is favorited?", "what's favorited", "list my favorites", "show favorites", "favorites", "my favorites?"] {
            assert_eq!(t.query(u), Some(NlQuery::Favorites), "{u}");
        }
        for u in ["is this favorited?", "is the current song starred", "did i favorite this?", "is current favorited?"] {
            assert_eq!(t.query(u), Some(NlQuery::IsCurrentFavorited), "{u}");
        }
        // Imperatives and unrelated asks are NOT questions (they stay on the action path).
        for u in ["play my favorites", "shuffle my favorites", "stop", "what is this"] {
            assert_eq!(t.query(u), None, "{u}");
        }
    }

    /// GOLDEN corpus (5znoe5i): one table of utterance -> exact plan batch (or the
    /// exact error), spanning every rules intent. A grammar change that shifts any
    /// row is a visible diff here.
    #[test]
    fn corpus_curation_filters() {
        use hypodj_core::plan::{CurateFilter as F, QueueSelector};
        let c = ctx(Some("s1"), 10);
        let rm = |f: F| raw(RawTrigger::Immediate, Action::Remove { sel: QueueSelector::Upcoming(f) });
        let cases: &[(&str, RawPlan)] = &[
            ("no remixes", rm(F::Remixes)),
            ("No more remixes please", rm(F::Remixes)),
            ("drop the remixes", rm(F::Remixes)),
            ("no live versions", rm(F::Live)),
            ("drop the rest of this album", rm(F::RestOfAlbum)),
            ("skip the rest of the album", rm(F::RestOfAlbum)),
            ("no Daft Punk", rm(F::Artist("Daft Punk".into()))),
            ("no more Daft Punk", rm(F::Artist("Daft Punk".into()))),
            ("no Pharrell as lead", rm(F::LeadArtist("Pharrell".into()))),
            ("no Pharrell as the main artist", rm(F::LeadArtist("Pharrell".into()))),
            ("drop everything by Aphex Twin", rm(F::Artist("Aphex Twin".into()))),
        ];
        for (u, want) in cases {
            let got = tr(u, &c).unwrap_or_else(|e| panic!("{u}: {e:?}"));
            assert_eq!(got.len(), 1, "{u}");
            assert_eq!(j(&got[0]), j(want), "{u}");
        }
        for u in ["no voices", "no vocals please", "more motion", "less energy", "only instrumentals"] {
            match tr(u, &c) {
                Err(NlError::Unresolvable(m)) => assert!(m.contains("iwwy04x"), "{u}: {m}"),
                other => panic!("{u}: must answer not-yet, got {other:?}"),
            }
        }
        // Not curation: falls through (never an artist named "thanks" or "this").
        for u in ["no thanks", "skip this", "remove the last 3 tracks"] {
            if let Ok(p) = tr(u, &c) {
                assert!(
                    !p.iter().any(|p| matches!(&p.action, Action::Remove { sel: QueueSelector::Upcoming(_) })),
                    "{u} read as curation"
                );
            }
        }
    }

    #[test]
    fn golden_corpus() {
        let c = ctx(Some("song-42"), 5);
        let fade_out = |s: f64| Action::Fade(FadeIntentIr::Out { secs: s });
        // Debug form, not JSON: the Calmer/Similar id selectors are not
        // internally-tag serializable, and Debug covers every field.
        let d = |p: &RawPlan| format!("{p:?}");
        let ok = |ps: Vec<RawPlan>| -> Result<Vec<String>, NlError> { Ok(ps.iter().map(d).collect()) };
        let imm = |a: Action| raw(RawTrigger::Immediate, a);
        let rows: Vec<(&str, Result<Vec<String>, NlError>)> = vec![
            ("fade out", ok(vec![imm(fade_out(30.0))])),
            ("fade in", ok(vec![imm(Action::Fade(FadeIntentIr::In { secs: 30.0 }))])),
            ("fade out over 20 minutes", ok(vec![imm(fade_out(1200.0))])),
            ("fade out in 20 minutes", ok(vec![raw(RawTrigger::SpanElapsed { secs: 1200.0 }, fade_out(30.0))])),
            ("fade the 3rd track", ok(vec![raw(RawTrigger::QueuePosition { n: 3, base: PosBase::Absolute }, fade_out(30.0))])),
            ("stop", ok(vec![imm(Action::Stop)])),
            ("pause", ok(vec![imm(Action::Pause)])),
            ("stop after this album", ok(vec![raw(RawTrigger::AlbumBoundary { track: TrackSel::Current }, Action::Stop)])),
            ("stop after this track", ok(vec![raw(RawTrigger::TrackAfterCurrent, Action::Stop)])),
            ("pause in 10 minutes", ok(vec![raw(RawTrigger::SpanElapsed { secs: 600.0 }, Action::Pause)])),
            ("something calmer", ok(vec![raw(RawTrigger::TrackAfterCurrent, Action::Enqueue { selector: Selector::Calmer(SongId("song-42".into())), count: 5 })])),
            ("more like this", ok(vec![raw(RawTrigger::TrackAfterCurrent, Action::Enqueue { selector: Selector::Similar(SongId("song-42".into())), count: 5 })])),
            ("play something calmer", ok(vec![imm(Action::PlayNow { selector: Selector::Calmer(SongId("song-42".into())), count: 5 })])),
            ("shuffle on", ok(vec![mode(PlayMode::Random, true)])),
            ("turn off random", ok(vec![mode(PlayMode::Random, false)])),
            ("repeat this", ok(vec![mode(PlayMode::Repeat, true), mode(PlayMode::Single, true)])),
            ("single mode", ok(vec![mode(PlayMode::Single, true)])),
            ("consume", ok(vec![mode(PlayMode::Consume, true)])),
            ("play my favorites", ok(vec![imm(Action::PlayNow { selector: Selector::Starred, count: 20 })])),
            ("queue my favorites", ok(vec![imm(Action::Enqueue { selector: Selector::Starred, count: 20 })])),
            ("wind down", ok(vec![imm(Action::Fade(FadeIntentIr::ToFloor { secs: 300.0 }))])),
            ("sleep in 30 minutes", ok(vec![raw(RawTrigger::SpanElapsed { secs: 1800.0 }, fade_out(480.0))])),
            ("", Err(NlError::NotUnderstood)),
            ("do a barrel roll", Err(NlError::NotUnderstood)),
            ("fade the last track", Err(NlError::NotUnderstood)),
            ("stop in a bit", Err(NlError::NotUnderstood)),
            ("pause in 2 days", Err(NlError::NotUnderstood)),
            ("sleep timer", Err(NlError::Ambiguous("sleep for how long?".into()))),
            ("repeat and shuffle", Err(NlError::Ambiguous("more than one playback mode named".into()))),
        ];
        for (u, want) in rows {
            let got = tr(u, &c).map(|ps| ps.iter().map(d).collect::<Vec<_>>());
            assert_eq!(got, want, "golden row {u:?}");
        }
        // The wake row carries a clock-resolved instant; pinned under the fixed ctx.
        let w = tr("wake me at 7 with jazz", &c).unwrap();
        assert_eq!(
            d(&w[0]),
            d(&raw(
                RawTrigger::WallClock { at: Utc.with_ymd_and_hms(2026, 7, 15, 7, 0, 0).unwrap() },
                Action::Wake { selector: Some(Selector::Genre("jazz".into())), count: 5 }
            ))
        );
    }

    // Sleep and wind-down name the daemon's single-instance FEATURES, so the hit
    // says so (the handler then arms them through its own singleton path). Nothing
    // else does - not even a fade that happens to share the sleep shape.
    #[test]
    fn sleep_and_winddown_are_marked_as_sessions() {
        use hypodj_core::nl::NlSession;
        let c = ctx(Some("s1"), 5);
        let s = |u: &str| RulesTranslator.translate(u, &c).unwrap().session;
        assert_eq!(s("sleep in 30 minutes"), Some(NlSession::Sleep));
        assert_eq!(s("sleep timer 45 minutes"), Some(NlSession::Sleep));
        assert_eq!(s("wind down in 10 minutes"), Some(NlSession::WindDown));
        assert_eq!(s("start a wind-down"), Some(NlSession::WindDown));
        assert_eq!(s("stop"), None);
        assert_eq!(s("shuffle on"), None);
    }

    // ── HYBRID orchestration ──────────────────────────────────────────────────

    struct StubLlm;
    impl Translator for StubLlm {
        fn translate(&self, _u: &str, _c: &NlContext) -> Result<NlHit, NlError> {
            Ok(NlHit {
                plans: vec![raw(RawTrigger::Immediate, Action::Stop)],
                source: NlSource::Llm,
                session: None,
            })
        }
    }

    #[test]
    fn hybrid_falls_through_only_on_not_understood() {
        let c = ctx(Some("s1"), 5);
        // Rules-only: an unknown utterance stays NotUnderstood.
        let rules_only = HybridTranslator::rules_only();
        assert_eq!(
            rules_only.translate("do a barrel roll", &c).unwrap_err(),
            NlError::NotUnderstood
        );
        // With an LLM: NotUnderstood falls through to the model.
        let hybrid = HybridTranslator::new(Some(Box::new(StubLlm)));
        let hit = hybrid.translate("do a barrel roll", &c).unwrap();
        assert_eq!(hit.source, NlSource::Llm);
        // But Unresolvable is a real fail and does NOT fall through to the model.
        let stopped = ctx(None, 0);
        assert_eq!(
            hybrid.translate("more like this", &stopped).unwrap_err(),
            NlError::Unresolvable("nothing playing to match".into())
        );
        // And a rules HIT is returned without consulting the model.
        let hit = hybrid.translate("stop", &c).unwrap();
        assert_eq!(hit.source, NlSource::Rules);
    }

    // ── ECHO round-trip: render DSL -> re-parse via parse_plan -> same IR ──────

    #[test]
    fn echo_round_trips_through_plan_dsl() {
        use hypodj_core::mpd::{parse, MpdCommand, PlanCmd};
        let c = ctx(Some("s1"), 5);
        for utterance in [
            "fade the 3rd track counting current as 1st",
            "fade out over 20 minutes",
            "fade out in 20 minutes",
            "stop after this album",
            "stop after this track",
            "shuffle on",
            "turn off repeat",
            "repeat this",
            "play my favorites",
            "queue my favorites",
        ] {
            let plans = tr(utterance, &c).unwrap();
            for p in &plans {
                let dsl = render_dsl(p).expect("rules plans are DSL-expressible");
                match parse(&format!("plan add {dsl}")) {
                    MpdCommand::Plan(PlanCmd::Add(reparsed)) => {
                        // The re-parsed plan carries origin "mpd"; compare the
                        // trigger + action + once only (origin is adapter-stamped).
                        assert_eq!(
                            serde_json::to_value(&reparsed.trigger).unwrap(),
                            serde_json::to_value(&p.trigger).unwrap(),
                            "trigger round-trip for {utterance:?}"
                        );
                        assert_eq!(
                            serde_json::to_value(&reparsed.action).unwrap(),
                            serde_json::to_value(&p.action).unwrap(),
                            "action round-trip for {utterance:?}"
                        );
                        assert_eq!(reparsed.once, p.once);
                    }
                    other => panic!("echo did not re-parse for {utterance:?}: {other:?}"),
                }
            }
        }
    }

    // ── LLM subset parse WITHOUT a model (canned constrained JSON) ─────────────

    #[test]
    fn canned_constrained_json_parses_to_raw_plan() {
        // Exactly the FLAT shape Claude emits for the worked example: a `type`
        // discriminator + flat scalars, with the queue slot on `slot`.
        let json = r#"{"type":"fade_out","secs":30.0,"when":"queue_position","slot":3,"once":true}"#;
        let raw = parse_llm_output(json).unwrap();
        assert_eq!(
            serde_json::to_value(&raw.trigger).unwrap(),
            serde_json::to_value(&RawTrigger::QueuePosition {
                n: 3,
                base: PosBase::Absolute
            })
            .unwrap()
        );
        match raw.action {
            Action::Fade(FadeIntentIr::Out { secs }) => assert_eq!(secs, 30.0),
            other => panic!("got {other:?}"),
        }
        // The adapter (not the model) owns origin: the model surface leaves it "".
        assert_eq!(raw.origin, "");
        // A malformed / off-surface JSON fails loud, never panics.
        assert!(parse_llm_output("{not json").is_err());
    }

    // ── FEATURE guard: the daemon default build pulls no model runtime ─────────

    #[test]
    fn default_build_is_model_free() {
        // Assert `cargo tree` on the DEFAULT feature set of hypodj-daemon contains
        // no llama-cpp / candle / ort crate. Skips gracefully if cargo is absent.
        let manifest = env!("CARGO_MANIFEST_DIR");
        let ws_root = std::path::Path::new(manifest)
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root");
        let out = std::process::Command::new("cargo")
            .args(["tree", "-e", "normal", "-p", "hypodj-daemon"])
            .current_dir(ws_root)
            .output();
        let out = match out {
            Ok(o) if o.status.success() => o,
            _ => return, // cargo unavailable / offline resolve failed: skip.
        };
        let tree = String::from_utf8_lossy(&out.stdout).to_lowercase();
        for banned in ["llama-cpp", "candle-core", " ort ", "llama_cpp"] {
            assert!(
                !tree.contains(banned),
                "default daemon build must stay model-free, found {banned:?}"
            );
        }
    }
}
