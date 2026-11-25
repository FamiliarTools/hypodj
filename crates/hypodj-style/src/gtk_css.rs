//! The GTK4 / libadwaita adapter: CSS text, no GTK dependency.
//!
//! The room is the OS. libadwaita owns the window, header bar, lists and the
//! accent, and this adapter never touches any of them - in particular it never
//! redefines `--accent-color` / `--accent-bg-color`. HypoDJ owns ONE material
//! surface, the now-playing "instrument", and everything here is scoped to the
//! `.instrument` node and its subtree through HypoDJ's own `--hypodj-*` custom
//! properties.
//!
//! Why a private namespace rather than overriding the room variables on the node:
//! libadwaita's styles-and-appearance and css-variables docs say that when a
//! background colour is overridden for a specific widget, the colours derived from
//! it (the "standalone" `--accent-color`, for example) must be overridden too - the
//! derived ones are computed once, at the root, and inherit as values. Redefining
//! a room variable on a subtree therefore leaves its derivations stale. Our own
//! names have no derivations, so they cannot go stale.
//!
//! High contrast drops the tint entirely: the instrument becomes the plain room,
//! which is exactly what a high-contrast user asked the OS for.

use crate::color::{
    fit_ember, fit_ink_primary, fit_ink_quiet, oklch_to_srgb, srgb_to_oklch, BgSource, Rgb, TermBg,
};

/// The warm-neutral ground with no cover, dark room. Never pure black.
pub const GROUND_DARK: Rgb = [0x11, 0x0e, 0x0c];
/// The warm-paper ground with no cover, light room. Never `#fff`.
pub const GROUND_LIGHT: Rgb = [0xf3, 0xee, 0xe6];

/// OKLCH lightness of a cover-derived ground in each room, and the chroma cap that
/// keeps it a ground rather than a colour field.
const L_DARK: f32 = 0.20;
const L_LIGHT: f32 = 0.94;
const C_MAX: f32 = 0.045;

/// The instrument ground: the cover's average colour held at a fixed lightness and
/// a low chroma, or the warm fallback when there is no cover.
pub fn instrument_ground(cover_avg: Option<Rgb>, dark: bool) -> Rgb {
    match cover_avg {
        None => {
            if dark {
                GROUND_DARK
            } else {
                GROUND_LIGHT
            }
        }
        Some(avg) => {
            let mut o = srgb_to_oklch(avg);
            o.l = if dark { L_DARK } else { L_LIGHT };
            o.c = o.c.min(C_MAX);
            oklch_to_srgb(o)
        }
    }
}

fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// The whole instrument stylesheet for one cover in one room. Load it into ONE
/// `gtk::CssProvider` and replace its contents when the cover or the room changes.
///
/// Named state classes inside the instrument: `.ember` (armed / kept - HypoDJ is
/// holding it for you) and `.quiet`. Nothing else is coloured.
pub fn instrument_css(cover_avg: Option<Rgb>, dark: bool, high_contrast: bool) -> String {
    if high_contrast {
        // The room, unchanged: no ground, no tint. Ember falls back to bold words.
        return ".instrument .ember { font-weight: bold; }\n".to_string();
    }
    let ground = instrument_ground(cover_avg, dark);
    let bg = TermBg::from_rgb(ground, BgSource::Provided);
    let ink = fit_ink_primary(bg);
    let quiet = fit_ink_quiet(bg);
    let ember = fit_ember(bg);
    format!(
        ".instrument {{\n  --hypodj-ground: {g};\n  --hypodj-ink: {i};\n  --hypodj-ink-quiet: {q};\n  --hypodj-ember: {e};\n  background-color: var(--hypodj-ground);\n  color: var(--hypodj-ink);\n  border-radius: 12px;\n}}\n\
.instrument .quiet {{ color: var(--hypodj-ink-quiet); }}\n\
.instrument .ember {{ color: var(--hypodj-ember); }}\n\
.instrument button.flat {{ color: var(--hypodj-ink); }}\n",
        g = hex(ground),
        i = hex(ink),
        q = hex(quiet),
        e = hex(ember),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::contrast_ratio;
    use crate::tokens;

    fn grab(css: &str, var: &str) -> Rgb {
        let at = css.find(var).unwrap() + var.len() + 3;
        let h = &css[at..at + 6];
        [
            u8::from_str_radix(&h[0..2], 16).unwrap(),
            u8::from_str_radix(&h[2..4], 16).unwrap(),
            u8::from_str_radix(&h[4..6], 16).unwrap(),
        ]
    }

    #[test]
    fn every_ground_meets_the_ink_and_ember_floors_in_both_rooms() {
        let t = tokens();
        let covers = [None, Some([255, 0, 0]), Some([0, 0, 255]), Some([250, 250, 250]), Some([5, 5, 5])];
        for dark in [true, false] {
            for c in covers {
                let css = instrument_css(c, dark, false);
                let g = grab(&css, "--hypodj-ground");
                assert!(contrast_ratio(grab(&css, "--hypodj-ink"), g) >= t.room.ink_primary_min_contrast);
                assert!(contrast_ratio(grab(&css, "--hypodj-ink-quiet"), g) >= t.room.ink_quiet_min_contrast);
                assert!(
                    contrast_ratio(grab(&css, "--hypodj-ember"), g) + 1e-3 >= t.ember.min_contrast,
                    "ember on {g:?} (dark {dark})"
                );
            }
        }
    }

    #[test]
    fn the_room_accent_is_never_redefined_and_high_contrast_drops_the_tint() {
        for hc in [false, true] {
            let css = instrument_css(Some([10, 200, 30]), true, hc);
            assert!(!css.contains("--accent"), "{css}");
        }
        assert!(!instrument_css(None, true, true).contains("background-color"));
    }

    #[test]
    fn the_fallback_ground_is_warm_and_never_pure() {
        assert_ne!(instrument_ground(None, false), [255, 255, 255]);
        assert_ne!(instrument_ground(None, true), [0, 0, 0]);
    }
}
