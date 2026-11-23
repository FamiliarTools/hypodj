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
    v.override_text_color = Some(ink);
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
    v.widgets.noninteractive.fg_stroke.color = quiet;
    let sel = selection(bg);
    v.selection.stroke = Stroke::new(1.0, sel.stroke);
    v.selection.bg_fill = sel.fill;
    v
}

/// Selection, inverted the way the TUI's REVERSED modifier is: the fill is
/// primary ink, the text is the room, the stroke is primary ink.
pub struct SelectionInk {
    pub fill: Color32,
    pub text: Color32,
    pub stroke: Color32,
}

pub fn selection(bg: TermBg) -> SelectionInk {
    let ink = color32(fit_ink_primary(bg));
    let room = color32(bg.rgb);
    SelectionInk { fill: ink, text: room, stroke: ink }
}

/// One colour per line. The words are unchanged.
pub fn render(spoken: &hypodj_client::Spoken, bg: TermBg) -> Vec<(String, Color32)> {
    let colour = role_color(crate::role_for(spoken.state), bg);
    spoken.lines.iter().map(|line| (line.clone(), colour)).collect()
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
    use crate::{contrast_ratio, ember_rgb, tokens, BgSource};

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
            assert_eq!(v.error_fg_color, v.override_text_color.unwrap());
            let err = rgb_of(v.error_fg_color);
            assert!(contrast_ratio(err, bg.rgb) >= tokens().room.ink_primary_min_contrast);
            assert_ne!(err, [255, 0, 0]);
            let sel = selection(bg);
            assert_eq!(rgb_of(v.selection.bg_fill), rgb_of(sel.fill));
            assert_eq!(rgb_of(sel.text), bg.rgb, "selected text is the room");
            assert_eq!(rgb_of(v.selection.stroke.color), rgb_of(sel.stroke));
            assert_ne!(rgb_of(sel.fill), ember_rgb(bg));
            assert!(
                contrast_ratio(bg.rgb, fit_ink_primary(bg)) >= tokens().room.ink_quiet_min_contrast,
                "selected text on its fill"
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
        let spoken = hypodj_client::Spoken::new(
            hypodj_client::State::Echo,
            vec!["[1] fade out".into(), hypodj_client::spoken::CONFIRM_LINE.into()],
        );
        let rows = render(&spoken, bg);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "[1] fade out");
        assert_eq!(rows[1].0, hypodj_client::spoken::CONFIRM_LINE);
    }
}
