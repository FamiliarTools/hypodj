//! ANSI adapter for the CLI.
//!
//! Colour is on only for a terminal, and only when `NO_COLOR` is unset.
//! A pipe, a capture, or `NO_COLOR` is plain text. Meaning stays in the words:
//! stripping the escapes yields the plain line.

use crate::{ember_rgb, fit_decoration, fit_info, fit_rule, Role, TermBg};

/// Whether this process should emit colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ansi {
    enabled: bool,
}

impl Ansi {
    pub fn off() -> Self {
        Self { enabled: false }
    }

    pub fn on() -> Self {
        Self { enabled: true }
    }

    /// `is_tty` is stdout (or whichever stream will carry the line).
    /// `no_color` is set when the `NO_COLOR` environment variable is present.
    pub fn from_env(is_tty: bool, no_color: bool) -> Self {
        Self {
            enabled: is_tty && !no_color,
        }
    }

    pub fn enabled(self) -> bool {
        self.enabled
    }

    /// Paint `text` for `role` against `bg`. Off mode returns `text` unchanged.
    /// Refused paints nothing: a refusal has no colour.
    pub fn paint(self, role: Role, text: &str, bg: TermBg) -> String {
        if !self.enabled || text.is_empty() {
            return text.to_string();
        }
        let Some(open) = open_code(role, bg) else {
            return text.to_string();
        };
        format!("{open}{text}\u{1b}[0m")
    }
}

fn fg(rgb: [u8; 3]) -> String {
    format!("\u{1b}[38;2;{};{};{}m", rgb[0], rgb[1], rgb[2])
}

fn open_code(role: Role, bg: TermBg) -> Option<String> {
    match role {
        // The terminal's own foreground. Live adds bold; primary does not.
        Role::Live => Some("\u{1b}[1m".to_string()),
        Role::InkPrimary => None,
        Role::InkQuiet | Role::MusicNone => Some("\u{1b}[2m".to_string()),
        Role::Echo => Some("\u{1b}[2;4m".to_string()),
        Role::Rule => Some(fg(fit_rule(bg))),
        Role::Ember | Role::Armed => Some(fg(ember_rgb(bg))),
        // Refused is primary ink with no colour and no alarm.
        Role::Refused | Role::Room => None,
        // Swatch roles need a cover. The CLI has no cover; treat as quiet.
        Role::MusicInfo | Role::MusicDecoration => Some("\u{1b}[2m".to_string()),
    }
}

/// Paint a music swatch. Used when a surface does have a cover colour.
pub fn paint_swatch(ansi: Ansi, info: bool, swatch: [u8; 3], text: &str, bg: TermBg) -> String {
    if !ansi.enabled() || text.is_empty() {
        return text.to_string();
    }
    let rgb = if info {
        fit_info(swatch, bg)
    } else {
        fit_decoration(swatch, bg)
    };
    format!("{}\u{1b}[0m", format!("{}{text}", fg(rgb)))
}

/// Drop CSI and OSC sequences. The words that remain are the line.
pub fn strip(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b {
            if i + 1 < b.len() && b[i + 1] == b'[' {
                i += 2;
                while i < b.len() && !b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                if i < b.len() {
                    i += 1;
                }
                continue;
            }
            if i + 1 < b.len() && b[i + 1] == b']' {
                i += 2;
                while i < b.len() && b[i] != 0x07 && !(b[i] == b'\\' && i > 0 && b[i - 1] == 0x1b) {
                    i += 1;
                }
                if i < b.len() {
                    i += 1;
                }
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{speak, spoken_states, Act, TermBg};

    #[test]
    fn from_env_is_plain_off_a_tty_or_under_no_color() {
        assert!(!Ansi::from_env(false, false).enabled());
        assert!(!Ansi::from_env(true, true).enabled());
        assert!(!Ansi::from_env(false, true).enabled());
        assert!(Ansi::from_env(true, false).enabled());
    }

    #[test]
    fn colour_on_and_off_match_once_stripped() {
        let bg = TermBg::dark_default();
        let light = TermBg::from_rgb([0xff, 0xff, 0xff], crate::BgSource::Provided);
        for spoken in spoken_states() {
            for bg in [bg, light] {
                let off = Ansi::off().paint(spoken.state.role(), &spoken.words, bg);
                let on = Ansi::on().paint(spoken.state.role(), &spoken.words, bg);
                assert_eq!(off, spoken.words, "off mode is the words");
                assert_eq!(strip(&on), spoken.words, "strip restores the words");
                assert_eq!(strip(&on), strip(&off));
                if let Some(undo) = &spoken.undo {
                    let on_undo = Ansi::on().paint(crate::Role::InkQuiet, undo, bg);
                    assert_eq!(strip(&on_undo), *undo);
                }
            }
        }
    }

    #[test]
    fn immediate_and_deferred_lines_survive_colour() {
        let bg = TermBg::dark_default();
        for act in [
            Act::Pause,
            Act::Skip,
            Act::Volume,
            Act::Stop,
            Act::Clear,
            Act::WallClock,
            Act::FutureTrigger,
        ] {
            let spoken = speak(act);
            let on = Ansi::on().paint(spoken.state.role(), &spoken.words, bg);
            let off = Ansi::off().paint(spoken.state.role(), &spoken.words, bg);
            assert_eq!(strip(&on), off);
        }
    }

    #[test]
    fn refused_adds_no_escape() {
        let bg = TermBg::dark_default();
        let line = "That did not run, because nothing is playing.";
        let on = Ansi::on().paint(crate::Role::Refused, line, bg);
        assert_eq!(on, line);
        assert!(!on.contains('\u{1b}'));
    }

    #[test]
    fn ember_uses_a_colour_and_strip_removes_it() {
        let bg = TermBg::dark_default();
        let on = Ansi::on().paint(crate::Role::Ember, "\u{2665}\u{FE0E}", bg);
        assert!(on.contains('\u{1b}'), "kept mark is coloured when enabled");
        assert_eq!(strip(&on), "\u{2665}\u{FE0E}");
        let off = Ansi::off().paint(crate::Role::Ember, "\u{2665}\u{FE0E}", bg);
        assert_eq!(off, "\u{2665}\u{FE0E}");
        assert_eq!(strip(&on), off);
    }
}
