//! egui adapter. The GUI port does not exist yet; this is the mapping only.
//!
//! Room fill is the detected background. Error and warning colours are ink,
//! not an alarm red. Motion tokens become animation durations.

use std::time::Duration;

use ::egui::{Color32, Stroke, Visuals};

use crate::{ember_rgb, fit_ink_primary, fit_ink_quiet, fit_rule, tokens, Rgb, Role, TermBg};

pub fn color32(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// Animation times, in the same units egui's `animate_*` helpers take (seconds).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionTimes {
    pub floor: Duration,
    pub skip: Duration,
    pub pause: Duration,
    pub floor_secs: f32,
    pub skip_secs: f32,
    pub pause_secs: f32,
}

pub fn motion_times() -> MotionTimes {
    let m = &tokens().motion;
    let secs = |ms: u64| ms as f32 / 1000.0;
    MotionTimes {
        floor: Duration::from_millis(m.floor_ms),
        skip: Duration::from_millis(m.skip_ms),
        pause: Duration::from_millis(m.pause_ms),
        floor_secs: secs(m.floor_ms),
        skip_secs: secs(m.skip_ms),
        pause_secs: secs(m.pause_ms),
    }
}

/// A full egui theme resolved from the room. Starts from egui's dark or light
/// preset so unspecified widget metrics stay sane, then overwrites every colour
/// from a role. There is no red.
pub fn visuals(bg: TermBg) -> Visuals {
    let mut v = if bg.is_dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    let room = color32(bg.rgb);
    let ink = color32(fit_ink_primary(bg));
    let quiet = color32(fit_ink_quiet(bg));
    let rule = color32(fit_rule(bg));
    v.window_fill = room;
    v.panel_fill = room;
    v.extreme_bg_color = room;
    v.faint_bg_color = room;
    v.code_bg_color = room;
    // Leave this unset. A forced ink colour paints selected text the same
    // colour as the selection fill, which is ink on ink.
    v.override_text_color = None;
    // A refusal and a warning are calm ink. No alarm colour.
    v.warn_fg_color = quiet;
    v.error_fg_color = ink;
    v.hyperlink_color = ink;
    v.window_stroke = Stroke::new(1.0, rule);

    for widget in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        widget.fg_stroke.color = ink;
        widget.bg_stroke.color = rule;
        widget.bg_fill = room;
        widget.weak_bg_fill = room;
    }
    v.widgets.inactive.fg_stroke.color = quiet;
    // Labels are noninteractive. Their text is primary ink. Leave
    // override_text_color unset so that ink is not forced onto a selection.
    // Selected rows copy `selection.stroke` onto the text and `bg_fill` onto
    // the background (`Style::interact_selectable`). Text is the room, fill
    // is primary ink: the same inversion as the TUI's reverse.
    v.selection.bg_fill = ink;
    v.selection.stroke = Stroke::new(1.0, room);
    v
}

/// One painted line. Echo is underlined so it is not a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Painted {
    pub text: String,
    pub color: Color32,
    pub underline: bool,
}

/// One colour per line. The words are unchanged.
pub fn render(spoken: &crate::Spoken, bg: TermBg) -> Vec<Painted> {
    let colour = role_color(crate::role_for(spoken.state), bg);
    let underline = spoken.state == crate::State::Echo;
    spoken
        .lines
        .iter()
        .map(|line| Painted {
            text: line.clone(),
            color: colour,
            underline,
        })
        .collect()
}

pub fn role_color(role: Role, bg: TermBg) -> Color32 {
    match role {
        Role::Room => color32(bg.rgb),
        Role::InkPrimary | Role::Live | Role::Refused => color32(fit_ink_primary(bg)),
        Role::InkQuiet | Role::Echo | Role::MusicNone => color32(fit_ink_quiet(bg)),
        Role::Rule => color32(fit_rule(bg)),
        Role::Ember | Role::Armed => color32(ember_rgb(bg)),
        // No swatch here. Callers with a cover use [`color32`] on a fitted swatch.
        Role::MusicInfo | Role::MusicDecoration => color32(fit_ink_quiet(bg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{contrast_ratio, tokens, BgSource};

    fn rgb_of(c: Color32) -> Rgb {
        [c.r(), c.g(), c.b()]
    }

    #[test]
    fn visuals_follow_the_room_and_carry_no_alarm_red() {
        for bg in [
            TermBg::dark_default(),
            TermBg::from_rgb([0xf7, 0xf4, 0xef], BgSource::Provided),
        ] {
            let v = visuals(bg);
            assert_eq!(rgb_of(v.panel_fill), bg.rgb);
            assert!(
                v.override_text_color.is_none(),
                "must not force ink on selected text"
            );
            assert_eq!(
                v.widgets.noninteractive.fg_stroke.color,
                color32(fit_ink_primary(bg)),
                "labels are primary ink"
            );
            let err = rgb_of(v.error_fg_color);
            assert!(contrast_ratio(err, bg.rgb) >= tokens().room.ink_primary_min_contrast);
            assert_ne!(err, [255, 0, 0]);
            let fill = rgb_of(v.selection.bg_fill);
            let text = rgb_of(v.selection.stroke.color);
            assert_eq!(
                fill,
                rgb_of(color32(fit_ink_primary(bg))),
                "fill is primary ink"
            );
            assert_eq!(text, bg.rgb, "selected text is the room");
            assert_ne!(text, fill, "ink on ink");
            assert!(
                contrast_ratio(text, fill) >= tokens().room.ink_quiet_min_contrast,
                "selected text {:.2}:1 against its fill",
                contrast_ratio(text, fill)
            );
        }
    }

    #[test]
    fn motion_times_match_the_tokens() {
        let m = motion_times();
        assert_eq!(m.floor, Duration::from_millis(tokens().motion.floor_ms));
        assert_eq!(m.skip, Duration::from_millis(tokens().motion.skip_ms));
        assert_eq!(m.pause, Duration::from_millis(tokens().motion.pause_ms));
        assert!((m.floor_secs - 0.250).abs() < 1e-6);
        assert!((m.skip_secs - 0.350).abs() < 1e-6);
        assert!((m.pause_secs - 0.500).abs() < 1e-6);
    }

    #[test]
    fn render_keeps_the_lines() {
        let bg = TermBg::dark_default();
        let echo = crate::Spoken::new(crate::State::Echo, vec!["confirm? [y/N]".into()]);
        let note = crate::Spoken::new(crate::State::Note, vec!["NOTE: a wake".into()]);
        let echo_row = &render(&echo, bg)[0];
        let note_row = &render(&note, bg)[0];
        assert!(echo_row.underline);
        assert!(!note_row.underline);
        assert_eq!(echo_row.text, "confirm? [y/N]");
    }
}
