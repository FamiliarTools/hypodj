//! WCAG contrast and OKLCH nudges.
//!
//! Moved out of the TUI. Thresholds come from [`crate::tokens`], not from
//! literals in this file. Hue on the wire (the token file) is degrees, as in
//! CSS OKLCH. The math below uses radians.

use crate::tokens;

/// sRGB, 8-bit per channel.
pub type Rgb = [u8; 3];

/// xterm 6-level cube axis. Shared with the 256-colour recheck.
pub const XTERM_CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// No-cover fallback. A fixed neutral, not a hue HypoDJ owns.
/// The sigil's shape still comes from the album hash.
pub const NEUTRAL_VIBRANT: Rgb = [0x88, 0x88, 0x88];
pub const NEUTRAL_MUTED: Rgb = [0x55, 0x55, 0x55];

/// Swatch the TUI already feeds through the info policy for a confirm prompt.
/// It is an input to [`fit_info`], not a second owned colour. Ember stays the
/// only colour HypoDJ claims. Kept so that prompt does not change appearance.
pub const CONFIRM_INFO_SWATCH: Rgb = [0xff, 0xd0, 0x66];

/// Where a resolved background came from, in fallback order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgSource {
    Osc11,
    ColorFgBg,
    /// Reserved tier between COLORFGBG and the dark default.
    #[allow(dead_code)]
    LuminanceGuess,
    DarkDefault,
    /// The caller already knows the background (a test, or a GUI theme).
    Provided,
}

/// A background the contrast policies can push against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TermBg {
    pub rgb: Rgb,
    pub luminance: f32,
    pub is_dark: bool,
    pub source: BgSource,
}

impl TermBg {
    pub fn from_rgb(rgb: Rgb, source: BgSource) -> TermBg {
        let luminance = relative_luminance(rgb);
        TermBg {
            rgb,
            luminance,
            is_dark: luminance < 0.5,
            source,
        }
    }

    /// Near-black fallback when nothing reliable was detected.
    pub fn dark_default() -> TermBg {
        TermBg::from_rgb([0x12, 0x12, 0x12], BgSource::DarkDefault)
    }

    pub fn light(rgb: Rgb) -> TermBg {
        TermBg::from_rgb(rgb, BgSource::Provided)
    }
}

/// OKLCH: lightness, chroma, hue in radians.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklch {
    pub l: f32,
    pub c: f32,
    pub h: f32,
}

fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let v = if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (v * 255.0).round().clamp(0.0, 255.0) as u8
}

/// WCAG relative luminance in `[0, 1]`.
pub fn relative_luminance(c: Rgb) -> f32 {
    0.2126 * srgb_to_linear(c[0]) + 0.7152 * srgb_to_linear(c[1]) + 0.0722 * srgb_to_linear(c[2])
}

/// WCAG contrast ratio in `1.0..=21.0`, order-free.
pub fn contrast_ratio(a: Rgb, b: Rgb) -> f32 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// sRGB to OKLCH (Ottosson Oklab as lightness, chroma, hue).
pub fn srgb_to_oklch(c: Rgb) -> Oklch {
    let r = srgb_to_linear(c[0]);
    let g = srgb_to_linear(c[1]);
    let b = srgb_to_linear(c[2]);
    let l = 0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b;
    let m = 0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b;
    let s = 0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b;
    let l_ = l.cbrt();
    let m_ = m.cbrt();
    let s_ = s.cbrt();
    let ll = 0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_;
    let aa = 1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_;
    let bb = 0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_;
    Oklch {
        l: ll,
        c: (aa * aa + bb * bb).sqrt(),
        h: bb.atan2(aa),
    }
}

/// OKLCH to sRGB, gamut-clamped on the linear channels.
pub fn oklch_to_srgb(o: Oklch) -> Rgb {
    let a = o.c * o.h.cos();
    let b = o.c * o.h.sin();
    let l_ = o.l + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = o.l - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = o.l - 0.0894841775 * a - 1.2914855480 * b;
    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;
    let r = 4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s;
    let g = -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s;
    let bl = -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s;
    [linear_to_srgb(r), linear_to_srgb(g), linear_to_srgb(bl)]
}

/// Luminance where contrast to black equals contrast to white (`L ~= 0.179`).
/// Below it, push lightness up. Above it, push down. Not the 0.5 dark/light cut.
const INFO_PIVOT: f32 = 0.179;

fn info_dir(bg: TermBg) -> f32 {
    if bg.luminance < INFO_PIVOT {
        1.0
    } else {
        -1.0
    }
}

fn info_neutral(bg: TermBg) -> Rgb {
    if bg.luminance < INFO_PIVOT {
        [0xe0, 0xe0, 0xe0]
    } else {
        [0x20, 0x20, 0x20]
    }
}

/// Push `swatch` in OKLCH lightness until contrast against `bg` reaches `target`.
pub fn fit_min_contrast(swatch: Rgb, bg: TermBg, target: f32) -> Rgb {
    if contrast_ratio(swatch, bg.rgb) >= target {
        return swatch;
    }
    let mut o = srgb_to_oklch(swatch);
    let dir = info_dir(bg);
    for _ in 0..50 {
        o.l = (o.l + dir * 0.02).clamp(0.0, 1.0);
        let c = oklch_to_srgb(o);
        if contrast_ratio(c, bg.rgb) >= target {
            return c;
        }
    }
    info_neutral(bg)
}

/// Music info policy. Target comes from the token file (`music.info_min_contrast`).
pub fn fit_info(swatch: Rgb, bg: TermBg) -> Rgb {
    fit_min_contrast(swatch, bg, tokens().music.info_min_contrast)
}

/// Music decoration policy. Chroma down, then lightness toward the room,
/// until contrast sits under `music.decoration_max_contrast`.
pub fn fit_decoration(swatch: Rgb, bg: TermBg) -> Rgb {
    let mut o = srgb_to_oklch(swatch);
    let bgo = srgb_to_oklch(bg.rgb);
    o.c *= 0.5;
    o.l = o.l * 0.55 + bgo.l * 0.45;
    let cap = tokens().music.decoration_max_contrast;
    for _ in 0..50 {
        if contrast_ratio(oklch_to_srgb(o), bg.rgb) <= cap {
            break;
        }
        o.l += (bgo.l - o.l) * 0.2;
    }
    oklch_to_srgb(o)
}

/// Ember as specified, before it is fitted to a room.
pub fn ember_rgb_raw() -> Rgb {
    let [l, c, h_deg] = tokens().ember.oklch;
    oklch_to_srgb(Oklch {
        l,
        c,
        h: h_deg.to_radians(),
    })
}

/// Ember fitted to `bg` by the same nudge as music info, to ember's own floor.
pub fn fit_ember(bg: TermBg) -> Rgb {
    fit_min_contrast(ember_rgb_raw(), bg, tokens().ember.min_contrast)
}

/// Alias kept for call sites that want the fitted colour by the token's name.
pub fn ember_rgb(bg: TermBg) -> Rgb {
    fit_ember(bg)
}

/// Primary ink. The extreme (white or black) in the high-headroom direction,
/// then the info nudge if that extreme is still short of 7:1.
pub fn fit_ink_primary(bg: TermBg) -> Rgb {
    let seed = if bg.luminance < INFO_PIVOT {
        [0xff, 0xff, 0xff]
    } else {
        [0x00, 0x00, 0x00]
    };
    fit_min_contrast(seed, bg, tokens().room.ink_primary_min_contrast)
}

/// Quiet ink: step away from the room until the 4.5:1 floor is met, and stop.
pub fn fit_ink_quiet(bg: TermBg) -> Rgb {
    let target = tokens().room.ink_quiet_min_contrast;
    let dir = info_dir(bg);
    let mut o = srgb_to_oklch(bg.rgb);
    o.c = 0.0;
    for _ in 0..50 {
        o.l = (o.l + dir * 0.02).clamp(0.0, 1.0);
        let c = oklch_to_srgb(o);
        if contrast_ratio(c, bg.rgb) >= target {
            return c;
        }
    }
    info_neutral(bg)
}

/// A hairline. Pulled toward the room until contrast is at most the rule cap.
pub fn fit_rule(bg: TermBg) -> Rgb {
    let mut o = srgb_to_oklch(if bg.is_dark {
        NEUTRAL_VIBRANT
    } else {
        [0x66, 0x66, 0x66]
    });
    let bgo = srgb_to_oklch(bg.rgb);
    let cap = tokens().room.rule_max_contrast;
    for _ in 0..50 {
        if contrast_ratio(oklch_to_srgb(o), bg.rgb) <= cap {
            break;
        }
        o.l += (bgo.l - o.l) * 0.25;
        o.c *= 0.5;
    }
    oklch_to_srgb(o)
}

/// Nearest xterm-256 colour: index and the RGB that cell actually shows.
pub fn quantize_256(c: Rgb) -> (u8, Rgb) {
    let dist = |a: Rgb, b: Rgb| -> i32 {
        (0..3)
            .map(|i| {
                let d = a[i] as i32 - b[i] as i32;
                d * d
            })
            .sum()
    };
    let mut best_idx = 16u8;
    let mut best_rgb = [0u8; 3];
    let mut best_d = i32::MAX;
    for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                let rgb = [XTERM_CUBE[r], XTERM_CUBE[g], XTERM_CUBE[b]];
                let d = dist(rgb, c);
                if d < best_d {
                    best_d = d;
                    best_rgb = rgb;
                    best_idx = 16 + (36 * r + 6 * g + b) as u8;
                }
            }
        }
    }
    for j in 0..24u8 {
        let v = 8 + j * 10;
        let rgb = [v, v, v];
        let d = dist(rgb, c);
        if d < best_d {
            best_d = d;
            best_rgb = rgb;
            best_idx = 232 + j;
        }
    }
    (best_idx, best_rgb)
}

/// Motion curve: lightness moves in OKLCH. Chroma and hue stay with `from`.
/// `t` is clamped to `0..=1`. No overshoot.
pub fn lerp_oklch_lightness(from: Rgb, to: Rgb, t: f32) -> Rgb {
    let mut a = srgb_to_oklch(from);
    let b = srgb_to_oklch(to);
    let t = t.clamp(0.0, 1.0);
    a.l = a.l + (b.l - a.l) * t;
    oklch_to_srgb(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_target() -> f32 {
        tokens().music.info_min_contrast
    }

    #[test]
    fn luminance_and_contrast_anchors() {
        assert!(relative_luminance([0, 0, 0]).abs() < 1e-6);
        assert!((relative_luminance([255, 255, 255]) - 1.0).abs() < 1e-6);
        assert!((contrast_ratio([0, 0, 0], [255, 255, 255]) - 21.0).abs() < 0.01);
        assert_eq!(
            contrast_ratio([10, 20, 30], [200, 200, 200]),
            contrast_ratio([200, 200, 200], [10, 20, 30])
        );
    }

    #[test]
    fn oklch_round_trips() {
        for c in [[120, 60, 200], [255, 0, 0], [10, 200, 90], [200, 200, 200]] {
            let back = oklch_to_srgb(srgb_to_oklch(c));
            for i in 0..3 {
                assert!(
                    (back[i] as i32 - c[i] as i32).abs() <= 2,
                    "round trip {c:?} -> {back:?}"
                );
            }
        }
    }

    #[test]
    fn fit_info_clears_three_to_one_on_dark_and_light() {
        let swatch = [90, 70, 60];
        let dark = TermBg::dark_default();
        let light = TermBg::from_rgb([0xf0, 0xf0, 0xf0], BgSource::Provided);
        let on_dark = fit_info(swatch, dark);
        let on_light = fit_info(swatch, light);
        assert!(contrast_ratio(on_dark, dark.rgb) >= info_target());
        assert!(contrast_ratio(on_light, light.rgb) >= info_target());
        let h0 = srgb_to_oklch(swatch).h;
        let h1 = srgb_to_oklch(on_dark).h;
        assert!((h0 - h1).abs() < 0.35, "hue preserved within tolerance");
    }

    #[test]
    fn fit_info_clears_on_medium_gray_backgrounds() {
        let swatch = [220, 40, 40];
        for g in [150u8, 160, 170, 180] {
            let bg = TermBg::from_rgb([g, g, g], BgSource::Provided);
            let fitted = fit_info(swatch, bg);
            assert!(
                contrast_ratio(fitted, bg.rgb) >= info_target(),
                "clears 3:1 on gray {g}: got {:.2} for {fitted:?}",
                contrast_ratio(fitted, bg.rgb)
            );
        }
    }

    #[test]
    fn fit_decoration_reduces_chroma_and_contrast() {
        let swatch = [220, 40, 40];
        let bg = TermBg::dark_default();
        let deco = fit_decoration(swatch, bg);
        assert!(
            srgb_to_oklch(deco).c < srgb_to_oklch(swatch).c,
            "chroma pulled down"
        );
        assert!(contrast_ratio(deco, bg.rgb) <= contrast_ratio(swatch, bg.rgb));
    }

    #[test]
    fn fit_decoration_caps_light_cover_below_ceiling() {
        let cap = tokens().music.decoration_max_contrast;
        for bg in [
            TermBg::dark_default(),
            TermBg::from_rgb([0xf4, 0xf4, 0xf4], BgSource::Provided),
        ] {
            for swatch in [[0xf0, 0xf0, 0xf0], [0xff, 0xd0, 0x40], [0x10, 0x10, 0x10]] {
                let deco = fit_decoration(swatch, bg);
                assert!(
                    contrast_ratio(deco, bg.rgb) <= cap + 0.01,
                    "sigil {swatch:?} on bg {:?} exceeds ceiling: {:.2}",
                    bg.rgb,
                    contrast_ratio(deco, bg.rgb)
                );
            }
        }
    }

    #[test]
    fn ember_clears_three_to_one_on_dark_and_light() {
        let floor = tokens().ember.min_contrast;
        let dark = TermBg::dark_default();
        let light = TermBg::from_rgb([0xf7, 0xf4, 0xef], BgSource::Provided);
        let white = TermBg::from_rgb([0xff, 0xff, 0xff], BgSource::Provided);
        for bg in [dark, light, white] {
            let ember = fit_ember(bg);
            assert!(
                contrast_ratio(ember, bg.rgb) >= floor,
                "ember {:?} on {:?} is only {:.2}:1",
                ember,
                bg.rgb,
                contrast_ratio(ember, bg.rgb)
            );
        }
    }

    #[test]
    fn ink_and_rule_meet_their_floors_on_the_default_rooms() {
        let dark = TermBg::dark_default();
        let light = TermBg::from_rgb([0xf7, 0xf4, 0xef], BgSource::Provided);
        for bg in [dark, light] {
            let primary = fit_ink_primary(bg);
            let quiet = fit_ink_quiet(bg);
            let rule = fit_rule(bg);
            assert!(
                contrast_ratio(primary, bg.rgb) >= tokens().room.ink_primary_min_contrast,
                "primary {:?} on {:?}",
                primary,
                bg.rgb
            );
            assert!(
                contrast_ratio(quiet, bg.rgb) >= tokens().room.ink_quiet_min_contrast,
                "quiet {:?} on {:?}",
                quiet,
                bg.rgb
            );
            assert!(
                contrast_ratio(rule, bg.rgb) <= tokens().room.rule_max_contrast + 0.01,
                "rule {:?} on {:?}",
                rule,
                bg.rgb
            );
        }
    }

    #[test]
    fn quantize_256_anchors() {
        let (idx, rgb) = quantize_256([0, 0, 0]);
        assert_eq!(idx, 16);
        assert_eq!(rgb, [0, 0, 0]);
        let (_i, realized) = quantize_256([250, 10, 10]);
        assert!(
            realized[0] > realized[1],
            "stays red-dominant after quantization"
        );
    }

    #[test]
    fn lightness_lerp_is_not_an_rgb_blend() {
        assert_eq!(tokens().motion.curve, "oklch_lightness");
        let from = [0x20, 0x20, 0x20];
        let to = [0xe0, 0xe0, 0xe0];
        let mid = lerp_oklch_lightness(from, to, 0.5);
        // A linear blend of the channels lands on 0x80. Perceptual lightness does not.
        assert_ne!(
            mid,
            [0x80, 0x80, 0x80],
            "OKLCH lightness midpoint is not the sRGB midpoint"
        );
        let start = lerp_oklch_lightness(from, to, -0.2);
        let at_zero = lerp_oklch_lightness(from, to, 0.0);
        assert_eq!(start, at_zero, "no undershoot");
        let over = lerp_oklch_lightness(from, to, 1.5);
        let end = lerp_oklch_lightness(from, to, 1.0);
        assert_eq!(over, end, "no overshoot past the end");
    }
}
