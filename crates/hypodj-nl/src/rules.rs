//! The deterministic RULES fast-path: a hand-rolled tokenizer + keyword matcher
//! that turns the common corpus phrasings into a bounded [`RawPlan`]. ZERO model,
//! fully deterministic, table-tested (utterance -> exact RawPlan / exact NlError).
//!
//! Every emitted plan carries `origin: ""`: the adapter (handler) stamps
//! `nl:rules`, never this grammar. WallClock and Immediate ARE allowed here (they
//! are Rules-only; the LLM subset omits them).

use chrono::{TimeZone, Utc};

use hypodj_core::config::{DEFAULT_SLEEP_FADE_SECS, DEFAULT_WINDDOWN_FADE_SECS};
use hypodj_core::nl::{NlContext, NlError, NlHit, NlQuery, NlSession, NlSource, Translator};
use hypodj_core::plan::{
    Action, CurateFilter, DaySet, FadeIntentIr, PlayMode, PosBase, QueueSelector, RawPlan,
    RawTrigger, Selector, TrackSel,
};

/// The default fade duration a bare "fade out"/"fade in" emits. 30s matches the
/// P2 executor fixture (`fixture_track3_fade_out_on_track_start`), so the worked
/// example RawPlan is byte-identical to the fixture's.
pub const DEFAULT_FADE_SECS: f64 = 30.0;
/// The default track count a bare Enqueue selector ("something calmer", "wake me
/// with jazz") emits. Append-only + count-clamped keeps this a bounded hole.
pub const DEFAULT_ENQUEUE_COUNT: u32 = 5;
/// The default track count for a bare "play my favorites". Larger than a mood
/// enqueue (favorites are a session, not a nudge); still clamped by `max_enqueue`.
pub const DEFAULT_FAVORITES_COUNT: u32 = 20;

/// The closed genre lexicon: "with <X>" maps to [`Selector::Genre`] only when X is
/// one of these; anything else is the safe free-text [`Selector::Query`].
const GENRES: &[&str] = &[
    "jazz", "ambient", "techno", "classical", "house", "drum and bass", "dnb",
    "hip hop", "hip-hop", "rock", "pop", "electronic", "soul", "funk", "reggae",
    "blues", "metal", "disco", "folk",
];

/// The deterministic, stateless rules translator.
pub struct RulesTranslator;

impl Translator for RulesTranslator {
    fn translate(&self, utterance: &str, ctx: &NlContext) -> Result<NlHit, NlError> {
        let mut session = None;
        let plans = rules_body(utterance, ctx, &mut session)?;
        Ok(NlHit { plans, source: NlSource::Rules, session })
    }

    fn query(&self, utterance: &str) -> Option<NlQuery> {
        query_rules(utterance).or_else(|| cancel_rules(utterance))
    }
}

/// Lowercased words with surrounding punctuation stripped ("favorited?" ->
/// "favorited"), so questions and trailing punctuation match the lexicon.
fn words(utterance: &str) -> Vec<String> {
    utterance
        .to_lowercase()
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '-').to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

/// A favorites word: favorite(s)/favourite(s), favorited/favourited, starred, faves.
fn is_fav_word(w: &str) -> bool {
    matches!(
        w,
        "favorite" | "favorites" | "favourite" | "favourites" | "favorited" | "favourited"
            | "starred" | "faves" | "fave"
    )
}

/// The READ-side grammar: a QUESTION about favorites returns an [`NlQuery`] (an
/// immediate answer, no plan). Only clearly interrogative shapes match, so an
/// imperative ("play my favorites") stays on the action path.
pub fn query_rules(utterance: &str) -> Option<NlQuery> {
    let w = words(utterance);
    let wv: Vec<&str> = w.iter().map(String::as_str).collect();
    if !wv.iter().any(|t| is_fav_word(t)) {
        return None;
    }
    let first = *wv.first()?;
    let about_current = wv
        .iter()
        .any(|t| matches!(*t, "this" | "current" | "it" | "playing" | "song" | "track"));
    // "is this favorited?", "is the current song starred?", "did i favorite this?"
    if matches!(first, "is" | "isn't" | "did" | "have" | "was") && about_current {
        return Some(NlQuery::IsCurrentFavorited);
    }
    // "what is favorited?", "list my favorites", "show favorites", "my favorites?",
    // a bare "favorites".
    let listing = matches!(first, "what" | "what's" | "whats" | "which" | "list" | "show")
        || (wv.len() <= 2 && (first == "my" || is_fav_word(first)));
    if listing && !about_current {
        return Some(NlQuery::Favorites);
    }
    None
}

/// A feature-cancel ask ("cancel the sleep timer", "turn off the wind-down"):
/// a cancel word plus exactly one feature, and NO time in it - "cancel the sleep
/// timer in 30 minutes" is a timed cancel hypodj cannot schedule, so it falls to
/// the action path, where the negation guard makes it ambiguous.
pub fn cancel_rules(utterance: &str) -> Option<NlQuery> {
    let w = words(utterance);
    let wv: Vec<&str> = w.iter().map(String::as_str).collect();
    let low = utterance.to_lowercase();
    let cancel = wv.iter().any(|t| matches!(*t, "cancel" | "abort" | "disable" | "kill" | "off" | "stop" | "clear" | "remove"));
    if !cancel || wv.iter().any(|t| t.chars().any(|c| c.is_ascii_digit())) {
        return None;
    }
    let sleep = wv.contains(&"sleep");
    let wind = low.contains("wind down") || low.contains("wind-down") || low.contains("winddown");
    match (sleep, wind) {
        (true, false) => Some(NlQuery::CancelSession(NlSession::Sleep)),
        (false, true) => Some(NlQuery::CancelSession(NlSession::WindDown)),
        _ => None,
    }
}

/// The shared NEGATION / CONTRAST guard. A word that negates or inverts the ask
/// ("don't go to sleep", "anything but my favorites", "never allow remixes",
/// "cancel ... in 30 minutes") makes every positive rule below read the opposite
/// of what was meant, so rather than teach each rule to parse it, the action path
/// refuses the whole utterance as ambiguous. `Some(word)` names the trigger.
///
/// Deliberately NOT here: "no" / "without" / "skip" / "drop" (the subtractive
/// curation grammar IS a negation and owns them), and the mode grammar's own
/// off-words, which [`rules_body`] lets through before applying this guard.
fn negation(wv: &[&str]) -> Option<String> {
    const NEGATORS: &[&str] = &[
        "don't", "dont", "never", "not", "isn't", "won't", "wont", "shouldn't", "doesn't",
        "except", "besides", "excluding", "instead", "rather", "cancel", "abort",
    ];
    for (i, w) in wv.iter().enumerate() {
        if NEGATORS.contains(w) {
            return Some((*w).to_string());
        }
        let prev = if i > 0 { wv[i - 1] } else { "" };
        // "anything but X" / "everything but X", and "other than X".
        if *w == "but" && matches!(prev, "anything" | "everything" | "something" | "all" | "nothing") {
            return Some(format!("{prev} but"));
        }
        if *w == "than" && prev == "other" {
            return Some("other than".into());
        }
    }
    None
}

/// The utterance split into clauses at `,` / `;` / a spaced dash, trimmed, empty
/// clauses dropped. Always at least one entry (the whole text when unsplit).
fn clauses(text: &str) -> Vec<&str> {
    let v: Vec<&str> = text
        .split([',', ';'])
        .flat_map(|c| c.split(" - "))
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    if v.is_empty() { vec![text.trim()] } else { v }
}

/// The ONE negation shape the guard lets through: the head clause is a mood ask
/// (calmer / like this) carrying no negation of its own, and every negation sits
/// in a LATER clause that is only a degree qualifier ("not too loud", "but
/// nothing too fast", "not so heavy"). Such a clause cannot negate the verb or the
/// object, and a soft mood selector already leans the way it asks. Anything else
/// (a negated head, a tail that names a thing - "not my favorites", "except Daft
/// Punk" - or a hard action like a fade or stop) stays refused.
fn tolerated_degree_tail(text: &str) -> bool {
    let cs = clauses(text);
    if cs.len() < 2 {
        return false;
    }
    let head = words(cs[0]);
    let hv: Vec<&str> = head.iter().map(String::as_str).collect();
    if negation(&hv).is_some() {
        return false;
    }
    let hl = cs[0].to_lowercase();
    if !(hl.contains("calmer") || hl.contains("more like this") || hl.contains("something like this")) {
        return false;
    }
    cs[1..].iter().all(|c| {
        let w = words(c);
        let wv: Vec<&str> = w.iter().map(String::as_str).collect();
        let wv: Vec<&str> = match wv.first() {
            Some(&"but") | Some(&"and") => wv[1..].to_vec(),
            _ => wv,
        };
        matches!(wv.as_slice(), [neg, deg, _] | [neg, deg, _, "please"]
            if matches!(*neg, "not" | "nothing")
                && matches!(*deg, "too" | "so" | "very" | "overly"))
    })
}

/// Emit a raw plan with `origin` left empty (the adapter stamps it) and `once`.
fn plan(trigger: RawTrigger, action: Action, once: bool) -> RawPlan {
    RawPlan { version: 1, trigger, action, once, origin: String::new() }
}

/// The single entry point: match the utterance to one of the closed corpus
/// patterns, or fail with a SPECIFIC [`NlError`] (NotUnderstood falls through).
pub fn translate_rules(utterance: &str, ctx: &NlContext) -> Result<Vec<RawPlan>, NlError> {
    rules_body(utterance, ctx, &mut None)
}

/// [`translate_rules`], also reporting through `session` when the match was one of
/// the daemon's single-instance features (see [`NlHit::session`]).
fn rules_body(
    utterance: &str,
    ctx: &NlContext,
    session: &mut Option<NlSession>,
) -> Result<Vec<RawPlan>, NlError> {
    let text = utterance.trim();
    if text.is_empty() {
        return Err(NlError::NotUnderstood);
    }
    let low = text.to_lowercase();
    let toks: Vec<String> = low.split_whitespace().map(String::from).collect();
    let tv: Vec<&str> = toks.iter().map(String::as_str).collect();

    // 0. the negation / contrast guard: only the mode grammar parses its own
    // negation ("don't shuffle"); every other rule would read the inverse.
    {
        let wd = words(text);
        let wdv: Vec<&str> = wd.iter().map(String::as_str).collect();
        if let Some(neg) = negation(&wdv) {
            if tolerated_degree_tail(text) {
                // The negation is scoped to a trailing degree qualifier of a mood
                // ask ("something calmer, not too loud"): it governs neither the
                // verb nor the object, so the head clause is read alone.
                let head = clauses(text)[0];
                return rules_body(head, ctx, session);
            }
            if let Some(plans) = modes(&wdv)? {
                return Ok(plans);
            }
            return Err(NlError::Ambiguous(format!(
                "\"{neg}\" negates the ask; say what to do rather than what not to (e.g. \"sleep off\", \"clear filters\")"
            )));
        }
    }

    // 1. wake ("wake me at 7 with jazz") - an ordered same-instant batch.
    if tv.contains(&"wake") && tv.contains(&"at") {
        return wake(&tv, &low, text, ctx);
    }

    // 1b. favorites actions ("play my favorites", "queue my favorites", "shuffle
    // my favorites"). Questions about favorites never reach here (query path).
    let wd = words(text);
    let wdv: Vec<&str> = wd.iter().map(String::as_str).collect();
    if wdv.iter().any(|t| is_fav_word(t)) {
        return favorites(&wdv);
    }

    // 1c. playback modes ("shuffle on", "turn off random", "repeat this").
    // Before stop/pause so "stop shuffling" is a mode change, not a stop.
    if let Some(plans) = modes(&wdv)? {
        return Ok(plans);
    }

    // 1c'. subtractive curation over the UPCOMING queue ("no remixes", "no Daft
    // Punk as lead", "drop the rest of this album"), and the honest not-yet for
    // refinements that need audio-content metadata ("no voices", "more motion").
    // Its inverse first: "allow remixes again" / "clear filters" lifts a HELD
    // exclusion, and must not be read as a curation by the "no X" rule.
    if let Some(plans) = allow_again(text) {
        return Ok(plans);
    }
    if let Some(plans) = curate(text, ctx)? {
        return Ok(plans);
    }

    // 1d. sessions: wind-down ("start a wind-down", "wind down in 10 minutes") and
    // the sleep timer ("sleep in 30 minutes").
    if low.contains("wind down") || low.contains("wind-down") || low.contains("winddown") {
        let trigger = boundary_or_time(&tv, &low, text, ctx)?;
        let secs = DEFAULT_WINDDOWN_FADE_SECS as f64;
        *session = Some(NlSession::WindDown);
        return one(plan(trigger, Action::Fade(FadeIntentIr::ToFloor { secs }), true));
    }
    if tv.first() == Some(&"sleep") || low.contains("sleep timer") || low.contains("go to sleep")
    {
        let p = sleep(&tv)?;
        *session = Some(NlSession::Sleep);
        return one(p);
    }

    // 2. calmer / similar ("something calmer", "more like this").
    if low.contains("calmer") {
        return one(calmer_similar(&tv, true, ctx)?);
    }
    if low.contains("more like this") || low.contains("something like this") {
        return one(calmer_similar(&tv, false, ctx)?);
    }

    // 3. "fade the Nth track [counting current as 1st]". A positional fade names a
    // "track"/"song"; if the ordinal is outside the closed set, fade_track returns
    // NotUnderstood (it must NOT silently downgrade to a plain immediate fade).
    if tv.contains(&"fade") && (tv.contains(&"track") || tv.contains(&"song")) {
        return one(fade_track(&tv, &low)?);
    }

    // 4. stop / pause (with an optional boundary/time trigger).
    if tv.first() == Some(&"stop") || tv.first() == Some(&"pause") {
        return one(stop_pause(&tv, &low, text, ctx)?);
    }

    // 5. immediate / timed fade ("fade out [over|in] ...").
    if tv.contains(&"fade") {
        return one(fade_immediate(&tv, &low, text, ctx)?);
    }

    Err(NlError::NotUnderstood)
}

fn one(p: RawPlan) -> Result<Vec<RawPlan>, NlError> {
    Ok(vec![p])
}

// ── favorites / modes / sleep ────────────────────────────────────────────────

/// "play my favorites" -> PlayNow{Starred}; "queue|add my favorites" ->
/// Enqueue{Starred} (append-only); "shuffle my favorites" -> random ON, then
/// PlayNow{Starred}. Anything else naming favorites is not understood here (a
/// "star this" write has no Action yet - punt rather than guess).
fn favorites(wv: &[&str]) -> Result<Vec<RawPlan>, NlError> {
    let count = leading_count(wv).unwrap_or(DEFAULT_FAVORITES_COUNT);
    let first = wv.first().copied().unwrap_or("");
    let play_now = || plan(RawTrigger::Immediate, Action::PlayNow { selector: Selector::Starred, count }, true);
    match first {
        "play" | "put" | "throw" | "start" => Ok(vec![play_now()]),
        "shuffle" => Ok(vec![
            plan(RawTrigger::Immediate, Action::SetMode { mode: PlayMode::Random, on: true }, true),
            play_now(),
        ]),
        "queue" | "add" | "enqueue" | "append" => Ok(vec![plan(
            RawTrigger::Immediate,
            Action::Enqueue { selector: Selector::Starred, count },
            true,
        )]),
        _ => Err(NlError::NotUnderstood),
    }
}

/// Map a word to the playback mode it names, if any.
fn mode_word(w: &str) -> Option<PlayMode> {
    Some(match w {
        "shuffle" | "shuffling" | "shuffled" | "random" | "randomize" | "unshuffle" => {
            PlayMode::Random
        }
        "repeat" | "repeating" | "loop" | "looping" => PlayMode::Repeat,
        "single" => PlayMode::Single,
        "consume" | "consuming" => PlayMode::Consume,
        _ => return None,
    })
}

/// Playback modes. Exactly one mode word must appear (two is ambiguous, e.g.
/// "repeat and shuffle"); the default direction is ON, flipped by an off-word.
/// "repeat this [song|track]" means repeat-ONE: repeat ON + single ON (the MPD
/// spelling of looping the current track). `Ok(None)` means no mode word.
fn modes(wv: &[&str]) -> Result<Option<Vec<RawPlan>>, NlError> {
    let mut found: Vec<PlayMode> = wv.iter().filter_map(|w| mode_word(w)).collect();
    found.dedup();
    let mode = match found.as_slice() {
        [] => return Ok(None),
        [m] => *m,
        _ => return Err(NlError::Ambiguous("more than one playback mode named".into())),
    };
    // Utterances that only MENTION a mode word inside a longer content request
    // ("play some random jazz") are not mode toggles: keep this to short commands
    // whose other words are all from the closed mode-phrase vocabulary.
    const FILLER: &[&str] = &[
        "turn", "set", "switch", "put", "enable", "disable", "on", "off", "mode", "the",
        "please", "playback", "this", "song", "track", "current", "one", "no", "stop",
        "don't", "dont", "it", "now", "again", "queue", "and", "to",
    ];
    if wv.iter().any(|w| mode_word(w).is_none() && !FILLER.contains(w)) {
        return Ok(None);
    }
    let off = wv
        .iter()
        .any(|w| matches!(*w, "off" | "disable" | "no" | "stop" | "don't" | "dont" | "unshuffle"));
    let set = |mode| plan(RawTrigger::Immediate, Action::SetMode { mode, on: !off }, true);
    let this = wv.iter().any(|w| matches!(*w, "this" | "song" | "track" | "current" | "one"));
    if mode == PlayMode::Repeat && this && !off {
        return Ok(Some(vec![set(PlayMode::Repeat), set(PlayMode::Single)]));
    }
    Ok(Some(vec![set(mode)]))
}

// ── curation ────────────────────────────────────────────────────────────────

/// Case-insensitive ASCII prefix strip that keeps the ORIGINAL casing of the rest
/// (an artist name is echoed as he typed it).
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

/// Case-insensitive ASCII suffix strip, keeping the original casing.
fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let cut = s.len().checked_sub(suffix.len())?;
    let tail = s.get(cut..)?;
    tail.eq_ignore_ascii_case(suffix).then(|| &s[..cut])
}

/// Refinements that need to HEAR the music (vocal presence, energy, motion).
/// hypodj has no such metadata yet; answering them from titles would be a guess.
const CONTENT_WORDS: &[&str] = &[
    "voices", "voice", "vocals", "vocal", "singing", "singer", "lyrics", "words",
    "instrumental", "instrumentals", "motion", "movement", "energy", "energetic",
    "groove", "tempo", "faster", "slower", "brighter", "darker", "heavier",
];

/// Generic nouns that a bare "no X" must not read as an artist name.
const NOT_ARTISTS: &[&str] = &[
    "thanks", "thank you", "music", "songs", "tracks", "song", "track", "sound", "volume",
    "more", "way", "problem", "idea", "", "this", "that", "it",
];

/// Function words that are never an artist name on their own ("no the", "no a").
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "i", "me", "my", "you", "we", "he", "she", "they", "them", "of", "to", "and",
    "or", "but", "so", "is", "it's", "its", "this", "that", "these", "those", "some", "any",
];

/// Lift a HELD curation exclusion. `None` when the utterance is not one.
/// "clear filters" / "reset the filters" lifts every one; "allow remixes again" /
/// "bring Daft Punk back" / "remixes are fine again" lifts one. An unprefixed
/// phrase (or the loose "bring X") must carry an again/back/fine marker, so a
/// bare "remixes" or "bring me something" never reads as a lift.
fn allow_again(text: &str) -> Option<Vec<RawPlan>> {
    let t = text.trim().trim_end_matches(['.', '!', '?']).trim();
    let low = t.to_lowercase();
    let allow = |filter: Option<CurateFilter>| {
        Some(vec![plan(RawTrigger::Immediate, Action::Allow { filter }, true)])
    };
    const ALL: &[&str] = &[
        "clear filters", "clear the filters", "clear all filters", "clear filter", "clear the filter",
        "reset filters", "reset the filters", "remove filters", "remove the filters", "drop the filters",
        "drop filters", "no filters", "allow everything again", "allow anything again", "unfilter",
        "stop filtering", "lift the filters", "lift filters", "clear exclusions", "clear the exclusions",
    ];
    if ALL.contains(&low.as_str()) {
        return allow(None);
    }
    let marked = |b: &str| {
        let mut b = b;
        let mut hit = false;
        for s in [" again", " back in", " back", " are fine", " is fine", " are ok", " is ok"] {
            if let Some(x) = strip_suffix_ci(b, s) {
                b = x.trim_end();
                hit = true;
            }
        }
        (b.to_string(), hit)
    };
    let mut body = None;
    for (p, need) in [
        ("allow ", false),
        ("bring back ", false),
        ("unblock ", false),
        ("unfilter ", false),
        ("stop filtering ", false),
        ("stop excluding ", false),
        ("bring ", true),
    ] {
        if let Some(rest) = strip_prefix_ci(t, p) {
            body = Some((rest.trim(), need));
            break;
        }
    }
    let (name, hit) = match body {
        Some((b, need)) => {
            let (b, hit) = marked(b);
            (b, hit || !need)
        }
        None => marked(t),
    };
    if !hit {
        return None;
    }
    let mut name = name.as_str();
    for lead in ["the ", "some ", "any "] {
        if let Some(b) = strip_prefix_ci(name, lead) {
            name = b;
        }
    }
    let nl = name.to_lowercase();
    if matches!(nl.as_str(), "remixes" | "remix" | "rmx" | "remixed tracks") {
        return allow(Some(CurateFilter::Remixes));
    }
    if matches!(nl.as_str(), "live" | "live versions" | "live tracks" | "live recordings" | "live songs") {
        return allow(Some(CurateFilter::Live));
    }
    if matches!(nl.as_str(), "everything" | "anything" | "it all" | "all") {
        return allow(None);
    }
    if NOT_ARTISTS.contains(&nl.as_str()) || nl.split_whitespace().count() > 5 {
        return None;
    }
    allow(Some(CurateFilter::Artist(name.to_string())))
}

/// Subtractive curation over the UPCOMING queue. `Ok(None)` when the utterance is
/// not a curation request (the caller falls through to the other rules). Every
/// hit is one immediate `Remove { Upcoming(filter) }`, which the daemon journals,
/// so `undo` restores the rows.
fn curate(text: &str, ctx: &NlContext) -> Result<Option<Vec<RawPlan>>, NlError> {
    let t = text.trim().trim_end_matches(['.', '!', '?']).trim();
    let low = t.to_lowercase();
    let lw = words(t);
    let content = lw.iter().any(|w| CONTENT_WORDS.contains(&w.as_str()));
    let refining = ["no ", "less ", "more ", "fewer ", "without ", "drop ", "skip ", "only "]
        .iter()
        .any(|p| low.starts_with(p));
    if content && refining {
        return Err(NlError::Unresolvable(
            "that refinement needs audio-content metadata (vocals, energy, motion) hypodj does not have yet - see task iwwy04x".into(),
        ));
    }
    let mut body = None;
    let mut by_artist_only = false;
    for p in ["no more ", "no ", "without ", "none of ", "lose the ", "drop ", "skip ", "remove ", "cut "] {
        if let Some(rest) = strip_prefix_ci(t, p) {
            by_artist_only = !matches!(p, "no more " | "no " | "without " | "none of ");
            body = Some(rest.trim());
            break;
        }
    }
    let Some(mut body) = body else { return Ok(None) };
    for tail in [" please", " anymore", " from the queue", " in the queue", " from the list"] {
        if let Some(b) = strip_suffix_ci(body, tail) {
            body = b.trim_end();
        }
    }
    for lead in ["the ", "any ", "all "] {
        if let Some(b) = strip_prefix_ci(body, lead) {
            body = b;
        }
    }
    let bl = body.to_lowercase();
    let remove = |f: CurateFilter| {
        Ok(Some(vec![plan(
            RawTrigger::Immediate,
            Action::Remove { sel: QueueSelector::Upcoming(f) },
            true,
        )]))
    };
    if matches!(bl.as_str(), "remixes" | "remix" | "remixed tracks" | "remixed stuff" | "rmx") {
        return remove(CurateFilter::Remixes);
    }
    if matches!(
        bl.as_str(),
        "live" | "live versions" | "live tracks" | "live stuff" | "live recordings" | "live songs"
    ) {
        return remove(CurateFilter::Live);
    }
    if matches!(
        bl.as_str(),
        "rest of this album" | "rest of the album" | "rest of album" | "this album" | "more of this album"
    ) || bl == "of this album"
    {
        return remove(CurateFilter::RestOfAlbum);
    }
    // "drop everything by X" / "skip anything by X": a verb-led removal must name
    // its artist with "by", so a bare "skip this" never reads as an artist.
    let mut name = body;
    if by_artist_only {
        let mut found = None;
        for p in ["everything by ", "anything by ", "songs by ", "tracks by ", "stuff by ", "by "] {
            if let Some(n) = strip_prefix_ci(body, p) {
                found = Some(n.trim());
                break;
            }
        }
        match found {
            Some(n) => name = n,
            None => return Ok(None),
        }
    } else {
        for p in ["more ", "songs by ", "tracks by ", "stuff by "] {
            if let Some(n) = strip_prefix_ci(name, p) {
                name = n.trim();
            }
        }
    }
    let mut lead_only = false;
    for s in [" as the lead artist", " as lead artist", " as the main artist", " as main artist", " as the lead", " as lead", " leading"] {
        if let Some(n) = strip_suffix_ci(name, s) {
            name = n.trim_end();
            lead_only = true;
            break;
        }
    }
    let nl = name.to_lowercase();
    if NOT_ARTISTS.contains(&nl.as_str()) || STOP_WORDS.contains(&nl.as_str()) {
        return Ok(None);
    }
    let name = name.to_string();
    let filter = if lead_only { CurateFilter::LeadArtist(name.clone()) } else { CurateFilter::Artist(name.clone()) };
    // An artist is only an artist if the queue credits one by that name, judged by
    // the very predicate the remove runs - otherwise "no jazz" or "no i mean the
    // other one" would arm an exclusion for a name nobody has.
    if !ctx.queue_rows.iter().any(|r| hypodj_core::plan::curate_matches(&filter, r, None)) {
        return Err(NlError::Ambiguous(format!("no artist \"{name}\" in the queue")));
    }
    remove(filter)
}

/// "sleep in 30 minutes" / "sleep timer 45 minutes" / "go to sleep in an hour"
/// -> after the span, fade OUT over the default sleep fade (the `sleep` command's
/// shape). A sleep with no readable duration is Ambiguous: guessing a span would
/// arm a timer the user never set.
fn sleep(tv: &[&str]) -> Result<RawPlan, NlError> {
    let secs = (0..tv.len())
        .find_map(|i| parse_dur(tv, i).filter(|_| tv[i].starts_with(|c: char| c.is_ascii_digit())))
        .map(|(s, _)| s)
        .ok_or_else(|| NlError::Ambiguous("sleep for how long?".into()))?;
    Ok(plan(
        RawTrigger::SpanElapsed { secs },
        Action::Fade(FadeIntentIr::Out { secs: DEFAULT_SLEEP_FADE_SECS as f64 }),
        true,
    ))
}

// ── ordinals ────────────────────────────────────────────────────────────────

/// Closed ordinal lexicon first..tenth (+ the "1st".."10th"/"3rd" digit forms).
/// Anything outside the closed set ("last", roman numerals, "twenty-third") is
/// NOT an ordinal here (the caller returns NotUnderstood so the model may punt).
fn parse_ordinal(w: &str) -> Option<usize> {
    let n = match w {
        "first" => 1, "second" => 2, "third" => 3, "fourth" => 4, "fifth" => 5,
        "sixth" => 6, "seventh" => 7, "eighth" => 8, "ninth" => 9, "tenth" => 10,
        _ => {
            let digits = w
                .trim_end_matches("st")
                .trim_end_matches("nd")
                .trim_end_matches("rd")
                .trim_end_matches("th");
            let v: usize = digits.parse().ok()?;
            if w == digits {
                // A bare number ("3") is NOT an ordinal ("the 3 track" is not a
                // phrasing we accept); require an ordinal suffix or word.
                return None;
            }
            v
        }
    };
    (1..=10).contains(&n).then_some(n)
}


// ── fades ─────────────────────────────────────────────────────────────────────

fn fade_dir(tv: &[&str]) -> FadeIntentIr {
    // Explicit "fade in" -> In; everything else defaults to Out (colloquial
    // "fade [out]"); the echo spells the guessed direction out.
    let fi = tv.iter().position(|t| *t == "fade");
    match fi.and_then(|i| tv.get(i + 1)) {
        Some(&"in") => FadeIntentIr::In { secs: DEFAULT_FADE_SECS },
        _ => FadeIntentIr::Out { secs: DEFAULT_FADE_SECS },
    }
}

fn set_fade_secs(dir: FadeIntentIr, secs: f64) -> FadeIntentIr {
    match dir {
        FadeIntentIr::In { .. } => FadeIntentIr::In { secs },
        FadeIntentIr::Out { .. } => FadeIntentIr::Out { secs },
        other => other,
    }
}

/// "fade the Nth track [counting current as 1st]" -> QueuePosition + Fade.
fn fade_track(tv: &[&str], low: &str) -> Result<RawPlan, NlError> {
    // The ordinal right before "track"/"song".
    let n = tv
        .windows(2)
        .find_map(|w| {
            if w[1] == "track" || w[1] == "song" {
                parse_ordinal(w[0])
            } else {
                None
            }
        })
        .ok_or(NlError::NotUnderstood)?;
    let base = if low.contains("counting current")
        || low.contains("including this")
        || low.contains("from here")
    {
        PosBase::CurrentIsOne
    } else {
        PosBase::Absolute
    };
    let dir = fade_dir(tv);
    Ok(plan(
        RawTrigger::QueuePosition { n, base },
        Action::Fade(dir),
        true,
    ))
}

/// "fade out [in <dur>|at <time>|over <dur>|<dur>]" - the TRIGGER-vs-FADE-DURATION
/// preposition gate. "in/at" bind the TRIGGER; "over/for/across" or a bare trailing
/// duration bind the FADE secs.
fn fade_immediate(
    tv: &[&str],
    low: &str,
    text: &str,
    ctx: &NlContext,
) -> Result<RawPlan, NlError> {
    let fi = tv.iter().position(|t| *t == "fade").ok_or(NlError::NotUnderstood)?;
    // Skip the explicit direction word so "fade in 20 minutes" reads "in" as the
    // DIRECTION, not a trigger preposition.
    let mut rest_start = fi + 1;
    if matches!(tv.get(rest_start), Some(&"out") | Some(&"in")) {
        rest_start += 1;
    }
    let rest = &tv[rest_start.min(tv.len())..];

    let mut dir = fade_dir(tv);
    let mut trigger = RawTrigger::Immediate;
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "over" | "for" | "across" => {
                let (secs, c) = parse_dur(rest, i + 1).ok_or(NlError::NotUnderstood)?;
                dir = set_fade_secs(dir, secs);
                i += 1 + c;
            }
            "in" => {
                let (secs, c) = parse_dur(rest, i + 1).ok_or(NlError::NotUnderstood)?;
                trigger = RawTrigger::SpanElapsed { secs };
                i += 1 + c;
            }
            "at" => {
                let (_, _, c) = parse_clock(rest, i + 1, low).ok_or(NlError::NotUnderstood)?;
                trigger = at_trigger(rest, i, low, ctx).ok_or(NlError::NotUnderstood)?;
                i += 1 + c;
            }
            _ => {
                // A bare trailing duration after "fade out" binds the FADE secs.
                if let Some((secs, c)) = parse_dur(rest, i) {
                    dir = set_fade_secs(dir, secs);
                    i += c;
                } else {
                    i += 1;
                }
            }
        }
    }
    let _ = text;
    Ok(plan(trigger, Action::Fade(dir), true))
}

// ── stop / pause ──────────────────────────────────────────────────────────────

fn stop_pause(
    tv: &[&str],
    low: &str,
    text: &str,
    ctx: &NlContext,
) -> Result<RawPlan, NlError> {
    let action = if tv.first() == Some(&"pause") { Action::Pause } else { Action::Stop };
    let trigger = boundary_or_time(tv, low, text, ctx)?;
    Ok(plan(trigger, action, true))
}

/// Resolve a trailing "after this album|track", "in <dur>", or "at <time>" into a
/// trigger; a bare command with no such tail is Immediate. A missed "album"
/// keyword must NEVER silently downgrade to a track boundary.
fn boundary_or_time(
    tv: &[&str],
    low: &str,
    text: &str,
    ctx: &NlContext,
) -> Result<RawTrigger, NlError> {
    if low.contains("after this album") {
        return Ok(RawTrigger::AlbumBoundary { track: TrackSel::Current });
    }
    if low.contains("after this track")
        || low.contains("after this song")
        || low.contains("after this")
    {
        return Ok(RawTrigger::TrackAfterCurrent);
    }
    // An explicit "in"/"at" clause MUST parse: a misheard duration/time is a
    // NotUnderstood (punt to the model / loud ACK), NEVER a silent Immediate.
    if recurrence(tv).is_some() {
        let p = tv
            .iter()
            .position(|t| *t == "at")
            .ok_or_else(|| NlError::Ambiguous("a repeating plan needs a time of day (at HH:MM)".into()))?;
        return at_trigger(tv, p, low, ctx).ok_or(NlError::NotUnderstood);
    }
    if let Some(p) = tv.iter().position(|t| *t == "in") {
        let (secs, _) = parse_dur(tv, p + 1).ok_or(NlError::NotUnderstood)?;
        return Ok(RawTrigger::SpanElapsed { secs });
    }
    let at = tv.iter().position(|t| *t == "at");
    // A recurrence ("every day", "on weekdays") names a time-of-day schedule: it
    // needs an "at <time>", and must never degrade to a one-shot "in"/Immediate.
    if recurrence(tv).is_some() && at.is_none() {
        return Err(NlError::Ambiguous("a repeating plan needs a time of day (at HH:MM)".into()));
    }
    if let Some(p) = at {
        return at_trigger(tv, p, low, ctx).ok_or(NlError::NotUnderstood);
    }
    // A DEFERRAL conjunction we did not resolve above ("stop after the album ends",
    // "pause once this song finishes") is a deferred intent we could not parse -
    // punt (NotUnderstood), NEVER silently downgrade to stopping NOW. A bare
    // "stop"/"pause" or only filler words ("stop now") still falls through to
    // Immediate.
    const DEFER_WORDS: &[&str] = &["after", "when", "once", "until", "before"];
    if tv.iter().any(|t| DEFER_WORDS.contains(t)) {
        return Err(NlError::NotUnderstood);
    }
    let _ = text;
    Ok(RawTrigger::Immediate)
}

// ── calmer / similar ──────────────────────────────────────────────────────────

fn calmer_similar(tv: &[&str], calmer: bool, ctx: &NlContext) -> Result<RawPlan, NlError> {
    let id = ctx
        .current
        .clone()
        .ok_or_else(|| NlError::Unresolvable("nothing playing to match".into()))?;
    let count = leading_count(tv).unwrap_or(DEFAULT_ENQUEUE_COUNT);
    let selector = if calmer { Selector::Calmer(id) } else { Selector::Similar(id) };
    // A "play" ask must START playback, never silently append (the play_now
    // correction): "play something calmer" enqueue-then-starts NOW, matching
    // "play something calm" (no -er, which reaches the model and play_now-starts).
    // A bare "something calmer" / "more like this" stays append-only after current.
    if leads_with_play(tv) {
        return Ok(plan(
            RawTrigger::Immediate,
            Action::PlayNow { selector, count },
            true,
        ));
    }
    Ok(plan(
        RawTrigger::TrackAfterCurrent,
        Action::Enqueue { selector, count },
        true,
    ))
}

/// A leading start-playback verb ("play"/"put on"/"throw on"/"start"). Present ->
/// the ask STARTS playback (Action::PlayNow) rather than appending after current.
fn leads_with_play(tv: &[&str]) -> bool {
    matches!(tv.first(), Some(&"play") | Some(&"put") | Some(&"throw") | Some(&"start"))
}

/// A leading small count in "play 3 calmer tracks"; None -> use the default.
fn leading_count(tv: &[&str]) -> Option<u32> {
    tv.iter().find_map(|t| t.parse::<u32>().ok())
}

// ── wake ──────────────────────────────────────────────────────────────────────

/// "wake me at 7 with jazz" -> ONE [`Action::Wake`] at the resolved WallClock
/// instant. A single atomic Wake (enqueue -> start-from-silence -> play -> ramp IN)
/// is what actually STARTS playback from a stopped / wound-down deck; a plain
/// Enqueue + Fade(In) pair is append-only + volume-only and would be a silent no-op
/// from silence. Matches the direct `wake` command (`HypodjHandler::wake_set`).
fn wake(tv: &[&str], low: &str, text: &str, ctx: &NlContext) -> Result<Vec<RawPlan>, NlError> {
    let at_pos = tv.iter().position(|t| *t == "at").ok_or(NlError::NotUnderstood)?;
    // "wake me every weekday at 7" is a RECURRING wake; a bare "at 7" is one-shot.
    let trigger = at_trigger(tv, at_pos, low, ctx)
        .ok_or_else(|| NlError::Ambiguous("could not read the wake time".into()))?;
    // Optional "with <X>" selector; None means wake into the existing queue.
    let selector = with_selector(text);
    Ok(vec![plan(trigger, Action::Wake { selector, count: DEFAULT_ENQUEUE_COUNT }, true)])
}

/// The "with <X>" selector class: Genre only for the closed lexicon, else Query
/// (safe free-text). Uses the ORIGINAL-case text so a Query keeps "Bon Iver".
fn with_selector(text: &str) -> Option<Selector> {
    // Locate " with " in the ORIGINAL text via a case-insensitive ASCII byte-window
    // match. Searching the lowercased copy and slicing the original desyncs when a
    // preceding char changes byte length under lowercasing (garbled selector, or a
    // mid-codepoint slice panic). The needle is pure ASCII, so the match position
    // is a valid char boundary in `text`.
    let needle = b" with ";
    let idx = text
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))?;
    let raw = text[idx + needle.len()..].trim();
    // Trim trailing punctuation AND a small set of politeness filler words
    // ("please", "thanks", "thank you") so "with jazz please" resolves the closed
    // genre "jazz", not the free-text Query "jazz please".
    let raw = strip_trailing_filler(raw);
    if raw.is_empty() {
        return None;
    }
    let key = raw.to_lowercase();
    if GENRES.contains(&key.as_str()) {
        Some(Selector::Genre(raw.to_string()))
    } else {
        Some(Selector::Query(raw.to_string()))
    }
}

/// Strip trailing punctuation and politeness filler ("please", "thanks", "thank
/// you") from a selector phrase, iterating so "with jazz please." reduces to
/// "jazz". Borrows from the input (no allocation); case-insensitive match on a
/// whole trailing word (never a mid-word suffix).
fn strip_trailing_filler(s: &str) -> &str {
    const FILLER: &[&str] = &["thank you", "thanks", "please"];
    let mut cur = s.trim().trim_end_matches(['.', '!', '?', ',']).trim();
    loop {
        let mut stripped = false;
        for f in FILLER {
            if cur.len() > f.len() && cur.is_char_boundary(cur.len() - f.len()) {
                let split = cur.len() - f.len();
                let head = &cur[..split];
                let tail = &cur[split..];
                // Whole-word boundary: the filler must be preceded by a space so
                // "increase" is never truncated by the "please"... (it is not a
                // suffix here, but the space guard keeps the match honest).
                if tail.eq_ignore_ascii_case(f) && head.ends_with(' ') {
                    cur = head.trim_end().trim_end_matches(['.', '!', '?', ',']).trim_end();
                    stripped = true;
                    break;
                }
            }
        }
        if !stripped {
            return cur;
        }
    }
}

// ── duration / time parsing ───────────────────────────────────────────────────

/// Parse a duration starting at `toks[i]`: a bare number (seconds), a number +
/// unit word ("20 minutes"), or an `s`-suffixed token ("20s"). Returns the
/// seconds and how many tokens were consumed.
fn parse_dur(toks: &[&str], i: usize) -> Option<(f64, usize)> {
    let t = toks.get(i)?;
    if let Some(v) = t.strip_suffix('s') {
        if let Ok(n) = v.parse::<f64>() {
            if n.is_finite() && n >= 0.0 {
                return Some((n, 1));
            }
        }
    }
    let n: f64 = t.parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    match toks.get(i + 1).and_then(|u| unit_secs(u)) {
        Some(mult) => Some((n * mult, 2)),
        // A recognized-but-unsupported time unit ("days", "weeks", ...) must NOT
        // collapse to bare seconds - that would emit a confident wrong plan
        // ("in 2 days" -> 2s). Punt so the request falls through to NotUnderstood.
        None if toks.get(i + 1).is_some_and(|u| is_unsupported_time_unit(u)) => None,
        None => Some((n, 1)),
    }
}

fn unit_secs(w: &str) -> Option<f64> {
    match w {
        "second" | "seconds" | "sec" | "secs" | "s" => Some(1.0),
        "minute" | "minutes" | "min" | "mins" | "m" => Some(60.0),
        "hour" | "hours" | "hr" | "hrs" | "h" => Some(3600.0),
        _ => None,
    }
}

/// Time-unit words we deliberately do NOT support. A number followed by one of
/// these is an out-of-range duration request, not a bare-seconds count, so
/// `parse_dur` punts rather than silently dropping the unit.
fn is_unsupported_time_unit(w: &str) -> bool {
    matches!(
        w,
        "day" | "days"
            | "week" | "weeks"
            | "month" | "months"
            | "year" | "years"
            | "millisecond" | "milliseconds" | "ms"
    )
}

/// Parse a clock time at `toks[i]` ("7", "7:30", "7pm", "7:30am") into the NEXT
/// matching absolute UTC instant via `ctx.tz`. A bare hour with no meridian
/// defaults to AM (a wake implies morning); "pm"/"night"/"evening" force PM.
///
/// Read a clock time ("7", "7:30", "7pm", "7 pm", "23:00") at `toks[i]` as a
/// 24h `(hour, minute, tokens consumed)`.
fn parse_clock(toks: &[&str], i: usize, low: &str) -> Option<(u32, u32, usize)> {
    let mut raw = toks.get(i)?.to_string();
    let mut consumed = 1;
    let mut meridian: Option<bool> = None; // Some(true) = pm
    if let Some(v) = raw.strip_suffix("pm") {
        meridian = Some(true);
        raw = v.to_string();
    } else if let Some(v) = raw.strip_suffix("am") {
        meridian = Some(false);
        raw = v.to_string();
    }
    let (h, m) = parse_hm(&raw)?;
    if meridian.is_none() {
        match toks.get(i + 1).map(|s| *s) {
            Some("pm") => {
                meridian = Some(true);
                consumed = 2;
            }
            Some("am") => {
                meridian = Some(false);
                consumed = 2;
            }
            _ => {}
        }
    }
    if meridian.is_none() && (low.contains("night") || low.contains("evening") || low.contains("pm")) {
        meridian = Some(true);
    }
    let hour24 = apply_meridian(h, meridian.unwrap_or(false))?;
    if m > 59 {
        return None;
    }
    Some((hour24, m, consumed))
}

/// Read a clock time at `toks[i]` and resolve it to the next local occurrence.
fn parse_time(
    toks: &[&str],
    i: usize,
    low: &str,
    ctx: &NlContext,
) -> Option<(chrono::DateTime<Utc>, usize)> {
    let (h, m, consumed) = parse_clock(toks, i, low)?;
    Some((resolve_next_local(ctx, h, m)?, consumed))
}

/// The trigger for an "at <time>" clause whose "at" sits at `toks[at]`: a
/// RECURRING [`RawTrigger::Daily`] when the utterance names a recurrence, else
/// the one-shot next local occurrence. The recurrence is read from the WHOLE
/// utterance (`low`), so it may come before or after the time.
fn at_trigger(toks: &[&str], at: usize, low: &str, ctx: &NlContext) -> Option<RawTrigger> {
    let all: Vec<&str> = low.split_whitespace().collect();
    match recurrence(&all) {
        Some(days) => {
            let (h, m, _) = parse_clock(toks, at + 1, low)?;
            Some(RawTrigger::Daily { hour: h as u8, minute: m as u8, days })
        }
        None => parse_time(toks, at + 1, low, ctx).map(|(at, _)| RawTrigger::WallClock { at }),
    }
}

/// The recurrence an utterance names, if any: "every day"/"daily"/"every
/// morning|night|evening"/"nightly" -> every day; "weekday(s)" -> Mon..Fri;
/// "weekend(s)" -> Sat+Sun; "every monday" / "on mondays and fridays" -> those
/// days. `None` means one-shot. A day NAME alone ("at 7 on monday") is not a
/// recurrence - it needs "every" or the plural.
fn recurrence(tv: &[&str]) -> Option<DaySet> {
    let w: Vec<&str> = tv
        .iter()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()))
        .collect();
    let has = |x: &str| w.contains(&x);
    if has("weekday") || has("weekdays") {
        return Some(DaySet::WEEKDAYS);
    }
    if has("weekend") || has("weekends") {
        return Some(DaySet::WEEKENDS);
    }
    const DAYS: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];
    let every = has("every") || has("each");
    let mut bits = 0u8;
    for t in &w {
        for (i, d) in DAYS.iter().enumerate() {
            let plural = t.strip_suffix('s') == Some(*d);
            if plural || (every && *t == *d) {
                bits |= 1 << i;
            }
        }
    }
    if bits != 0 {
        return Some(DaySet(bits));
    }
    if has("daily") || has("everyday") || has("nightly") {
        return Some(DaySet::EVERY);
    }
    if every
        && w.windows(2).any(|p| {
            matches!(p[0], "every" | "each")
                && matches!(p[1], "day" | "morning" | "night" | "evening" | "afternoon")
        })
    {
        return Some(DaySet::EVERY);
    }
    None
}

fn parse_hm(raw: &str) -> Option<(u32, u32)> {
    match raw.split_once(':') {
        Some((h, m)) => Some((h.parse().ok()?, m.parse().ok()?)),
        None => Some((raw.parse().ok()?, 0)),
    }
}

/// Map a 12-hour clock hour + meridian to a 24-hour hour. `h` must be 1..=12 for a
/// meridian form, but a bare 0..=23 (24h input) is passed through when no meridian
/// was supplied and the hour is already > 12.
fn apply_meridian(h: u32, pm: bool) -> Option<u32> {
    if h > 23 {
        return None;
    }
    if h == 12 {
        return Some(if pm { 12 } else { 0 });
    }
    if h > 12 {
        // Already 24h (e.g. "at 19"); meridian is irrelevant.
        return Some(h);
    }
    Some(if pm { h + 12 } else { h })
}

/// The next local `h:m` (today if still ahead, else tomorrow) as an absolute UTC
/// instant, via the fixed-offset zone. Deterministic under a fixed `ctx.now_civil`.
fn resolve_next_local(ctx: &NlContext, h: u32, m: u32) -> Option<chrono::DateTime<Utc>> {
    let tz = ctx.tz;
    let local_now = ctx.now_civil.with_timezone(&tz);
    let today = local_now.date_naive();
    let naive_today = today.and_hms_opt(h, m, 0)?;
    let dt = match tz.from_local_datetime(&naive_today).single() {
        Some(d) if d > local_now => d,
        _ => {
            let tomorrow = today.succ_opt()?;
            tz.from_local_datetime(&tomorrow.and_hms_opt(h, m, 0)?).single()?
        }
    };
    Some(dt.with_timezone(&Utc))
}
