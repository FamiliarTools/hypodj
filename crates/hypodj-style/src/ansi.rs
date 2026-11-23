//! ANSI adapter for the CLI.
//!
//! Colour is on only for a terminal, and only when `NO_COLOR` is set to a
//! non-empty value. An empty `NO_COLOR` is unset. A pipe is plain text.
//! Meaning stays in the words: stripping the escapes yields the plain line.

use crate::{cli_ember, fit_rule, Role, TermBg};

/// Whether this process should emit colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ansi {
    enabled: bool,
    truecolor: bool,
}

impl Ansi {
    pub fn off() -> Self {
        Self {
            enabled: false,
            truecolor: false,
        }
    }

    pub fn on_truecolor() -> Self {
        Self {
            enabled: true,
            truecolor: true,
        }
    }

    pub fn on_indexed() -> Self {
        Self {
            enabled: true,
            truecolor: false,
        }
    }

    /// `no_color` is the `NO_COLOR` value. Empty or absent does not disable colour.
    /// `colorterm` is `COLORTERM`: `truecolor` or `24bit` emits 24-bit, otherwise
    /// the 256-colour cell.
    pub fn from_env(is_tty: bool, no_color: Option<&str>, colorterm: Option<&str>) -> Self {
        let disabled = no_color.is_some_and(|v| !v.is_empty());
        let truecolor = matches!(colorterm, Some("truecolor") | Some("24bit"));
        Self {
            enabled: is_tty && !disabled,
            truecolor,
        }
    }

    pub fn enabled(self) -> bool {
        self.enabled
    }

    pub fn truecolor(self) -> bool {
        self.truecolor
    }

    /// Paint `text` for `role` against `bg`. Off mode returns `text` unchanged.
    /// Refused paints nothing: a refusal has no colour.
    pub fn paint(self, role: Role, text: &str, bg: TermBg) -> String {
        if !self.enabled || text.is_empty() {
            return text.to_string();
        }
        let Some(open) = open_code(self, role, bg) else {
            return text.to_string();
        };
        format!("{open}{text}\u{1b}[0m")
    }

    /// Paint every line of a [`crate::Spoken`]. The state picks the role.
    pub fn render(self, spoken: &crate::Spoken) -> String {
        let bg = TermBg::dark_default();
        let role = crate::role_for(spoken.state);
        spoken
            .lines
            .iter()
            .map(|line| self.paint(role, line, bg))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn fg(ansi: Ansi, rgb: [u8; 3], index: u8) -> String {
    if ansi.truecolor {
        format!("\u{1b}[38;2;{};{};{}m", rgb[0], rgb[1], rgb[2])
    } else {
        format!("\u{1b}[38;5;{index}m")
    }
}

fn open_code(ansi: Ansi, role: Role, bg: TermBg) -> Option<String> {
    match role {
        // The terminal's own foreground. Live adds bold. No explicit ink:
        // the CLI cannot see the room.
        Role::Live => Some("\u{1b}[1m".to_string()),
        Role::InkPrimary => None,
        Role::InkQuiet | Role::MusicNone => Some("\u{1b}[2m".to_string()),
        Role::Echo => Some("\u{1b}[2;4m".to_string()),
        Role::Rule => {
            let rgb = fit_rule(bg);
            Some(fg(ansi, rgb, crate::quantize_256(rgb).0))
        }
        // The CLI cannot see the room, so ember is the one colour that clears
        // the floor on both pure black and pure white.
        Role::Ember | Role::Armed => {
            let e = cli_ember();
            Some(fg(ansi, e.rgb, e.index))
        }
        // Refused is primary ink with no colour and no alarm.
        Role::Refused | Role::Room => None,
        // Swatch roles need a cover. The CLI has no cover; treat as quiet.
        Role::MusicInfo | Role::MusicDecoration => Some("\u{1b}[2m".to_string()),
    }
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
    use crate::TermBg;
    use crate::{Spoken, State};

    #[test]
    fn from_env_is_plain_off_a_tty_or_under_no_color() {
        assert!(!Ansi::from_env(false, None, None).enabled());
        assert!(!Ansi::from_env(true, Some("1"), None).enabled());
        assert!(
            Ansi::from_env(true, Some(""), None).enabled(),
            "empty NO_COLOR is unset"
        );
        assert!(Ansi::from_env(true, None, None).enabled());
        assert!(Ansi::from_env(true, None, Some("truecolor")).truecolor());
        assert!(Ansi::from_env(true, None, Some("24bit")).truecolor());
        assert!(!Ansi::from_env(true, None, Some("256")).truecolor());
        assert!(!Ansi::from_env(true, None, None).truecolor());
    }

    #[test]
    fn ember_is_truecolor_or_256_from_colorterm() {
        let bg = TermBg::dark_default();
        let tc = Ansi::on_truecolor().paint(crate::Role::Ember, "x", bg);
        let ix = Ansi::on_indexed().paint(crate::Role::Ember, "x", bg);
        assert!(tc.contains("\u{1b}[38;2;"), "truecolor: {tc:?}");
        assert!(ix.contains("\u{1b}[38;5;"), "indexed: {ix:?}");
        assert!(!ix.contains("\u{1b}[38;2;"));
        assert_eq!(strip(&tc), "x");
        assert_eq!(strip(&ix), "x");
    }

    #[test]
    fn render_strips_back_to_the_lines() {
        let spoken = Spoken::new(State::Armed, vec!["armed (plan 4)".into()]);
        let off = Ansi::off().render(&spoken);
        let on = Ansi::on_truecolor().render(&spoken);
        assert_eq!(off, spoken.text());
        assert_eq!(strip(&on), off);
        assert_ne!(on, off);
    }

    #[test]
    fn refused_adds_no_escape() {
        let bg = TermBg::dark_default();
        let line = "That did not run, because nothing is playing.";
        let on = Ansi::on_truecolor().paint(crate::Role::Refused, line, bg);
        assert_eq!(on, line);
        assert!(!on.contains('\u{1b}'));
    }

    #[test]
    fn ember_uses_a_colour_and_strip_removes_it() {
        let bg = TermBg::dark_default();
        let on = Ansi::on_truecolor().paint(crate::Role::Ember, "\u{2665}\u{FE0E}", bg);
        assert!(on.contains('\u{1b}'), "kept mark is coloured when enabled");
        assert_eq!(strip(&on), "\u{2665}\u{FE0E}");
        let off = Ansi::off().paint(crate::Role::Ember, "\u{2665}\u{FE0E}", bg);
        assert_eq!(off, "\u{2665}\u{FE0E}");
        assert_eq!(strip(&on), off);
    }
}
