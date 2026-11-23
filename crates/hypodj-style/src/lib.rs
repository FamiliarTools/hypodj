//! HypoDJ design tokens.
//!
//! `design/tokens.toml` is embedded with `include_str!` and parsed once. Roles,
//! contrast policy, and motion constants live here. A surface (ratatui, egui,
//! ANSI) only maps a role. It does not choose a colour.
//!
//! JetBrains Mono and the 8 px cell are HypoDJ's own choices. They are not
//! borrowed from a shared brand base.
//!
//! Terminal IO (OSC 11, DA1) stays in the TUI. This crate is pure.

pub mod ansi;
mod color;
mod tokens;
mod voice;

#[cfg(feature = "egui")]
mod egui_adapter;
#[cfg(feature = "ratatui")]
mod ratatui_adapter;

#[cfg(feature = "egui")]
pub mod egui {
    pub use crate::egui_adapter::*;
}
#[cfg(feature = "ratatui")]
pub mod ratatui {
    pub use crate::ratatui_adapter::*;
}

pub use ansi::Ansi;
pub use color::{
    contrast_ratio, ember_rgb, fit_decoration, fit_ember, fit_info, fit_ink_primary, fit_ink_quiet,
    fit_min_contrast, fit_rule, lerp_oklch_lightness, oklch_to_srgb, quantize_256,
    relative_luminance, srgb_to_oklch, BgSource, Oklch, Rgb, TermBg, CONFIRM_INFO_SWATCH,
    NEUTRAL_MUTED, NEUTRAL_VIBRANT, XTERM_CUBE,
};
pub use tokens::{tokens, Tokens};
pub use voice::{speak, spoken_states, Act, Spoken, State};

/// A semantic role. The colour, if any, is resolved by an adapter against the room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Detected background. HypoDJ does not paint its own.
    Room,
    /// Highest-contrast text. At least 7:1 where a colour must be emitted.
    InkPrimary,
    /// Secondary text. At least 4.5:1 where a colour must be emitted.
    InkQuiet,
    /// Hairline. At most 2:1.
    Rule,
    /// Cover swatch fitted to at least 3:1. The caller supplies the swatch.
    MusicInfo,
    /// Cover swatch capped at 2:1. The caller supplies the swatch.
    MusicDecoration,
    /// No cover. No hue of our own.
    MusicNone,
    /// The one owned colour. Armed, on air when marked, or kept.
    Ember,
    /// Proposed, not armed. Quiet, underlined.
    Echo,
    /// Confirmed and scheduled.
    Armed,
    /// Happening now. Primary, bold.
    Live,
    /// Did not happen. Primary ink, no alarm colour.
    Refused,
}

impl State {
    /// The role that paints this state.
    pub fn role(self) -> Role {
        match self {
            State::Echo => Role::Echo,
            State::Armed => Role::Armed,
            State::Live => Role::Live,
            State::Refused => Role::Refused,
        }
    }
}
