//! Text formatters for the hjq CLI, expressed on top of the shared parse layer in
//! `hypodj_client::model`. This crate owns ONLY the text card/queue rendering; the
//! NowPlaying + queue parsing lives in the client. A stopped/empty deck renders a
//! friendly "nothing playing".

use hypodj_client::model::{parse_queue, FieldState, NowPlaying};
use hypodj_style::ansi::Ansi;
use hypodj_style::{Role, TermBg};

/// Render the now-playing card as plain text (a couple of lines). A stopped or
/// empty deck is "nothing playing" - never a leftover title/artist.
#[cfg(test)]
pub fn render_card(np: &NowPlaying) -> String {
    render_card_with(np, Ansi::off())
}

/// The line the binary prints after a confirm. Same words on the CLI and the TUI.
/// A confirm, one line, every plan, each in its own state.
pub fn render_confirm(pairs: &[(String, String)], ansi: Ansi) -> String {
    hypodj_client::spoken::from_confirm(pairs)
        .parts
        .iter()
        .map(|part| ansi.render(part))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Sleep, wake, an answer, or undo. The same `command_spoken` the TUI calls.
pub fn render_command(line: &str, pairs: &[(String, String)], ansi: Ansi) -> Option<String> {
    hypodj_client::spoken::command_spoken(line, pairs).map(|spoken| ansi.render(&spoken))
}

pub fn render_outcome(
    pairs: &[(String, String)],
    act: Option<hypodj_client::spoken::Act>,
    ansi: Ansi,
) -> String {
    ansi.render(&hypodj_client::spoken::from_reply(pairs, act))
}

/// `djc features`: the daemon's measured coverage, then what is known about the
/// current track, saying plainly whether the numbers were MEASURED or guessed from
/// metadata.
pub fn render_features(pairs: &[(String, String)]) -> String {
    let get = |k: &str| pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.as_str());
    let mut lines = vec![match get("X-Features") {
        Some("off") | None => "audio features: off".to_string(),
        Some(c) => match get("X-FeaturesStale") {
            Some(n) => format!("audio features: {c} ({n} stale, re-run hypodj-mir)"),
            None => format!("audio features: {c}"),
        },
    }];
    if let Some(src) = get("X-FeatureSource") {
        let mut l = format!("this track ({src}): energy {}", get("X-FeatureEnergy").unwrap_or("?"));
        if let Some(v) = get("X-FeatureVoice") {
            l.push_str(&format!(", voice {v}"));
        }
        if let Some(b) = get("X-FeatureBpm") {
            l.push_str(&format!(", {b} bpm"));
        }
        lines.push(l);
    }
    lines.join("\n")
}

/// The line the binary prints for a daemon ACK.
pub fn render_refusal(msg: &str, ansi: Ansi) -> String {
    ansi.render(&hypodj_client::spoken::refusal_ack(msg))
}

/// Same card as [`render_card`], with colour from the ANSI adapter when `ansi` is on.
/// Off (not a TTY, or `NO_COLOR`) is the plain card. The words do not change.
pub fn render_card_with(np: &NowPlaying, ansi: Ansi) -> String {
    let stopped = np.state.as_deref() == Some("stop");
    let empty = np.title.is_none() && np.artist.is_none() && np.album.is_none();
    let armed: Vec<String> = hypodj_client::spoken::armed_hold(&np.armed)
        .iter()
        .map(|s| ansi.render(s))
        .collect();
    let field = render_field(&np.field);
    if stopped || empty {
        // A stopped deck can still hold an armed wake alarm or a held pull - surface
        // them so the machine's hold stays visible even with nothing playing. Field,
        // then the ambient hint, are the last, quietest lines.
        let mut lines = vec!["nothing playing".to_string()];
        // The modes still decide what the next `play` does, so a stopped deck says them.
        lines.extend(np.modes.label().map(|m| format!("modes: {m}")));
        lines.extend(armed.clone());
        lines.extend(field);
        if let Some(hint) = &np.hint {
            lines.push(hint.phrase());
        }
        // The standing continuation-radio hint is the quietest last line: when armed
        // the deck will flow into this station at end-of-queue instead of stopping.
        if let Some(station) = &np.continuation {
            lines.push(format!("then: {station}"));
        }
        if let Some(store) = &np.store {
            lines.push(format!("store: {store}"));
        }
        return lines.join("\n");
    }
    let mut lines = Vec::new();
    if let Some(t) = &np.title {
        // A heart marks a Subsonic favorite. U+2665 is East-Asian-Width ambiguous,
        // so it is followed by U+FE0E (text-presentation selector) to force a single
        // cell on emoji-presentation terminals. Prepended only when starred.
        if np.starred {
            // The heart is the kept mark. Ember when colour is on; the glyph stays
            // either way, so the line still says favourite with the colour stripped.
            let mark = ansi.paint(Role::Ember, "\u{2665}\u{FE0E}", TermBg::dark_default());
            // Ember here is the CLI colour that clears both black and white.
            lines.push(format!("{mark} {t}"));
        } else {
            lines.push(t.clone());
        }
    }
    // "Artist - Album" (whichever of the two is present).
    let sub: Vec<&str> = [np.artist.as_deref(), np.album.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    if !sub.is_empty() {
        lines.push(sub.join(" - "));
    }

    // Status line: [playing|paused] | vol V% | N of M | duration.
    let mut status_bits = Vec::new();
    match np.state.as_deref() {
        Some("play") => status_bits.push("playing".to_string()),
        Some("pause") => status_bits.push("paused".to_string()),
        Some(other) => status_bits.push(other.to_string()),
        None => {}
    }
    if let Some(v) = np.volume {
        if v >= 0 {
            status_bits.push(format!("vol {v}%"));
        }
    }
    if let (Some(song), Some(m)) = (np.song, np.playlistlength) {
        status_bits.push(format!("{} of {}", song.saturating_add(1), m));
    }
    if let Some(d) = np.duration {
        status_bits.push(fmt_dur(d));
    }
    if let Some(m) = np.modes.label() {
        status_bits.push(m);
    }
    if !status_bits.is_empty() {
        lines.push(status_bits.join(" | "));
    }
    // The server's own listening history for this track, when it has one. A quiet
    // line, and absent rather than zeroed when the server records nothing - "never
    // played" is a claim the daemon makes by staying silent, and inventing "0 plays"
    // here would put words in its mouth.
    if let Some(l) = render_history(np.plays, np.last_played_days) {
        lines.push(l);
    }
    lines.extend(armed);
    // The field is the last, faintest line of the card - the quietest voice.
    if let Some(fl) = field {
        lines.push(fl);
    }
    // The standing continuation-radio hint rides below the field: what the deck will
    // flow into when the queue drains. Present only when continuation is armed.
    if let Some(station) = &np.continuation {
        lines.push(format!("then: {station}"));
    }
    // THE OFFLINE MIRROR, last line, already worded by the daemon. It is the only
    // passive answer to "is the backfill running, held, or done?" a client tool has -
    // `store` is not in the `commands` advertisement, so nothing else surfaces it.
    // Absent (no store, or no full pass yet) renders nothing, exactly like the armed
    // and field HUDs.
    if let Some(store) = &np.store {
        lines.push(format!("store: {store}"));
    }
    lines.join("\n")
}

/// The server's play history for the current track as one line, e.g.
/// `23 plays, last heard 3 days ago`. `None` when the server records neither, so a
/// track it has never seen played renders nothing at all rather than a row of zeros.
///
/// Both halves are independently optional because the daemon emits them
/// independently: a count with no stamp, or a stamp with no count, is a real shape.
fn render_history(plays: Option<u32>, last_played_days: Option<u32>) -> Option<String> {
    let mut bits = Vec::new();
    if let Some(n) = plays {
        bits.push(format!("{n} {}", if n == 1 { "play" } else { "plays" }));
    }
    match last_played_days {
        Some(0) => bits.push("last heard today".to_string()),
        Some(1) => bits.push("last heard yesterday".to_string()),
        Some(d) => bits.push(format!("last heard {d} days ago")),
        None => {}
    }
    if bits.is_empty() {
        return None;
    }
    Some(bits.join(", "))
}

/// The active latent-field pulls as one unobtrusive line, e.g.
/// `toward calmer (58%, 3m ago) \u{b7} toward warmer (41%, 1m ago)`. `None` when no
/// pull is active, so a resting field renders no line. Reconstructed from the numeric
/// X- pairs; the verbose provenance prose stays reserved for the interactive `field`
/// command.
///
/// Both numbers are LABELLED, and the wording is byte-identical to the dj-gui's
/// `field_hud` - one HUD, two clients, and a reader who learns it once.
fn render_field(field: &FieldState) -> Option<String> {
    if !field.active() {
        return None;
    }
    let line = field
        .pulls
        .iter()
        .map(|p| format!("toward {} ({}%, {}m ago)", p.label, p.strength, p.age_mins))
        .collect::<Vec<_>>()
        .join(" \u{00b7} ");
    Some(line)
}

/// `M:SS` for a track duration on the card.
fn fmt_dur(secs: f64) -> String {
    let total = secs as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// Render the queue from `playlistinfo` pairs, iterating the shared structured
/// parse (`parse_queue`) so the CLI and TUI share one queue model. Prints
/// "<Pos+1>. <Title> - <Artist>" per item. Empty -> "queue is empty".
pub fn render_queue(pairs: &[(String, String)]) -> String {
    let items = parse_queue(pairs);
    if items.is_empty() {
        return "queue is empty".to_string();
    }
    items
        .iter()
        .map(|it| match &it.artist {
            Some(a) => format!("{}. {} - {}", it.pos + 1, it.title, a),
            None => format!("{}. {}", it.pos + 1, it.title),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn features_says_measured_or_metadata() {
        let p = |v: &[(&str, &str)]| -> Vec<(String, String)> {
            v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
        };
        let measured = p(&[
            ("X-Features", "7 of 422 analysed"),
            ("X-FeatureSource", "measured"),
            ("X-FeatureEnergy", "0.26"),
            ("X-FeatureVoice", "0.01 instrumental"),
            ("X-FeatureBpm", "139.6"),
        ]);
        assert_eq!(
            super::render_features(&measured),
            "audio features: 7 of 422 analysed\nthis track (measured): energy 0.26, voice 0.01 instrumental, 139.6 bpm"
        );
        assert_eq!(super::render_features(&p(&[("X-Features", "off")])), "audio features: off");
        assert_eq!(
            super::render_features(&p(&[("X-Features", "3 of 9 analysed"), ("X-FeaturesStale", "2")])),
            "audio features: 3 of 9 analysed (2 stale, re-run hypodj-mir)"
        );
    }

    use super::*;
    use hypodj_client::model::now_playing;

    fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn card_playing() {
        let status = p(&[
            ("volume", "70"),
            ("playlistlength", "12"),
            ("state", "play"),
            ("song", "2"),
            ("duration", "215.000"),
        ]);
        let current = p(&[
            ("file", "song/42"),
            ("Title", "Blue in Green"),
            ("Artist", "Miles Davis"),
            ("Album", "Kind of Blue"),
            ("Pos", "2"),
        ]);
        let card = render_card(&now_playing(&status, &current));
        assert!(card.contains("Blue in Green"));
        assert!(card.contains("Miles Davis - Kind of Blue"));
        assert!(card.contains("playing"));
        assert!(card.contains("vol 70%"));
        assert!(card.contains("3 of 12"));
        assert!(card.contains("3:35"));
        assert!(!card.to_lowercase().contains("elapsed"));
    }

    #[test]
    fn card_prepends_heart_when_starred() {
        let status = p(&[("state", "play")]);
        let current = p(&[
            ("file", "song/42"),
            ("Title", "Blue in Green"),
            ("X-Starred", "1"),
        ]);
        let card = render_card(&now_playing(&status, &current));
        assert!(card.contains("\u{2665}\u{FE0E} Blue in Green"));
        // Without the pair the heart is absent (plain title).
        let plain = render_card(&now_playing(
            &status,
            &p(&[("file", "song/42"), ("Title", "Blue in Green")]),
        ));
        assert!(!plain.contains('\u{2665}'));
        assert!(plain.contains("Blue in Green"));
    }

    #[test]
    fn card_colour_on_and_off_match_once_stripped() {
        let status = p(&[("state", "play"), ("volume", "40")]);
        let current = p(&[
            ("file", "song/42"),
            ("Title", "Blue in Green"),
            ("Artist", "Miles Davis"),
            ("X-Starred", "1"),
        ]);
        let np = now_playing(&status, &current);
        let off = render_card_with(&np, Ansi::off());
        let on = render_card_with(&np, Ansi::on_truecolor());
        assert!(on.contains('\u{1b}'), "colour on actually emits an escape");
        assert_eq!(hypodj_style::ansi::strip(&on), off);
        assert_eq!(hypodj_style::ansi::strip(&off), off);
        assert!(off.contains("\u{2665}\u{FE0E} Blue in Green"));
        assert!(!Ansi::from_env(false, None, None).enabled());
        assert!(!Ansi::from_env(true, Some("1"), None).enabled());
    }

    #[test]
    fn a_canned_plan_echo_strips_to_the_same_words() {
        use hypodj_client::nl::split_echo;
        use hypodj_client::spoken::plan_echo;
        use hypodj_style::State;
        let parts = split_echo("via rules | [1] fade out | NOTE: a wake also needs a play");
        for spoken in plan_echo(&parts) {
            let off = Ansi::off().render(&spoken);
            let on = Ansi::on_truecolor().render(&spoken);
            let indexed = Ansi::on_indexed().render(&spoken);
            assert_eq!(off, spoken.text());
            assert_eq!(hypodj_style::ansi::strip(&on), off);
            assert_eq!(hypodj_style::ansi::strip(&indexed), off);
            if spoken.state == State::Echo {
                assert!(on.contains("\u{1b}[2;4m"), "echo is quiet and underlined");
            }
            if spoken.state == State::Note {
                assert_eq!(off, "NOTE: a wake also needs a play");
                assert!(!on.contains("\u{1b}[4m"), "a note is not underlined");
            }
        }
        let joined = plan_echo(&parts)
            .into_iter()
            .map(|s| s.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("[1] fade out"));
        assert!(joined.trim_end().ends_with("confirm? [y/N]"));
    }

    #[test]
    fn every_handler_shape_renders_through_the_cli_path() {
        use hypodj_client::spoken::{reply_shapes, speak_shape};
        use hypodj_style::State;
        for shape in reply_shapes() {
            let spoken = speak_shape(&shape);
            let off = Ansi::off().render(&spoken);
            let on = Ansi::on_truecolor().render(&spoken);
            let indexed = Ansi::on_indexed().render(&spoken);
            assert_eq!(off, spoken.text(), "{}", shape.name);
            assert_eq!(hypodj_style::ansi::strip(&on), off, "{}", shape.name);
            assert_eq!(hypodj_style::ansi::strip(&indexed), off, "{}", shape.name);
            assert_eq!(spoken.state, shape.state, "{}", shape.name);
            assert_eq!(spoken.lines, shape.lines, "{}", shape.name);
            match shape.state {
                State::Live => {
                    assert!(!on.contains("38;2;"), "live paints no ink: {}", shape.name);
                    assert!(!on.contains("38;5;"), "live paints no ink: {}", shape.name);
                    if !spoken.text().is_empty() {
                        assert!(on.contains("\u{1b}[1m"), "live is bold: {}", shape.name);
                    }
                }
                State::Refused => {
                    assert_eq!(on, off, "a refusal has no colour: {}", shape.name);
                    assert!(!on.contains('\u{1b}'), "{}", shape.name);
                }
                State::Armed => assert!(on.contains("38;2;"), "armed is ember: {}", shape.name),
                _ => {}
            }
        }
    }

    #[test]
    fn a_confirm_and_a_status_use_the_same_words_on_the_cli() {
        use hypodj_client::spoken::{command_spoken, from_confirm};
        let mixed = [
            ("plan_id".into(), "1".into()),
            ("result".into(), "added 5 tracks".into()),
            ("plan_id".into(), "2".into()),
        ];
        let off = render_confirm(&mixed, Ansi::off());
        let on = render_confirm(&mixed, Ansi::on_truecolor());
        assert_eq!(off, "added 5 tracks; armed (plan 2)");
        assert_eq!(hypodj_style::ansi::strip(&on), off);
        assert!(on.contains("\u{1b}[1m"), "the live plan is bold");
        assert!(on.contains("38;2;"), "the armed plan is ember");

        let batch = [
            ("plan_id".into(), "7".into()),
            ("plan_id".into(), "8".into()),
        ];
        let batch_off = render_confirm(&batch, Ansi::off());
        assert_eq!(batch_off, from_confirm(&batch).text());
        assert!(batch_off.contains("armed (plan 7)"));
        assert!(batch_off.contains("armed (plan 8)"));

        let sleep = vec![("sleep_remaining".into(), "720".into())];
        let sleep_spoken = command_spoken("sleep", &sleep).unwrap();
        let hold = hypodj_client::spoken::armed_hold(&hypodj_client::model::ArmedFeatures {
            sleep_remaining: Some(720),
            ..hypodj_client::model::ArmedFeatures::default()
        });
        assert_eq!(sleep_spoken.state, hypodj_style::State::Armed);
        assert_eq!(hold[0].state, sleep_spoken.state);
        let sleep_on = render_command("sleep", &sleep, Ansi::on_truecolor()).unwrap();
        assert!(sleep_on.contains("38;2;"), "a countdown is ember");
        assert_eq!(hypodj_style::ansi::strip(&sleep_on), sleep_spoken.text());

        let quiet = vec![("wake".into(), "none".into())];
        let quiet_spoken = command_spoken("wake", &quiet).unwrap();
        assert_eq!(quiet_spoken.state, hypodj_style::State::Note);
        let quiet_on = render_command("wake", &quiet, Ansi::on_truecolor()).unwrap();
        assert!(!quiet_on.contains("38;2;"), "nothing pending is not ember");

        for (line, raw) in [
            (
                "nl",
                vec![("nl_answer".into(), "yes, \"Blue\" is a favorite".into())],
            ),
            ("undo", vec![("jdid".into(), "cleared 7".into())]),
        ] {
            let spoken = command_spoken(line, &raw).unwrap();
            assert_eq!(
                render_command(line, &raw, Ansi::off()).as_deref(),
                Some(spoken.text().as_str())
            );
            assert_eq!(spoken.state, hypodj_style::State::Live, "{line}");
        }
    }

    #[test]
    fn winddown_phase_is_explicit_on_the_cli() {
        use hypodj_client::spoken::{reply_shapes, speak_shape, State};
        let shapes = reply_shapes();
        for name in [
            "wind down at album end",
            "wind down after this track",
            "wind down in 5m",
            "winding down",
        ] {
            let shape = shapes.iter().find(|s| s.name == name).expect(name);
            let spoken = speak_shape(shape);
            let off = render_command("winddown", &shape.pairs, Ansi::off()).unwrap();
            let on = render_command("winddown", &shape.pairs, Ansi::on_truecolor()).unwrap();
            assert_eq!(off, spoken.text(), "{name}");
            assert_eq!(hypodj_style::ansi::strip(&on), off);
            if spoken.state == State::Armed {
                assert!(on.contains("38;2;"), "{name} is ember");
            }
            if spoken.state == State::Live {
                assert!(on.contains("\u{1b}[1m"), "{name} is live");
                assert!(!on.contains("38;2;"), "{name} is not ember");
            }
        }
    }

    #[test]
    fn armed_hud_on_the_card_is_ember_and_strips_clean() {
        let status = p(&[("state", "play"), ("X-hypodj-sleep-remaining", "720")]);
        let current = p(&[("file", "song/1"), ("Title", "X")]);
        let np = now_playing(&status, &current);
        let off = render_card_with(&np, Ansi::off());
        let on = render_card_with(&np, Ansi::on_truecolor());
        assert!(off.contains("sleep in 12m"));
        assert_eq!(hypodj_style::ansi::strip(&on), off);
        assert_ne!(on, off, "the armed line is coloured");
    }

    #[test]
    fn card_stopped() {
        let status = p(&[("volume", "50"), ("playlistlength", "3"), ("state", "stop")]);
        assert_eq!(render_card(&now_playing(&status, &[])), "nothing playing");
    }

    #[test]
    fn card_empty_currentsong() {
        let status = p(&[("volume", "50"), ("playlistlength", "0"), ("state", "play")]);
        assert_eq!(render_card(&now_playing(&status, &[])), "nothing playing");
    }

    #[test]
    fn card_shows_ambient_hint_when_stopped() {
        // A stopped deck surfaces the just-finished hint as the quietest last line.
        let status = p(&[
            ("state", "stop"),
            ("X-hypodj-hint-kind", "just-finished"),
            ("X-hypodj-hint-title", "303 (Ninajirachi Remix)"),
        ]);
        let card = render_card(&now_playing(&status, &[]));
        assert!(card.starts_with("nothing playing"));
        assert!(card.contains("just finished 303 (Ninajirachi Remix)"));
        // The hint is the LAST line (quietest voice).
        assert!(card.ends_with("just finished 303 (Ninajirachi Remix)"));
    }

    #[test]
    fn card_up_next_hint_when_empty_deck() {
        let status = p(&[
            ("state", "play"),
            ("playlistlength", "0"),
            ("X-hypodj-hint-kind", "up-next"),
            ("X-hypodj-hint-title", "Blue in Green"),
        ]);
        let card = render_card(&now_playing(&status, &[]));
        assert!(card.contains("up next Blue in Green"));
    }

    #[test]
    fn card_shows_continuation_hint_when_armed() {
        // A stopped deck armed for continuation surfaces the station as the quietest
        // last line: what it will flow into when the queue drains.
        let status = p(&[
            ("state", "stop"),
            ("X-hypodj-continuation", "on"),
            ("X-hypodj-continuation-station", "NTS 1"),
        ]);
        let out = render_card(&now_playing(&status, &[]));
        assert!(
            out.contains("then: NTS 1"),
            "continuation hint line present:\n{out}"
        );
        // Disarmed -> no line.
        let np = now_playing(&p(&[("state", "stop")]), &[]);
        assert!(!render_card(&np).contains("then:"));
    }

    #[test]
    fn card_no_hint_line_when_absent() {
        // No hint pair -> no extra line, just "nothing playing".
        let status = p(&[("state", "stop")]);
        assert_eq!(render_card(&now_playing(&status, &[])), "nothing playing");
    }

    #[test]
    fn card_playing_never_renders_hint() {
        // A hint pair should not exist on the wire while a library track plays (the
        // daemon suppresses it), but even if one leaked, the playing card never
        // renders it - the current track is shown by title, never duplicated by a hint.
        let status = p(&[
            ("state", "play"),
            ("song", "0"),
            ("playlistlength", "1"),
            ("X-hypodj-hint-kind", "just-finished"),
            ("X-hypodj-hint-title", "Some Other Track"),
        ]);
        let current = p(&[("file", "song/42"), ("Title", "Now Playing Track")]);
        let card = render_card(&now_playing(&status, &current));
        assert!(card.contains("Now Playing Track"));
        assert!(!card.contains("just finished"));
        assert!(!card.contains("Some Other Track"));
    }

    #[test]
    fn card_shows_armed_features() {
        let status = p(&[
            ("volume", "70"),
            ("playlistlength", "12"),
            ("state", "play"),
            ("song", "0"),
            ("X-hypodj-sleep-remaining", "720"),
            ("X-hypodj-wake-remaining", "25200"),
        ]);
        let current = p(&[("file", "song/1"), ("Title", "X"), ("Artist", "Y")]);
        let card = render_card(&now_playing(&status, &current));
        assert!(card.contains("sleep in 12m"), "card: {card}");
        assert!(card.contains("wake in 7h 00m"), "card: {card}");
    }

    #[test]
    fn card_stopped_still_shows_armed_wake() {
        let status = p(&[
            ("volume", "50"),
            ("state", "stop"),
            ("X-hypodj-wake-remaining", "25200"),
        ]);
        let card = render_card(&now_playing(&status, &[]));
        assert!(card.contains("nothing playing"));
        assert!(card.contains("wake in 7h 00m"), "card: {card}");
    }

    #[test]
    fn card_no_armed_line_when_nothing_armed() {
        let status = p(&[("volume", "70"), ("state", "play"), ("song", "0")]);
        let current = p(&[("file", "song/1"), ("Title", "X"), ("Artist", "Y")]);
        let card = render_card(&now_playing(&status, &current));
        assert!(!card.contains("sleep"));
        assert!(!card.contains("wake"));
    }

    #[test]
    fn card_shows_field_pull_line() {
        let status = p(&[
            ("state", "play"),
            ("song", "0"),
            ("X-hypodj-field-count", "2"),
            ("X-hypodj-field-0-label", "calmer"),
            ("X-hypodj-field-0-strength", "58"),
            ("X-hypodj-field-0-age", "3"),
            ("X-hypodj-field-1-label", "warmer"),
            ("X-hypodj-field-1-strength", "41"),
            ("X-hypodj-field-1-age", "1"),
        ]);
        let current = p(&[("file", "song/1"), ("Title", "X"), ("Artist", "Y")]);
        let card = render_card(&now_playing(&status, &current));
        assert!(card.contains("toward calmer (58%, 3m ago)"), "card: {card}");
        assert!(card.contains("toward warmer (41%, 1m ago)"), "card: {card}");
        assert!(
            card.contains("toward calmer (58%, 3m ago) \u{b7} toward warmer (41%, 1m ago)"),
            "card: {card}"
        );
    }

    #[test]
    fn card_no_field_line_at_rest() {
        let status = p(&[("state", "play"), ("song", "0")]);
        let current = p(&[("file", "song/1"), ("Title", "X"), ("Artist", "Y")]);
        let card = render_card(&now_playing(&status, &current));
        assert!(!card.contains("toward"), "no field line at rest: {card}");
    }

    #[test]
    fn card_stopped_still_shows_held_pull() {
        let status = p(&[
            ("state", "stop"),
            ("X-hypodj-field-count", "1"),
            ("X-hypodj-field-0-label", "calmer"),
            ("X-hypodj-field-0-strength", "58"),
            ("X-hypodj-field-0-age", "3"),
        ]);
        let card = render_card(&now_playing(&status, &[]));
        assert!(card.contains("nothing playing"));
        assert!(card.contains("toward calmer (58%, 3m ago)"), "card: {card}");
    }

    #[test]
    fn card_hides_unknown_volume() {
        let status = p(&[
            ("volume", "-1"),
            ("playlistlength", "1"),
            ("state", "play"),
            ("song", "0"),
        ]);
        let current = p(&[("file", "song/1"), ("Title", "X"), ("Artist", "Y")]);
        let card = render_card(&now_playing(&status, &current));
        assert!(!card.contains("vol"));
        assert!(card.contains("1 of 1"));
    }

    #[test]
    fn queue_render() {
        let pairs = p(&[
            ("file", "song/1"),
            ("Title", "One"),
            ("Artist", "A"),
            ("Pos", "0"),
            ("file", "song/2"),
            ("Title", "Two"),
            ("Artist", "B"),
            ("Pos", "1"),
        ]);
        assert_eq!(render_queue(&pairs), "1. One - A\n2. Two - B");
    }

    #[test]
    fn queue_empty() {
        assert_eq!(render_queue(&[]), "queue is empty");
    }

    #[test]
    fn card_carries_the_offline_store_line_playing_or_stopped() {
        // "How do I know the backfill is running?" has to be answerable from `djc status`,
        // because `store` is not in the `commands` advertisement and nothing else in the
        // toolchain surfaces it. Both branches of the card render it - a stopped deck is
        // exactly when a multi-day mirror is most likely to be filling.
        let status = p(&[
            ("volume", "70"),
            ("playlistlength", "12"),
            ("state", "play"),
            ("song", "2"),
            (
                "X-Store",
                "318 of 347 tracks, 12.1 of 16.0 GiB, paused while streaming",
            ),
        ]);
        let current = p(&[("file", "song/42"), ("Title", "Blue in Green")]);
        let card = render_card(&now_playing(&status, &current));
        assert!(
            card.contains("store: 318 of 347 tracks, 12.1 of 16.0 GiB, paused while streaming"),
            "card: {card}"
        );

        let stopped = p(&[("state", "stop"), ("X-Store", "all 347 tracks, 9.8 GiB")]);
        let card = render_card(&now_playing(&stopped, &[]));
        assert!(card.contains("nothing playing"));
        assert!(
            card.contains("store: all 347 tracks, 9.8 GiB"),
            "card: {card}"
        );

        // No store (or no full pass yet): no line at all, exactly like the armed HUD.
        let lean = p(&[("state", "stop")]);
        assert!(!render_card(&now_playing(&lean, &[])).contains("store:"));
    }

    #[test]
    fn the_card_shows_the_servers_play_history_and_stays_silent_without_it() {
        let status = p(&[("state", "play"), ("volume", "70")]);
        let card = render_card(&now_playing(
            &status,
            &p(&[
                ("file", "song/42"),
                ("Title", "Blue in Green"),
                ("X-Plays", "23"),
                ("X-LastPlayed", "3"),
            ]),
        ));
        assert!(
            card.contains("23 plays, last heard 3 days ago"),
            "card: {card}"
        );

        // A track the server has never seen played renders NOTHING - not "0 plays",
        // which would be a claim the daemon deliberately declined to make.
        let card = render_card(&now_playing(
            &status,
            &p(&[("file", "song/42"), ("Title", "Blue in Green")]),
        ));
        assert!(!card.contains("plays"), "no history, no line: {card}");
        assert!(!card.contains("last heard"), "card: {card}");

        // The two English corners a bare "N days ago" gets wrong.
        let today = render_card(&now_playing(
            &status,
            &p(&[("Title", "T"), ("X-Plays", "1"), ("X-LastPlayed", "0")]),
        ));
        assert!(today.contains("1 play, last heard today"), "card: {today}");
        let yday = render_card(&now_playing(
            &status,
            &p(&[("Title", "T"), ("X-LastPlayed", "1")]),
        ));
        assert!(yday.contains("last heard yesterday"), "card: {yday}");
        assert!(
            !yday.contains("plays"),
            "a stamp with no count invents no count: {yday}"
        );
    }

    #[test]
    fn card_shows_playback_modes_only_when_on() {
        let cur = p(&[("Title", "T")]);
        let on = now_playing(
            &p(&[("state", "play"), ("random", "1"), ("repeat", "1")]),
            &cur,
        );
        let card = render_card(&on);
        assert!(card.contains("| random repeat"), "card: {card}");
        let off = now_playing(&p(&[("state", "play"), ("random", "0")]), &cur);
        assert!(!render_card(&off).contains("random"));
        let stopped = now_playing(&p(&[("state", "stop"), ("single", "1")]), &[]);
        assert!(render_card(&stopped).contains("modes: single"));
    }
}
