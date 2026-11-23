//! `design/tokens.toml`, embedded. One source of truth for the values.

use std::sync::OnceLock;

use serde::Deserialize;

/// GUI face and the 8 px cell are HypoDJ's own. Not a shared brand base.
const TOML: &str = include_str!("../../../design/tokens.toml");

/// Fade defaults, read as source so this crate does not link the player.
#[cfg(test)]
const FADE_SRC: &str = include_str!("../../../crates/hypodj-core/src/config.rs");

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Tokens {
    pub room: RoomTokens,
    pub music: MusicTokens,
    pub ember: EmberTokens,
    pub state: StateTokens,
    pub motion: MotionTokens,
    #[serde(rename = "type")]
    pub type_face: TypeTokens,
    pub grid: GridTokens,
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
    pub uses: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StateLook {
    pub ink: String,
    #[serde(default)]
    pub decoration: Option<String>,
    #[serde(default)]
    pub weight: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StateTokens {
    pub echo: StateLook,
    pub armed: StateLook,
    pub live: StateLook,
    pub refused: StateLook,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MotionTokens {
    pub floor_ms: u64,
    pub skip_ms: u64,
    pub pause_ms: u64,
    pub max_chrome_hz: u64,
    pub curve: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TypeTokens {
    pub gui_face: String,
    pub weights: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct GridTokens {
    pub gui_px_per_cell_x: u32,
}

/// The embedded token file. Panics if it does not parse: the file is part of
/// the source tree, so a parse failure is a build error that surfaces at first use.
pub fn tokens() -> &'static Tokens {
    static T: OnceLock<Tokens> = OnceLock::new();
    T.get_or_init(|| {
        toml::from_str(TOML).unwrap_or_else(|e| panic!("design/tokens.toml failed to parse: {e}"))
    })
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
        assert_eq!(t.ember.uses, ["armed", "on_air_marked", "kept"]);
        assert_eq!(t.state.echo.ink, "quiet");
        assert_eq!(t.state.echo.decoration.as_deref(), Some("underline"));
        assert!(t.state.echo.weight.is_none());
        assert_eq!(t.state.armed.ink, "ember");
        assert!(t.state.armed.decoration.is_none());
        assert_eq!(t.state.live.ink, "primary");
        assert_eq!(t.state.live.weight.as_deref(), Some("bold"));
        assert_eq!(t.state.refused.ink, "primary");
        assert!(t.state.refused.decoration.is_none());
        assert!(t.state.refused.weight.is_none());
        assert_eq!(t.motion.curve, "oklch_lightness");
        assert_eq!(t.motion.max_chrome_hz, 4);
        assert_eq!(t.type_face.gui_face, "JetBrains Mono");
        assert_eq!(t.type_face.weights, ["regular", "bold"]);
        assert_eq!(t.grid.gui_px_per_cell_x, 8);
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
