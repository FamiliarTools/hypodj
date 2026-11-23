//! ratatui adapter.
//!
//! Emphasis (bold, quiet, reverse, underline) is the TUI's existing weight.
//! The terminal keeps its own foreground for ink. Ember and cover swatches are
//! the colours this adapter actually sets, fitted in this crate.

use ::ratatui::style::{Color, Modifier, Style};
use ::ratatui::text::{Line, Span};

use crate::{
    contrast_ratio, ember_rgb, fit_decoration, fit_info, oklch_to_srgb, quantize_256,
    srgb_to_oklch, tokens, Rgb, Role, TermBg,
};

/// Weights the TUI already uses. Regular is the absence of these.
pub trait Emphasis {
    fn quiet(self) -> Self;
    fn bold(self) -> Self;
    fn reverse(self) -> Self;
    fn underline(self) -> Self;
}

impl Emphasis for Style {
    fn quiet(self) -> Self {
        self.add_modifier(Modifier::DIM)
    }
    fn bold(self) -> Self {
        self.add_modifier(Modifier::BOLD)
    }
    fn reverse(self) -> Self {
        self.add_modifier(Modifier::REVERSED)
    }
    fn underline(self) -> Self {
        self.add_modifier(Modifier::UNDERLINED)
    }
}

pub fn to_color(c: Rgb, truecolor: bool) -> Color {
    if truecolor {
        Color::Rgb(c[0], c[1], c[2])
    } else {
        Color::Indexed(quantize_256(c).0)
    }
}

/// Info policy as a ratatui colour. On a 256-colour cell the contrast is
/// rechecked after quantization, because the cube can drop a fitted RGB under 3:1.
pub fn info_color(swatch: Rgb, bg: TermBg, truecolor: bool) -> Color {
    fitted_color(
        fit_info(swatch, bg),
        bg,
        tokens().music.info_min_contrast,
        truecolor,
    )
}

/// Ember as a ratatui colour, fitted, then rechecked on a 256-colour cell.
pub fn ember_color(bg: TermBg, truecolor: bool) -> Color {
    fitted_color(ember_rgb(bg), bg, tokens().ember.min_contrast, truecolor)
}

pub fn decoration_color(swatch: Rgb, bg: TermBg, truecolor: bool) -> Color {
    to_color(fit_decoration(swatch, bg), truecolor)
}

fn fitted_color(fitted: Rgb, bg: TermBg, target: f32, truecolor: bool) -> Color {
    if truecolor {
        return Color::Rgb(fitted[0], fitted[1], fitted[2]);
    }
    let (idx, realized) = quantize_256(fitted);
    if contrast_ratio(realized, bg.rgb) >= target {
        return Color::Indexed(idx);
    }
    let mut o = srgb_to_oklch(fitted);
    let dir = if bg.luminance < crate::INFO_PIVOT {
        1.0
    } else {
        -1.0
    };
    for _ in 0..50 {
        o.l = (o.l + dir * 0.02).clamp(0.0, 1.0);
        let (idx, realized) = quantize_256(oklch_to_srgb(o));
        if contrast_ratio(realized, bg.rgb) >= target {
            return Color::Indexed(idx);
        }
    }
    Color::Indexed(quantize_256(neutral_for(bg)).0)
}

fn neutral_for(bg: TermBg) -> Rgb {
    // Same legible neutral the info policy falls back to.
    if bg.luminance < crate::INFO_PIVOT {
        [0xe0, 0xe0, 0xe0]
    } else {
        [0x20, 0x20, 0x20]
    }
}

/// One style for every line of a [`crate::Spoken`].
pub fn style_for_state(state: crate::State, bg: TermBg, truecolor: bool) -> Style {
    style_for(crate::role_for(state), bg, truecolor)
}

/// Render a spoken reply. Each line carries the state's style.
pub fn render(spoken: &crate::Spoken, bg: TermBg, truecolor: bool) -> Vec<Line<'static>> {
    let style = style_for_state(spoken.state, bg, truecolor);
    spoken
        .lines
        .iter()
        .map(|line| Line::from(Span::styled(line.clone(), style)))
        .collect()
}

/// Style for a role that does not need a cover swatch.
/// Ink is the terminal's own foreground unless OSC 11 reported the room.
/// Ember and armed always set a foreground. A rule does too: it is not ink.
pub fn style_for(role: Role, bg: TermBg, truecolor: bool) -> Style {
    match role {
        Role::InkPrimary | Role::Refused | Role::Room => ink_style(bg, truecolor, false),
        Role::InkQuiet | Role::MusicNone => ink_style(bg, truecolor, true).quiet(),
        Role::Echo => ink_style(bg, truecolor, true).quiet().underline(),
        Role::Live => ink_style(bg, truecolor, false).bold(),
        Role::Ember | Role::Armed => Style::default().fg(ember_color(bg, truecolor)),
        Role::Rule => Style::default().fg(to_color(crate::fit_rule(bg), truecolor)),
        Role::MusicInfo | Role::MusicDecoration => Style::default(),
    }
}

/// Primary or quiet ink. An explicit colour only when OSC 11 reported the room.
fn ink_style(bg: TermBg, truecolor: bool, quiet: bool) -> Style {
    if bg.source != crate::BgSource::Osc11 {
        return Style::default();
    }
    let rgb = if quiet {
        crate::fit_ink_quiet(bg)
    } else {
        crate::fit_ink_primary(bg)
    };
    Style::default().fg(to_color(rgb, truecolor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BgSource, XTERM_CUBE};
    use crate::{Spoken, State};

    fn idx_to_rgb(i: u8) -> Rgb {
        if i >= 232 {
            let v = 8 + (i - 232) * 10;
            [v, v, v]
        } else {
            let i = i - 16;
            [
                XTERM_CUBE[(i / 36) as usize],
                XTERM_CUBE[((i / 6) % 6) as usize],
                XTERM_CUBE[(i % 6) as usize],
            ]
        }
    }

    #[test]
    fn to_color_truecolor_vs_indexed() {
        assert_eq!(to_color([10, 20, 30], true), Color::Rgb(10, 20, 30));
        assert!(matches!(to_color([10, 20, 30], false), Color::Indexed(_)));
    }

    #[test]
    fn info_color_holds_three_to_one_after_quantization() {
        let bg = TermBg::dark_default();
        let target = tokens().music.info_min_contrast;
        let realized = |c: Rgb| -> Rgb {
            match info_color(c, bg, false) {
                Color::Indexed(i) => idx_to_rgb(i),
                _ => panic!("non-truecolor path returns Indexed"),
            }
        };
        assert!(contrast_ratio(realized([0, 14, 7]), bg.rgb) >= target);
        for r in (0..=255).step_by(51) {
            for g in (0..=255).step_by(51) {
                for b in (0..=255).step_by(51) {
                    let got = realized([r as u8, g as u8, b as u8]);
                    assert!(
                        contrast_ratio(got, bg.rgb) >= target,
                        "swatch {:?} realized {got:?} only {:.2}:1",
                        [r, g, b],
                        contrast_ratio(got, bg.rgb)
                    );
                }
            }
        }
    }

    #[test]
    fn ember_indexed_clears_three_to_one_on_dark_and_light() {
        let floor = tokens().ember.min_contrast;
        for bg in [
            TermBg::dark_default(),
            TermBg::from_rgb([0xf7, 0xf4, 0xef], BgSource::Provided),
        ] {
            let realized = match ember_color(bg, false) {
                Color::Indexed(i) => idx_to_rgb(i),
                other => panic!("expected indexed, got {other:?}"),
            };
            assert!(
                contrast_ratio(realized, bg.rgb) >= floor,
                "indexed ember {realized:?} on {:?} is {:.2}",
                bg.rgb,
                contrast_ratio(realized, bg.rgb)
            );
        }
    }

    #[test]
    fn render_keeps_every_line_and_the_state_style() {
        let bg = TermBg::dark_default();
        let echo = Spoken::new(
            State::Echo,
            vec![
                "(via rules)".into(),
                "[1] fade out".into(),
                "confirm? [y/N]".into(),
            ],
        );
        let lines = render(&echo, bg, true);
        assert_eq!(lines.len(), 3);
        for line in &lines {
            let style = line.spans[0].style;
            assert!(style.add_modifier.contains(Modifier::DIM));
            assert!(style.add_modifier.contains(Modifier::UNDERLINED));
        }
        let armed = Spoken::new(State::Armed, vec!["armed (plan 4)".into()]);
        let styled = render(&armed, bg, true);
        assert!(matches!(
            styled[0].spans[0].style.fg,
            Some(Color::Rgb(_, _, _))
        ));
    }

    #[test]
    fn live_ink_is_reset_unless_osc11_reported_the_room() {
        let fallback = TermBg::dark_default();
        let live = style_for_state(State::Live, fallback, true);
        assert!(live.fg.is_none(), "fallback live has no explicit ink");
        assert!(live.add_modifier.contains(Modifier::BOLD));
        let quiet = style_for(Role::InkQuiet, fallback, true);
        assert!(quiet.fg.is_none());
        let osc = TermBg::from_rgb([0x12, 0x12, 0x12], BgSource::Osc11);
        let reported = style_for_state(State::Live, osc, true);
        assert!(
            reported.fg.is_some(),
            "a reported room may carry fitted ink"
        );
        let fgbg = TermBg::from_rgb([0x12, 0x12, 0x12], BgSource::ColorFgBg);
        assert!(style_for_state(State::Live, fgbg, true).fg.is_none());
    }

    #[test]
    fn emphasis_matches_the_historical_modifiers() {
        let s = Style::default();
        assert!(s.quiet().add_modifier.contains(Modifier::DIM));
        assert!(s.bold().add_modifier.contains(Modifier::BOLD));
        assert!(s.reverse().add_modifier.contains(Modifier::REVERSED));
        assert!(s.underline().add_modifier.contains(Modifier::UNDERLINED));
    }
}
