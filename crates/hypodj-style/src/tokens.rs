//! `design/tokens.toml`, embedded. One source of truth for the values.

use std::sync::OnceLock;

use serde::Deserialize;

const TOML: &str = include_str!("../../../design/tokens.toml");

/// Fade defaults, read as source so this crate does not link the player.
#[cfg(test)]
const FADE_SRC: &str = include_str!("../../../crates/hypodj-core/src/config.rs");

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Tokens {
    pub room: RoomTokens,
    pub music: MusicTokens,
    pub ember: EmberTokens,
    pub motion: MotionTokens,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RoomTokens {
    pub ink_primary_min_contrast: f32,
    pub ink_quiet_min_contrast: f32,
    pub rule_max_contrast: f32,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MusicTokens {
    pub info_min_contrast: f32,
    pub decoration_max_contrast: f32,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct EmberTokens {
    /// CSS-style OKLCH: lightness, chroma, hue in degrees.
    pub oklch: [f32; 3],
    pub min_contrast: f32,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MotionTokens {
    pub floor_ms: u64,
    pub skip_ms: u64,
    pub pause_ms: u64,
    pub max_chrome_hz: u64,
    pub curve: String,
}

/// The embedded token file. Panics if it does not parse: the file is part of
/// the source tree, so a parse failure is a build error that surfaces at first use.
pub fn tokens() -> &'static Tokens {
    static T: OnceLock<Tokens> = OnceLock::new();
    T.get_or_init(|| {
        toml::from_str(TOML).unwrap_or_else(|e| panic!("design/tokens.toml failed to parse: {e}"))
    })
}

/// Chrome frame index at `secs`, at most `max_chrome_hz`. The level meter does
/// not use this; it is the one fast mover.
pub fn chrome_frame(secs: f64, frames: usize) -> usize {
    if frames == 0 {
        return 0;
    }
    let hz = tokens().motion.max_chrome_hz.max(1) as f64;
    ((secs.max(0.0) * hz) as usize) % frames
}

/// Quantise a clock to the chrome period so nothing visible steps faster than
/// the motion token.
pub fn chrome_secs(secs: f64) -> f64 {
    let hz = tokens().motion.max_chrome_hz.max(1) as f64;
    (secs.max(0.0) * hz).floor() / hz
}

/// `pub const NAME: <type> = <literal>;` from the fade config source.
#[cfg(test)]
fn fade_const(name: &str) -> f64 {
    let needle = format!("pub const {name}:");
    let Some(at) = FADE_SRC.find(&needle) else {
        panic!("FadeConfig is missing {name}");
    };
    let after = &FADE_SRC[at + needle.len()..];
    let Some(eq) = after.find('=') else {
        panic!("{name} has no value");
    };
    let lit = after[eq + 1..].trim();
    let Some(end) = lit.find(';') else {
        panic!("{name} has no terminator");
    };
    let lit = lit[..end].trim().replace('_', "");
    lit.parse()
        .unwrap_or_else(|_| panic!("{name} is not a number: {lit}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_matches_the_spec() {
        let t = tokens();
        assert_eq!(t.room.ink_primary_min_contrast, 7.0);
        assert_eq!(t.room.ink_quiet_min_contrast, 4.5);
        assert_eq!(t.room.rule_max_contrast, 2.0);
        assert_eq!(t.music.info_min_contrast, 3.0);
        assert_eq!(t.music.decoration_max_contrast, 2.0);
        assert_eq!(t.ember.oklch, [0.74, 0.15, 60.0]);
        assert_eq!(t.ember.min_contrast, 3.0);
        assert_eq!(t.motion.curve, "oklch_lightness");
        assert_eq!(t.motion.max_chrome_hz, 4);
    }

    #[test]
    fn chrome_does_not_step_faster_than_the_token() {
        let hz = tokens().motion.max_chrome_hz.max(1) as f64;
        let period = 1.0 / hz;
        assert_eq!(chrome_secs(0.0), 0.0);
        assert_eq!(chrome_secs(period * 0.99), 0.0);
        assert_eq!(chrome_secs(period), period);
        assert_eq!(chrome_frame(0.0, 4), chrome_frame(period * 0.99, 4));
        assert_ne!(chrome_frame(0.0, 4), chrome_frame(period, 4));
    }

    /// Motion tokens are the fade defaults. Fails if either side drifts.
    #[test]
    fn motion_matches_fade_config_defaults() {
        let t = tokens();
        let min_slew = fade_const("DEFAULT_MIN_SLEW_MS");
        let tick = fade_const("DEFAULT_TICK_MS");
        let skip = fade_const("DEFAULT_SKIP_FADE_SECS");
        let pause = fade_const("DEFAULT_PAUSE_FADE_SECS");
        assert_eq!(t.motion.floor_ms as f64, min_slew, "floor is min slew");
        assert_eq!(
            t.motion.skip_ms as f64,
            (skip * 1000.0).round(),
            "skip ms is the skip fade"
        );
        assert_eq!(
            t.motion.pause_ms as f64,
            (pause * 1000.0).round(),
            "pause ms is the pause fade"
        );
        assert_eq!(
            t.motion.max_chrome_hz as f64,
            (1000.0 / tick).round(),
            "chrome rate is the fade tick"
        );
        assert_eq!(
            t.motion.floor_ms as f64, tick,
            "floor and the fade tick agree"
        );
    }
}
