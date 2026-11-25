//! The Qt adapter: the same instrument as [`crate::gtk_css`], as colours for QML.
//!
//! The room is the OS: the Qt app never sets a `QPalette`, a Controls style or an
//! accent, so its chrome follows the host (Breeze or Kirigami on Plasma, the
//! platform theme elsewhere). HypoDJ owns only the now-playing instrument, and these
//! are its colours - the ground, the inks and ember - computed by the one fitting
//! code the GTK adapter uses, so the two shells paint the same instrument.

use crate::color::{fit_ember, fit_ink_primary, fit_ink_quiet, BgSource, Rgb, TermBg};
use crate::gtk_css::instrument_ground;

/// The instrument's colours as `#rrggbb`, ready for a QML `color` property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instrument {
    pub ground: String,
    pub ink: String,
    pub ink_quiet: String,
    pub ember: String,
}

fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// The instrument for one cover in one room. `None` under high contrast: the
/// instrument is then the plain room (the palette's own window and text colours),
/// exactly what a high-contrast user asked the OS for.
pub fn instrument(cover_avg: Option<Rgb>, dark: bool, high_contrast: bool) -> Option<Instrument> {
    if high_contrast {
        return None;
    }
    let ground = instrument_ground(cover_avg, dark);
    let bg = TermBg::from_rgb(ground, BgSource::Provided);
    Some(Instrument {
        ground: hex(ground),
        ink: hex(fit_ink_primary(bg)),
        ink_quiet: hex(fit_ink_quiet(bg)),
        ember: hex(fit_ember(bg)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Qt instrument is the GTK instrument: same ground, inks and ember.
    #[test]
    fn qt_and_gtk_paint_the_same_instrument() {
        for dark in [true, false] {
            for cover in [None, Some([200, 40, 30]), Some([20, 20, 240])] {
                let q = instrument(cover, dark, false).expect("not high contrast");
                let css = crate::gtk_css::instrument_css(cover, dark, false);
                for (var, v) in [
                    ("--hypodj-ground", &q.ground),
                    ("--hypodj-ink", &q.ink),
                    ("--hypodj-ink-quiet", &q.ink_quiet),
                    ("--hypodj-ember", &q.ember),
                ] {
                    assert!(css.contains(&format!("{var}: {v};")), "{var} {v} not in {css}");
                }
            }
        }
        assert!(instrument(None, true, true).is_none());
    }
}
