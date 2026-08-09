//! Album-art fetch + terminal rendering. Cover bytes come from the daemon's MPD
//! `albumart "<uri>" <offset>` command (binary framing: `size:`/`binary:`/raw
//! bytes/`OK`), which the client's text-only [`hypodj_client::mpd::MpdConn`] cannot
//! read - so we fetch on a DEDICATED short-lived connection (the daemon caches
//! decoded covers, so this is cheap and never desyncs the main session socket).
//!
//! Rendering: each terminal cell is split into two vertical pixels via the upper
//! half-block `U+2580`, `fg` = top pixel, `bg` = bottom pixel, so a `cols x rows`
//! cell area shows a `cols x (rows*2)` image. A small ordered (Bayer 4x4) dither is
//! applied per channel to break up banding on the coarse terminal grid - the
//! "dithering to make it look better" the layout calls for.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use image::RgbImage;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

const ART_TIMEOUT: Duration = Duration::from_secs(3);

/// How the art pane should be drawn.
///
/// `HYPODJ_ART_CELLS` picks one explicitly. The escape hatch predates sixel and its
/// reason still holds: a renderer can be right on this terminal and wrong on the next
/// one, and the user must be able to fix that without a rebuild. `sixel` forces images
/// even if the probe said no, which is what makes a terminal that answers DA1 badly
/// still usable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArtMode {
    Auto,
    Sixel,
    Sextant,
    Half,
}

/// Read the pinned art mode, per call, so it can be changed without a restart.
pub fn art_mode() -> ArtMode {
    match std::env::var("HYPODJ_ART_CELLS") {
        Ok(v) if v.eq_ignore_ascii_case("half") => ArtMode::Half,
        Ok(v) if v.eq_ignore_ascii_case("sextant") => ArtMode::Sextant,
        Ok(v) if v.eq_ignore_ascii_case("sixel") => ArtMode::Sixel,
        _ => ArtMode::Auto,
    }
}
/// Decoded-thumbnail edge (px). Fetch+decode once per track; the per-frame downscale
/// from this to the cell grid is cheap.
///
/// This is a hard FIDELITY CEILING, so it must stay above what the pane can actually
/// show: a sextant cell carries 2x3 subcells, so a 30x15 pane already wants 60x45 and a
/// large pane on a big terminal wants more. It now also has to clear a SIXEL pane at
/// native pixels, which wants roughly cell_px times the pane's cell size - a few
/// hundred pixels on an ordinary layout - so 256 was itself becoming the ceiling.
const THUMB: u32 = 512;

/// A decoded cover thumbnail, cached per track uri. Rendering downscales it to the
/// current cell area every frame (cheap); the expensive fetch+decode happens once.
pub struct AlbumArt {
    img: RgbImage,
    /// The last sixel payload, keyed by the pixel geometry it was encoded for.
    ///
    /// Encoding is the expensive part and the pane geometry rarely changes, so this
    /// turns a per-frame cost into a per-resize one. It also makes the payload STRING
    /// STABLE, which is what lets the frame diff decide when to re-send it.
    sixel: std::cell::RefCell<Option<(u32, u32, std::sync::Arc<str>)>>,
    /// The ranked cover palette, extracted once at decode time. Shared by the sigil
    /// (DECORATION) and the waveform (INFO) so the visual system reads from one source.
    pub palette: crate::album_color::Palette,
}

impl AlbumArt {
    /// Fetch + decode the cover for `uri`; `None` when there is no art or anything
    /// fails (a missing cover must never break the UI).
    pub fn load(host: &str, port: u16, uri: &str) -> Option<AlbumArt> {
        let bytes = fetch_albumart(host, port, uri)?;
        let img = image::load_from_memory(&bytes).ok()?;
        let thumb = img
            .resize_exact(THUMB, THUMB, image::imageops::FilterType::Triangle)
            .to_rgb8();
        let palette = crate::album_color::extract_palette(&thumb);
        Some(AlbumArt { img: thumb, palette, sixel: std::cell::RefCell::new(None) })
    }

    /// Render the art into `cols x rows` cells, using the SEXTANT renderer (2x3 subcells
    /// per cell) by default and the half-block renderer when asked.
    ///
    /// `HYPODJ_ART_CELLS=half` forces the old 1x2 rendering. That escape hatch exists
    /// because sextants are only guaranteed to look right where the terminal draws them
    /// itself (VTE does) or the font covers Symbols for Legacy Computing; a terminal with
    /// neither would show tofu, and the user must be able to fix that without a rebuild.
    pub fn lines(&self, cols: usize, rows: usize) -> Vec<Line<'static>> {
        if art_mode() == ArtMode::Half {
            render_lines(&self.img, cols, rows)
        } else {
            render_lines_sextant(&self.img, cols, rows)
        }
    }
}

/// Read one CRLF/LF-terminated line, stripping the terminator. EOF -> error.
fn read_line(r: &mut impl BufRead) -> std::io::Result<String> {
    let mut s = String::new();
    if r.read_line(&mut s)? == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof"));
    }
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    Ok(s)
}

/// MPD binary-safe quoting for the uri argument.
fn quote(uri: &str) -> String {
    format!("\"{}\"", uri.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Fetch the full cover for `uri` over a dedicated connection, looping the
/// `albumart` offset chunks until the reported total is assembled. `None` on no
/// art / ACK / any IO error.
fn fetch_albumart(host: &str, port: u16, uri: &str) -> Option<Vec<u8>> {
    let stream = TcpStream::connect((host, port)).ok()?;
    stream.set_read_timeout(Some(ART_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(ART_TIMEOUT)).ok()?;
    let mut w = stream.try_clone().ok()?;
    let mut r = BufReader::new(stream);
    if !read_line(&mut r).ok()?.starts_with("OK MPD") {
        return None;
    }
    let mut all: Vec<u8> = Vec::new();
    loop {
        w.write_all(format!("albumart {} {}\n", quote(uri), all.len()).as_bytes())
            .ok()?;
        w.flush().ok()?;
        let mut total = 0usize;
        let mut chunk = 0usize;
        loop {
            let line = read_line(&mut r).ok()?;
            if let Some(v) = line.strip_prefix("size: ") {
                total = v.trim().parse().ok()?;
            } else if let Some(v) = line.strip_prefix("binary: ") {
                let n: usize = v.trim().parse().ok()?;
                // Sanity clamp: never trust a wild length into an allocation.
                if n > 8 * 1024 * 1024 {
                    return None;
                }
                let mut buf = vec![0u8; n];
                r.read_exact(&mut buf).ok()?;
                all.extend_from_slice(&buf);
                chunk = n;
                // The raw payload is followed by a lone `\n` then `OK`; the empty
                // line is consumed (and ignored) by the next read_line.
            } else if line == "OK" {
                break;
            } else if line.starts_with("ACK") {
                return if all.is_empty() { None } else { Some(all) };
            }
        }
        if total == 0 || chunk == 0 || all.len() >= total {
            break;
        }
    }
    if all.is_empty() {
        None
    } else {
        Some(all)
    }
}

impl AlbumArt {
    /// A solid-colour cover, for tests that need a real AlbumArt without a daemon.
    #[cfg(test)]
    pub fn for_test_solid(c: [u8; 3]) -> AlbumArt {
        let img = RgbImage::from_fn(THUMB, THUMB, |_, _| image::Rgb(c));
        let palette = crate::album_color::extract_palette(&img);
        AlbumArt { img, palette, sixel: std::cell::RefCell::new(None) }
    }

    /// The sixel payload for this cover at exactly `px_w x px_h` pixels.
    ///
    /// Cached by geometry. Returning the SAME `Arc<str>` for an unchanged cover and
    /// pane is the whole mechanism behind the redraw policy: the payload is the symbol
    /// of one cell, so an identical string means the frame diff emits nothing at all.
    pub fn sixel(&self, px_w: u32, px_h: u32) -> std::sync::Arc<str> {
        if let Some((w, h, payload)) = self.sixel.borrow().as_ref() {
            if *w == px_w && *h == px_h {
                return payload.clone();
            }
        }
        let payload: std::sync::Arc<str> = encode_sixel(&self.img, px_w, px_h).into();
        *self.sixel.borrow_mut() = Some((px_w, px_h, payload.clone()));
        payload
    }
}

/// Encode `img` as a SIXEL payload sized exactly `px_w x px_h`.
///
/// Sixel is PALETTE INDEXED and carries colour components as percentages, so the wire
/// format has roughly 101 levels per channel however many registers we use. Both limits
/// band a photographic cover, so the image is dithered with the same Bayer matrix the
/// cell renderers use BEFORE it is quantized - dithering after quantizing would do
/// nothing, since the damage is already done by then.
///
/// 64 registers rather than the 1024 the terminal offers: the payload is re-sent
/// whenever the cover or the pane geometry changes, so its size is a bandwidth cost we
/// pay repeatedly, and 64 keeps a 300 px cover near 70 KB against roughly 130 KB at 256
/// without banding that shows on real covers.
fn encode_sixel(img: &RgbImage, px_w: u32, px_h: u32) -> String {
    if px_w == 0 || px_h == 0 {
        return String::new();
    }

    let scaled = image::imageops::resize(img, px_w, px_h, image::imageops::FilterType::Triangle);

    // The palette is built from the REAL pixels, undithered. Dithering first would
    // spend the register budget on the dither noise: median_cut would cluster around
    // the bias values, adjacent pixels in a flat region would land on different
    // registers, and the result is both a worse palette and a payload with no runs
    // left to compress.
    let pixels: Vec<crate::album_color::Rgb> = scaled.pixels().map(|p| p.0).collect();

    let palette = crate::album_color::median_cut(pixels.clone(), SIXEL_REGISTERS);
    if palette.is_empty() {
        return String::new();
    }

    // Dither at MATCH time instead: the bias nudges each pixel before it picks a
    // register, so a gradient crosses register boundaries at different points per
    // pixel (which is what breaks banding) while a flat region still maps every pixel
    // to the same register (which is what keeps the runs).
    let mut indexed: Vec<u8> = Vec::with_capacity(pixels.len());
    for y in 0..px_h {
        for x in 0..px_w {
            let px = pixels[(y * px_w + x) as usize];
            let bias = BAYER4[(y % 4) as usize][(x % 4) as usize] - 8;
            let biased = [
                (px[0] as i16 + bias).clamp(0, 255) as u8,
                (px[1] as i16 + bias).clamp(0, 255) as u8,
                (px[2] as i16 + bias).clamp(0, 255) as u8,
            ];
            indexed.push(nearest_register(&palette, biased));
        }
    }

    let mut out = String::with_capacity((px_w as usize * px_h as usize) / 4 + 1024);

    // Three parameters, never a fourth: P1 aspect, P2 background select, P3 grid.
    // P2 = 0 means unset pixels take the background rather than staying transparent,
    // which is what pins the image's extent to the raster attribute below.
    out.push_str("\x1bP0;0;0q");
    out.push_str(&format!("\"1;1;{px_w};{px_h}"));

    for (i, c) in palette.iter().enumerate() {
        // Components are percentages, not 0..255.
        let pc = |v: u8| (v as u32 * 100 + 127) / 255;
        out.push_str(&format!("#{};2;{};{};{}", i, pc(c[0]), pc(c[1]), pc(c[2])));
    }

    let bands = px_h.div_ceil(6);
    let mut row_bits = vec![0u8; px_w as usize];

    for band in 0..bands {
        let top = band * 6;

        for (reg, _) in palette.iter().enumerate() {
            row_bits.iter_mut().for_each(|b| *b = 0);
            let mut any = false;

            for sub in 0..6u32 {
                let y = top + sub;
                if y >= px_h {
                    break;
                }
                let row = (y * px_w) as usize;
                for x in 0..px_w as usize {
                    if indexed[row + x] as usize == reg {
                        row_bits[x] |= 1 << sub;
                        any = true;
                    }
                }
            }

            if !any {
                continue;
            }

            out.push_str(&format!("#{reg}"));

            // Run-length encode: DECGRI pays for itself from four repeats up, and a
            // flat-coloured cover is mostly long runs.
            let mut x = 0usize;
            while x < row_bits.len() {
                let bits = row_bits[x];
                let mut run = 1usize;
                while x + run < row_bits.len() && row_bits[x + run] == bits {
                    run += 1;
                }
                let ch = (0x3f + bits) as char;
                if run >= 4 {
                    out.push_str(&format!("!{run}{ch}"));
                } else {
                    for _ in 0..run {
                        out.push(ch);
                    }
                }
                x += run;
            }

            out.push('$');
        }

        if band + 1 < bands {
            out.push('-');
        }
    }

    out.push_str("\x1b\\");
    out
}

/// Colour registers used by the sixel encoder. See [`encode_sixel`].
const SIXEL_REGISTERS: usize = 64;

/// Index of the palette entry nearest `px`, by squared distance.
fn nearest_register(palette: &[crate::album_color::Rgb], px: crate::album_color::Rgb) -> u8 {
    let mut best = 0usize;
    let mut best_d = u32::MAX;
    for (i, c) in palette.iter().enumerate() {
        let dr = c[0] as i32 - px[0] as i32;
        let dg = c[1] as i32 - px[1] as i32;
        let db = c[2] as i32 - px[2] as i32;
        let d = (dr * dr + dg * dg + db * db) as u32;
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best as u8
}

/// Bayer 4x4 ordered-dither matrix, centered to roughly [-8, +7].
const BAYER4: [[i16; 4]; 4] = [
    [0, 8, 2, 10],
    [12, 4, 14, 6],
    [3, 11, 1, 9],
    [15, 7, 13, 5],
];

/// The four 2x3 bit patterns that already have their own characters, so the Symbols
/// for Legacy Computing sextant block skips them: blank, left half, right half, full.
/// Bit i is subcell i in reading order (0 = top-left, 5 = bottom-right), so the left
/// column is bits 0/2/4 = 21 and the right column is bits 1/3/5 = 42.
const SEXTANT_SPECIAL: [(u8, char); 4] =
    [(0, ' '), (21, '\u{258C}'), (42, '\u{2590}'), (63, '\u{2588}')];

/// The character for one 2x3 sextant bit pattern (bit set = foreground).
///
/// `U+1FB00..=U+1FB3B` holds the 60 patterns that do NOT already exist elsewhere, in
/// ascending pattern order, so the codepoint is the base plus the pattern's rank among
/// the non-special values. Verified exhaustively: 60 characters ending exactly at
/// `U+1FB3B`.
///
/// VTE draws this whole range ITSELF (`minifont.cc`, dispatched before any font
/// lookup), so it renders correctly in GNOME Console regardless of the installed font -
/// which is the case that matters here.
fn sextant_char(pattern: u8) -> char {
    if let Some((_, c)) = SEXTANT_SPECIAL.iter().find(|(p, _)| *p == pattern) {
        return *c;
    }
    let rank = (0..pattern).filter(|p| !SEXTANT_SPECIAL.iter().any(|(s, _)| s == p)).count();
    char::from_u32(0x1FB00 + rank as u32).unwrap_or('\u{2588}')
}

/// Perceptual weight of a colour, for splitting a cell's subcells into a foreground and
/// a background group. Integer Rec.601 luma - exact, and no float ordering hazard.
fn luma(c: [u8; 3]) -> u32 {
    299 * c[0] as u32 + 587 * c[1] as u32 + 114 * c[2] as u32
}

/// Map an image into `cols x rows` SEXTANT cells: each cell carries a 2x3 subcell grid,
/// so the pane shows `cols*2 x rows*3` pixels - THREE times the half-block renderer's
/// `cols x rows*2` for the same area.
///
/// A cell can still only show two colours, so the subcells are split at their mean luma
/// into a foreground and a background group, each averaged; the resulting bit pattern
/// picks the glyph. That two-colour ceiling - not the subcell count - is the real limit,
/// which is why going further (2x4 octants) measures as a rounding error while this step
/// does not. Pure + unit-tested.
fn render_lines_sextant(img: &RgbImage, cols: usize, rows: usize) -> Vec<Line<'static>> {
    let (iw, ih) = img.dimensions();
    if cols == 0 || rows == 0 || iw == 0 || ih == 0 {
        return Vec::new();
    }
    let (pw, ph) = (cols * 2, rows * 3);
    let sample = |px: usize, py: usize| -> [u8; 3] {
        let sx = (px * iw as usize / pw).min(iw as usize - 1);
        let sy = (py * ih as usize / ph).min(ih as usize - 1);
        let p = img.get_pixel(sx as u32, sy as u32);
        [p[0], p[1], p[2]]
    };
    let mut lines = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut spans = Vec::with_capacity(cols);
        for col in 0..cols {
            // Reading order, matching the sextant bit numbering.
            let mut sub = [[0u8; 3]; 6];
            for i in 0..6 {
                sub[i] = sample(col * 2 + i % 2, row * 3 + i / 2);
            }
            let mean = sub.iter().map(|c| luma(*c)).sum::<u32>() / 6;
            let mut pattern = 0u8;
            let (mut fg_acc, mut bg_acc) = ([0u32; 3], [0u32; 3]);
            let (mut fg_n, mut bg_n) = (0u32, 0u32);
            for (i, c) in sub.iter().enumerate() {
                if luma(*c) >= mean {
                    pattern |= 1 << i;
                    for k in 0..3 {
                        fg_acc[k] += c[k] as u32;
                    }
                    fg_n += 1;
                } else {
                    for k in 0..3 {
                        bg_acc[k] += c[k] as u32;
                    }
                    bg_n += 1;
                }
            }
            // A flat cell puts every subcell in the foreground group; then the glyph is
            // the full block and the background colour is never shown, so reuse the
            // foreground rather than inventing one.
            let avg = |acc: [u32; 3], n: u32| -> Color {
                if n == 0 {
                    return Color::Rgb(0, 0, 0);
                }
                Color::Rgb((acc[0] / n) as u8, (acc[1] / n) as u8, (acc[2] / n) as u8)
            };
            let fg = avg(fg_acc, fg_n);
            let bg = if bg_n == 0 { fg } else { avg(bg_acc, bg_n) };
            let ch = sextant_char(pattern);
            let mut buf = [0u8; 4];
            spans.push(Span::styled(
                ch.encode_utf8(&mut buf).to_string(),
                Style::default().fg(fg).bg(bg),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Map an image into `cols x rows` upper-half-block cells (top pixel = fg, bottom =
/// bg), nearest-neighbour sampled and ordered-dithered. Pure + unit-tested.
fn render_lines(img: &RgbImage, cols: usize, rows: usize) -> Vec<Line<'static>> {
    let (iw, ih) = img.dimensions();
    if cols == 0 || rows == 0 || iw == 0 || ih == 0 {
        return Vec::new();
    }
    let pw = cols;
    let ph = rows * 2;
    let sample = |px: usize, py: usize| -> [u8; 3] {
        let sx = (px * iw as usize / pw).min(iw as usize - 1);
        let sy = (py * ih as usize / ph).min(ih as usize - 1);
        let p = img.get_pixel(sx as u32, sy as u32);
        [p[0], p[1], p[2]]
    };
    let dither = |c: [u8; 3], x: usize, y: usize| -> Color {
        let t = BAYER4[y % 4][x % 4] - 8;
        let ch = |v: u8| (v as i16 + t).clamp(0, 255) as u8;
        Color::Rgb(ch(c[0]), ch(c[1]), ch(c[2]))
    };
    let mut lines = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut spans = Vec::with_capacity(cols);
        for col in 0..cols {
            let top = sample(col, row * 2);
            let bot = sample(col, row * 2 + 1);
            let fg = dither(top, col, row * 2);
            let bg = dither(bot, col, row * 2 + 1);
            spans.push(Span::styled("\u{2580}", Style::default().fg(fg).bg(bg)));
        }
        lines.push(Line::from(spans));
    }
    lines
}

#[cfg(test)]
impl AlbumArt {
    /// A test-only constructor so ui.rs render tests can exercise the album-swatch
    /// waveform coloring and the now-playing art pane without a live cover fetch. The
    /// image is a 1x1 stub; the palette is read by the waveform styling and the stub
    /// still renders as half-block cells in the art pane.
    pub fn for_test(palette: crate::album_color::Palette) -> AlbumArt {
        AlbumArt { img: RgbImage::new(1, 1), palette, sixel: std::cell::RefCell::new(None) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sextant_block_is_exactly_the_60_non_special_patterns() {
        // The mapping rule is "rank among the patterns that do not already have a
        // character", so it is only correct if it lands exactly on U+1FB00..=U+1FB3B.
        // Pin both ends and the count, or a silent off-by-one shifts every glyph.
        let mut seen = std::collections::BTreeSet::new();
        for p in 0u8..64 {
            let c = sextant_char(p);
            match p {
                0 => assert_eq!(c, ' '),
                21 => assert_eq!(c, '\u{258C}', "left half"),
                42 => assert_eq!(c, '\u{2590}', "right half"),
                63 => assert_eq!(c, '\u{2588}', "full block"),
                _ => {
                    let cp = c as u32;
                    assert!(
                        (0x1FB00..=0x1FB3B).contains(&cp),
                        "pattern {p} -> {cp:#x} outside the sextant block"
                    );
                    assert!(seen.insert(cp), "pattern {p} duplicated codepoint {cp:#x}");
                }
            }
        }
        assert_eq!(seen.len(), 60, "60 sextants in the block");
        assert_eq!(*seen.iter().next().unwrap(), 0x1FB00);
        assert_eq!(*seen.iter().next_back().unwrap(), 0x1FB3B);
    }

    #[test]
    fn sextant_bit_order_is_reading_order() {
        // Bit 0 is top-left and bit 5 is bottom-right, so the left column (0/2/4) must
        // be the left-half block and the right column (1/3/5) the right-half block. If
        // the bit order were transposed these two would swap and every image would be
        // mirrored diagonally.
        assert_eq!(sextant_char(0b000001 | 0b000100 | 0b010000), '\u{258C}');
        assert_eq!(sextant_char(0b000010 | 0b001000 | 0b100000), '\u{2590}');
    }

    #[test]
    fn render_lines_sextant_shape_and_flat_cell() {
        // A solid image has every subcell at the mean, so all six land in the foreground
        // group: the glyph is the full block and bg is reused rather than invented.
        let img = RgbImage::from_pixel(8, 8, image::Rgb([120, 60, 200]));
        let lines = render_lines_sextant(&img, 3, 2);
        assert_eq!(lines.len(), 2, "rows");
        for l in &lines {
            assert_eq!(l.spans.len(), 3, "cols");
            for s in &l.spans {
                assert_eq!(s.content.as_ref(), "\u{2588}", "flat cell is the full block");
                assert_eq!(s.style.fg, s.style.bg, "no invented background on a flat cell");
            }
        }
    }

    #[test]
    fn render_lines_sextant_splits_a_two_tone_cell() {
        // Top half black, bottom half white, one cell: the split must produce a real
        // two-colour cell with a partial glyph, not a flat block.
        let mut img = RgbImage::from_pixel(2, 3, image::Rgb([0, 0, 0]));
        img.put_pixel(0, 2, image::Rgb([255, 255, 255]));
        img.put_pixel(1, 2, image::Rgb([255, 255, 255]));
        let lines = render_lines_sextant(&img, 1, 1);
        let s = &lines[0].spans[0];
        assert_ne!(s.style.fg, s.style.bg, "two tones must yield two colours");
        assert_ne!(s.content.as_ref(), "\u{2588}", "not a flat block");
        assert_ne!(s.content.as_ref(), " ", "not blank");
    }

    #[test]
    fn render_lines_sextant_degenerate_is_empty() {
        let img = RgbImage::from_pixel(2, 2, image::Rgb([0, 0, 0]));
        assert!(render_lines_sextant(&img, 0, 4).is_empty());
        assert!(render_lines_sextant(&img, 4, 0).is_empty());
    }

    #[test]
    fn render_lines_shape_and_halfblock() {
        // A 4x4 solid image -> 3x2 cells = 3 spans/line, 2 lines, all upper-half.
        let img = RgbImage::from_pixel(4, 4, image::Rgb([120, 60, 200]));
        let lines = render_lines(&img, 3, 2);
        assert_eq!(lines.len(), 2, "rows");
        for l in &lines {
            assert_eq!(l.spans.len(), 3, "cols");
            for s in &l.spans {
                assert_eq!(s.content.as_ref(), "\u{2580}", "upper half block");
                assert!(s.style.fg.is_some() && s.style.bg.is_some(), "fg=top bg=bottom");
            }
        }
    }

    #[test]
    fn render_lines_degenerate_is_empty() {
        let img = RgbImage::from_pixel(2, 2, image::Rgb([0, 0, 0]));
        assert!(render_lines(&img, 0, 4).is_empty());
        assert!(render_lines(&img, 4, 0).is_empty());
    }

    #[test]
    fn quote_escapes() {
        assert_eq!(quote("song/1"), "\"song/1\"");
        assert_eq!(quote(r#"a"b\c"#), "\"a\\\"b\\\\c\"");
    }
}

#[cfg(test)]
mod sixel_tests {
    use super::*;

    /// A flat image of one colour, plus a checkerboard, are the two shapes that pin
    /// the wire format: one exercises run-length coding, the other multiple registers.
    fn flat(w: u32, h: u32, c: [u8; 3]) -> RgbImage {
        RgbImage::from_fn(w, h, |_, _| image::Rgb(c))
    }

    #[test]
    fn payload_is_a_well_formed_dcs() {
        let out = encode_sixel(&flat(12, 12, [200, 40, 40]), 12, 12);

        assert!(out.starts_with("\x1bP"), "must open with a DCS introducer");
        assert!(out.ends_with("\x1b\\"), "must close with ST, ESC + ONE backslash");

        // Exactly three parameters before the q, never a fourth: a fourth parameter
        // is a different command shape and terminals disagree about it.
        let head = &out[2..out.find('q').expect("DCS needs a q")];
        assert_eq!(head.split(';').count(), 3, "header was {head:?}");
    }

    #[test]
    fn raster_attribute_pins_the_extent() {
        // Without this the image's height is whatever the data happens to fill, and a
        // dark cover would silently occupy fewer rows than the pane reserved.
        let out = encode_sixel(&flat(12, 18, [10, 10, 10]), 12, 18);
        assert!(out.contains("\"1;1;12;18"), "missing raster attributes");
    }

    #[test]
    fn data_bytes_stay_in_the_printable_range() {
        let img = RgbImage::from_fn(16, 12, |x, y| {
            image::Rgb(if (x / 2 + y / 2) % 2 == 0 { [250, 250, 250] } else { [5, 5, 5] })
        });
        let out = encode_sixel(&img, 16, 12);

        // Everything after the header must be either a command byte or a sixel data
        // byte in 0x3f..=0x7e. Anything outside that range is a corrupt payload that
        // some terminals render as garbage rather than refusing.
        let body = &out[out.find('q').unwrap() + 1..out.len() - 2];
        for ch in body.chars() {
            let ok = matches!(ch, '#' | '$' | '-' | '!' | '"' | ';' | '0'..='9')
                || ('\u{3f}'..='\u{7e}').contains(&ch);
            assert!(ok, "byte {ch:?} is outside the sixel alphabet");
        }
    }

    #[test]
    fn a_two_colour_image_defines_at_least_two_registers() {
        let img = RgbImage::from_fn(16, 12, |x, _| {
            image::Rgb(if x < 8 { [250, 10, 10] } else { [10, 10, 250] })
        });
        let out = encode_sixel(&img, 16, 12);
        assert!(out.matches("#").count() >= 2, "expected several palette writes");
    }

    #[test]
    fn band_count_covers_every_row() {
        // 18 rows is exactly 3 bands; 20 rows needs 4, and the partial band must still
        // be emitted or the bottom of the cover is cut off.
        let three = encode_sixel(&flat(6, 18, [90, 90, 90]), 6, 18);
        let four = encode_sixel(&flat(6, 20, [90, 90, 90]), 6, 20);
        assert_eq!(three.matches('-').count(), 2, "3 bands means 2 separators");
        assert_eq!(four.matches('-').count(), 3, "4 bands means 3 separators");
    }

    #[test]
    fn run_length_coding_keeps_a_cover_affordable() {
        // The payload is re-sent on every cover or geometry change, so its size is a
        // recurring cost. A flat 300x300 must compress hard.
        let out = encode_sixel(&flat(300, 300, [123, 45, 67]), 300, 300);
        assert!(out.contains('!'), "expected DECGRI runs");
        assert!(out.len() < 120_000, "payload was {} bytes", out.len());
    }

    #[test]
    fn degenerate_geometry_yields_nothing_rather_than_a_broken_dcs() {
        assert!(encode_sixel(&flat(4, 4, [1, 2, 3]), 0, 10).is_empty());
        assert!(encode_sixel(&flat(4, 4, [1, 2, 3]), 10, 0).is_empty());
    }
}
