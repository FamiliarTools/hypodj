//! The OPTIONAL model (Claude Code / local-model) path.
//!
//! The RESTRICTED model surface ([`LlmRawPlan`]) is the TRUST BOUNDARY: it matches
//! the shape the LIVE Claude 2.1.204 capture proves the CLI emits - a top-level
//! `{"actions":[ {type, ...flat scalars} ]}` of FLAT, internally-tagged objects on
//! a CLOSED string enum. Off-surface intents (an unknown action `type`, an
//! absolute wall-clock instant, an identity-bearing `SongId` selector, or a
//! `Wake`/`ToFloor` fade) are UNREPRESENTABLE in this DTO, so they cannot
//! be produced no matter what the backend emits. The DTO + the VALIDATING
//! conversion + [`parse_llm_output`] are ALWAYS compiled and model-free
//! (unit-testable with a canned JSON string, no model, no network).
//!
//! Only the constrained-decode BACKEND ([`LlmBackend`]/[`LlmTranslator`]) and the
//! GBNF derivation from the JSON-Schema are gated behind `feature = "llm"`.

use serde::Deserialize;

use hypodj_core::plan::{
    Action, ClearScope, FadeIntentIr, MoveDest, PosBase, QueueSelector, RawPlan, RawTrigger,
    Selector, TrackSel,
};

/// Closed action lexicon. fade_out/fade_in are SEPARATE variants (no nested dir
/// tag), so To/ToFloor/Wake are unrepresentable. An unknown string fails
/// serde -> rejected. Claude emits this under the key `type` (every live sample);
/// `action`/`act` are drift aliases.
#[derive(Clone, Copy, Debug, Deserialize)]
#[cfg_attr(feature = "llm", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LlmActionKind {
    FadeOut,
    FadeIn,
    Stop,
    Pause,
    SetVolume,
    Enqueue,
    /// Remove queue entries a selector resolves (destructive; through the gate).
    Remove,
    /// Move selected entries to a destination.
    Move,
    /// Clear part or all of the queue (destructive; through the gate).
    Clear,
    /// Jump playback to the selected track.
    Play,
    /// Resolve a LIBRARY song (query/genre/radio), enqueue it, and START playback
    /// on it: the "play this specific song NOW" path (enqueue-then-start), distinct
    /// from the append-only Enqueue.
    PlayNow,
    /// Honest "no action": an off-topic / non-music / non-queue request. Prevents
    /// fabricating a wrong enqueue for a request that has no valid action.
    Noop,
}

/// Closed queue-selector lexicon (FLAT). An unknown string fails serde -> rejected.
#[derive(Clone, Copy, Debug, Deserialize)]
#[cfg_attr(feature = "llm", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LlmQSel {
    Current,
    Position,
    Range,
    Query,
    Last,
    /// CONTENT selector (enqueue/play_now only): "more like what is playing".
    /// Carries NO id - the daemon fills the current-track seed server-side, so the
    /// off-surface-id boundary is untouched. Rejected for the queue-edit actions
    /// (remove/move/clear/play) by [`build_qsel`]: it is not a live-queue selector.
    SimilarToCurrent,
}

/// Closed move-destination lexicon (FLAT).
#[derive(Clone, Copy, Debug, Deserialize)]
#[cfg_attr(feature = "llm", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LlmMoveDest {
    Position,
    Relative,
}

/// Closed clear-scope lexicon (FLAT).
#[derive(Clone, Copy, Debug, Deserialize)]
#[cfg_attr(feature = "llm", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LlmClearScope {
    All,
    AfterCurrent,
    Range,
}

/// Closed trigger lexicon, FLAT. `Now` is the default (most DJ intents are
/// immediate). No absolute-clock variant and NO DateTime-typed field anywhere ->
/// an absolute civil instant is unrepresentable.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[cfg_attr(feature = "llm", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LlmWhen {
    #[default]
    Now,
    AfterCurrent,
    AfterSecs,
    AlbumBoundary,
    QueuePosition,
    TimeRemaining,
}

/// The unit a spoken span was given in. The model copies the number and the
/// unit it heard ("20" + "min"); Rust does the arithmetic, so a small model never
/// has to multiply by 60.
#[derive(Clone, Copy, Debug, Deserialize)]
#[cfg_attr(feature = "llm", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LlmUnit {
    S,
    Min,
    H,
}

impl LlmUnit {
    fn secs(self) -> f64 {
        match self {
            LlmUnit::S => 1.0,
            LlmUnit::Min => 60.0,
            LlmUnit::H => 3600.0,
        }
    }
}

/// `n` in `unit` (seconds when the unit is omitted) as seconds, else the legacy
/// seconds field.
fn span(n: Option<f64>, unit: Option<LlmUnit>, legacy: Option<f64>) -> Option<f64> {
    n.map(|n| n * unit.map_or(1.0, LlmUnit::secs)).or(legacy)
}

/// The FLAT model surface: one object, a required closed discriminator plus
/// OPTIONAL scalars. deny_unknown_fields is deliberately OFF (Claude's payload
/// key names drift run-to-run); load-bearing fields carry aliases for the names
/// observed live, stray keys are ignored. Safety does NOT rest on rejecting
/// stray keys - it rests on the closed action/when enums + presence-only selector
/// inference, which make off-surface intents unrepresentable regardless of noise.
#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(feature = "llm", derive(schemars::JsonSchema))]
pub struct LlmRawPlan {
    /// REQUIRED closed discriminator. PRIMARY key is `type` (Claude's instinct).
    #[serde(rename = "type", alias = "action", alias = "act")]
    pub kind: LlmActionKind,

    /// Flat trigger; DEFAULTS to Now when Claude omits it - the direct fix for the
    /// omnipresent live "missing field `trigger`".
    #[serde(default, alias = "trigger")]
    pub when: LlmWhen,

    // -- action scalars (all optional; cross-checked in TryFrom) --
    #[serde(default, alias = "seconds", alias = "duration")]
    pub secs: Option<f64>,
    #[serde(default, alias = "volume", alias = "vol")]
    pub level: Option<u8>,
    #[serde(default, alias = "limit", alias = "n", alias = "amount")]
    pub count: Option<u32>,

    // -- enqueue selector: inferred by PRESENCE. No source enum, no id field ->
    //    only Query/Genre/Radio are reachable; Exact/Similar/Calmer (SongId) have
    //    nowhere to land. --
    #[serde(default, alias = "q", alias = "value")]
    pub query: Option<String>,
    #[serde(default, alias = "name")]
    pub genre: Option<String>,
    #[serde(default)]
    pub radio: bool,

    // -- `when` scalars --
    #[serde(default, alias = "seconds_remaining")]
    pub when_secs: Option<f64>,
    /// 1-based absolute queue slot for when = queue_position. NOT named `position`:
    /// Claude uses `position` for STRING hints ("end") that we must IGNORE, and a
    /// usize-typed `position` field would hard-fail deserialization on that string.
    #[serde(default, alias = "queue_slot")]
    pub slot: Option<usize>,

    // -- queue-edit scalars (Remove/Move/Clear/Play). The selector reuses `slot`
    //    (position), `count` (last-n), `query` (query match); range + dest + scope
    //    carry their own flat fields. All closed enums, so nothing off-surface. --
    #[serde(default)]
    pub sel: Option<LlmQSel>,
    #[serde(default, alias = "range_from")]
    pub range_start: Option<usize>,
    #[serde(default, alias = "range_to")]
    pub range_end: Option<usize>,
    #[serde(default)]
    pub dest: Option<LlmMoveDest>,
    #[serde(default)]
    pub dest_slot: Option<usize>,
    #[serde(default)]
    pub dest_rel: Option<i32>,
    #[serde(default)]
    pub scope: Option<LlmClearScope>,

    // -- spoken spans: number + unit as heard, converted in TryFrom. Preferred
    //    over the legacy seconds fields (`secs`/`when_secs`, which the cc backend
    //    still emits) when both are present. --
    /// Fade length: "over 20 minutes" -> over 20, over_unit min.
    #[serde(default)]
    pub over: Option<f64>,
    #[serde(default)]
    pub over_unit: Option<LlmUnit>,
    /// Delay before the action: "in 20 minutes" -> in 20, in_unit min.
    #[serde(default, rename = "in")]
    pub in_: Option<f64>,
    #[serde(default)]
    pub in_unit: Option<LlmUnit>,
    /// Time left in the current track for when = time_remaining.
    #[serde(default)]
    pub left: Option<f64>,
    #[serde(default)]
    pub left_unit: Option<LlmUnit>,

    #[serde(default)]
    pub once: bool,
}

/// Build a [`QueueSelector`] from the flat selector fields. Closed `sel` lexicon +
/// presence-checked scalars, so nothing off-surface is constructible.
fn build_qsel(p: &LlmRawPlan) -> Result<QueueSelector, String> {
    match p.sel.ok_or("queue action needs a `sel`")? {
        LlmQSel::Current => Ok(QueueSelector::Current),
        LlmQSel::Position => Ok(QueueSelector::Position(p.slot.ok_or("sel=position needs slot")?)),
        LlmQSel::Last => Ok(QueueSelector::Last(p.count.ok_or("sel=last needs count")? as usize)),
        LlmQSel::Range => Ok(QueueSelector::Range {
            start: p.range_start.ok_or("sel=range needs range_start")?,
            end: p.range_end.ok_or("sel=range needs range_end")?,
        }),
        LlmQSel::Query => Ok(QueueSelector::QueryMatch(
            p.query.clone().ok_or("sel=query needs query")?,
        )),
        // similar_to_current is a CONTENT selector (enqueue/play_now), never a
        // live-queue target - refuse it here so a remove/move/clear/play cannot
        // smuggle it in.
        LlmQSel::SimilarToCurrent => Err("similar_to_current is not a queue selector".into()),
    }
}

/// VALIDATING conversion (fallible: a flat bag needs cross-field checks). Every
/// candidate crosses here on the way to plan.rs validate/clamp. Nothing off-surface
/// is constructible (no Wake action, no To/ToFloor fade, no SongId-bearing
/// selector, no wall-clock). count is clamped to MAX_ENQUEUE downstream in
/// plan::validate; version=1 and origin="" (the adapter stamps nl:cc:<model>).
impl TryFrom<LlmRawPlan> for RawPlan {
    type Error = String;
    fn try_from(p: LlmRawPlan) -> Result<Self, String> {
        // A missing number means the default the rules use for the same ask, so
        // "fade in" from the model and from the rules arm the same plan. plan.rs
        // validate() still clamps fade secs to [min_dur, max_dur].
        use crate::rules::{DEFAULT_ENQUEUE_COUNT, DEFAULT_FADE_SECS};
        let delay = span(p.in_, p.in_unit, p.when_secs);
        let left = span(p.left, p.left_unit, None).or(delay);
        let fade_secs = span(p.over, p.over_unit, p.secs).unwrap_or(DEFAULT_FADE_SECS);
        // A specific title plays once; a genre, the radio or "more like this" is a
        // mix of the rules' default size.
        let content_count = |s: &Selector| {
            p.count.unwrap_or(if matches!(s, Selector::Query(_)) { 1 } else { DEFAULT_ENQUEUE_COUNT })
        };

        let queue_edit = matches!(p.kind, LlmActionKind::Remove | LlmActionKind::Play | LlmActionKind::Move);
        let trigger = match p.when {
            // Claude routinely omits `when` (-> Now) yet still expresses a delay
            // via when_secs/slot alone. Treating that as Immediate would silently
            // drop the delay and fire NOW, so infer the trigger from the timing
            // scalar rather than discarding it: a bare when_secs is a span, a bare
            // slot is a queue position. Only a truly bare Now stays Immediate. A
            // queue edit's `slot` is its SELECTOR ("remove the 2nd track"), never
            // a trigger: reading it as both armed "remove track 2 once track 2
            // plays" for an ask that meant "now".
            LlmWhen::Now => match (delay, p.slot.filter(|_| !queue_edit)) {
                (Some(secs), _) => RawTrigger::SpanElapsed { secs },
                (None, Some(n)) => RawTrigger::QueuePosition { n, base: PosBase::Absolute },
                (None, None) => RawTrigger::Immediate,
            },
            LlmWhen::AfterCurrent => RawTrigger::TrackAfterCurrent,
            LlmWhen::AfterSecs => RawTrigger::SpanElapsed {
                secs: delay.ok_or("after_secs needs a delay")?,
            },
            LlmWhen::AlbumBoundary => RawTrigger::AlbumBoundary { track: TrackSel::Current },
            LlmWhen::QueuePosition => RawTrigger::QueuePosition {
                n: p.slot.ok_or("queue_position needs slot")?,
                base: PosBase::Absolute,
            },
            LlmWhen::TimeRemaining => RawTrigger::TimeRemaining {
                track: TrackSel::Current,
                secs: left.ok_or("time_remaining needs a time left")?,
            },
        };

        let action = match p.kind {
            LlmActionKind::FadeOut => {
                Action::Fade(FadeIntentIr::Out { secs: fade_secs })
            }
            LlmActionKind::FadeIn => {
                Action::Fade(FadeIntentIr::In { secs: fade_secs })
            }
            LlmActionKind::Stop => Action::Stop,
            LlmActionKind::Pause => Action::Pause,
            LlmActionKind::SetVolume => Action::SetVolume(p.level.ok_or("set_volume needs level")?),
            LlmActionKind::Enqueue => {
                // similar_to_current wins (an explicit "more like this" token); else
                // presence-only inference: radio, then genre, then query. NO SongId
                // path exists, so this builds only SimilarToCurrent/Query/Genre/Radio
                // (the daemon fills the seed id server-side for SimilarToCurrent).
                let selector = if matches!(p.sel, Some(LlmQSel::SimilarToCurrent)) {
                    Selector::SimilarToCurrent
                } else if p.radio {
                    Selector::Radio
                } else if let Some(g) = p.genre.clone() {
                    Selector::Genre(g)
                } else if let Some(q) = p.query.clone() {
                    Selector::Query(q)
                } else {
                    return Err("enqueue needs query, genre, radio, or similar_to_current".into());
                };
                let count = content_count(&selector);
                Action::Enqueue { selector, count }
            }
            LlmActionKind::PlayNow => {
                // Same inference as Enqueue (similar_to_current > radio > genre > query);
                // no SongId path exists, so only SimilarToCurrent/Query/Genre/Radio.
                let selector = if matches!(p.sel, Some(LlmQSel::SimilarToCurrent)) {
                    Selector::SimilarToCurrent
                } else if p.radio {
                    Selector::Radio
                } else if let Some(g) = p.genre.clone() {
                    Selector::Genre(g)
                } else if let Some(q) = p.query.clone() {
                    Selector::Query(q)
                } else {
                    return Err("play_now needs query, genre, radio, or similar_to_current".into());
                };
                let count = content_count(&selector);
                Action::PlayNow { selector, count }
            }
            LlmActionKind::Remove => Action::Remove { sel: build_qsel(&p)? },
            LlmActionKind::Play => Action::Play { sel: build_qsel(&p)? },
            LlmActionKind::Noop => Action::Noop,
            LlmActionKind::Clear => {
                let scope = match p.scope.unwrap_or(LlmClearScope::All) {
                    LlmClearScope::All => ClearScope::All,
                    LlmClearScope::AfterCurrent => ClearScope::AfterCurrent,
                    LlmClearScope::Range => ClearScope::Range {
                        start: p.range_start.ok_or("scope=range needs range_start")?,
                        end: p.range_end.ok_or("scope=range needs range_end")?,
                    },
                };
                Action::Clear { scope }
            }
            LlmActionKind::Move => {
                let sel = build_qsel(&p)?;
                let dest = match p.dest.ok_or("move needs a `dest`")? {
                    LlmMoveDest::Position => {
                        MoveDest::Position(p.dest_slot.ok_or("dest=position needs dest_slot")?)
                    }
                    LlmMoveDest::Relative => {
                        MoveDest::Relative(p.dest_rel.ok_or("dest=relative needs dest_rel")?)
                    }
                };
                Action::Move { sel, dest }
            }
        };

        // origin stays empty; the adapter stamps nl:cc:<model>, never the model.
        Ok(RawPlan { version: 1, trigger, action, once: p.once, origin: String::new() })
    }
}

/// Parse Claude's reply into a validated RawPlan. Tolerates the
/// {"actions":[ {...} ]} wrapper Claude gravitates to (live capture) AND a bare
/// flat object. Takes actions[0] (one plan per utterance). Model-free + always
/// compiled - the SINGLE model-free choke point. An off-surface or malformed reply
/// surfaces a readable error rather than panicking or fabricating a plan.
pub fn parse_llm_output(json: &str) -> Result<RawPlan, String> {
    RawPlan::try_from(parse_llm_flat(json)?)
}

/// The flat surface object of a reply, before conversion (see
/// [`parse_llm_output`] for the accepted shapes).
pub fn parse_llm_flat(json: &str) -> Result<LlmRawPlan, String> {
    #[derive(Deserialize)]
    struct Wrapper {
        actions: Vec<LlmRawPlan>,
    }

    let t = json.trim();
    match serde_json::from_str::<Wrapper>(t) {
        Ok(w) => w.actions.into_iter().next().ok_or_else(|| "empty actions array".into()),
        Err(_) => serde_json::from_str::<LlmRawPlan>(t).map_err(|e| e.to_string()),
    }
}

/// GROUNDING: every free-text string and every number in a model plan must be
/// something the human actually SAID. A small model under a grammar always
/// produces a well-formed plan, so well-formedness proves nothing; what it gets
/// wrong is inventing content - a delay for "in a bit", a genre for "more like
/// this", a count for "some". A query/genre must occur in the utterance (case
/// folded); a count/level/slot/range/destination must be one of the quantities it
/// says; a span (number x unit) must equal a said duration in seconds, where a
/// spoken quantity counts only with its unit (the "a" of "in a bit" is no span). Omitted numbers take the rules' defaults
/// in `TryFrom`, so "never invent a number" costs the model nothing. `Err` names
/// the first ungrounded field; the translator folds it to NotUnderstood.
pub fn grounded(utterance: &str, p: &LlmRawPlan) -> Result<(), String> {
    let said = utterance.to_lowercase();
    for (name, text) in [("query", &p.query), ("genre", &p.genre)] {
        if let Some(t) = text {
            let t = t.trim().to_lowercase();
            if t.is_empty() || !said.contains(&t) {
                return Err(format!("{name} {t:?} is not in the request"));
            }
        }
    }
    let (values, spans) = crate::rules::spoken_quantities(utterance);
    let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
    let is_value = |v: f64| values.iter().any(|&x| close(x, v));
    let is_span = |secs: f64| spans.iter().any(|&x| close(x, secs));
    let numbers = [
        ("level", p.level.map(f64::from)),
        ("count", p.count.map(f64::from)),
        ("slot", p.slot.map(|v| v as f64)),
        ("range_start", p.range_start.map(|v| v as f64)),
        ("range_end", p.range_end.map(|v| v as f64)),
        ("dest_slot", p.dest_slot.map(|v| v as f64)),
        ("dest_rel", p.dest_rel.map(|v| f64::from(v.unsigned_abs()))),
    ];
    for (name, v) in numbers {
        if let Some(v) = v.filter(|&v| !is_value(v)) {
            return Err(format!("{name} {v} is not in the request"));
        }
    }
    let spans_given = [
        ("over", span(p.over, p.over_unit, p.secs)),
        ("in", span(p.in_, p.in_unit, p.when_secs)),
        ("left", span(p.left, p.left_unit, None)),
    ];
    for (name, secs) in spans_given {
        if let Some(secs) = secs.filter(|&s| !is_span(s)) {
            return Err(format!("{name} {secs}s is not in the request"));
        }
    }
    Ok(())
}

// ── constrained-decode backend (feature = "llm") ─────────────────────────────

#[cfg(feature = "llm")]
mod backend {
    use hypodj_core::nl::{NlContext, NlError, NlHit, NlSource, Translator};

    /// One chat message: `role` is `system`, `user` or `assistant`.
    #[derive(Clone, Debug)]
    pub struct ChatTurn {
        pub role: &'static str,
        pub content: String,
    }

    /// A local constrained-decode backend. Constrains generation against a GBNF
    /// and returns the raw JSON string the grammar produced for the assistant
    /// turn that follows `chat`.
    pub trait LlmBackend: Send + Sync {
        fn generate(&self, chat: &[ChatTurn], gbnf: &str) -> Result<String, String>;

        /// Evaluate `chat` ahead of any ask so a caching backend can reuse the
        /// state for every later ask sharing its prefix; others ignore it.
        fn prefill(&self, _chat: &[ChatTurn]) -> Result<(), String> {
            Ok(())
        }
    }

    /// A shared backend is a backend, so one model can be warmed on a side
    /// thread while the translator that owns it serves asks.
    impl<B: LlmBackend> LlmBackend for std::sync::Arc<B> {
        fn generate(&self, chat: &[ChatTurn], gbnf: &str) -> Result<String, String> {
            (**self).generate(chat, gbnf)
        }
        fn prefill(&self, chat: &[ChatTurn]) -> Result<(), String> {
            (**self).prefill(chat)
        }
    }

    /// The model translator: build a prompt, constrained-decode against the
    /// IR-derived GBNF, parse the output into a [`RawPlan`].
    pub struct LlmTranslator<B: LlmBackend> {
        backend: B,
        gbnf: &'static str,
    }

    impl<B: LlmBackend> LlmTranslator<B> {
        pub fn new(backend: B) -> Self {
            Self { backend, gbnf: crate::gbnf::GBNF }
        }

        /// The raw constrained-decode text for one ask, before parsing; the
        /// backend's reason on failure (the translator folds it to NotUnderstood).
        pub fn decode(&self, utterance: &str, _ctx: &NlContext) -> Result<String, String> {
            self.backend.generate(&Self::prompt(utterance), self.gbnf)
        }

        /// Evaluate the fixed instructions and examples once, before the first
        /// ask, so a caching backend answers every later ask from the warm prefix.
        /// Loads the model on first use; the daemon runs it off the async runtime.
        pub fn warm(&self) -> Result<(), String> {
            let mut chat = Self::prompt("");
            chat.pop();
            self.backend.prefill(&chat)
        }

        /// Parse an already-decoded plan for `utterance`, refuse it unless it is
        /// [`super::grounded`] in what was said, and convert it. Every refusal
        /// folds to NotUnderstood, the same answer the rules gave.
        pub fn translate_decoded(&self, utterance: &str, out: &str) -> Result<NlHit, NlError> {
            let flat = super::parse_llm_flat(out).map_err(|_| NlError::NotUnderstood)?;
            super::grounded(utterance, &flat).map_err(|_| NlError::NotUnderstood)?;
            let raw = hypodj_core::plan::RawPlan::try_from(flat).map_err(|_| NlError::NotUnderstood)?;
            Ok(NlHit { plans: vec![raw], source: NlSource::Llm, session: None })
        }

        /// The conversation a small local model decodes under: fixed
        /// instructions, then worked examples as real user/assistant turns, then
        /// the ask. Everything before the ask is identical on every call, so a
        /// caching backend evaluates it once. The grammar already fixes the
        /// SHAPE; the examples carry the MEANING, chosen in contrast pairs where a
        /// small model confuses two readings ("fade out in" vs "over"), with a
        /// default-only example per number ("fade in" carries no length), and with
        /// as many refusals as a small model needs to see that noop is a real
        /// answer. The queue state is deliberately NOT in the prompt: no answer
        /// depends on it, and it would split the cached prefix.
        fn prompt(utterance: &str) -> Vec<ChatTurn> {
            let mut chat = vec![ChatTurn { role: "system", content: SYSTEM.into() }];
            for (ask, plan) in SHOTS {
                chat.push(ChatTurn { role: "user", content: (*ask).into() });
                chat.push(ChatTurn { role: "assistant", content: (*plan).into() });
            }
            chat.push(ChatTurn { role: "user", content: utterance.into() });
            chat
        }
    }

    const SYSTEM: &str = "You turn one request to a music player into one JSON plan. \
Copy a number and its unit exactly as the request says it; never invent a \
        number, and leave a number out when the request gives none. \"in N minutes\" \
        or \"after N minutes\" DELAYS the action (in, in_unit). \"over N minutes\" \
        or \"for N minutes\" is how LONG a fade takes (over, over_unit). \"after this track\" is when after_current; \"after this album\" \
        is when album_boundary. \"more like this\" is sel similar_to_current. A \
        query or genre must be words from the request. If the request is vague \
        about when, is not about playing music, or you are not sure, answer noop.";

    /// Worked examples: (ask, plan) pairs in the grammar's exact output form.
    /// Refusals are spread through the list rather than bunched at its end, where
    /// recency made a 1.5B model answer noop to the very asks it had an example
    /// for ("more like this", "stop after this track").
    pub(super) const SHOTS: &[(&str, &str)] = &[
        ("fade in", r#"{"type":"fade_in","once":true}"#),
        ("fade out over 20 minutes", r#"{"type":"fade_out","over":20,"over_unit":"min","once":true}"#),
        ("fade out in 20 minutes", r#"{"type":"fade_out","in":20,"in_unit":"min","once":true}"#),
        ("stop in a bit", r#"{"type":"noop","once":true}"#),
        ("pause after this track", r#"{"type":"pause","when":"after_current","once":true}"#),
        ("more like this", r#"{"type":"enqueue","sel":"similar_to_current","once":true}"#),
        ("what is the capital of France", r#"{"type":"noop","once":true}"#),
        ("in 10 minutes fade out over 30 seconds", r#"{"type":"fade_out","over":30,"over_unit":"s","in":10,"in_unit":"min","once":true}"#),
        ("volume 40", r#"{"type":"set_volume","level":40,"once":true}"#),
        ("fade out later", r#"{"type":"noop","once":true}"#),
        ("queue 3 jazz songs", r#"{"type":"enqueue","genre":"jazz","count":3,"once":true}"#),
        ("stop when the album is over", r#"{"type":"stop","when":"album_boundary","once":true}"#),
        ("what song is this", r#"{"type":"noop","once":true}"#),
        ("play some bon iver", r#"{"type":"play_now","query":"bon iver","once":true}"#),
        ("fade the music out", r#"{"type":"fade_out","once":true}"#),
        ("turn on the kitchen lights", r#"{"type":"noop","once":true}"#),
        ("remove the 2nd track", r#"{"type":"remove","sel":"position","slot":2,"once":true}"#),
        ("stop in an hour", r#"{"type":"stop","in":1,"in_unit":"h","once":true}"#),
        ("make me a sandwich", r#"{"type":"noop","once":true}"#),
        ("fade out the 4th track", r#"{"type":"fade_out","when":"queue_position","slot":4,"once":true}"#),
        ("clear the queue", r#"{"type":"clear","scope":"all","once":true}"#),
        ("set a reminder for tomorrow", r#"{"type":"noop","once":true}"#),
    ];

    impl<B: LlmBackend> Translator for LlmTranslator<B> {
        fn translate(&self, utterance: &str, ctx: &NlContext) -> Result<NlHit, NlError> {
            let out = self.decode(utterance, ctx).map_err(|_| NlError::NotUnderstood)?;
            self.translate_decoded(utterance, &out)
        }
    }
}

#[cfg(feature = "llm")]
pub use backend::{ChatTurn, LlmBackend, LlmTranslator};
#[cfg(all(feature = "llm", test))]
use backend::SHOTS;

#[cfg(test)]
mod tests {
    use super::*;
    use hypodj_core::plan::{Action, FadeIntentIr, Selector};

    // EVERY allowed action AND trigger must round-trip through parse_llm_output on
    // the FLAT surface Claude actually emits (`type` discriminator + flat scalars),
    // including the omitted-trigger case that defaults to Now/Immediate.
    #[test]
    fn parse_round_trips_every_action_kind() {
        // fade out with an explicit span trigger.
        let p = parse_llm_output(
            r#"{"type":"fade_out","secs":10.0,"when":"after_secs","when_secs":300.0}"#,
        )
        .unwrap();
        assert!(matches!(p.action, Action::Fade(FadeIntentIr::Out { .. })));
        assert!(matches!(p.trigger, RawTrigger::SpanElapsed { .. }));

        // fade in.
        let p = parse_llm_output(r#"{"type":"fade_in","secs":10.0}"#).unwrap();
        assert!(matches!(p.action, Action::Fade(FadeIntentIr::In { .. })));

        // OMITTED trigger => Now => Immediate (the direct fix for "missing field
        // `trigger`"); omitted fade secs still plans (clamped downstream).
        let p = parse_llm_output(r#"{"type":"fade_out"}"#).unwrap();
        assert!(matches!(p.action, Action::Fade(FadeIntentIr::Out { .. })));
        assert!(matches!(p.trigger, RawTrigger::Immediate));

        // stop / pause, immediate.
        let p = parse_llm_output(r#"{"type":"stop"}"#).unwrap();
        assert!(matches!(p.action, Action::Stop));
        assert!(matches!(p.trigger, RawTrigger::Immediate));
        let p = parse_llm_output(r#"{"type":"pause"}"#).unwrap();
        assert!(matches!(p.action, Action::Pause));

        // set_volume(level), immediate.
        let p = parse_llm_output(r#"{"type":"set_volume","level":42}"#).unwrap();
        assert!(matches!(p.action, Action::SetVolume(42)));

        // enqueue(query).
        let p =
            parse_llm_output(r#"{"type":"enqueue","query":"bon iver","count":5}"#).unwrap();
        match p.action {
            Action::Enqueue { selector: Selector::Query(q), count } => {
                assert_eq!(q, "bon iver");
                assert_eq!(count, 5);
            }
            other => panic!("expected enqueue(query), got {other:?}"),
        }

        // enqueue(genre).
        let p = parse_llm_output(r#"{"type":"enqueue","genre":"jazz","count":3}"#).unwrap();
        match p.action {
            Action::Enqueue { selector: Selector::Genre(g), count } => {
                assert_eq!(g, "jazz");
                assert_eq!(count, 3);
            }
            other => panic!("expected enqueue(genre), got {other:?}"),
        }

        // enqueue(radio); an omitted count is the rules' mix size, 5.
        let p = parse_llm_output(r#"{"type":"enqueue","radio":true}"#).unwrap();
        assert!(matches!(
            p.action,
            Action::Enqueue { selector: Selector::Radio, count: 5 }
        ));

        // The {"actions":[...]} wrapper Claude gravitates to; first action wins.
        let p = parse_llm_output(r#"{"actions":[{"type":"pause"}]}"#).unwrap();
        assert!(matches!(p.action, Action::Pause));

        // Every FLAT trigger kind maps to its IR trigger.
        let p = parse_llm_output(r#"{"type":"stop","when":"after_current"}"#).unwrap();
        assert!(matches!(p.trigger, RawTrigger::TrackAfterCurrent));
        let p = parse_llm_output(r#"{"type":"stop","when":"album_boundary"}"#).unwrap();
        assert!(matches!(p.trigger, RawTrigger::AlbumBoundary { .. }));
        let p =
            parse_llm_output(r#"{"type":"stop","when":"queue_position","slot":3}"#).unwrap();
        assert!(matches!(
            p.trigger,
            RawTrigger::QueuePosition { n: 3, base: PosBase::Absolute }
        ));
        let p = parse_llm_output(
            r#"{"type":"fade_out","secs":10.0,"when":"time_remaining","when_secs":30.0}"#,
        )
        .unwrap();
        assert!(matches!(p.trigger, RawTrigger::TimeRemaining { .. }));
    }

    // A delay expressed ONLY via when_secs/slot (Claude omitted the `when` tag, so
    // it defaults to Now) must NOT be silently dropped and fired immediately: the
    // timing scalar drives the trigger.
    #[test]
    fn parse_infers_delay_when_when_tag_omitted() {
        // stop "in 300s" with no `when` tag => span, not immediate.
        let p = parse_llm_output(r#"{"type":"stop","when_secs":300.0}"#).unwrap();
        match p.trigger {
            RawTrigger::SpanElapsed { secs } => assert_eq!(secs, 300.0),
            other => panic!("expected SpanElapsed, got {other:?}"),
        }
        // fade_out with when_secs but no `when` tag => span, not immediate.
        let p = parse_llm_output(r#"{"type":"fade_out","secs":10.0,"when_secs":300.0}"#).unwrap();
        assert!(matches!(p.trigger, RawTrigger::SpanElapsed { .. }));
        // slot-only queue_position intent with no `when` tag => queue position.
        let p = parse_llm_output(r#"{"type":"stop","slot":3}"#).unwrap();
        assert!(matches!(
            p.trigger,
            RawTrigger::QueuePosition { n: 3, base: PosBase::Absolute }
        ));
        // Truly bare Now stays Immediate.
        let p = parse_llm_output(r#"{"type":"stop"}"#).unwrap();
        assert!(matches!(p.trigger, RawTrigger::Immediate));
    }

    // The queue-edit actions (remove/move/clear/play) + the noop class round-trip
    // through the FLAT surface Claude emits, building the intended selector/scope/dest.
    #[test]
    fn parse_round_trips_queue_edit_actions() {
        use hypodj_core::plan::{ClearScope, MoveDest, QueueSelector};

        // remove last 3.
        let p = parse_llm_output(r#"{"type":"remove","sel":"last","count":3}"#).unwrap();
        assert!(matches!(
            p.action,
            Action::Remove { sel: QueueSelector::Last(3) }
        ));
        // A queue edit's slot selects the row; it is not a deferred trigger.
        let p = parse_llm_output(r#"{"type":"remove","sel":"position","slot":2}"#).unwrap();
        assert!(matches!(p.trigger, RawTrigger::Immediate));
        assert!(matches!(p.action, Action::Remove { sel: QueueSelector::Position(2) }));
        // remove current.
        let p = parse_llm_output(r#"{"type":"remove","sel":"current"}"#).unwrap();
        assert!(matches!(p.action, Action::Remove { sel: QueueSelector::Current }));
        // remove by query.
        let p = parse_llm_output(r#"{"type":"remove","sel":"query","query":"so what"}"#).unwrap();
        match p.action {
            Action::Remove { sel: QueueSelector::QueryMatch(q) } => assert_eq!(q, "so what"),
            other => panic!("got {other:?}"),
        }
        // remove a range.
        let p = parse_llm_output(
            r#"{"type":"remove","sel":"range","range_start":2,"range_end":5}"#,
        )
        .unwrap();
        assert!(matches!(
            p.action,
            Action::Remove { sel: QueueSelector::Range { start: 2, end: 5 } }
        ));
        // clear after_current / all / range.
        assert!(matches!(
            parse_llm_output(r#"{"type":"clear","scope":"after_current"}"#).unwrap().action,
            Action::Clear { scope: ClearScope::AfterCurrent }
        ));
        assert!(matches!(
            parse_llm_output(r#"{"type":"clear","scope":"all"}"#).unwrap().action,
            Action::Clear { scope: ClearScope::All }
        ));
        assert!(matches!(
            parse_llm_output(r#"{"type":"clear"}"#).unwrap().action,
            Action::Clear { scope: ClearScope::All }
        ));
        // move last->top and a relative move.
        assert!(matches!(
            parse_llm_output(
                r#"{"type":"move","sel":"last","count":1,"dest":"position","dest_slot":1}"#
            )
            .unwrap()
            .action,
            Action::Move { sel: QueueSelector::Last(1), dest: MoveDest::Position(1) }
        ));
        assert!(matches!(
            parse_llm_output(
                r#"{"type":"move","sel":"position","slot":4,"dest":"relative","dest_rel":-2}"#
            )
            .unwrap()
            .action,
            Action::Move { sel: QueueSelector::Position(4), dest: MoveDest::Relative(-2) }
        ));
        // play by query / position.
        match parse_llm_output(r#"{"type":"play","sel":"query","query":"blue"}"#).unwrap().action {
            Action::Play { sel: QueueSelector::QueryMatch(q) } => assert_eq!(q, "blue"),
            other => panic!("got {other:?}"),
        }
        assert!(matches!(
            parse_llm_output(r#"{"type":"play","sel":"position","slot":6}"#).unwrap().action,
            Action::Play { sel: QueueSelector::Position(6) }
        ));
        // noop: honest no-action for an off-topic ask (never a fabricated enqueue).
        assert!(matches!(parse_llm_output(r#"{"type":"noop"}"#).unwrap().action, Action::Noop));
    }

    // play_now (enqueue-then-start) parses from the SAME flat query/genre/radio
    // surface as enqueue, but builds the distinct Action::PlayNow. "queue X" stays
    // append-only Action::Enqueue - the two are never conflated.
    #[test]
    fn parse_distinguishes_play_now_from_append_only_enqueue() {
        // "play <specific title>" -> play_now (query), count defaults to 1.
        let p = parse_llm_output(r#"{"type":"play_now","query":"at the door"}"#).unwrap();
        match p.action {
            Action::PlayNow { selector: Selector::Query(q), count } => {
                assert_eq!(q, "at the door");
                assert_eq!(count, 1);
            }
            other => panic!("expected play_now(query), got {other:?}"),
        }
        // play_now by genre / radio also reachable (same presence inference).
        assert!(matches!(
            parse_llm_output(r#"{"type":"play_now","genre":"jazz"}"#).unwrap().action,
            Action::PlayNow { selector: Selector::Genre(_), .. }
        ));
        assert!(matches!(
            parse_llm_output(r#"{"type":"play_now","radio":true}"#).unwrap().action,
            Action::PlayNow { selector: Selector::Radio, .. }
        ));
        // play_now with no selector fails loud (never a fabricated SongId path).
        assert!(parse_llm_output(r#"{"type":"play_now"}"#).is_err());
        assert!(parse_llm_output(r#"{"type":"play_now","id":"song-42"}"#).is_err());

        // The SAME title under "enqueue" stays append-only Enqueue, NOT PlayNow.
        assert!(matches!(
            parse_llm_output(r#"{"type":"enqueue","query":"at the door","count":1}"#)
                .unwrap()
                .action,
            Action::Enqueue { selector: Selector::Query(_), .. }
        ));
    }

    // "more like what is playing": the model emits sel=similar_to_current (NO id,
    // no query/genre) on enqueue OR play_now, and it parses to the id-free
    // Selector::SimilarToCurrent. The daemon fills the seed server-side, so the
    // off-surface-id boundary is untouched.
    #[test]
    fn parse_similar_to_current_carries_no_id() {
        // enqueue "more like this one".
        let p = parse_llm_output(
            r#"{"type":"enqueue","sel":"similar_to_current","count":5}"#,
        )
        .unwrap();
        match p.action {
            Action::Enqueue { selector: Selector::SimilarToCurrent, count } => {
                assert_eq!(count, 5);
            }
            other => panic!("expected enqueue(similar_to_current), got {other:?}"),
        }
        // play_now "play more like what is playing"; an omitted count is 5.
        let p = parse_llm_output(r#"{"type":"play_now","sel":"similar_to_current"}"#).unwrap();
        assert!(matches!(
            p.action,
            Action::PlayNow { selector: Selector::SimilarToCurrent, count: 5 }
        ));
        // similar_to_current wins even if the model ALSO (wrongly) attaches an id or
        // query: the id is ignored (no field lands it) and the selector stays id-free.
        let p = parse_llm_output(
            r#"{"type":"enqueue","sel":"similar_to_current","id":"song-42","query":"x","count":3}"#,
        )
        .unwrap();
        assert!(matches!(
            p.action,
            Action::Enqueue { selector: Selector::SimilarToCurrent, .. }
        ));
        // similar_to_current is a CONTENT selector, NOT a live-queue selector: a
        // remove/move/play/clear that tries it is rejected (never a wrong-target op).
        assert!(parse_llm_output(r#"{"type":"remove","sel":"similar_to_current"}"#).is_err());
        assert!(parse_llm_output(r#"{"type":"play","sel":"similar_to_current"}"#).is_err());
    }

    // A queue-edit action with a MISSING required scalar fails loud (no fabricated
    // wrong-target op): a remove with a `sel` but no supporting field is rejected.
    #[test]
    fn parse_rejects_incomplete_queue_edits() {
        assert!(parse_llm_output(r#"{"type":"remove"}"#).is_err());
        assert!(parse_llm_output(r#"{"type":"remove","sel":"last"}"#).is_err());
        assert!(parse_llm_output(r#"{"type":"remove","sel":"query"}"#).is_err());
        assert!(parse_llm_output(r#"{"type":"move","sel":"current"}"#).is_err());
        assert!(parse_llm_output(r#"{"type":"move","sel":"current","dest":"relative"}"#).is_err());
        assert!(parse_llm_output(r#"{"type":"play"}"#).is_err());
    }

    // Spoken spans: the model copies number + unit, Rust multiplies, and an
    // omitted number takes the rules' default rather than a guess.
    #[test]
    fn spans_convert_in_rust_and_defaults_match_the_rules() {
        let p = parse_llm_output(r#"{"type":"fade_out","over":20,"over_unit":"min"}"#).unwrap();
        assert!(matches!(p.action, Action::Fade(FadeIntentIr::Out { secs }) if secs == 1200.0));
        assert!(matches!(p.trigger, RawTrigger::Immediate));
        let p = parse_llm_output(r#"{"type":"fade_out","when":"after_secs","in":20,"in_unit":"min"}"#).unwrap();
        assert!(matches!(p.trigger, RawTrigger::SpanElapsed { secs } if secs == 1200.0));
        assert!(matches!(p.action, Action::Fade(FadeIntentIr::Out { secs }) if secs == 30.0));
        let p = parse_llm_output(r#"{"type":"stop","in":1.5,"in_unit":"h"}"#).unwrap();
        assert!(matches!(p.trigger, RawTrigger::SpanElapsed { secs } if secs == 5400.0));
        let p = parse_llm_output(r#"{"type":"stop","when":"time_remaining","left":30,"left_unit":"s"}"#).unwrap();
        assert!(matches!(p.trigger, RawTrigger::TimeRemaining { secs, .. } if secs == 30.0));
        let p = parse_llm_output(r#"{"type":"fade_in"}"#).unwrap();
        assert!(matches!(p.action, Action::Fade(FadeIntentIr::In { secs }) if secs == 30.0));
        // A specific title plays once; a mix is the rules' default size.
        let p = parse_llm_output(r#"{"type":"play_now","query":"at the door"}"#).unwrap();
        assert!(matches!(p.action, Action::PlayNow { count: 1, .. }));
        let p = parse_llm_output(r#"{"type":"enqueue","genre":"jazz"}"#).unwrap();
        assert!(matches!(p.action, Action::Enqueue { count: 5, .. }));
    }

    // GROUNDING: a plan whose words or numbers the human never said is refused;
    // one built only from what was said (or from omitted defaults) passes.
    #[test]
    fn grounding_refuses_invented_content() {
        let g = |u: &str, json: &str| grounded(u, &parse_llm_flat(json).unwrap());
        // Invented numbers: the 0.5B/1.5B failure shapes.
        assert!(g("stop in a bit", r#"{"type":"stop","when":"after_secs","in":60,"in_unit":"s"}"#).is_err());
        assert!(g("fade in", r#"{"type":"fade_in","over":5,"over_unit":"s"}"#).is_err());
        assert!(g("queue some jazz", r#"{"type":"enqueue","genre":"jazz","count":5}"#).is_err());
        assert!(g("volume up", r#"{"type":"set_volume","level":60}"#).is_err());
        // Invented words.
        assert!(g("more like this", r#"{"type":"enqueue","genre":"pop"}"#).is_err());
        assert!(g("play something nice", r#"{"type":"play_now","query":"bon iver"}"#).is_err());
        // What was said, in any unit the said quantity converts to.
        assert!(g("fade out in 20 minutes", r#"{"type":"fade_out","when":"after_secs","in":20,"in_unit":"min"}"#).is_ok());
        assert!(g("fade out in 20 minutes", r#"{"type":"fade_out","when":"after_secs","in":1200,"in_unit":"s"}"#).is_ok());
        assert!(g("fade it out over a quarter hour", r#"{"type":"fade_out","over":15,"over_unit":"min"}"#).is_ok());
        assert!(g("fade it out over a quarter hour", r#"{"type":"fade_out","over":0.25,"over_unit":"h"}"#).is_ok());
        assert!(g("stop in an hour and a half", r#"{"type":"stop","in":90,"in_unit":"min"}"#).is_ok());
        assert!(g("shut it off in forty-five minutes", r#"{"type":"stop","in":45,"in_unit":"min"}"#).is_ok());
        assert!(g("queue 3 Jazz songs", r#"{"type":"enqueue","genre":"jazz","count":3}"#).is_ok());
        assert!(g("get rid of the second song", r#"{"type":"remove","sel":"position","slot":2}"#).is_ok());
        assert!(g("make it louder, like 70%", r#"{"type":"set_volume","level":70}"#).is_ok());
        assert!(g("move it up two", r#"{"type":"move","sel":"current","dest":"relative","dest_rel":-2}"#).is_ok());
        // Defaults are never ungrounded, and noop says nothing.
        assert!(g("fade in", r#"{"type":"fade_in"}"#).is_ok());
        assert!(g("tell me a joke", r#"{"type":"noop"}"#).is_ok());
    }

    // Every worked example teaches a plan the parser accepts and the grounding
    // check would let through, so the prompt never demonstrates a refused answer.
    #[cfg(feature = "llm")]
    #[test]
    fn every_shot_parses_and_is_grounded() {
        for (ask, plan) in SHOTS {
            let flat = parse_llm_flat(plan).unwrap_or_else(|e| panic!("{ask:?}: {e}"));
            grounded(ask, &flat).unwrap_or_else(|e| panic!("{ask:?}: {e}"));
            RawPlan::try_from(flat).unwrap_or_else(|e| panic!("{ask:?}: {e}"));
        }
    }

    // An OFF-surface intent must be REJECTED even if a backend ignores the schema.
    // The closed `type`/`when` enums + presence-only selector inference make every
    // off-surface effect UNREPRESENTABLE, so serde/TryFrom fails loud instead of
    // arming it.
    #[test]
    fn parse_rejects_off_surface_actions() {
        // An unknown action `type` (Action::Wake and friends) is not in the lexicon.
        assert!(parse_llm_output(r#"{"type":"wake","count":5}"#).is_err());
        // The nested fade dirs (wake_to / to_floor / to) have NO representation:
        // there is no `dir` field and no such action `type`.
        assert!(parse_llm_output(r#"{"type":"wake_to","secs":10.0}"#).is_err());
        assert!(parse_llm_output(r#"{"type":"to_floor","secs":10.0}"#).is_err());
        // A wall_clock trigger is off the `when` lexicon; an absolute datetime has
        // nowhere to land (no variant, no DateTime field).
        assert!(parse_llm_output(r#"{"type":"stop","when":"wall_clock"}"#).is_err());
        assert!(parse_llm_output(
            r#"{"type":"stop","when":"wall_clock","at":"2026-01-01T00:00:00Z"}"#
        )
        .is_err());
        // An identity-bearing selector cannot be built: an `id`/`song_id` field is
        // ignored, and with no query/genre/radio the enqueue conversion fails loud
        // rather than fabricating a SongId selector.
        assert!(parse_llm_output(r#"{"type":"enqueue","id":"song-42"}"#).is_err());
        // A missing discriminator is rejected (no default for the action kind).
        assert!(parse_llm_output(r#"{"when":"now"}"#).is_err());
        // An unknown queue-selector / clear-scope tag is off the closed lexicon.
        assert!(parse_llm_output(r#"{"type":"remove","sel":"everything"}"#).is_err());
        assert!(parse_llm_output(r#"{"type":"clear","scope":"nuke"}"#).is_err());
        assert!(parse_llm_output(r#"{"type":"move","sel":"current","dest":"warp"}"#).is_err());
    }
}
