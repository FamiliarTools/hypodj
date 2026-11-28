//! Thin TuiState -> Frame render. No state mutation beyond a scratch ListState for
//! the queue highlight; all decisions come from TuiState.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;

use hypodj_style::ratatui::Emphasis;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use hypodj_client::model::{
    server_words, store_badge, store_badge_short, FieldState, HintKind, NowPlaying,
};

use crate::keymap;
use crate::find::{FindRow, Focus, Phase};
use crate::state::{album_mark, queue_mark_glyph, Browse, Mode, Screen, TuiState};

/// The stretch of ambient wave the idle bottom bar keeps for itself, whatever else
/// wants the row. The store badge is drawn only when it fits BESIDE this many cells,
/// so a long mirror line can never turn the quietest row on the screen into a status
/// bar.
const MIN_WAVE_CELLS: usize = 16;

/// Columns at which the deck stops being a strip on top and becomes a rail on the
/// left with a real cover: below this a 28-column rail costs the list its titles.
const RAIL_MIN_W: u16 = 110;
/// The rail's width when the terminal is wide enough for one.
const RAIL_W: u16 = 34;

/// The regions of one frame, decided once so every drawer, popup and test agrees on
/// where the deck is and where the body is.
pub(crate) struct Regions {
    /// The now-playing instrument: a strip across the top, or a rail on the left.
    pub deck: Rect,
    /// Whether `deck` is the wide-terminal rail.
    pub rail: bool,
    /// The view line: view names on the left, the server word on the right.
    pub views: Rect,
    /// The one body view, and every drawer and popup that opens over it.
    pub body: Rect,
    /// The bottom line: the `:` input, a status sentence, or the idle wave.
    pub bar: Rect,
}

/// THE DECK GEOMETRY. One instrument that no overlay ever covers, one view line, one
/// body that swaps, one bottom line. Pure, so the layout is testable without drawing.
pub(crate) fn regions(area: Rect, state: &TuiState) -> Regions {
    let rows = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(area);
    let (main, bar) = (rows[0], rows[1]);
    if area.width >= RAIL_MIN_W {
        let cols = Layout::horizontal([Constraint::Length(RAIL_W), Constraint::Min(20)]).split(main);
        let right = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(cols[1]);
        return Regions { deck: cols[0], rail: true, views: right[0], body: right[1], bar };
    }
    // Six rows normally; more only while the body would still keep BODY_KEEP rows, so a
    // tall terminal shows every held plan and a short one counts the rest.
    const BODY_KEEP: u16 = 10;
    let cap = main.height.saturating_sub(1 + BODY_KEEP).max(6) as usize;
    let strip_h = deck_lines(state, area.width.saturating_sub(strip_cover_cols(state)) as usize, false, None)
        .len()
        .clamp(3, cap) as u16;
    let v = Layout::vertical([
        Constraint::Length(strip_h),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(main);
    Regions { deck: v[0], rail: false, views: v[1], body: v[2], bar }
}

/// Draw the Deck: the now-playing instrument (a strip, or a rail on a wide terminal),
/// the view line, ONE body view, and the bottom line. Info, help, heard, the journal,
/// the row menu and the confirm all open in the BODY, so the title, the transport and
/// the next song stay in sight while the listener digs.
pub fn render(f: &mut Frame, state: &TuiState) {
    // Decided BEFORE anything is drawn, because the art is painted early in this
    // function and the generation has to be right by the time it is: an overlay that
    // closed since the last frame must re-send the image on THIS frame, not on the one
    // after it. See `TuiState::sixel_gen`. The overlays now open in the body, away from
    // the deck's cover, but the pin stays: a resize can still bring them together, and
    // the cost of keeping it is one image re-send on close.
    let overlay_drawn = state.menu.is_some()
        || state.help_open
        || state.heard_open
        || state.card.is_some()
        || (state.mode == Mode::Confirm && state.screen != Screen::Dj);
    if overlay_drawn {
        state.sixel_covered.set(true);
    } else if state.sixel_covered.replace(false) {
        state.sixel_gen.set(!state.sixel_gen.get());
    }
    let r = regions(f.area(), state);
    if r.rail {
        render_rail(f, r.deck, state);
    } else {
        render_strip(f, r.deck, state);
    }
    render_tabs(f, r.views, state.screen, state.now.server.as_deref());
    let body = r.body;
    match state.screen {
        Screen::Queue => render_queue(f, body, state),
        Screen::Find => render_find(f, body, state),
        Screen::Albums if state.albums.is_album_grid() && grid_fits(body) => {
            render_album_grid(f, body, state)
        }
        Screen::Albums => {
            state.albums.grid_cols.set(0);
            render_browse(f, body, &state.albums, state, true)
        }
        Screen::Playlists => render_browse(f, body, &state.playlists, state, false),
        // The DJ view is the whole body now: the queue it used to sit beside is one
        // Tab away, and the next song is already on the deck.
        Screen::Dj => render_dj(f, body, state),
    }
    // A status sentence longer than the bar BORROWS the body's last row rather than
    // growing the layout, so nothing moves. Two rows hold a mark sentence WITH its tape
    // clause; at 60 columns the clause is the whole tail.
    let bar = r.bar;
    let status_area = match &state.status_msg {
        Some(msg)
            if state.mode == Mode::Normal
                && status_lines(msg, bar.width as usize, 2).len() > 1 =>
        {
            Rect { y: bar.y.saturating_sub(1), height: 2, ..bar }.intersection(f.area())
        }
        _ => bar,
    };
    if status_area.height > 1 {
        f.render_widget(Clear, status_area);
    }
    render_command(f, status_area, state);
    // Confirm is a small popup in the body so the bottom bar never grows. On the DJ
    // screen the confirm is INLINE in the chat scrollback (pushed by enter_confirm).
    if state.mode == Mode::Confirm && state.screen != Screen::Dj {
        render_confirm_popup(f, body, state);
    }
    // The drawers share the body: heard / journal, then the info card, then the row
    // menu, then help, in the order their keys are intercepted in `key_normal`.
    if state.heard_open {
        render_heard_overlay(f, body, state);
    }
    if state.card.is_some() {
        render_info_card(f, body, state);
    }
    if let Some(menu) = &state.menu {
        render_menu(f, body, menu, state);
    }
    if state.help_open {
        render_help_overlay(f, body, state);
    }
}

/// The geometry every overlay here shares: a popup sized to its CONTENT (plus the
/// border ring and a one-column side pad), clamped inside `region` and centered in it.
/// Extracted so the help, confirm and menu popups cannot drift into three subtly
/// different boxes - and so a short terminal clamps them all the same way.
fn centered_popup(region: Rect, lines: &[Line]) -> Rect {
    let content_h = lines.len() as u16 + 2;
    let content_w = lines
        .iter()
        .map(|l| l.width() as u16)
        .max()
        .unwrap_or(0)
        .saturating_add(4);
    let w = content_w.min(region.width).max(1);
    let h = content_h.min(region.height).max(1);
    let x = region.x + (region.width.saturating_sub(w)) / 2;
    let y = region.y + (region.height.saturating_sub(h)) / 2;
    Rect { x, y, width: w, height: h }
}

/// The row CONTEXT MENU popup. Rendered from the EXACT `Vec<MenuRow>` dispatch
/// consumes, so it is its own help and there is no second table to drift: a blocked
/// row is drawn DIM (its reason goes to the status line when picked, where there is
/// room for a sentence), and the heading names the target plus where it came from -
/// the same song reached from the queue and from a Find hit offers different rows, so
/// the origin has to be visible.
fn render_menu(f: &mut Frame, region: Rect, menu: &crate::menu::Menu, state: &TuiState) {
    use crate::menu::Avail;
    // A legible fg for the detected background, the same INFO policy the help overlay
    // uses, so the two modals read as one visual system on light and dark terminals.
    let fg = hypodj_style::ratatui::info_color(hypodj_style::NEUTRAL_VIBRANT, state.term_bg, state.truecolor);
    let base = Style::default().fg(fg);
    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(menu.target.label.clone(), base.bold()),
        Span::styled(
            format!("  ({})", menu.target.origin.word()),
            base.quiet(),
        ),
    ])];
    for (i, row) in menu.rows.iter().enumerate() {
        let blocked = matches!(row.avail, Avail::No(_));
        let mut style = if blocked { base.quiet() } else { base };
        if i == menu.selected {
            style = style.reverse();
        }
        lines.push(Line::from(Span::styled(
            format!(" {}  {} ", row.item.hotkey(), row.label),
            style,
        )));
    }
    let popup = centered_popup(region, &lines);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(base)
        .title(Span::styled("Menu", base.bold()));
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block).style(base), popup);
}

/// Wrap `msg` to `width` columns, at most `max_rows` rows, marking a truncation IN the
/// text rather than letting the terminal clip it silently.
///
/// The silent clip is the bug this exists to close: the daemon appends what a `mark`
/// kept - the duration and the cut label - to the END of its sentence, so a single 60
/// column row drops exactly the part that says audio was captured at all. A sentence cut
/// short with no marker reads as a complete sentence.
///
/// Breaks at the last space that fits; a run with no space in it is hard-split rather
/// than dropped. It slices the ORIGINAL string rather than rebuilding it from words, so
/// the daemon's own double-space column separators survive inside a row - they are how a
/// `heard` line separates its time, its title and its url.
///
/// Pure and total: any width, any row count, no panic on a multibyte boundary (every cut
/// walks `char_indices`).
fn status_lines(msg: &str, width: usize, max_rows: usize) -> Vec<String> {
    let width = width.max(1);
    let max_rows = max_rows.max(1);
    let flat = msg.replace('\n', " ");
    let mut rows: Vec<String> = Vec::new();
    let mut rest: &str = &flat;
    loop {
        if rest.chars().count() <= width {
            rows.push(rest.trim_end().to_string());
            break;
        }
        // The byte offset just past the `width`th char; `rest` is longer, so it exists.
        let cut = rest.char_indices().nth(width).map(|(i, _)| i).unwrap_or(rest.len());
        let (take, skip) = match rest[..cut].rfind(' ') {
            Some(i) if i > 0 => (i, i + 1),
            // No space to break on (a url, one long word): split on the char boundary
            // and lose nothing.
            _ => (cut, cut),
        };
        rows.push(rest[..take].trim_end().to_string());
        rest = rest[skip..].trim_start_matches(' ');
        if rest.is_empty() {
            break;
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    if rows.len() > max_rows {
        rows.truncate(max_rows);
        // The marker is only affordable on a row wide enough to hold it; below that a
        // clip is all there is, and inventing three dots would eat the text instead.
        if width >= 3 {
            if let Some(last) = rows.last_mut() {
                while last.chars().count() + 3 > width && !last.is_empty() {
                    last.pop();
                }
                last.push_str("...");
            }
        }
    }
    rows
}

/// Wrap a reply's parts with the same breaks as [`status_lines`], keeping each
/// part's style across the wrap. A later row is used when the area has one.
fn styled_status_lines(
    parts: &[hypodj_style::Spoken],
    width: usize,
    max_rows: usize,
    bg: hypodj_style::TermBg,
    truecolor: bool,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let max_rows = max_rows.max(1);
    let mut cells: Vec<(char, Style)> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            for ch in "; ".chars() {
                cells.push((ch, Style::default()));
            }
        }
        let style = hypodj_style::ratatui::style_for_state(part.state, bg, truecolor);
        for ch in part.text().replace('\n', " ").chars() {
            cells.push((ch, style));
        }
    }
    if cells.is_empty() {
        return vec![Line::from("")];
    }
    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut rest = cells.as_slice();
    loop {
        if rest.len() <= width {
            rows.push(trim_styled_end(rest));
            break;
        }
        let window = &rest[..width];
        let (take, skip) = match window.iter().rposition(|(ch, _)| *ch == ' ') {
            Some(i) if i > 0 => (i, i + 1),
            _ => (width, width),
        };
        rows.push(trim_styled_end(&rest[..take]));
        rest = &rest[skip..];
        while rest.first().is_some_and(|(ch, _)| *ch == ' ') {
            rest = &rest[1..];
        }
        if rest.is_empty() {
            break;
        }
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    if rows.len() > max_rows {
        rows.truncate(max_rows);
        if width >= 3 {
            if let Some(last) = rows.last_mut() {
                while last.len() + 3 > width && !last.is_empty() {
                    last.pop();
                }
                let style = last.last().map(|(_, s)| *s).unwrap_or_default();
                for ch in "...".chars() {
                    last.push((ch, style));
                }
            }
        }
    }
    rows.into_iter().map(line_of_styled).collect()
}

fn trim_styled_end(row: &[(char, Style)]) -> Vec<(char, Style)> {
    let end = row.iter().rposition(|(ch, _)| *ch != ' ').map(|i| i + 1).unwrap_or(0);
    row[..end].to_vec()
}

fn line_of_styled(row: Vec<(char, Style)>) -> Line<'static> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut style = Style::default();
    for (ch, st) in row {
        if buf.is_empty() {
            style = st;
        }
        if st == style {
            buf.push(ch);
        } else {
            spans.push(Span::styled(std::mem::take(&mut buf), style));
            style = st;
            buf.push(ch);
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    Line::from(spans)
}

/// The heard/tape read-back overlay: the daemon's own `heard` render, whole, in a
/// scrollable modal.
///
/// It shows what the daemon says and interprets NOTHING. The render lives daemon-side
/// because only the daemon can join a ledger row to the audio still on disk beside it,
/// and re-phrasing any of it here is how the two surfaces come to disagree about what a
/// press kept. Every line is WRAPPED rather than clipped: a marked row ends in its
/// `[tape <n>: ...]` annotation, which is precisely what a horizontal clip removes.
fn render_heard_overlay(f: &mut Frame, region: Rect, state: &TuiState) {
    let fg = hypodj_style::ratatui::info_color(hypodj_style::NEUTRAL_VIBRANT, state.term_bg, state.truecolor);
    let base = Style::default().fg(fg);
    let head = base.bold();

    // The WHOLE frame, unlike the help overlay's content-sized box: a heard line carries
    // a time, a title, a url and its tape annotation, so every column is worth having -
    // and a margin would leave a ring of the jukebox underneath showing through as
    // fragments of borders, which reads as damage rather than as depth.
    let (w, h) = (region.width.max(1), region.height.max(1));
    let popup = Rect { x: region.x, y: region.y, width: w, height: h };
    // Two columns are reserved for the continuation indent so an indented wrap can never
    // push its own tail off the right edge - which would clip exactly the `[tape ...]`
    // annotation this panel exists to show.
    let inner_w = w.saturating_sub(4).max(1) as usize;
    let inner_h = h.saturating_sub(2);

    let mut lines: Vec<Line> = Vec::new();
    for l in &state.heard_lines {
        // A blank daemon line is a deliberate separator (the render puts one under the
        // marks block); keep it rather than collapsing the shape it was drawing.
        if l.trim().is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        for (i, row) in status_lines(l, inner_w, usize::MAX).into_iter().enumerate() {
            // A continuation is indented so a wrapped row never reads as a new entry.
            let text = if i == 0 { row } else { format!("  {row}") };
            lines.push(Line::from(Span::styled(text, base)));
        }
    }

    let scroll = state.heard.measure(lines.len(), inner_h);
    let max_scroll = state.heard.max.get();
    // Record the measured max so the scroll keys can clamp against it: without it `j`
    // past the end keeps incrementing an offset only the renderer clamps, and `k` then
    // appears stuck at the bottom while it unwinds the phantom distance.
    // Title and close key both come from the panel kind, so the overlay can never
    // tell the human to press a letter that does not close what they are looking at.
    let (name, key) = (state.panel.title(), state.panel.close_key());
    let title = if max_scroll > 0 {
        format!("{name} ({}/{})  j/k scroll  {key} closes", scroll + 1, max_scroll + 1)
    } else {
        format!("{name}  {key} closes")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(base)
        .title(Span::styled(title, head));
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).block(block).style(base).scroll((scroll, 0)),
        popup,
    );
}

/// Cells available for a card VALUE: the popup's inner width less the label column and
/// its two-space gutter, less the two border columns.
fn popup_inner_w(popup_w: u16, label_w: usize) -> usize {
    (popup_w as usize).saturating_sub(label_w + 2 + 2)
}

/// The `i` detail card: everything the daemon already told us about one song, laid out
/// as a label column and a value column.
///
/// It renders PAIRS, not a typed song, and that is the point - a field the daemon did
/// not send is simply a row that is not here, so there is no per-field `Option` to
/// thread and no placeholder to invent. What the card can show grows the moment the
/// daemon emits more, with no change on this side.
///
/// Content-sized rather than full-frame (the help overlay's shape, not heard's): the
/// rows are short label/value pairs, so a full-frame box would be mostly empty and
/// would hide the queue for no gain.
fn render_info_card(f: &mut Frame, region: Rect, state: &TuiState) {
    let Some(base) = &state.card else { return };
    // CONCATENATED AT RENDER, which is what makes the two replies order-independent.
    let rows: Vec<(String, String)> =
        base.iter().chain(state.card_more.iter()).cloned().collect();
    let rows = &rows;
    let fg = hypodj_style::ratatui::info_color(hypodj_style::NEUTRAL_VIBRANT, state.term_bg, state.truecolor);
    let base = Style::default().fg(fg);
    let head = base.bold();

    // The label column is measured from the content, so a long label never collides with
    // its own value and a card of short labels does not carry a wide empty gutter.
    // Measured over FACT rows only. A heading lives in the label slot but is not
    // label-column content, so counting it would push every value across the card.
    let label_w = rows
        .iter()
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    let widest = rows
        .iter()
        .map(|(k, v)| label_w.saturating_sub(k.chars().count()) + k.chars().count() + 2 + v.chars().count())
        .max()
        .unwrap_or(0);

    // CAPPED, not merely clamped to the terminal. `widest` is the longest UNWRAPPED
    // value, and a Last.fm biography is one ~1900-character paragraph - sizing to it
    // would stretch the card across a 129-column terminal and set prose in one
    // enormous measure, which is unreadable. 76 columns is a comfortable line for
    // running text, and a card of short label/value rows still shrinks to fit.
    const CARD_MAX_W: u16 = 76;
    // The cover is part of the content, so the card is at least as wide as a SQUARE
    // cover of ART_ROWS rows. Sizing from the text alone let a short card (one title
    // row) clamp the cover's columns while its rows stayed fixed - a squashed image.
    const ART_ROWS: usize = 10;
    let art_want = square_art_cols(ART_ROWS as u16, state.sixel_cell_px) as usize;
    let content_w = if state.card_art.is_some() { widest.max(art_want) } else { widest };
    let w = (content_w as u16 + 4)
        .min(CARD_MAX_W)
        .min(region.width.saturating_sub(2))
        .max(20);

    // WRAPPED, not clipped, and wrapped HERE rather than with Paragraph::wrap. The
    // renderer measures max-scroll from the line count, and `.wrap()` does its wrapping
    // after that measurement - so a wrapped card would know neither how tall it really
    // is nor how far it may scroll, and the tail would be unreachable.
    //
    // This is not a hypothetical: Last.fm returns an artist biography as ONE paragraph
    // with no newlines, measured at 1907 characters on a real request. Clipped, a card
    // would show its first line and silently swallow the rest.
    //
    // `status_lines` is the same helper the heard overlay wraps with, so the two
    // overlays break lines identically.
    let full_w = (w as usize).saturating_sub(4).max(8);
    let value_w = (popup_inner_w(w, label_w)).max(8);
    let mut lines: Vec<Line> = Vec::new();

    // THE COVER, PREPENDED INTO THE CARD'S OWN LINES rather than laid out in a separate
    // region. The cell renderer already returns `Vec<Line>`, so prepending means the art
    // participates in the same scroll and the same measurement as everything else - no
    // second geometry to keep in step, and no chance of the scroll knowing a height the
    // art does not have.
    //
    // Cell rendering, never sixel: the card is an overlay, and `overlay_drawn` marks the
    // sixel surface COVERED while one is open precisely so a popup does not punch holes
    // in an inline image. Drawing a second sixel from inside that overlay would fight
    // the mechanism protecting it.
    // Square on screen: real cell pixels under sixel, the ~2:1 cell ratio otherwise.
    // Only a terminal narrower than the cover itself still clamps it.
    let art_cols = art_want.min(full_w);
    if let Some(a) = &state.card_art {
        // Centred over the card's width, so a narrow thumb does not sit oddly left.
        let pad = " ".repeat(full_w.saturating_sub(art_cols) / 2);
        for row in a.lines(art_cols, ART_ROWS) {
            let mut spans = vec![Span::raw(pad.clone())];
            spans.extend(row.spans);
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
    }
    for (k, v) in rows {
        // THREE ROW SHAPES, told apart by which half is empty - see
        // `more_rows_from_pairs`. A heading (label, no value) sets flush and bold. A
        // body line (value, no label) is running text and gets the FULL width, because
        // indenting a paragraph under an empty label column would waste a seventh of a
        // narrow card on nothing. A fact (both) keeps the label/value columns.
        if v.is_empty() && k.is_empty() {
            lines.push(Line::from(""));
        } else if v.is_empty() {
            lines.push(Line::from(Span::styled(k.clone(), head)));
        } else if k.is_empty() {
            for part in status_lines(v, full_w, usize::MAX) {
                lines.push(Line::from(Span::styled(part, base)));
            }
        } else {
            for (i, part) in status_lines(v, value_w, usize::MAX).iter().enumerate() {
                // The label prints once, on the first visual row; continuations align
                // under the value so a wrapped value never reads as a new fact.
                let label =
                    if i == 0 { format!("{k:<label_w$}  ") } else { " ".repeat(label_w + 2) };
                lines.push(Line::from(vec![
                    Span::styled(label, base.quiet()),
                    Span::styled(part.clone(), base),
                ]));
            }
        }
    }

    // HEIGHT FROM THE FINAL LINE COUNT, not from `rows`. Sizing before the cover and
    // the wrapping were applied made the box exactly as tall as the unwrapped row count
    // - so ten rows of art filled it and pushed every fact out of sight. The box has to
    // be measured against what will actually be drawn in it.
    let h = (lines.len() as u16 + 2).min(region.height).max(3);
    let popup = Rect {
        x: region.x + region.width.saturating_sub(w) / 2,
        y: region.y + region.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    let inner_h = h.saturating_sub(2);
    let scroll = state.card_scroll.measure(lines.len(), inner_h);
    let max_scroll = state.card_scroll.max.get();
    let title = if max_scroll > 0 {
        format!("Info ({}/{})  j/k scroll  i closes", scroll + 1, max_scroll + 1)
    } else {
        "Info  i closes".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(base)
        .title(Span::styled(title, head));
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).block(block).style(base).scroll((scroll, 0)),
        popup,
    );

    // THE SAME COVER AT THE SAME FIDELITY AS NOW PLAYING. The cell art above is 20x10
    // sextant cells, about 40x30 effective pixels, which read as coarse blocks right
    // beside a native-pixel sixel cover of the same album. So when the terminal draws
    // sixel, the cell art is painted OVER with a sixel of the card's own cover, anchored
    // to exactly the rect those cells occupy. The cells stay the fallback and the
    // layout: nothing else on the card moves.
    //
    // Scrolled, only the band of the cover still in view is sent. A sixel is
    // cell-anchored and cannot be clipped by the terminal, so the visible rows are
    // cropped out of the full-size image and encoded on their own, anchored at the
    // first inner row: the cover stays sharp while `j` scrolls it away, and the cell
    // art is only what a terminal with no image protocol (or unknown cell pixels) sees.
    // This is a second image inside the overlay, not under it: the Now Playing cover
    // stays pinned and covered exactly as before, and its own re-send on close is
    // untouched.
    // Not while help is drawn over the card: an image cannot sit under a popup.
    if let Some(a) = state.card_art.as_ref().filter(|_| !state.help_open) {
        let s = scroll as usize;
        let vis_rows = ART_ROWS.saturating_sub(s).min(inner_h as usize);
        if vis_rows > 0 {
            let inner_x = popup.x + 1;
            let rect = Rect {
                x: inner_x + (full_w.saturating_sub(art_cols) / 2) as u16,
                y: popup.y + 1,
                width: art_cols as u16,
                height: vis_rows as u16,
            };
            let _ = paint_sixel_rows(f, rect, a, state.sixel_cell_px, s as u16, ART_ROWS as u16);
        }
    }
}

/// Columns that make `rows` rows of art SQUARE on screen. The inverse of
/// [`square_art_rows`], with the same aspect source.
fn square_art_cols(rows: u16, cell_px: Option<(u16, u16)>) -> u16 {
    match cell_px {
        Some((cw, ch)) if cw > 0 => rows.saturating_mul(ch) / cw,
        _ => rows * 2,
    }
}

/// The `?` help overlay: a centered, bordered popup laid out in two columns from the
/// single-source keymap table (grouped, with a keys column and a one-line description).
/// Theme-aware: fg/border are nudged off the detected background via the INFO policy so
/// the panel stays legible on light or dark terminals. Derived entirely from
/// [`keymap::grouped`], so it can never drift from the real bindings.
fn render_help_overlay(f: &mut Frame, region: Rect, state: &TuiState) {
    use crate::keymap::grouped;
    // A legible fg for the detected background (a neutral swatch pushed to >= 3:1).
    let fg = hypodj_style::ratatui::info_color(hypodj_style::NEUTRAL_VIBRANT, state.term_bg, state.truecolor);
    let base = Style::default().fg(fg);
    let head = base.bold();
    let key_style = base.bold();
    let desc_style = base.quiet();

    // Widest keys column across all rows, so the description column aligns.
    let key_w = keymap::KEYMAP
        .iter()
        .map(|b| b.keys.len())
        .chain(keymap::VIEW_KEYS.iter().map(|v| v.keys.len()))
        .chain(keymap::GRID_KEYS.iter().map(|g| g.keys.len()))
        .max()
        .unwrap_or(6)
        .min(18);
    let row = |keys: &str, desc: String| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{:<kw$} ", keys, kw = key_w), key_style),
            Span::styled(desc, desc_style),
        ])
    };

    // THE CURRENT VIEW FIRST. Every row comes from a table - the view's own keys
    // (`VIEW_KEYS`), the cover grid's cursor (`GRID_KEYS`, the table `key_grid`
    // dispatches from) and the shared rows scoped to this view - so the section is as
    // derived as the global table below it and cannot drift from the bindings.
    let view = state.help_view();
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(format!("{} keys", view.title()), head)));
    if view == keymap::HelpView::Albums {
        for g in keymap::GRID_KEYS {
            lines.push(row(g.keys, format!("{} (cover grid)", g.help)));
        }
    }
    for v in keymap::VIEW_KEYS.iter().filter(|v| v.view == view) {
        lines.push(row(v.keys, v.help.to_string()));
    }
    let in_view = |b: &keymap::Binding| view.scope() == Some(b.scope);
    for b in keymap::KEYMAP.iter().filter(|b| in_view(b)) {
        lines.push(row(b.keys, b.help.to_string()));
    }
    lines.push(Line::from(""));

    // THEN EVERYTHING ELSE, grouped. Rows already shown above are left out; rows for
    // OTHER views stay, tagged, so every binding is still reachable from any view.
    let groups: Vec<(keymap::Group, Vec<&'static keymap::Binding>)> = grouped()
        .into_iter()
        .map(|(g, rows)| (g, rows.into_iter().filter(|b| !in_view(b)).collect::<Vec<_>>()))
        .filter(|(_, rows)| !rows.is_empty())
        .collect();
    let mid = groups.len().div_ceil(2);
    // Two columns of groups: the GROUPS are split into two vertical columns joined per
    // line.
    let (left, right) = groups.split_at(mid);
    let render_col = |cols: &[(keymap::Group, Vec<&'static keymap::Binding>)]| -> Vec<Line<'static>> {
        let mut out: Vec<Line> = Vec::new();
        for (g, rows) in cols {
            out.push(Line::from(Span::styled(g.title().to_string(), head)));
            for b in rows {
                // Tag screen-specific bindings so the overlay does not imply a Queue- or
                // browse-only key works everywhere; Global keys carry no tag (the common
                // case, kept uncluttered).
                let desc = match b.scope {
                    keymap::Scope::Global => b.help.to_string(),
                    keymap::Scope::Queue => format!("{} [queue]", b.help),
                    keymap::Scope::Browse => format!("{} [browse]", b.help),
                };
                out.push(row(b.keys, desc));
            }
            out.push(Line::from(""));
        }
        out
    };
    let lcol = render_col(left);
    let rcol = render_col(right);
    let lwidth = lcol.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let rwidth = rcol.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    // The two-column layout only fits when both columns plus the gap and borders clear
    // the region width. On a NARROW terminal we fall back to a SINGLE stacked column
    // (always fits width-wise) and rely on vertical scroll so every binding is still
    // reachable - never a horizontal truncation that hides the right column's tail.
    let two_col_w = lwidth + 3 + rwidth + 4;
    if two_col_w <= region.width {
        let rows = lcol.len().max(rcol.len());
        for i in 0..rows {
            let mut spans: Vec<Span> = Vec::new();
            let lspans = lcol.get(i).map(|l| l.spans.clone()).unwrap_or_default();
            let lused: usize = lspans.iter().map(|s| s.content.chars().count()).sum();
            spans.extend(lspans);
            // Pad the gap between the two columns.
            let pad = (lwidth as usize + 3).saturating_sub(lused);
            spans.push(Span::raw(" ".repeat(pad)));
            if let Some(r) = rcol.get(i) {
                spans.extend(r.spans.clone());
            }
            lines.push(Line::from(spans));
        }
    } else {
        lines.extend(render_col(&groups));
    }

    let popup = centered_popup(region, &lines);
    // The inner text height (popup minus the top/bottom border rows). When the table is
    // taller than this, the overlay SCROLLS instead of silently truncating: the offset
    // is clamped to the last full page so a short terminal can still reach every binding.
    let inner_h = popup.height.saturating_sub(2);
    let scroll = state.help.measure(lines.len(), inner_h);
    let max_scroll = state.help.max.get();
    // See `render_heard_overlay`: the keys clamp against this measured max.
    let title = if max_scroll > 0 {
        // Every other overlay says how to leave it in its own title; the one titled
        // "Help" was the single panel that did not.
        format!("Help ({}/{})  j/k scroll  Esc closes", scroll + 1, max_scroll + 1)
    } else {
        "Help  Esc closes".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(base)
        .title(Span::styled(title, head));
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).block(block).style(base).scroll((scroll, 0)),
        popup,
    );
}

/// Split `label` into styled spans, underlining+bolding the FIRST case-insensitive
/// occurrence of `query`. The middle (match) span carries `hit`, the rest `base`.
/// An empty query or no match is a fast path -> a single plain `base` span. The
/// hit style is a MODIFIER (underline+bold), never a fg color: the selected row is
/// already REVERSED, and a fg would invert into a background swatch on that row,
/// whereas a modifier survives the reverse swap and stays legible everywhere. Pure
/// and unit-tested.
fn match_spans(label: &str, query: &str, base: Style, hit: Style) -> Vec<Span<'static>> {
    if query.is_empty() {
        return vec![Span::styled(label.to_string(), base)];
    }
    let needle = query.to_lowercase();
    // Search on the ORIGINAL label and return byte offsets valid in it. We cannot
    // reuse offsets from `label.to_lowercase()`: `to_lowercase` can change byte
    // length (e.g. Turkish dotted-capital-I 'I' -> "i" combining, or German 'SS'
    // -> "ss"), which would put the slice off a char boundary and panic the whole
    // render. `find_ci` walks char boundaries of the original, so every offset is
    // guaranteed valid there.
    match find_ci(label, &needle) {
        Some((start, end)) => {
            let mut spans = Vec::new();
            if start > 0 {
                spans.push(Span::styled(label[..start].to_string(), base));
            }
            spans.push(Span::styled(label[start..end].to_string(), hit));
            if end < label.len() {
                spans.push(Span::styled(label[end..].to_string(), base));
            }
            spans
        }
        None => vec![Span::styled(label.to_string(), base)],
    }
}

/// Find the FIRST case-insensitive occurrence of `needle_lower` (already
/// lowercased) in `label`, returning a `(start, end)` byte range that is always
/// valid in `label` (both ends land on char boundaries of the original). Unlike
/// searching `label.to_lowercase()`, this never yields offsets that fall off a
/// char boundary when the lowercase mapping changes byte length. An empty needle
/// yields no match (callers treat an empty query as the fast path).
fn find_ci(label: &str, needle_lower: &str) -> Option<(usize, usize)> {
    if needle_lower.is_empty() {
        return None;
    }
    let starts: Vec<usize> = label.char_indices().map(|(i, _)| i).collect();
    for &start in &starts {
        // Lowercase the tail one char at a time, comparing against the needle as a
        // growing prefix. Stop as soon as it diverges or matches.
        let mut lowered = String::new();
        let mut end = start;
        for ch in label[start..].chars() {
            for lc in ch.to_lowercase() {
                lowered.push(lc);
            }
            end += ch.len_utf8();
            if lowered.len() >= needle_lower.len() {
                if lowered == needle_lower {
                    return Some((start, end));
                }
                break;
            }
            if !needle_lower.starts_with(&lowered) {
                break;
            }
        }
    }
    None
}

/// The substring-highlight style: underline + bold, composed over the selected
/// row's REVERSED modifier (see [`match_spans`]).
fn hit_style() -> Style {
    Style::default().underline().bold()
}

/// The view line: the five view names on the left, the server word on the right
/// ("server online", "server unreachable", "no network"), because reachability
/// decides what every other surface can do.
///
/// BUDGETED, never clipped. The names step down from whole words to first letters
/// (`Q A L D F`) when the words do not fit beside the server word, and the server word
/// itself is never what gives. The current view is the reversed one.
fn render_tabs(f: &mut Frame, area: ratatui::layout::Rect, screen: Screen, server: Option<&str>) {
    let words = server.map(server_words);
    let server_w = words.as_ref().map_or(0, |(w, _)| w.chars().count() + 2);
    let width = area.width as usize;
    let mut spans = view_spans(screen, width.saturating_sub(server_w));
    if let Some((words, bad)) = words {
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let room = width.saturating_sub(used);
        // Never truncated into a half-word: a clipped "server unrea" is worse than the
        // view names alone, and the names already stepped down to make this room.
        if room >= words.chars().count() + 2 {
            spans.push(Span::raw(" ".repeat(room - words.chars().count())));
            let style = if bad { Style::default().bold() } else { Style::default().quiet() };
            spans.push(Span::styled(words, style));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The view names that fit in `room` cells: whole words, else first letters, else the
/// current view's name alone. Each form is whole, so a label is never cut mid-word.
fn view_spans(screen: Screen, room: usize) -> Vec<Span<'static>> {
    let name = |s: Screen| match s {
        Screen::Queue => "Queue",
        Screen::Albums => "Albums",
        Screen::Playlists => "Lists",
        Screen::Dj => "DJ",
        // The code says Find because every `Search` identifier is already the `/`
        // cursor jump; the line says Find too, the word the `find>` prompt uses.
        Screen::Find => "Find",
    };
    let build = |short: bool| -> Vec<Span<'static>> {
        let mut spans = vec![Span::raw(" ")];
        for (i, s) in Screen::ORDER.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(if short { " " } else { "  " }));
            }
            let label = if short { name(*s)[..1].to_string() } else { name(*s).to_string() };
            let style = if *s == screen { Style::default().reverse() } else { Style::default().quiet() };
            spans.push(Span::styled(label, style));
        }
        spans
    };
    let width = |v: &[Span]| v.iter().map(|s| s.content.chars().count()).sum::<usize>();
    let full = build(false);
    if width(&full) <= room {
        return full;
    }
    let short = build(true);
    if width(&short) <= room {
        return short;
    }
    let alone = vec![Span::raw(" "), Span::styled(name(screen).to_string(), Style::default().reverse())];
    if width(&alone) <= room {
        return alone;
    }
    // Narrower still: the server word keeps the line, and the view names give way.
    Vec::new()
}

/// The terminal window/tab title for the current now-playing snapshot, emitted via
/// OSC (crossterm `SetTitle`). "HypoDJ" when stopped or nothing is playing;
/// "HypoDJ - <artist> - <title>" when both are known; "HypoDJ - <title>" when only
/// the title is. Pure and testable - mirrors the stopped/empty test in render_now.
pub fn window_title(np: &NowPlaying) -> String {
    let stopped = np.state.as_deref() == Some("stop");
    let empty = np.title.is_none() && np.artist.is_none();
    if stopped || empty {
        return "HypoDJ".to_string();
    }
    let artist = np.artist.as_deref().map(sanitize_title);
    let title = np.title.as_deref().map(sanitize_title);
    match (artist, title) {
        (Some(a), Some(t)) => format!("HypoDJ - {a} - {t}"),
        (None, Some(t)) => format!("HypoDJ - {t}"),
        _ => "HypoDJ".to_string(),
    }
}

/// Strip control characters from an MPD tag before it reaches the OSC window
/// title. crossterm `SetTitle` wraps the string as `ESC]0;<title>BEL`, so an
/// embedded BEL (0x07) or ESC (0x1b) in an artist/title tag would terminate the
/// OSC early and let the terminal interpret the trailing bytes as raw
/// output/escape sequences (title injection). Dropping every control char
/// (including newlines/tabs) keeps only printable text.
fn sanitize_title(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Columns the strip's cover takes (plus a one-cell gap), sized to be square at the
/// strip's three rows. Zero when nothing is playing, so an empty deck is all words.
fn strip_cover_cols(state: &TuiState) -> u16 {
    if deck_is_empty(&state.now) {
        return 0;
    }
    square_art_cols(3, state.sixel_cell_px) + 1
}

/// Nothing to name: stopped, or no song identity at all.
fn deck_is_empty(np: &NowPlaying) -> bool {
    np.state.as_deref() == Some("stop")
        || (np.title.is_none() && np.artist.is_none() && np.album.is_none())
}

/// THE DECK'S WORDS, shared by the strip and the rail so the two shapes can never say
/// different things: what is playing, the transport line, why it stopped, what comes
/// next, and one line per hold (timer, armed plan) and field pull. A line that has
/// nothing to say is not drawn, so a resting deck reserves no row for it.
/// `max_rows` is the strip's height when the caller knows it: held plans that do not
/// fit are COUNTED on a `+N more held` line rather than cut off by the edge with no
/// trace. `None` is the measuring pass (and the rail, which wraps).
fn deck_lines(state: &TuiState, width: usize, rail: bool, max_rows: Option<usize>) -> Vec<Line<'static>> {
    let np = &state.now;
    let quiet = hypodj_style::ratatui::style_for(hypodj_style::Role::InkQuiet, state.term_bg, state.truecolor);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let holds = hypodj_client::spoken::armed_hold(&np.armed);
    // WHY the deck is not playing what it should, drawn loud and WRAPPED to two rows
    // so the reason is never the part the edge cuts off.
    let error_lines = |lines: &mut Vec<Line<'static>>| {
        if let Some(e) = &np.error {
            for row in status_lines(e, width.max(8), 2) {
                lines.push(Line::from(Span::styled(row, Style::default().bold())));
            }
        }
    };
    if deck_is_empty(np) {
        lines.push(Line::from(Span::styled("nothing playing", Style::default().bold())));
        error_lines(&mut lines);
        if np.error.is_none() {
            // The one empty state a new user meets, so it says the act, not the keys
            // to navigate to it.
            // Wrapped into the strip's spare row, so a narrow terminal keeps the end.
            for row in status_lines("press : and say what to play, or Tab to browse albums", width.max(8), 2) {
                lines.push(Line::from(Span::styled(row, quiet)));
            }
        }
        if let Some(m) = np.modes.label() {
            lines.push(Line::from(Span::styled(format!("modes: {m}"), quiet)));
        }
    } else {
        let (headline, station) = stream_headline(np);
        let mut head = Vec::new();
        // A heart marks a Subsonic favorite. U+2665 is East-Asian-Width ambiguous, so
        // U+FE0E forces a single cell. Ember: a kept item. There is no red.
        if np.starred {
            head.push(Span::styled(
                "\u{2665}\u{FE0E} ",
                Style::default().fg(hypodj_style::ratatui::ember_color(state.term_bg, state.truecolor)),
            ));
        }
        head.push(Span::styled(headline, Style::default().bold()));
        // Who and from where, after the title: the title is what the edge must never
        // take, so it goes first and the rest yields. The artist keeps full weight;
        // a PLACEHOLDER artist ("Various Artists") says less than the album beside
        // it, so it drops to the album's dim level - still shown, since the tag is
        // what the server says.
        let dim = Style::default().quiet();
        let mut sep = "  ";
        // The rail is narrow and tall: each of who and from where gets its own line
        // there instead of wrapping mid-phrase.
        let mut rows: Vec<Line<'static>> = Vec::new();
        if rail {
            lines.push(Line::from(std::mem::take(&mut head)));
            for (text, style) in [
                (station.clone(), dim),
                (np.artist.clone(), np.artist.as_deref().map_or(dim, |a| if is_placeholder_artist(a) { dim } else { Style::default() })),
                (np.album.clone(), dim),
            ] {
                if let Some(t) = text {
                    rows.push(Line::from(Span::styled(t, style)));
                }
            }
        }
        let station = if rail { None } else { station };
        if let Some(st) = station {
            head.push(Span::styled(format!("{sep}{st}"), dim));
            sep = " \u{00b7} ";
        }
        if let Some(a) = np.artist.as_deref().filter(|_| !rail) {
            head.push(Span::styled(sep.to_string(), dim));
            head.push(Span::styled(a.to_string(), if is_placeholder_artist(a) { dim } else { Style::default() }));
            sep = " \u{00b7} ";
        }
        if let Some(al) = np.album.as_deref().filter(|_| !rail) {
            head.push(Span::styled(format!("{sep}{al}"), dim));
        }
        if !rail {
            lines.push(Line::from(head));
        }
        lines.extend(rows);
        lines.push(Line::from(Span::styled(
            status_line(np, state.live_elapsed(std::time::Instant::now()), width),
            quiet,
        )));
        error_lines(&mut lines);
    }
    if let Some(next) = next_line(state) {
        lines.push(next);
    }
    for a in &holds {
        lines.extend(hypodj_style::ratatui::render(a, state.term_bg, state.truecolor));
    }
    // Every other plan held for the listener, in the daemon's words, counted down.
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let held: Vec<Line<'static>> = state
        .held_plans
        .iter()
        .filter(|p| !p.is_feature())
        .map(|p| {
            Line::from(vec![
                Span::styled("held  ", Style::default().quiet()),
                // The rail wraps, so it keeps the whole sentence; the strip has one row.
                Span::raw(if rail { p.sentence(now_unix) } else { ellipsize_words(&p.sentence(now_unix), width.saturating_sub(6)) }),
            ])
        })
        .collect();
    let mut tail: Vec<Line<'static>> = Vec::new();
    if let Some(fl) = field_hud(&np.field) {
        tail.push(Line::from(Span::styled(fl, Style::default().quiet())));
    }
    // The just-finished hint: the one ambient line no other surface shows, since the
    // track has left the queue. Off the DJ view only (the DJ view owns that voice).
    // A playing deck already names its track, so only a resting one carries it.
    if let Some(hint) = np.hint.as_ref().filter(|_| np.error.is_none() && deck_is_empty(np)) {
        if state.screen != Screen::Dj && hint_earns_line(&hint.kind) {
            tail.push(Line::from(Span::styled(hint.phrase(), Style::default().quiet())));
        }
    }
    let avail = max_rows.map_or(usize::MAX, |m| m.saturating_sub(lines.len()));
    if held.len() + tail.len() <= avail {
        lines.extend(held);
        lines.extend(tail);
    } else {
        // Held plans outrank the ambient tail, and the count of what did not fit is
        // never the part that goes missing.
        let shown = held.len().min(avail.saturating_sub(1));
        let more = held.len() - shown;
        lines.extend(held.into_iter().take(shown));
        if more > 0 {
            lines.push(Line::from(Span::styled(format!("+{more} more held"), Style::default().quiet())));
        }
        let used = shown + usize::from(more > 0);
        lines.extend(tail.into_iter().take(avail.saturating_sub(used)));
    }
    lines
}

/// Cut `s` to `width` columns at a word boundary, ending in an ellipsis, so a held
/// plan never reads as a whole sentence that the edge took the end of.
fn ellipsize_words(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let room = width - 1;
    let cut = s.char_indices().nth(room).map_or(s.len(), |(i, _)| i);
    let head = match s[..cut].rfind(' ') {
        Some(i) if i > 0 => &s[..i],
        _ => &s[..cut],
    };
    format!("{}\u{2026}", head.trim_end())
}

/// `next  <title> · <artist> · on disk · +N more`: the song after this one, with its
/// availability in words (decision b) from the daemon's per-row `X-Offline`, so the
/// glance answers "will the next one play with the server gone". The continuation
/// station when the queue ends into radio; nothing when nothing follows.
fn next_line(state: &TuiState) -> Option<Line<'static>> {
    let start = state.now.song.map(|c| c + 1).unwrap_or(0);
    let lead = Span::styled("next  ", Style::default().quiet());
    if let Some(it) = state.queue.iter().find(|it| it.pos >= start) {
        let mut spans = vec![lead, Span::raw(it.title.clone())];
        let mut tail = Vec::new();
        if let Some(a) = it.artist.as_deref().filter(|a| Some(*a) != state.now.artist.as_deref()) {
            tail.push(a.to_string());
        }
        if let Some((_, v)) = it.details.iter().find(|(k, _)| k == "X-Offline") {
            tail.push(if v == "1" { "on disk" } else { "stream only" }.to_string());
        }
        let after = state.queue.iter().filter(|q| q.pos > it.pos).count();
        if after > 0 {
            tail.push(format!("+{after} more"));
        }
        if !tail.is_empty() {
            spans.push(Span::styled(format!(" \u{00b7} {}", tail.join(" \u{00b7} ")), Style::default().quiet()));
        }
        return Some(Line::from(spans));
    }
    if let Some(station) = &state.now.continuation {
        return Some(Line::from(vec![lead, Span::styled(format!("radio: {station}"), Style::default().quiet())]));
    }
    None
}

/// The cover slot: the cover fetched for THIS album, else the album's sigil (no image
/// protocol), else a neutral placeholder. Never the previous album's cover: a skip
/// whose fetch has not landed shows the swatch until it does.
fn render_cover(f: &mut Frame, rect: Rect, state: &TuiState) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let (cols, rows) = (rect.width as usize, rect.height as usize);
    match state.art_for_now() {
        Some(a)
            if render_sixel_art(
                f,
                rect,
                a,
                state.sixel_cell_px,
                state.sixel_gen.get(),
                &state.sixel_held,
                state.sixel_covered.get(),
            ) => {}
        Some(a) => f.render_widget(Paragraph::new(a.lines(cols, rows)), rect),
        None => match (&state.sigil, state.image_protocol) {
            (Some(sig), crate::album_color::ImageProtocol::None) => {
                f.render_widget(Paragraph::new(sig.lines(cols, rows)), rect)
            }
            _ => f.render_widget(art_placeholder(&state.now), rect),
        },
    }
}

/// The narrow deck: a strip across the top, cover on the left, words beside it.
fn render_strip(f: &mut Frame, area: Rect, state: &TuiState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let cover_w = strip_cover_cols(state).min(area.width / 3);
    if cover_w > 1 {
        let rect = Rect { x: area.x, y: area.y, width: cover_w - 1, height: area.height.min(3) };
        render_cover(f, rect, state);
    }
    let text = Rect { x: area.x + cover_w, width: area.width - cover_w, ..area };
    let lines = deck_lines(state, text.width as usize, false, Some(text.height as usize));
    f.render_widget(Paragraph::new(lines), text);
}

/// The wide deck: a rail on the left with a square cover on top and the same words
/// under it, wrapped rather than clipped (the rail is narrow by design).
fn render_rail(f: &mut Frame, area: Rect, state: &TuiState) {
    // One column of air either side of the rail's words.
    let inner = Rect { x: area.x + 1, width: area.width.saturating_sub(3), ..area };
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let mut y = inner.y;
    if !deck_is_empty(&state.now) {
        let rows = square_art_rows(inner.width, state.sixel_cell_px).min(inner.height / 2);
        let cols = square_art_cols(rows, state.sixel_cell_px).min(inner.width);
        if rows > 0 {
            render_cover(f, Rect { x: inner.x, y, width: cols, height: rows }, state);
            y += rows + 1;
        }
    }
    let text = Rect { y, height: (inner.y + inner.height).saturating_sub(y), ..inner };
    let lines = deck_lines(state, text.width as usize, true, None);
    f.render_widget(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }), text);
}

/// The now-playing HEADLINE and optional STATION sub-line for the current pane (task
/// lq54isr). A raw stream carries the station/show in `Name` and the now-playing (if
/// any) in `Title`; an unnamed stream's `Title` is just its URL. So the headline is the
/// station name when `Title` is only the URL (no real now-playing), else the `Title`;
/// and the station name rides a subtle second line whenever it is NOT already the
/// headline. A library song (no `Name`) keeps its `Title` headline and no station line.
/// Pure/testable.
///
/// The `title_is_url` branch is now DEAD against a current daemon, which puts the
/// resolved station name on `Title` itself rather than the raw url. It is kept
/// deliberately: the deployed build can lag master by several merges (the user switches
/// by hand), and an OLDER daemon still serves the url there. Against a new daemon the
/// rendered pane is byte-identical either way - `Title` equals `Name`, so the other
/// branch produces the same headline and the station sub-line filters itself out.
pub fn stream_headline(np: &NowPlaying) -> (String, Option<String>) {
    let title_is_url = np.title.is_some() && np.title == np.file;
    let headline = if title_is_url {
        np.name.clone().or_else(|| np.title.clone())
    } else {
        np.title.clone()
    }
    .unwrap_or_else(|| "(unknown)".to_string());
    let station = np.name.as_deref().filter(|n| *n != headline).map(str::to_string);
    (headline, station)
}

/// The rows the art may occupy and still be SQUARE inside `width` columns. Past this
/// the width clamp in `render_current` pins the columns and the extra rows only
/// stretch the cover. Mirrors that clamp's own aspect logic (real cell pixels under
/// sixel, the 2:1 half-block ratio otherwise) so the two cannot disagree.
fn square_art_rows(width: u16, cell_px: Option<(u16, u16)>) -> u16 {
    match cell_px {
        Some((cw, ch)) if ch > 0 => width.saturating_mul(cw) / ch,
        _ => width / 2,
    }
}

/// Whether an artist tag is a PLACEHOLDER rather than a name - the compilation and
/// missing-tag values a server emits, which carry no identity and should not be
/// rendered as though they do. Compared case-insensitively; anything else is a real
/// artist and is left alone.
fn is_placeholder_artist(artist: &str) -> bool {
    matches!(
        artist.trim().to_ascii_lowercase().as_str(),
        "various artists" | "various" | "va" | "unknown artist" | "unknown" | "[unknown]"
    )
}


/// The active latent-field pulls as one subtle HUD line, e.g.
/// `toward calmer (58%, 3m ago) \u{b7} toward warmer (41%, 1m ago)`. `None` when no pull
/// is active, so a resting field renders no line (and reserves no row). Mirrors the
/// CLI's `render_field` so the passive "see the field" HUD reads identically in both
/// clients.
///
/// The two numbers are LABELLED, like everything else on this card: `toward calmer 0.58
/// 3m` put a unitless fraction and a bare duration side by side with nothing saying
/// which was strength and which was age, on the one surface whose entire job is making
/// a nondeterministic mechanism inspectable. A percent is the same fact in the unit the
/// rest of the UI already uses, and `ago` says the direction time runs.
fn field_hud(field: &FieldState) -> Option<String> {
    if !field.active() {
        return None;
    }
    let line = field
        .pulls
        .iter()
        .map(|p| format!("toward {} ({}%, {}m ago)", p.label, p.strength, p.age_mins))
        .collect::<Vec<_>>()
        .join(" \u{00b7} ");
    Some(line)
}

/// Whether an ambient hint earns its OWN faint line. Only the just-finished track
/// does: it has already left the queue, so no other pane shows it. An up-next hint
/// always names the first-queued track, which the always-visible "Up Next" preview
/// pane already lists - drawing it as a hint line too would duplicate that pane, so
/// it is suppressed. The single gate both the card and the DJ pane consult, keeping
/// the ambient surface to one honest, non-redundant line.
fn hint_earns_line(kind: &HintKind) -> bool {
    matches!(kind, HintKind::JustFinished)
}

/// A dim placeholder shown when there is no cover art (stream, missing, or a fetch
/// failure): a bordered box with a centered music glyph, so the layout still reads.
fn art_placeholder(_np: &NowPlaying) -> Paragraph<'static> {
    Paragraph::new(vec![Line::from("\u{266B}")])
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL))
        .style(Style::default().quiet())
}

/// "[playing|paused|stopped] · vol [==#--] V% · track N of M · M:SS / M:SS".
///
/// Every number is LABELLED, because the unlabelled version lied. A bare
/// `[======#-----] 56% | 1 of 5 | 7:14` sits exactly where a seek bar sits in every
/// other player, so the fader read as playback progress and the duration read as
/// elapsed - two wrong beliefs from one line. `vol` in front of the fader and
/// `total` after the time cost four cells and remove both. `track` does the same for
/// the queue position, which otherwise competes with the store badge's own ratio.
///
/// The separator is a middle dot, not ` | `: three pipes in a forty-cell line read as
/// table rules on a surface whose whole register is quiet.
///
/// Volume is hidden when unknown. When the daemon reports a playhead the time reads
/// `1:23 / 7:14` (a scrub's only feedback); without one (an older daemon) it stays
/// `7:14 total`, because a bare duration is exactly what reads as elapsed.
///
/// `width` is the pane's inner width, and the line SHEDS PARTS to fit it rather than
/// being clipped. That is the whole reason width is a parameter: the paragraph does not
/// wrap, so an overlong line loses its RIGHTMOST cells - which is `7:14 total`, the very
/// number whose label this rewording exists to restore. Clipping would therefore
/// reconstruct the original lie on exactly the terminals with the least room.
///
/// Shed in reverse order of worth: the fader first (a picture of a number that is
/// printed beside it), then the volume entirely (a setting he just changed, and can
/// see change), then the queue position. Transport state and duration are never
/// dropped - they are what the line is FOR. Each surviving part keeps its label; a
/// half-labelled line is worse than a shorter one.
fn status_line(np: &NowPlaying, elapsed: Option<f64>, width: usize) -> String {
    let volume = np.volume.filter(|v| *v >= 0).map(|v| v.min(100) as u8);
    let modes = np.modes.label();
    let compose = |fader: bool, show_volume: bool, show_position: bool| {
        let mut bits = Vec::new();
        match np.state.as_deref() {
            Some("play") => bits.push("playing".to_string()),
            Some("pause") => bits.push("paused".to_string()),
            Some("stop") => bits.push("stopped".to_string()),
            Some(other) => bits.push(other.to_string()),
            None => {}
        }
        if let (Some(v), true) = (volume, show_volume) {
            bits.push(if fader {
                format!("vol {}", volume_slider(v, 12))
            } else {
                format!("vol {v}%")
            });
        }
        if let (Some(song), Some(m), true) = (np.song, np.playlistlength, show_position) {
            bits.push(format!("track {} of {}", song.saturating_add(1), m));
        }
        let mmss = |secs: f64| {
            let t = secs as u64;
            format!("{}:{:02}", t / 60, t % 60)
        };
        match (elapsed, np.duration) {
            (Some(e), Some(d)) => bits.push(format!("{} / {}", mmss(e), mmss(d))),
            (None, Some(d)) => bits.push(format!("{} total", mmss(d))),
            (Some(e), None) => bits.push(format!("{} in", mmss(e))),
            (None, None) => {}
        }
        // The ON playback modes. Never shed (they decide what happens when this track
        // ends), but LAST, so a pane too narrow for anything clips them before it
        // clips the labelled duration.
        if let Some(m) = &modes {
            bits.push(m.clone());
        }
        bits.join(" \u{00b7} ")
    };
    let mut last = String::new();
    for (fader, vol, pos) in [
        (true, true, true),
        (false, true, true),
        (false, false, true),
        (false, false, false),
    ] {
        last = compose(fader, vol, pos);
        if last.chars().count() <= width {
            return last;
        }
    }
    // Narrower than `playing · 7:14 total` - nothing left to shed, so the terminal
    // clips the shortest honest line rather than a needlessly long one.
    last
}

/// Render volume as a physical horizontal FADER: `[==#-----] V%`. `width` is the
/// number of inner track cells; the `#` thumb slides across them proportional to
/// `vol` (0..=100). A fader (not a round dial) maps 1:1 to a physical fader's
/// travel, needs only one row, reflows with pane width, and its thumb visibly
/// slides as the reported volume tracks a glide envelope. ASCII-safe glyphs
/// (`[`, `]`, `=`, `#`, `-`) so terminals without good unicode still render it.
/// Pure and deterministic - unit-tested below.
fn volume_slider(vol: u8, width: usize) -> String {
    let vol = vol.min(100);
    // At least one cell so the thumb always has a home.
    let width = width.max(1);
    // Thumb cell index in [0, width-1], proportional to vol.
    let pos = ((vol as f64 / 100.0) * (width as f64 - 1.0)).round() as usize;
    let pos = pos.min(width - 1);
    let filled = "=".repeat(pos);
    let empty = "-".repeat(width - 1 - pos);
    format!("[{filled}#{empty}] {vol}%")
}

#[cfg(test)]
mod tests {
    use super::{
        field_hud, hit_style, level_wave_row,
        match_spans, square_art_rows, status_line,
        stream_headline, track_seed, volume_slider, wave_glyphs, wave_row, window_title,
        BLOCK_GLYPHS,
    };
    use crate::state::{Mode, TuiState};
    use hypodj_client::model::{AmbientHint, ArmedFeatures, FieldPull, FieldState, HintKind, NowPlaying};
    use ratatui::style::Style;

    #[test]
    fn stream_headline_prefers_station_name_over_raw_url() {
        // A raw NTS stream with no now-playing: Title is just its URL, Name is the show.
        // The headline must be the station name (not the URL), with no duplicate station
        // sub-line (the name IS the headline).
        let url = "https://stream-mixtape-geo.ntslive.net/mixtape5";
        let np = NowPlaying {
            title: Some(url.into()),
            file: Some(url.into()),
            name: Some("4 To The Floor".into()),
            ..NowPlaying::default()
        };
        let (headline, station) = stream_headline(&np);
        assert_eq!(headline, "4 To The Floor", "the station name replaces the raw URL");
        assert_eq!(station, None, "no duplicate station line when it is already the headline");
    }

    #[test]
    fn stream_headline_on_a_current_daemon_shows_one_name_not_two() {
        // The shape a CURRENT daemon serves after the Title fix: it resolved the station
        // name and put it on `Title` as well as `Name`, so the client no longer has to
        // work around a url in the title. The pane must be byte-identical to what the
        // old workaround produced - ONE headline, and no duplicate station sub-line
        // repeating the same words directly beneath it.
        let np = NowPlaying {
            title: Some("4 To The Floor".into()),
            name: Some("4 To The Floor".into()),
            file: Some("https://stream-mixtape-geo.ntslive.net/mixtape5".into()),
            ..NowPlaying::default()
        };
        let (headline, station) = stream_headline(&np);
        assert_eq!(headline, "4 To The Floor");
        assert_eq!(station, None, "the name is already the headline - never printed twice");
    }

    #[test]
    fn stream_headline_keeps_now_playing_and_shows_station_beneath() {
        // A stream WITH a real now-playing (Title differs from the URL): the headline is
        // the now-playing line, and the station name rides a subtle second line.
        let np = NowPlaying {
            title: Some("Floating Points - Track".into()),
            file: Some("https://example.com/live".into()),
            name: Some("NTS 1".into()),
            ..NowPlaying::default()
        };
        let (headline, station) = stream_headline(&np);
        assert_eq!(headline, "Floating Points - Track");
        assert_eq!(station.as_deref(), Some("NTS 1"));
    }

    #[test]
    fn stream_headline_library_song_keeps_title_no_station() {
        // A library song carries a Title but no Name: headline is the Title, no station line.
        let np = NowPlaying {
            title: Some("Blue in Green".into()),
            file: Some("song/42".into()),
            ..NowPlaying::default()
        };
        let (headline, station) = stream_headline(&np);
        assert_eq!(headline, "Blue in Green");
        assert_eq!(station, None);
    }

    #[test]
    fn armed_hud_reads_human_and_absent_when_unarmed() {
        assert!(hypodj_client::spoken::armed_hold(&ArmedFeatures::default()).is_empty());
        let a = ArmedFeatures {
            sleep_remaining: Some(720),
            winddown_active: true,
            winddown_phase: Some("running".into()),
            winddown_when: None,
            winddown_remaining: None,
            wake_remaining: Some(25200),
            wake_at: Some(1750000000),
        };
        let lines = hypodj_client::spoken::armed_hold(&a);
        let hud = lines.iter().map(|s| s.text()).collect::<Vec<_>>().join(" ");
        assert!(
            !hud.contains("[armed]"),
            "no category tag on a line that is nothing but armed state: {hud}"
        );
        assert!(hud.contains("sleep in 12m"), "every countdown says `in`: {hud}");
        assert!(hud.contains("winding down"), "hud: {hud}");
        assert!(hud.contains("wake in 7h 00m"), "hud: {hud}");
        let wind = lines.iter().find(|s| s.text() == "winding down").expect("live wind-down");
        assert_eq!(wind.state, hypodj_style::State::Live);
    }

    #[test]
    fn field_hud_reads_pulls_and_absent_at_rest() {
        // No pull -> no line (the resting field is silent).
        assert_eq!(field_hud(&FieldState::default()), None);
        // Active pulls render as the pipe-joined "toward <label> <strength> <age>m"
        // line, mirroring the CLI's render_field verbatim.
        let field = FieldState {
            pulls: vec![
                FieldPull { label: "calmer".into(), strength: 58, age_mins: 3 },
                FieldPull { label: "warmer".into(), strength: 41, age_mins: 1 },
            ],
        };
        let hud = field_hud(&field).expect("active");
        assert_eq!(hud, "toward calmer (58%, 3m ago) \u{b7} toward warmer (41%, 1m ago)");
    }

    #[test]
    fn now_playing_pane_shows_active_field_pull() {
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.title = Some("Blue in Green".into());
        s.now.artist = Some("Miles Davis".into());
        s.now.field = FieldState {
            pulls: vec![FieldPull { label: "calmer".into(), strength: 58, age_mins: 3 }],
        };
        let out = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(out.contains("toward calmer (58%, 3m ago)"), "field pull line drawn:\n{out}");
    }

    #[test]
    fn now_playing_pane_draws_the_paused_playhead() {
        let mut s = TuiState::new();
        s.now.state = Some("pause".into());
        s.now.title = Some("Blue in Green".into());
        s.now.duration = Some(337.0);
        s.now.elapsed = Some(83.0);
        s.now_at = Some(std::time::Instant::now());
        let out = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(out.contains("paused \u{b7} 1:23 / 5:37"), "playhead drawn:\n{out}");
    }

    fn held(id: u64, what: &str) -> hypodj_client::model::HeldPlan {
        hypodj_client::model::HeldPlan {
            id,
            origin: "mcp:x".into(),
            what: Some(what.into()),
            when: "at 23:00".into(),
            next_fire: None,
        }
    }

    #[test]
    fn held_plans_past_the_strip_are_counted_and_cut_at_a_word() {
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.title = Some("A Song".into());
        s.now.file = Some("song/1".into());
        for i in 0..8 {
            s.held_plans.push(held(i, &format!("play the long ambient record number {i} quietly through the evening")));
        }
        let out = render_to_lines_sized(&s, 60, 20).join("\n");
        assert!(out.contains("more held"), "clipped held plans must be counted:\n{out}");
        assert!(out.contains('\u{2026}'), "a cut held line ends in an ellipsis:\n{out}");
        for line in out.lines().filter(|l| l.contains("held  play")) {
            let t = line.trim_end();
            assert!(t.ends_with('\u{2026}') || t.ends_with("23:00"), "cut mid-phrase: {t}");
        }
        // A tall terminal has room for every plan, and spends it.
        let tall = render_to_lines_sized(&s, 60, 40).join("\n");
        assert!(!tall.contains("more held"), "spare body rows show every plan:\n{tall}");
        assert_eq!(tall.matches("held  ").count(), 8, "{tall}");
    }

    #[test]
    fn empty_deck_hint_fits_a_narrow_strip() {
        let s = TuiState::new();
        let out = render_to_lines_sized(&s, 40, 20).join("\n");
        assert!(out.contains("browse albums"), "the hint must not be cut at 40 cols:\n{out}");
    }

    #[test]
    fn stream_cover_renders_image_cells_in_art_pane() {
        // A playing STREAM with a recognized cover and a decoded AlbumArt lights the
        // now-playing art pane (task kmrhj8m): the pane renders IMAGE cells, not the
        // image-less placeholder text. The stub cover is a flat 1x1, so the sextant
        // renderer resolves every cell to the full block - which is exactly the glyph
        // a flat cell must produce, and still proves the art path ran.
        use crate::album_color::Palette;
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.title = Some("Some Artist - Some Track".into());
        s.now.file = Some("https://stream.example/live".into());
        s.now.cover = Some("https://is1.example/hq.jpg".into());
        let pal = Palette { vibrant: [220, 40, 40], muted: [80, 30, 30], swatches: vec![[220, 40, 40]] };
        s.art = Some(crate::art::AlbumArt::for_test(pal));
        s.art_album = Some(s.album_identity());
        let out = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(
            out.contains('\u{2588}'),
            "the art pane renders cover cells for a stream:\n{out}"
        );
        assert!(
            !out.contains("no cover"),
            "the placeholder must not appear when art is present:\n{out}"
        );
    }

    #[test]
    fn now_playing_pane_at_rest_absent_and_layout_unchanged() {
        // A resting field draws no line AND does not perturb the layout: the whole
        // rendered buffer is byte-identical to a state with no field at all.
        let mut base = TuiState::new();
        base.now.state = Some("play".into());
        base.now.title = Some("Blue in Green".into());
        base.now.artist = Some("Miles Davis".into());
        let at_rest = render_to_lines_sized(&base, 100, 40);
        assert!(
            !at_rest.join("\n").contains("toward"),
            "no field line at rest:\n{}",
            at_rest.join("\n")
        );
        // Explicitly-empty field state must render the exact same bytes as the default.
        base.now.field = FieldState::default();
        let empty_field = render_to_lines_sized(&base, 100, 40);
        assert_eq!(at_rest, empty_field, "an empty field never jumps the layout");
    }

    #[test]
    fn match_spans_splits_before_match_after() {
        let base = Style::default();
        let hit = hit_style();
        // Case-insensitive middle match splits into (before, MATCH, after).
        let spans = match_spans("Kind of Blue", "of", base, hit);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "Kind ");
        assert_eq!(spans[1].content, "of");
        assert_eq!(spans[1].style, hit);
        assert_eq!(spans[2].content, " Blue");
        assert_eq!(spans[0].style, base);
    }

    #[test]
    fn match_spans_case_insensitive_and_edges() {
        let base = Style::default();
        let hit = hit_style();
        // A match at the very start has no `before` span.
        let spans = match_spans("Beta", "be", base, hit);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Be");
        assert_eq!(spans[0].style, hit);
        assert_eq!(spans[1].content, "ta");
        // A match at the very end has no `after` span.
        let spans = match_spans("Gamma", "MMA", base, hit);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].content, "mma");
        assert_eq!(spans[1].style, hit);
    }

    #[test]
    fn match_spans_empty_and_no_match_fast_path() {
        let base = Style::default();
        let hit = hit_style();
        // Empty query -> one plain span.
        let spans = match_spans("Alpha", "", base, hit);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style, base);
        // No match -> one plain span.
        let spans = match_spans("Alpha", "zzz", base, hit);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "Alpha");
        assert_eq!(spans[0].style, base);
    }

    #[test]
    fn match_spans_non_ascii_lowercase_length_change_no_panic() {
        let base = Style::default();
        let hit = hit_style();
        // 'I' (U+0130) lowercases to a 3-byte "i" + combining dot, growing the
        // byte length. Offsets from the lowercased copy would slice off a char
        // boundary of the original and panic; ours stay valid.
        let dotted = "\u{0130}"; // Turkish dotted capital I
        let label = format!("{dotted}a");
        let spans = match_spans(&label, "a", base, hit);
        let joined: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(joined, label);
        assert!(spans.iter().any(|s| s.content == "a" && s.style == hit));
        // A query that overshoots into the expanded lowercase must not match the
        // dotted 'I' and must not panic.
        let label = format!("a{dotted}b");
        let spans = match_spans(&label, "b", base, hit);
        let joined: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(joined, label);
        assert!(spans.iter().any(|s| s.content == "b" && s.style == hit));
    }

    #[test]
    fn window_title_default_and_stopped_are_plain() {
        // Empty now-playing -> plain product name.
        assert_eq!(window_title(&NowPlaying::default()), "HypoDJ");
        // Explicitly stopped, even with tags, is plain.
        let np = NowPlaying {
            state: Some("stop".into()),
            title: Some("T".into()),
            artist: Some("A".into()),
            ..NowPlaying::default()
        };
        assert_eq!(window_title(&np), "HypoDJ");
    }

    #[test]
    fn window_title_artist_and_title() {
        let np = NowPlaying {
            state: Some("play".into()),
            title: Some("Blue in Green".into()),
            artist: Some("Miles Davis".into()),
            ..NowPlaying::default()
        };
        assert_eq!(window_title(&np), "HypoDJ - Miles Davis - Blue in Green");
    }

    #[test]
    fn window_title_title_only() {
        let np = NowPlaying {
            state: Some("play".into()),
            title: Some("Live Stream".into()),
            ..NowPlaying::default()
        };
        assert_eq!(window_title(&np), "HypoDJ - Live Stream");
    }

    #[test]
    fn window_title_strips_control_chars() {
        // A BEL/ESC in a tag would terminate the OSC title early and inject the
        // trailing bytes into the terminal; they must be dropped.
        let np = NowPlaying {
            state: Some("play".into()),
            artist: Some("Ac\x1bDC".into()),
            title: Some("Foo\x07rm -rf".into()),
            ..NowPlaying::default()
        };
        assert_eq!(window_title(&np), "HypoDJ - AcDC - Foorm -rf");
    }

    #[test]
    fn thumb_hard_left_at_zero() {
        // vol 0 -> thumb at the very first cell, no fill before it.
        assert_eq!(volume_slider(0, 12), "[#-----------] 0%");
    }

    #[test]
    fn thumb_hard_right_at_full() {
        // vol 100 -> thumb at the last cell, everything before it filled.
        assert_eq!(volume_slider(100, 12), "[===========#] 100%");
    }

    #[test]
    fn thumb_centered_at_half() {
        let s = volume_slider(50, 12);
        // 12 inner cells, pos = round(0.5 * 11) = 6 (6 filled, thumb, 5 empty).
        assert_eq!(s, "[======#-----] 50%");
    }

    #[test]
    fn exact_cell_counts_and_percent_suffix() {
        let s = volume_slider(30, 12);
        assert!(s.ends_with(" 30%"), "percent suffix present: {s}");
        let inner = &s[1..s.find(']').unwrap()];
        assert_eq!(inner.chars().count(), 12, "inner track is exactly `width` cells");
        assert_eq!(inner.chars().filter(|&c| c == '#').count(), 1, "exactly one thumb");
    }

    #[test]
    fn every_number_on_the_transport_line_says_what_it_is() {
        // THE LINE THAT LIED. Unlabelled, it read `[======#-----] 56% | 1 of 5 | 7:14`:
        // a bar with a percent followed by a time, in the exact position every other
        // player puts a seek bar - so the fader was read as playback progress and the
        // duration as elapsed. Neither is true (the server emits no elapsed at all).
        // Each label below is load-bearing against one of those readings.
        let np = NowPlaying {
            state: Some("play".into()),
            volume: Some(56),
            song: Some(0),
            playlistlength: Some(5),
            duration: Some(434.0),
            ..Default::default()
        };
        assert_eq!(
            status_line(&np, None, 80),
            "playing \u{b7} vol [======#-----] 56% \u{b7} track 1 of 5 \u{b7} 7:14 total"
        );

        // And it stays honest as fields drop out: no volume, no queue, no duration.
        let lean = NowPlaying { state: Some("pause".into()), ..Default::default() };
        assert_eq!(status_line(&lean, None, 80), "paused");

        // An unknown volume (-1) is hidden rather than rendered as a fader at zero,
        // which would claim silence the daemon never reported.
        let unknown = NowPlaying {
            state: Some("play".into()),
            volume: Some(-1),
            ..Default::default()
        };
        assert_eq!(status_line(&unknown, None, 80), "playing");

        // NARROW: the fader is the first thing to go, because it is the only part that
        // says nothing the number does not. Every label survives, which is the point -
        // clipping would have taken `total` off the end and restored the old lie.
        let narrow = status_line(&np, None, 50);
        assert_eq!(narrow, "playing \u{b7} vol 56% \u{b7} track 1 of 5 \u{b7} 7:14 total");
        assert!(narrow.chars().count() <= 50, "and it now fits: {narrow}");

        // NARROWER STILL: the volume goes (he just set it, and can see it change), then
        // the queue position. Transport state and duration never go, and nothing that
        // survives loses its label.
        let tight = status_line(&np, None, 38);
        assert_eq!(tight, "playing \u{b7} track 1 of 5 \u{b7} 7:14 total");
        assert!(tight.chars().count() <= 38, "fits: {tight}");
        let tighter = status_line(&np, None, 25);
        assert_eq!(tighter, "playing \u{b7} 7:14 total");

        // Below even that there is nothing left to drop, so the shortest honest line is
        // handed over and the terminal clips it - never a longer one.
        assert_eq!(status_line(&np, None, 4), "playing \u{b7} 7:14 total");
    }

    #[test]
    fn transport_line_names_the_modes_that_are_on() {
        use hypodj_client::model::PlaybackModes;
        let np = NowPlaying {
            state: Some("play".into()),
            duration: Some(434.0),
            modes: PlaybackModes { random: true, consume: true, ..Default::default() },
            ..Default::default()
        };
        assert_eq!(status_line(&np, None, 80), "playing \u{b7} 7:14 total \u{b7} random consume");
        // All off adds nothing, so the default deck reads exactly as before.
        let plain = NowPlaying { modes: PlaybackModes::default(), ..np };
        assert_eq!(status_line(&plain, None, 80), "playing \u{b7} 7:14 total");
    }

    #[test]
    fn status_line_shows_the_playhead_when_reported() {
        let np = NowPlaying {
            state: Some("play".into()),
            duration: Some(434.0),
            ..Default::default()
        };
        assert_eq!(status_line(&np, Some(83.9), 80), "playing \u{b7} 1:23 / 7:14");
        let stream = NowPlaying { state: Some("play".into()), ..Default::default() };
        assert_eq!(status_line(&stream, Some(65.0), 80), "playing \u{b7} 1:05 in");
    }

    #[test]
    fn the_transport_line_is_drawn_labelled_in_a_real_frame() {
        // The formatter is pure, so pin it in the composed frame too - the pane could
        // clip the labels off and the unit test above would still pass.
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.title = Some("Blue in Green".into());
        s.now.volume = Some(56);
        s.now.song = Some(0);
        s.now.playlistlength = Some(5);
        s.now.duration = Some(434.0);
        let out = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(out.contains("vol [======#-----] 56%"), "the fader is named:\n{out}");
        assert!(out.contains("track 1 of 5"), "the position is named:\n{out}");
        assert!(out.contains("7:14 total"), "the duration is named:\n{out}");
    }

    #[test]
    fn the_empty_deck_says_how_to_leave_it() {
        // The stopped card is the first thing a fresh install shows, and every way out
        // of it is a function key. Before, the only pointer anywhere on screen was a dim
        // `? help` in the opposite corner.
        // The way out is the ACT (say it, or browse), not the function keys that used
        // to be the only route and are going away.
        let s = TuiState::new();
        let out = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(out.contains("nothing playing"), "the state is named:\n{out}");
        assert!(out.contains("press : and say what to play"), "and a way out:\n{out}");
        assert!(!out.contains("F5"), "no retired key is taught:\n{out}");
        // The empty queue says the same act, one wording, learned once.
        assert!(out.contains("Queue - empty"), "the queue is honest too:\n{out}");
        assert!(!out.contains("end of queue"), "not a cursor position:\n{out}");
    }

    #[test]
    fn the_deck_names_the_next_song_once_and_counts_the_rest() {
        // The deck carries ONE next song, by title, with how many follow - the queue
        // view is where the rest are listed. An artist the deck already names (the one
        // playing) is not said again; a CHANGE of artist always is.
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.title = Some("Playing".into());
        s.now.artist = Some("Various Artists".into());
        s.now.song = Some(0);
        let item = |pos: usize, title: &str, artist: &str| hypodj_client::model::QueueItem {
            pos,
            title: title.into(),
            artist: Some(artist.into()),
            uri: Some(format!("song/{pos}")),
            album_uri: None,
            artist_uri: None,
            details: vec![("X-Offline".into(), "1".into())],
        };
        s.queue = vec![
            item(0, "Playing", "Various Artists"),
            item(1, "Second", "Various Artists"),
            item(2, "Third", "Various Artists"),
            item(3, "Fourth", "C418"),
        ];
        let rows = render_to_lines_sized(&s, 80, 24);
        let next = rows.iter().take(4).find(|r| r.contains("next  ")).cloned().unwrap_or_default();
        assert!(next.contains("next  Second \u{b7} on disk \u{b7} +2 more"), "title, availability, count:\n{}", rows.join("\n"));
        assert!(!next.contains("Various Artists"), "the playing artist is not said twice:\n{next}");
        s.queue[1].artist = Some("C418".into());
        let rows = render_to_lines_sized(&s, 80, 24);
        let next = rows.iter().take(4).find(|r| r.contains("next  ")).cloned().unwrap_or_default();
        assert!(next.contains("next  Second \u{b7} C418"), "a change of artist is said:\n{next}");
    }

    /// What is drawn belongs to what is named: the held cover of the album before is
    /// never drawn under the next album's title while that album's own fetch is out
    /// (or will never land, the server being gone). Inside one album the cover holds.
    #[test]
    fn a_skip_into_another_album_never_draws_the_last_albums_cover() {
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.title = Some("Low Tide".into());
        s.now.artist = Some("Fixture Band".into());
        s.now.album = Some("Held Harbour".into());
        s.art = Some(crate::art::AlbumArt::for_test_solid([200, 40, 40]));
        s.art_album = Some(s.album_identity());
        let cover = |s: &TuiState| render_to_lines_sized(s, 80, 24)[0].chars().take(6).collect::<String>();
        assert!(cover(&s).contains('\u{2588}'), "the album's own cover is drawn: {:?}", cover(&s));
        // The next track of the SAME album: the cover holds, no blink.
        s.now.title = Some("Moorings".into());
        assert!(cover(&s).contains('\u{2588}'), "held inside the album: {:?}", cover(&s));
        // Into another album, its fetch still out: the swatch, never the old cover.
        s.now.title = Some("Ascent".into());
        s.now.album = Some("Stream Summit".into());
        assert!(!cover(&s).contains('\u{2588}'), "the old cover is gone: {:?}", cover(&s));
        assert!(s.art.is_some(), "it is the drawing that is gated, the fetch path is untouched");
    }

    /// The view line steps down whole (words, then letters, then nothing) and the
    /// server word is the last thing on it to go.
    #[test]
    fn the_view_line_steps_down_and_keeps_the_server_word() {
        let mut s = TuiState::new();
        s.now.server = Some("unreachable".into());
        let line = |w: u16| render_to_lines_sized(&s, w, 24)[3].trim_end().to_string();
        assert!(line(80).starts_with(" Queue  Albums  Lists  DJ  Find"), "{:?}", line(80));
        assert!(line(80).ends_with("server unreachable"), "{:?}", line(80));
        assert!(line(60).ends_with("server unreachable"), "{:?}", line(60));
        assert!(line(40).starts_with(" Q A L D F"), "letters at 40: {:?}", line(40));
        assert!(line(40).ends_with("server unreachable"), "{:?}", line(40));
        assert_eq!(line(24).trim(), "server unreachable", "names give way first: {:?}", line(24));
        for w in [24u16, 40, 60, 80] {
            assert!(!line(w).contains("server unrea") || line(w).contains("server unreachable"), "never clipped at {w}");
        }
    }

    /// The Deck gives the list the rows: at the classic 80x24, a long queue shows at
    /// least 16 rows (the bordered two-box layout showed 6).
    #[test]
    fn at_80x24_the_list_gets_at_least_sixteen_rows() {
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.title = Some("Song 1".into());
        s.now.song = Some(0);
        s.queue = (0..40)
            .map(|pos| hypodj_client::model::QueueItem {
                pos,
                title: format!("Song {}", pos + 1),
                artist: None,
                uri: Some(format!("song/{pos}")),
                album_uri: None,
                artist_uri: None,
                details: Vec::new(),
            })
            .collect();
        let rows = render_to_lines_sized(&s, 80, 24);
        let list = rows.iter().filter(|r| r.contains(". Song ")).count();
        assert!(list >= 16, "{list} list rows:\n{}", rows.join("\n"));
        // And the instrument is still on top: title, transport and next together.
        let top = rows[..3].join("\n");
        assert!(top.contains("Song 1") && top.contains("playing") && top.contains("next  Song 2"), "{top}");
    }

    #[test]
    fn a_placeholder_artist_is_dimmed_and_a_real_one_is_not() {
        // `Various Artists` was rendering at full weight as the second-most prominent
        // thing on the card while saying less than the dim album line under it.
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.title = Some("Track".into());
        s.now.album = Some("A Compilation".into());
        let weight_of = |s: &TuiState, needle: &str| -> ratatui::style::Modifier {
            use ratatui::backend::TestBackend;
            use ratatui::Terminal;
            let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
            t.draw(|f| super::render(f, s)).unwrap();
            let buf = t.backend().buffer().clone();
            for y in 0..buf.area.height {
                let row: String =
                    (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
                if row.contains(needle) {
                    let x = row.find(needle).unwrap() as u16;
                    return buf[(x, y)].style().add_modifier;
                }
            }
            panic!("{needle:?} is not on screen");
        };
        s.now.artist = Some("Various Artists".into());
        assert!(
            weight_of(&s, "Various Artists").contains(ratatui::style::Modifier::DIM),
            "a placeholder is dimmed to the album's level"
        );
        s.now.artist = Some("Miles Davis".into());
        assert!(
            !weight_of(&s, "Miles Davis").contains(ratatui::style::Modifier::DIM),
            "a real artist keeps its weight"
        );
    }

    #[test]
    fn the_art_never_takes_more_rows_than_it_can_be_square_in() {
        // Past the square limit the width clamp pins the columns, so extra rows only
        // stretch the cover while pushing the metadata into a footnote under it.
        assert_eq!(square_art_rows(60, None), 30, "half-block cells are 2:1");
        assert_eq!(square_art_rows(60, Some((10, 20))), 30, "and real cell pixels are honored");
        assert_eq!(square_art_rows(60, Some((20, 10))), 120);
        // Total: a zero-height cell (a terminal that answered nonsense) must not divide
        // by zero, and a zero-width pane simply has no room.
        assert_eq!(square_art_rows(60, Some((10, 0))), 30, "falls back, never panics");
        assert_eq!(square_art_rows(0, None), 0);
        // Saturating: a huge reported cell width must not overflow the multiply.
        assert_eq!(square_art_rows(u16::MAX, Some((u16::MAX, 1))), u16::MAX);
    }

    /// The badge does not truncate, it STEPS DOWN - so every cell the daemon adds to
    /// the line is paid for by a reason clause vanishing entirely at some width. This
    /// pins the REAL line against the REAL terminal (129 columns) so that trade stays
    /// visible: the head learning to name the in-flight count cost 17 cells, which the
    /// deferred clause's byte parenthetical paid for. Without this test the next clause
    /// to grow would silently drop both reasons on his actual screen.
    /// The `i` card renders from pairs the daemon ALREADY sent with the listing - no
    /// socket, no request, no latency. This is the whole reason step one was an emission
    /// fix rather than a fetch feature.
    /// A long unbroken paragraph must WRAP and stay reachable by scrolling, not clip.
    ///
    /// Last.fm returns an artist biography as a single ~1900-character line with no
    /// newlines - measured on a real request - so this is the shape the card actually
    /// meets, and clipping would silently swallow all but the first line.
    /// The cover belongs to the CARD now, not to the now-playing pane.
    ///
    /// An earlier design previewed a rested row's cover in that pane and retitled it
    /// "Preview". It worked, but it made a piece of global chrome stop meaning "what is
    /// playing", and the cost of a title swap is that the reader has to notice it. The
    /// card is already about one song and names it, so art inside it cannot mislead.
    #[test]
    fn the_info_card_draws_its_own_cover_and_leaves_now_playing_alone() {
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.title = Some("Something Else".into());
        // A cover is held for what is PLAYING, and must be untouched by the card.
        s.art = Some(crate::art::AlbumArt::for_test_solid([10, 20, 30]));
        s.art_album = Some(s.album_identity());
        s.card = Some(vec![("title".into(), "Heroes".into())]);
        s.card_art = Some(crate::art::AlbumArt::for_test_solid([200, 40, 40]));

        let out = render_to_lines_sized(&s, 129, 40).join("\n");
        assert!(out.contains("Info"), "the card is drawn:\n{out}");
        assert!(out.contains("Heroes"), "with its rows:\n{out}");
        // The deck keeps its ONE meaning - it still names what is playing beside the
        // card, and no "Preview" title appears anywhere.
        assert!(out.contains("Something Else"), "the deck is still itself:\n{out}");
        assert!(!out.contains("Preview"), "the preview title is gone:\n{out}");
        assert!(s.art.is_some(), "the card never touches the playing cover");
        // On a tall terminal the card simply GROWS to fit the cover, so it does not
        // scroll - and the facts stay visible, which is what the assertion above proves
        // and what sizing from `rows` alone got wrong.
        assert_eq!(s.card_scroll.max.get(), 0, "it fits, so nothing to scroll");

        // On a SHORT terminal it must scroll instead of hiding rows, and the max must
        // account for the cover's lines. That is the half a line-count taken before the
        // art was prepended would get wrong.
        let out = render_to_lines_sized(&s, 129, 12).join("\n");
        assert!(
            s.card_scroll.max.get() > 0,
            "a cover plus rows exceeds 12 terminal rows, so the card must know it scrolls:\n{out}"
        );
    }

    #[test]
    fn the_card_wraps_a_long_paragraph_instead_of_clipping_it() {
        let long = "Globally beloved for his ability to evolve both his musical style \
                    and personal image, his visionary pursuit of original, cutting-edge \
                    rock styles kept him at the top for over thirty-five years, selling \
                    well over a hundred million albums across a career that redefined \
                    what a popular musician was allowed to be."
            .to_string();
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.card = Some(vec![
            ("title".into(), "Heroes".into()),
            (String::new(), "about the artist".into()),
            (String::new(), long.clone()),
        ]);
        let out = render_to_lines_sized(&s, 129, 40).join("\n");

        assert!(out.contains("about the artist"), "the section heading renders:\n{out}");
        // The paragraph appears across MULTIPLE rows: its opening and a phrase from
        // deep inside it are both on screen, which clipping could not achieve.
        assert!(out.contains("Globally beloved"), "the start is shown:\n{out}");
        assert!(
            out.contains("redefined") || s.card_scroll.max.get() > 0,
            "either the tail is visible or the card knows it must scroll to reach it"
        );
        // The card does NOT stretch across the whole 129-column terminal just because
        // one value is long - prose in a single enormous measure is unreadable.
        let widest_row = out.lines().map(|l| l.trim_end().chars().count()).max().unwrap_or(0);
        assert!(widest_row <= 129, "never wider than the terminal:\n{out}");
        // The scroll measurement counts WRAPPED rows. On a 40-row terminal the card is
        // simply tall enough, so there is nothing to scroll - and this assertion used to
        // pass only because the box was sized from the UNWRAPPED row count and was
        // therefore too short for its own content. Squeeze the terminal and the real
        // behaviour shows: it scrolls, by an amount that knows about the wrapping.
        let short = render_to_lines_sized(&s, 129, 8).join("\n");
        assert!(
            s.card_scroll.max.get() > 0,
            "wrapped to 8 terminal rows, the card must know it scrolls:\n{short}"
        );
    }

    #[test]
    fn the_info_card_renders_the_metadata_the_row_already_carried() {
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.queue = vec![hypodj_client::model::QueueItem {
            pos: 0,
            title: "Ps Exclusive".into(),
            artist: Some("Life Without Buildings".into()),
            uri: Some("song/1".into()),
            album_uri: None,
            artist_uri: None,
            details: vec![
                ("Album".into(), "Any Other City".into()),
                ("X-Plays".into(), "12".into()),
                ("X-LastPlayed".into(), "1".into()),
                ("X-Rating".into(), "5".into()),
                ("X-Suffix".into(), "flac".into()),
                ("X-Size".into(), "41234567".into()),
                ("X-Starred".into(), "1".into()),
                // An unmapped pair: present on the wire, deliberately NOT shown, because
                // a raw `X-Whatever` in a human card is worse than an absent row.
                ("X-Undocumented".into(), "42".into()),
            ],
        }];
        s.selected = 0;
        s.card = Some(s.card_rows_for_selection_for_test());
        let out = render_to_lines_sized(&s, 100, 30).join("\n");

        assert!(out.contains("Info"), "the card is drawn:\n{out}");
        assert!(out.contains("Ps Exclusive"), "{out}");
        assert!(out.contains("Any Other City"), "{out}");
        assert!(out.contains("12"), "the play count the daemon already sent:\n{out}");
        // Rendered for a person: days get a unit, a rating gets its scale, bytes get
        // a human size - none of which the wire values carry.
        assert!(out.contains("yesterday"), "a bare '1' would read as a date:\n{out}");
        assert!(out.contains("5/5"), "{out}");
        assert!(out.contains("39 MiB"), "size is humanised, not raw bytes:\n{out}");
        assert!(out.contains("yes"), "starred is a presence flag:\n{out}");
        assert!(!out.contains("X-Undocumented"), "unmapped pairs stay hidden:\n{out}");
        assert!(!out.contains("41234567"), "no raw byte count:\n{out}");
    }

    #[test]
    fn the_real_mirror_line_still_shows_its_reasons_at_the_real_terminal_width() {
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        // Exactly what `store_status_pairs` builds for the live mirror on bubble-gum:
        // 384 held, 58 in flight, 4 given up (384 + 58 + 4 == 446), 3 below the line.
        s.now.store = Some(
            "384 of 446 songs - 62 still coming [\u{25c9}\u{25c9}\u{25c9}\u{25c9}\u{25c9}\u{25c9}\u{25c9}\u{25c9}\u{25c9}\u{25c9}], \
             15.9 of 16.0 GiB, 3 songs left out, 4 songs will not download"
                .into(),
        );
        let out = render_to_lines_sized(&s, 129, 30).join("\n");
        assert!(
            out.contains(
                "offline: 384 of 446 songs - 62 still coming \
                 [\u{25c9}\u{25c9}\u{25c9}\u{25c9}\u{25c9}\u{25c9}\u{25c9}\u{25c9}\u{25c9}\u{25c9}], \
                 3 songs left out, 4 songs will not download"
            ),
            "the FULL badge survives at 129 columns - not the `(held)` step-down, which \
             would trade both reasons away for the size clause:\n{out}"
        );
        assert!(!out.contains("(held)"), "the step-down did not fire:\n{out}");
        assert!(out.contains("? help"), "and the hint keeps its corner:\n{out}");
    }

    #[test]
    fn the_narrow_bar_steps_the_badge_down_instead_of_dropping_it() {
        // Dropping it outright made a HELD mirror and a finished one look identical on a
        // narrow terminal, and an empty bar reads as "nothing to report".
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.store =
            Some("318 of 347 tracks, 12.1 of 16.0 GiB, paused while streaming".into());
        let out = render_to_lines_sized(&s, 70, 30).join("\n");
        assert!(
            out.contains("offline: 318 of 347 tracks (held)"),
            "the count survives, and the fact that something stops it:\n{out}"
        );
        assert!(
            !out.contains("paused while streaming"),
            "the reason itself defers to `dj status` at this width:\n{out}"
        );
    }

    #[test]
    fn the_footer_offers_the_retraction_in_the_daemons_own_words() {
        // The badge text is produced by the SHARED formatter from the daemon's own
        // `PlanOutcome::render()`; the TUI only places it. Nothing a model said can
        // reach a character of this row.
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.undo_offer = Some("agent removed 3 - u to undo (12s)".into());
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("agent removed 3 - u to undo (12s)"), "{out}");
        // Nothing to retract -> no offer at all, rather than a dead one.
        let mut s2 = TuiState::new();
        s2.now.state = Some("play".into());
        let out2 = render_to_lines(&s2).join("\n");
        assert!(!out2.contains("to undo"), "{out2}");
    }

    #[test]
    fn the_offer_takes_the_badge_slot_ahead_of_the_store_line() {
        // Both want the one badge slot. The offer wins because it EXPIRES: the store
        // numbers will still be true in a minute and this will not.
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.store = Some("318 of 347 tracks, 12.1 of 16.0 GiB".into());
        s.undo_offer = Some("agent cleared 7 - u to undo (4s)".into());
        let out = render_to_lines_sized(&s, 80, 30).join("\n");
        assert!(out.contains("agent cleared 7 - u to undo (4s)"), "{out}");
        assert!(!out.contains("offline:"), "the store badge yields the slot:\n{out}");
        // And it hands the slot straight back once there is nothing to retract.
        s.undo_offer = None;
        let out = render_to_lines_sized(&s, 80, 30).join("\n");
        assert!(out.contains("offline: 318 of 347 tracks"), "{out}");
    }

    #[test]
    fn a_narrow_bar_steps_the_offer_down_to_the_key_instead_of_dropping_it() {
        // A retraction the human never learns about is the whole failure this badge
        // exists to prevent, so the last thing shed is the sentence, never the offer.
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.undo_offer = Some("agent removed The Man Machine - u to undo (12s)".into());
        let out = render_to_lines_sized(&s, 44, 24).join("\n");
        assert!(!out.contains("The Man Machine"), "the sentence does not fit:\n{out}");
        assert!(out.contains("u to undo"), "but the offer survives:\n{out}");
    }

    #[test]
    fn the_journal_overlay_says_its_own_name_and_its_own_close_key() {
        // One overlay, two contents - so the title and the close key must both come
        // from the panel kind, or it tells the human to press a letter that does not
        // close what they are looking at.
        let mut s = TuiState::new();
        s.open_journal(vec![
            "[ 12]    4s  agent sess-3f2a19bc     cleared 7  <- u".to_string(),
            "[ 11]    2m  mpd                     added 3".to_string(),
        ]);
        let out = render_to_lines_sized(&s, 80, 24).join("\n");
        assert!(out.contains("Journal"), "{out}");
        assert!(out.contains("U closes"), "{out}");
        assert!(out.contains("cleared 7"), "the daemon's own sentence renders:\n{out}");
        assert!(!out.contains("Marks"), "it is not the ledger:\n{out}");
        // The ledger still says its own name and key.
        let mut s = TuiState::new();
        s.open_heard(vec!["3 marks, oldest first".to_string(), "18:02  something".to_string()]);
        let out = render_to_lines_sized(&s, 80, 24).join("\n");
        assert!(out.contains("Marks"), "{out}");
        assert!(out.contains("t closes"), "{out}");
    }

    fn render_to_lines(state: &TuiState) -> Vec<String> {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|f| super::render(f, state)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Collect every distinct foreground color present in the rendered buffer, so a
    /// style assertion (colored waveform bars) is checkable headlessly.
    fn style_of_text(state: &TuiState, w: u16, h: u16, needle: &str) -> ratatui::style::Style {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| super::render(f, state)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let want: Vec<char> = needle.chars().collect();
        for y in 0..buf.area.height {
            let line: Vec<char> = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            if let Some(x) = line.windows(want.len()).position(|w| w == want.as_slice()) {
                return buf[(x as u16, y)].style();
            }
        }
        panic!("{needle:?} is not on screen");
    }

    fn render_fg_colors(state: &TuiState, w: u16, h: u16) -> Vec<ratatui::style::Color> {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| super::render(f, state)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = Vec::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let c = buf[(x, y)].style().fg;
                if let Some(col) = c {
                    if !out.contains(&col) {
                        out.push(col);
                    }
                }
            }
        }
        out
    }

    /// The rendered screen as ONE whitespace-normalized string, with the box-drawing
    /// frame dropped. A sentence that wraps across rows (or across a popup border) still
    /// has every word on screen, and this asserts that without pinning the exact column
    /// a break lands in.
    fn render_flat(state: &TuiState) -> String {
        render_to_lines(state)
            .join(" ")
            .chars()
            .map(|c| if ('\u{2500}'..='\u{257F}').contains(&c) { ' ' } else { c })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn render_to_lines_sized(state: &TuiState, w: u16, h: u16) -> Vec<String> {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| super::render(f, state)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn help_overlay_renders_groups_and_bindings_from_keymap() {
        // `?` opens a normal-mode modal drawn from the single-source keymap. A roomy
        // surface so the full two-column table lands (a cramped terminal clamps it).
        let mut s = TuiState::new();
        s.help_open = true;
        // Height sized to hold EVERY row at once (grows with the keymap; the scrolling
        // behaviour on a real terminal is the next test's job, not this one's).
        let out = render_to_lines_sized(&s, 120, 80).join("\n");
        // A group header and a couple of known binding descriptions are present, all
        // derived from keymap::grouped (so the overlay can never drift).
        assert!(out.contains("View"), "group header rendered:\n{out}");
        assert!(out.contains("play / pause"), "a keymap help line rendered:\n{out}");
        assert!(
            out.contains("endless radio from this row"),
            "the radio gesture is discoverable in the overlay:\n{out}"
        );
        assert!(out.contains("quit"), "quit binding rendered:\n{out}");
        assert!(out.contains("Help"), "overlay titled:\n{out}");
        assert!(out.contains("Esc closes"), "and says how to leave it:\n{out}");
    }

    #[test]
    fn help_overlay_fits_and_scrolls_on_a_short_terminal() {
        // On a standard-height terminal the table is taller than the screen; the overlay
        // must SCROLL rather than silently truncate. At the top, the last group (General
        // -> "quit") is off-screen; scrolled down it becomes visible, proving every
        // binding is reachable. The title also advertises the scroll position.
        let mut s = TuiState::new();
        s.help_open = true;
        s.help.scroll = 0;
        let top = render_to_lines_sized(&s, 60, 12).join("\n");
        assert!(top.contains("Help"), "overlay titled + fits:\n{top}");
        assert!(top.contains("scroll"), "scroll affordance shown when clamped:\n{top}");
        assert!(!top.contains("quit"), "tail binding is off-screen at the top:\n{top}");
        // The `quit` binding is off-screen at the top but becomes reachable by scrolling:
        // walk the offsets and assert it appears at some page (proving nothing is lost to
        // truncation). Over-scroll clamps to the last page, never panics.
        // The range is a probe, not a bound: it must stay comfortably past the real max
        // scroll, which grows by one row every time the keymap does.
        let reachable = (0..80u16).any(|off| {
            s.help.scroll = off;
            render_to_lines_sized(&s, 60, 12).join("\n").contains("quit")
        });
        assert!(reachable, "every binding is reachable by scrolling on a short terminal");
    }

    #[test]
    fn help_overlay_labels_screen_specific_bindings() {
        // Global keys carry no tag; a browse-scoped binding is marked so the overlay
        // does not imply it works everywhere.
        // On Queue the queue-only rows lead, untagged; the browse rows stay, tagged.
        let mut s = TuiState::new();
        s.help_open = true;
        let out = render_to_lines_sized(&s, 100, 60).join("\n");
        assert!(out.contains("[browse]"), "browse-scoped binding tagged:\n{out}");
        assert!(!out.contains("[queue]"), "queue rows are the view section here:\n{out}");
        // On Albums it is the other way round.
        s.screen = crate::state::Screen::Albums;
        let out = render_to_lines_sized(&s, 100, 60).join("\n");
        assert!(out.contains("[queue]"), "queue-scoped binding tagged:\n{out}");
        assert!(!out.contains("[browse]"), "browse rows are the view section here:\n{out}");
    }

    /// `?` leads with the CURRENT view's keys, then the global table - for every view,
    /// and for the info card. Every advertised row is a table row, so this also proves
    /// each view section is non-empty and drawn first.
    #[test]
    fn help_leads_with_the_current_views_keys() {
        use crate::keymap::HelpView;
        let cases: [(crate::state::Screen, bool, HelpView, &str); 6] = [
            (crate::state::Screen::Queue, false, HelpView::Queue, "jump to the playing song"),
            (crate::state::Screen::Albums, false, HelpView::Albums, "previous cover (cover grid)"),
            (crate::state::Screen::Playlists, false, HelpView::Lists, "drill into a directory"),
            (crate::state::Screen::Dj, false, HelpView::Dj, "send the ask"),
            (crate::state::Screen::Find, false, HelpView::Find, "walk the query history"),
            (crate::state::Screen::Queue, true, HelpView::Info, "close the card"),
        ];
        for (screen, card, view, want) in cases {
            let mut s = TuiState::new();
            s.screen = screen;
            if card {
                s.card = Some(vec![("title".into(), "Loro".into())]);
            }
            assert_eq!(s.help_view(), view);
            s.help_open = true;
            let lines = render_to_lines_sized(&s, 120, 90);
            let head = format!("{} keys", view.title());
            let at = |needle: &str| lines.iter().position(|l| l.contains(needle));
            let h = at(&head).unwrap_or_else(|| panic!("{head} heading:\n{}", lines.join("\n")));
            let w = at(want).unwrap_or_else(|| panic!("{want:?} on {head}:\n{}", lines.join("\n")));
            let g = at("Playback").expect("the global table follows");
            assert!(h < w && w < g, "{head}: view keys first, then global ({h} {w} {g})");
        }
    }

    /// A queue-song menu with NO album uri, so the popup carries both a live and a
    /// blocked row - the two things the render has to tell apart.
    fn menu_state() -> TuiState {
        use crate::menu::{Menu, Origin, Target, TargetKind};
        let mut s = TuiState::new();
        s.apply_snapshot(
            hypodj_client::model::NowPlaying::default(),
            vec![hypodj_client::model::QueueItem {
                pos: 0,
                title: "Sweden".into(),
                artist: Some("C418".into()),
                uri: Some("song/1".into()),
                album_uri: None,
                artist_uri: None,
                details: Vec::new(),
            }],
        );
        let target = Target {
            kind: TargetKind::LibrarySong,
            origin: Origin::Queue { pos: 0 },
            label: "Sweden".into(),
            uri: Some("song/1".into()),
            album_uri: None,
            artist: Some("C418".into()),
            artist_uri: None,
            match_uri: None,
        };
        s.menu = Some(Menu::new(target, 1));
        s
    }

    #[test]
    fn the_menu_popup_renders_its_rows_its_target_and_where_it_came_from() {
        let s = menu_state();
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("Menu"), "the popup is titled:\n{out}");
        assert!(out.contains("Sweden"), "it names its target:\n{out}");
        assert!(out.contains("queue row"), "and where the target came from:\n{out}");
        // The rendered rows ARE the ones dispatch consumes, so the popup is its own
        // help and there is no second table to drift.
        for want in [
            "play now",
            "go to album",
            "go to artist (search)",
            "start a radio from here",
            "star",
            "remove from queue",
        ] {
            assert!(out.contains(want), "row {want:?} rendered:\n{out}");
        }
        // The direct-pick letters are visible in the gutter, so the vocabulary is
        // learnable without a second lookup.
        assert!(out.contains("p  play now"), "hotkey gutter:\n{out}");
        assert!(out.contains("x  remove from queue"), "hotkey gutter:\n{out}");
    }

    #[test]
    fn a_blocked_menu_row_is_drawn_dim_and_the_preselected_one_reversed() {
        use ratatui::backend::TestBackend;
        use ratatui::style::Modifier;
        use ratatui::Terminal;
        let s = menu_state();
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|f| super::render(f, &s)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // The buffer is indexed by CELL, and the frame is full of multi-byte box
        // characters - so the needle has to be located by CHAR position, never by the
        // byte offset `str::find` returns.
        let style_of = |needle: &str| {
            let want: Vec<char> = needle.chars().collect();
            for y in 0..buf.area.height {
                let line: Vec<char> = (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect();
                if let Some(x) = line.windows(want.len()).position(|w| w == want.as_slice()) {
                    return buf[(x as u16, y)].style();
                }
            }
            panic!("{needle:?} is not on screen");
        };
        assert!(
            style_of("go to album").add_modifier.contains(Modifier::DIM),
            "a blocked row is dim - the reason itself goes to the status line, where a \
             sentence fits"
        );
        assert!(
            !style_of("start a radio from here").add_modifier.contains(Modifier::DIM),
            "a live row is not"
        );
        assert!(
            style_of("play now").add_modifier.contains(Modifier::REVERSED),
            "the cursor parks on the first LIVE row, so Enter is never a refusal"
        );
    }

    #[test]
    fn no_leftover_debug_printing_ships_in_this_file() {
        // Scaffolding written to diagnose popup geometry is easy to leave standing: it
        // prints on every `--nocapture` run and on every failing run of this binary
        // (including the one `nix/clients.nix` runs in the sandbox), burying the
        // assertion that actually failed. The instance this guards against was worse
        // than noise - it located its needle with `str::find`, a BYTE offset, and fed
        // that straight back as a CELL column, which is precisely the mistake `style_of`
        // exists to prevent: it printed the style of an unrelated cell and sat one
        // multi-byte box glyph away from an out-of-bounds buffer index, inside a test
        // whose assertions never read it. The renderer speaks through the buffer and
        // the assertion message; nothing here has anything to say on stdout.
        //
        // The needles are split so this test is not its own counter-example.
        let src = include_str!("ui.rs");
        for needle in [concat!("print", "ln!"), concat!("db", "g!")] {
            assert!(!src.contains(needle), "leftover {needle} in the renderer or its tests");
        }
    }

    #[test]
    fn the_menu_popup_still_fits_a_cramped_terminal() {
        // 40x12 is the small end this has to survive: the popup clamps to its region
        // rather than drawing outside it (which would panic in ratatui).
        let s = menu_state();
        let out = render_to_lines_sized(&s, 40, 12);
        assert_eq!(out.len(), 12);
        assert!(out.iter().all(|l| l.chars().count() == 40));
        let joined = out.join("\n");
        assert!(joined.contains("play now"), "the popup is still legible:\n{joined}");
    }

    #[test]
    fn help_toggle_open_and_close_and_search_still_works() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let ch = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let mut s = TuiState::new();
        // `?` opens.
        s.handle_key(ch('?'));
        assert!(s.help_open, "? opens the overlay");
        // While open, nav/transport keys are swallowed.
        assert_eq!(s.handle_key(ch('p')), None, "keys swallowed while help open");
        assert!(s.help_open, "still open after a swallowed key");
        // Esc closes.
        s.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!s.help_open, "Esc closes the overlay");
        // With help closed, `/` still enters search (never shadowed by `?`).
        s.handle_key(ch('/'));
        assert_eq!(s.mode, Mode::Search);
    }

    #[test]
    fn bottom_bar_shows_help_hint_when_idle() {
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("? help"), "subtle help hint on the bottom bar:\n{out}");
    }

    #[test]
    fn bottom_bar_carries_the_store_badge_and_gives_it_up_when_the_row_is_narrow() {
        // THE PASSIVE WINDOW. A starred backfill runs for days; without a badge the only
        // way to know whether it is running, held, or done is to open a socket and type
        // `store` by hand (the verb is deliberately absent from the `commands`
        // advertisement, so ncmpcpp will never show it either). The badge rides the
        // quietest row on the screen, left of the help hint.
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.store =
            Some("318 of 347 tracks, 12.1 of 16.0 GiB, paused while streaming".into());
        let out = render_to_lines_sized(&s, 100, 30).join("\n");
        assert!(
            out.contains("offline: 318 of 347 tracks, paused while streaming"),
            "the badge names its SUBJECT and the reason it is held:\n{out}"
        );
        assert!(out.contains("? help"), "and it does not displace the help hint:\n{out}");

        // No store line -> nothing drawn, exactly like the armed/field HUDs.
        s.now.store = None;
        let out = render_to_lines_sized(&s, 100, 30).join("\n");
        assert!(!out.contains("offline:"), "a lean status draws no badge:\n{out}");

        // A narrow terminal keeps the ambient row for itself rather than truncating the
        // badge into nonsense - `dj status` always carries the full sentence.
        s.now.store =
            Some("318 of 347 tracks, 12.1 of 16.0 GiB, paused while streaming".into());
        let out = render_to_lines_sized(&s, 40, 30).join("\n");
        assert!(!out.contains("offline:"), "no badge where it would eat the whole row:\n{out}");
    }

    #[test]
    fn sigil_fills_art_slot_when_no_cover_and_no_image_protocol() {
        use crate::album_color::{ImageProtocol, TermBg};
        use crate::sigil::Sigil;
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.title = Some("Track".into());
        s.now.artist = Some("Artist".into());
        s.now.album = Some("Album".into());
        s.art = None;
        s.image_protocol = ImageProtocol::None;
        s.sigil = Some(Sigil::build("artist\nalbum", None, TermBg::dark_default(), true));
        let out = render_to_lines(&s).join("\n");
        // The Truchet diagonals appear in the album-art slot (image-less fallback).
        assert!(
            out.contains('\u{2571}') || out.contains('\u{2572}'),
            "album sigil drawn in the art slot:\n{out}"
        );
    }

    #[test]
    fn current_prepends_heart_when_starred() {
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.title = Some("Blue in Green".into());
        s.now.starred = true;
        let out = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(out.contains('\u{2665}'), "heart glyph rendered when starred:\n{out}");
        assert!(out.contains("Blue in Green"), "title still present:\n{out}");
        // The heart is ember, fitted to the room. Red is not a colour we paint.
        s.truecolor = true;
        let ember = hypodj_style::ratatui::ember_color(s.term_bg, true);
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal.draw(|f| super::render(f, &s)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut saw = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].symbol().contains('\u{2665}') {
                    assert_eq!(buf[(x, y)].style().fg, Some(ember), "heart is ember");
                    assert_ne!(buf[(x, y)].style().fg, Some(ratatui::style::Color::Red));
                    saw = true;
                }
            }
        }
        assert!(saw, "heart cell is on screen");
        // Not starred -> no heart, title unchanged.
        s.now.starred = false;
        let plain = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(!plain.contains('\u{2665}'), "no heart when not starred:\n{plain}");
    }

    #[test]
    fn the_bars_never_move_under_a_deck_that_is_not_playing() {
        // The bottom-bar field is the one moving thing on the screen, so motion there
        // is a CLAIM: music is flowing. It was derived from `viz_playing`, a flag off
        // the daemon's frames, which let a stale or optimistic slot keep the bars
        // bobbing under a deck reading `paused` / `stopped` - the row contradicting the
        // status line one pane above it.
        let rest = wave_glyphs()[0];
        // The ambient row is the one built from wave glyphs - found by that, never by
        // the resting glyph itself, since a fully lit row contains none of it.
        let ambient = |s: &TuiState| -> String {
            render_to_lines_sized(s, 100, 30)
                .into_iter()
                .find(|r| r.chars().take(4).all(|c| wave_glyphs().contains(&c)))
                .expect("the ambient row")
        };
        let mut s = TuiState::new();
        s.now.file = Some("song/1".into());
        // A LIT, PLAYING level slot - exactly the stale-slot case.
        s.viz_active = true;
        s.viz_playing = true;
        s.viz_env = 0.9;
        for state in ["pause", "stop"] {
            s.now.state = Some(state.into());
            let bar = ambient(&s);
            let wave: String = bar.chars().take_while(|c| wave_glyphs().contains(c)).collect();
            assert!(
                !wave.is_empty() && wave.chars().all(|c| c == rest),
                "{state}: the field rests flat, it does not bob: {bar:?}"
            );
        }
        // An UNKNOWN transport is not playing either - a reconnect must not light it.
        s.now.state = None;
        let bar = ambient(&s);
        let wave: String = bar.chars().take_while(|c| wave_glyphs().contains(c)).collect();
        assert!(wave.chars().all(|c| c == rest), "unknown transport rests too: {bar:?}");

        // And it DOES move again the moment the deck says it is playing, so this is a
        // gate on the claim, not a dead row.
        s.now.state = Some("play".into());
        let bar = ambient(&s);
        assert!(
            bar.chars().any(|c| wave_glyphs().contains(&c) && c != rest),
            "a playing deck lights the field: {bar:?}"
        );
    }

    #[test]
    fn waveform_uses_album_swatch_color() {
        use crate::album_color::{info_color, Palette};
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.file = Some("song/1".into());
        s.viz_active = true;
        s.viz_playing = true;
        s.viz_env = 0.9;
        s.truecolor = true;
        // A vivid album palette -> the waveform is colored via the INFO policy.
        let pal = Palette { vibrant: [220, 40, 40], muted: [80, 30, 30], swatches: vec![[220, 40, 40]] };
        s.art = Some(crate::art::AlbumArt::for_test(pal.clone()));
        s.art_album = Some(s.album_identity());
        let expect = info_color(pal.vibrant, s.term_bg, true);
        let colors = render_fg_colors(&s, 60, 24);
        assert!(
            colors.contains(&expect),
            "the album-swatch INFO color appears in the rendered bars: want {expect:?} got {colors:?}"
        );
    }

    #[test]
    fn render_smoke_idle_playing_shows_wave_row() {
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.file = Some("song/1".into());
        s.anim_secs = 3.0;
        let lines = render_to_lines(&s);
        // The bottom bar row (index 21: top margin, tabs, list 0..17, now 18..? -
        // it is the second-to-last row, above the blank bottom margin) carries wave
        // glyphs when idle+playing.
        let joined = lines.join("\n");
        let allowed = wave_glyphs();
        assert!(
            joined.chars().any(|c| allowed.contains(&c)),
            "an idle playing frame draws ambient wave glyphs somewhere:\n{joined}"
        );
    }

    #[test]
    fn render_smoke_command_and_confirm() {
        let mut s = TuiState::new();
        // Command mode: the prompt is drawn (viz yields).
        s.mode = Mode::Command;
        s.input = "pause".into();
        let cmd = render_to_lines(&s).join("\n");
        assert!(cmd.contains("> pause"), "command prompt on the bar:\n{cmd}");
        // Confirm mode: the popup carries the prompt (bottom bar blank).
        let mut s2 = TuiState::new();
        s2.mode = Mode::Confirm;
        s2.pending = Some(crate::state::Pending {
            command: Some("clear".into()),
            token: None,
            steps: vec!["clear the whole queue".into()],
            note: None,
            trust: None,
        });
        let conf = render_to_lines(&s2).join("\n");
        assert!(conf.contains("confirm? [y/N]"), "confirm popup shown:\n{conf}");
        assert!(conf.contains("clear the whole queue"), "popup shows the step:\n{conf}");
    }

    #[test]
    fn the_mark_sentence_is_actually_drawn_on_the_bottom_bar() {
        // The whole gesture rests on the human SEEING what the press did. The worker
        // turns a `mark_result` pair into a Banner and the render loop parks it in
        // `status_msg`; this is the last link - that it lands on the bar at the harness
        // default 60x24, where the ambient wave would otherwise be. A press that
        // succeeds and shows nothing is the bug this replaces.
        let mut s = TuiState::new();
        s.set_reply(hypodj_client::spoken::Reply::one(hypodj_client::spoken::live_text(
            "noted: Sun Ra - Space at 21:14",
        )));
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("noted: Sun Ra - Space"), "the mark sentence is drawn:\n{out}");

        // The AMBIGUOUS answer is the one that stars nothing and asks for a word back,
        // so its lead must be legible at this width rather than starting off-screen.
        let mut s = TuiState::new();
        s.set_reply(hypodj_client::spoken::Reply::one(hypodj_client::spoken::live_text(
            "marked, but which one: \"A - One\" started 6s ago",
        )));
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("marked, but which one"), "the ambiguity is stated:\n{out}");

        // And the refusal path: a row that cannot be starred says WHY on the same bar.
        let mut s = TuiState::new();
        s.set_reply(hypodj_client::spoken::Reply::one(hypodj_client::spoken::refused_text(
            "can't star a smart list",
        )));
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("can't star a smart list"), "the reason is drawn:\n{out}");
    }

    #[test]
    fn a_mark_that_kept_audio_says_so_at_60_columns() {
        // The press KEEPS THE AUDIO, and the daemon appends what it kept to the end of
        // the sentence. On one 60-column row that tail is exactly what falls off the
        // edge - so the capture would be announced to nobody, which is the same silent
        // success the gesture was built to end. The bar borrows the blank breathing row
        // below it (already reserved, so nothing moves) and the clause lands.
        let sentence = "noted: NTS 2 at 23:17 - kept 5m of audio (window); `dj heard` has it";
        let pairs = [("mark_result".to_string(), sentence.to_string())];
        let crate::worker::Reply::Banner(spoken) =
            crate::worker::command_reply("mark", &pairs).expect("mark speaks")
        else {
            panic!("mark is a banner");
        };
        let mut s = TuiState::new();
        s.set_reply(spoken.into());
        let out = render_flat(&s);
        assert!(
            out.contains("noted: NTS 2 at 23:17 - kept 5m of audio (window); `dj heard` has it"),
            "the whole sentence is on screen, wrap and all:\n{out}"
        );
        // And the tail really is on a SECOND row: the first is full at 60 columns, so
        // without the borrowed row it does not exist at all.
        let rows = render_to_lines(&s);
        let head = rows.iter().position(|r| r.contains("kept 5m of audio")).expect("head row");
        let tail = rows.iter().position(|r| r.contains("heard` has it")).expect("tail row");
        assert_eq!(tail, head + 1, "the tail is the row directly under the bar:\n{rows:#?}");

        // The inapplicable case is just as load-bearing: a press that could not keep
        // audio must say WHY, not fall silent about the half that failed.
        let mut s = TuiState::new();
        s.set_reply(hypodj_client::spoken::Reply::one(hypodj_client::spoken::live_text(
            "noted: NTS 2 at 23:17 - no audio kept (only 3s is buffered so far)",
        )));
        let out = render_flat(&s);
        assert!(
            out.contains("no audio kept (only 3s is buffered so far)"),
            "the refusal and its reason are drawn:\n{out}"
        );

        // The layout does NOT move to make room: the now-playing pane and the tab strip
        // sit on exactly the rows they sit on with a short banner.
        let mut short = TuiState::new();
        short.set_reply(hypodj_client::spoken::Reply::one(hypodj_client::spoken::live_text(
            "noted",
        )));
        let a = render_to_lines(&short);
        let b = render_to_lines(&s);
        for row in 0..21 {
            assert_eq!(a[row], b[row], "row {row} moved when the banner grew");
        }
    }

    #[test]
    fn a_long_mixed_confirm_wraps_and_keeps_both_states() {
        use hypodj_client::spoken::from_confirm;
        let pairs = vec![
            ("plan_id".into(), "1".into()),
            (
                "result".into(),
                "added 5 tracks and then a long clause that cannot fit on one sixty column row".into(),
            ),
            ("plan_id".into(), "2".into()),
        ];
        let reply = from_confirm(&pairs);
        let mut s = TuiState::new();
        s.truecolor = true;
        s.set_reply(reply);
        let rows = render_to_lines(&s);
        let flat = rows.join("\n");
        assert!(flat.contains("added 5 tracks"), "{flat}");
        assert!(flat.contains("armed (plan 2)"), "{flat}");
        let head = rows.iter().position(|r| r.contains("added 5 tracks")).unwrap();
        let tail = rows.iter().position(|r| r.contains("armed (plan 2)")).unwrap();
        assert!(tail > head, "the second plan wraps onto a later row:\n{rows:#?}");
        let live = style_of_text(&s, 60, 24, "added 5 tracks");
        let armed = style_of_text(&s, 60, 24, "armed (plan 2)");
        assert!(live.add_modifier.contains(ratatui::style::Modifier::BOLD));
        assert_eq!(
            armed.fg,
            Some(hypodj_style::ratatui::ember_color(s.term_bg, true))
        );
    }

    #[test]
    fn the_tape_overlay_shows_the_daemon_render_whole_at_60x24() {
        // `t` asks the daemon for the marks and their audio; this is the panel it lands
        // in. It used to be one bar line plus "(+N more - run `dj heard`)" - a standing
        // instruction to leave the program to read an answer the program already had,
        // and the tape annotations live in precisely the lines that were dropped.
        let mut s = TuiState::new();
        s.heard_lines = vec![
            "3 marks, oldest first".into(),
            "21:02  * Sun Ra - Space Is The Place  [starred]".into(),
            "23:17  * NTS 2  https://stream-mixtape-geo.ntslive.net/mixtape5  [tape 2: 5m, window]"
                .into(),
            "23:41  * Modular Station  [tape swept]".into(),
            "2 tape segment(s) on disk; `heard keep <n>` pins one against eviction".into(),
        ];
        s.heard_open = true;
        let out = render_flat(&s);
        assert!(out.contains("Marks"), "the panel is titled:\n{out}");
        assert!(out.contains("t closes"), "and says how to leave it:\n{out}");
        assert!(out.contains("Sun Ra - Space Is The Place"), "an ordinary mark is listed:\n{out}");
        // THE POINT: the tape annotation survives at 60 columns. That row is 84 chars, so
        // without the panel it is a bar line that ends at "https://stream-mix" and the
        // `[tape 2: 5m, window]` tail is off the right edge of the screen entirely.
        assert!(out.contains("[tape 2: 5m, window]"), "the segment is named:\n{out}");
        assert!(
            out.contains("https://stream-mixtape-geo.ntslive.net/mixtape5"),
            "and the url it came from survives the wrap:\n{out}"
        );
        // A row whose audio has been swept says so - the tape is a rolling cache and a
        // row outliving its sound is designed for, not a defect to hide.
        assert!(out.contains("[tape swept]"), "a swept segment is honest:\n{out}");
        // And the pin surface is reachable from what is on screen.
        assert!(out.contains("heard keep <n>"), "how to pin one:\n{out}");
    }

    #[test]
    fn the_tape_overlay_is_honest_when_there_is_nothing_to_show() {
        // Every inapplicable case SAYS why. A daemon with no ledger, or an evening with
        // no presses, answers in one line - which is a banner, not a panel - but if it
        // ever reaches the panel it must still read as an answer rather than an empty
        // box.
        let mut s = TuiState::new();
        s.heard_lines = vec![
            "no marks recorded yet".into(),
            "no tape configured (set [tape].dir or a state directory)".into(),
        ];
        s.heard_open = true;
        let out = render_flat(&s);
        assert!(out.contains("no marks recorded yet"), "{out}");
        assert!(
            out.contains("no tape configured (set [tape].dir or a state directory)"),
            "{out}"
        );
    }

    #[test]
    fn status_lines_wraps_marks_its_truncation_and_never_panics() {
        use super::status_lines;
        // Pure, and the two properties that matter: nothing is dropped silently, and no
        // input shape panics the whole render.
        assert_eq!(status_lines("short", 20, 2), vec!["short".to_string()]);
        assert_eq!(
            status_lines("one two three four", 9, 4),
            vec!["one two".to_string(), "three".to_string(), "four".to_string()]
        );
        // The daemon's own double-space column separators survive INSIDE a row: they are
        // how a heard line separates its time, its title and its url.
        assert_eq!(
            status_lines("23:17  * NTS 2  x", 30, 2),
            vec!["23:17  * NTS 2  x".to_string()]
        );
        // Overflow past the last row is MARKED: a sentence cut short with no marker
        // reads as a complete sentence.
        let out = status_lines("aaa bbb ccc ddd eee fff", 7, 2);
        assert_eq!(out.len(), 2);
        assert!(out[1].ends_with("..."), "{out:?}");
        assert!(out.iter().all(|r| r.chars().count() <= 7), "{out:?}");
        // A run with no space in it (a url) is SPLIT, never dropped.
        let out = status_lines("supercalifragilistic", 6, 4);
        assert_eq!(out.concat(), "supercalifragilistic");
        // Totality: zero width, zero rows, empty input, multibyte.
        assert_eq!(status_lines("", 10, 1), vec![String::new()]);
        assert!(!status_lines("hello", 0, 0).is_empty());
        let multi = status_lines("Ale\u{0301}m Ame\u{0301}rica cora\u{e7}\u{e3}o", 4, 3);
        assert!(multi.iter().all(|r| r.chars().count() <= 4), "{multi:?}");
    }

    #[test]
    fn find_drill_keeps_the_query_visible_above_the_browse_list() {
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Find;
        s.find.query = "c418".into();
        s.find.drilling = true;
        s.find.drill.title = "C418".into();
        s.find.drill.rows = vec![crate::state::BrowseRow {
            label: "Minecraft - Volume Alpha".into(), uri: "album/9".into(),
            is_dir: true, song_count: Some(24),
            artist: None,
            album_uri: None, artist_uri: None, on_disk: None,
        }];
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("find> c418"), "the query that produced this branch stays visible:\n{out}");
        assert!(out.contains("back to hits"), "and the way back is stated:\n{out}");
        assert!(out.contains("C418"), "the drill is titled:\n{out}");
        assert!(out.contains("Minecraft - Volume Alpha"), "and renders as a plain browse list:\n{out}");
    }

    #[test]
    fn find_drill_in_flight_shows_a_spinner_not_an_empty_box() {
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Find;
        s.find.drilling = true;
        s.find.drill_loading = true;
        s.find.drill.title = "C418".into();
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("C418"), "titled while loading:\n{out}");
        // An empty bordered box carrying the artist name reads as "no albums"
        // rather than "still loading", which is the one state the inventory missed.
        assert!(
            out.contains('|') || out.contains('/') || out.contains('-') || out.contains('\\'),
            "a spinner frame is drawn:\n{out}"
        );
    }

    #[test]
    fn find_tab_is_in_the_strip_and_the_cold_screen_is_typeable() {
        // The tab strip is a hardcoded ARRAY, not a match, so nothing forces a new
        // screen into it: a missed edit ships a working tab that is invisible. This
        // is the guard.
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Find;
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("Find"), "Find is on the view line:\n{out}");
        assert!(out.contains("find>"), "the query prompt is drawn:\n{out}");
        assert!(
            out.contains("type a query and press enter"),
            "the COLD hint is drawn, and must read differently from no-results:\n{out}"
        );
    }

    #[test]
    fn find_renders_a_typed_query_and_its_hint() {
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Find;
        s.find.query = "c418".into();
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("find> c418"), "the query echoes on the prompt line:\n{out}");
        assert!(out.contains("enter: search"), "the query-focus hint is drawn:\n{out}");
    }

    #[test]
    fn find_no_results_echoes_the_query_and_differs_from_cold() {
        // The protocol cannot distinguish an empty result from an empty filter, so
        // the client - which knows what it sent - says so verbatim.
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Find;
        s.find.phase = crate::find::Phase::Done;
        s.find.submitted = "kalabrezze".into();
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("no results for"), "no-results state:\n{out}");
        assert!(out.contains("kalabrezze"), "the query is echoed back:\n{out}");
        assert!(
            !out.contains("type a query and press enter"),
            "must NOT read as the cold state:\n{out}"
        );
    }

    #[test]
    fn find_hit_rows_render_with_kind_sigils_at_the_default_60x24() {
        // The whole no-header-rows decision rests on this: the screen band is
        // height - 16, so at 60x24 there are five content rows. A design that needs a
        // taller terminal to show its main state fails here.
        use crate::find::{FindKind, FindRow};
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Find;
        s.find.phase = crate::find::Phase::Done;
        s.find.hits.tallies = vec![
            (FindKind::Artist, 1, None),
            (FindKind::Album, 1, None),
            (FindKind::Song, 1, None),
        ];
        s.find.hits.rows = vec![
            FindRow { kind: FindKind::Artist, label: "C418".into(), uri: "artist/1".into(), trailer: "12 albums".into(), song_count: None, album_uri: None, artist_uri: None, artist: None, },
            FindRow { kind: FindKind::Album, label: "Volume Alpha".into(), uri: "album/2".into(), trailer: "24 tracks".into(), song_count: Some(24), album_uri: None, artist_uri: None, artist: None, },
            FindRow { kind: FindKind::Song, label: "Sweden".into(), uri: "song/3".into(), trailer: "3:03".into(), song_count: None, album_uri: Some("album/2".into()), artist_uri: None, artist: None, },
        ];
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("@  C418"), "artist row with its @ sigil:\n{out}");
        assert!(out.contains("=  Volume Alpha"), "album row with its = sigil:\n{out}");
        assert!(out.contains("Sweden"), "song row:\n{out}");
        assert!(out.contains("1 artist / 1 album / 1 song"), "tallies in the title:\n{out}");
        assert!(out.contains("12 albums"), "the right-aligned trailer survives:\n{out}");
    }

    #[test]
    fn find_station_rows_render_between_albums_and_songs_at_the_default_60x24() {
        // The fourth kind has to fit the SAME five content rows the other three share at
        // 60x24 - no header row, kind in the one-character gutter, tally in the title.
        use crate::find::{FindKind, FindRow};
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Find;
        s.find.phase = crate::find::Phase::Done;
        s.find.hits.tallies = vec![
            (FindKind::Album, 1, None),
            // Two tokens on the wire, so no cap - and the title must not claim one.
            (FindKind::Station, 2, None),
            (FindKind::Song, 1, None),
        ];
        s.find.hits.rows = vec![
            FindRow { kind: FindKind::Album, label: "NTS Sessions".into(), uri: "album/2".into(), trailer: "24 tracks".into(), song_count: Some(24), album_uri: None, artist_uri: None, artist: None, },
            FindRow { kind: FindKind::Station, label: "NTS 4 To The Floor".into(), uri: "station/NTS 4 To The Floor".into(), trailer: "stream-mixtape-geo.ntslive.net".into(), song_count: None, album_uri: None, artist_uri: None, artist: None, },
            FindRow { kind: FindKind::Station, label: "KFJC 89.7 FM".into(), uri: "station/KFJC 89.7 FM".into(), trailer: "netcast.kfjc.org".into(), song_count: None, album_uri: None, artist_uri: None, artist: None, },
            FindRow { kind: FindKind::Song, label: "Sweden".into(), uri: "song/3".into(), trailer: "3:03".into(), song_count: None, album_uri: None, artist_uri: None, artist: None, },
        ];
        let lines = render_to_lines(&s);
        let out = lines.join("\n");
        assert!(out.contains(")  NTS 4 To The Floor"), "station row with its ) sigil:\n{out}");
        assert!(out.contains(")  KFJC 89.7 FM"), "the second station row too:\n{out}");
        assert!(out.contains("=  NTS Sessions"), "the album row keeps its = sigil:\n{out}");
        assert!(
            out.contains("1 album / 2 stations / 1 song"),
            "the station tally sits between album and song, with no (server cap):\n{out}"
        );
        assert!(!out.contains("server cap"), "a station kind has no cap to claim:\n{out}");
        // The trailer is the HOST - the only thing that tells two NTS stations apart.
        assert!(out.contains("netcast.kfjc.org"), "the host trailer survives:\n{out}");
        // Rows are painted in the display order, not just present somewhere.
        let row_of = |needle: &str| lines.iter().position(|l| l.contains(needle));
        let (alb, st, song) = (row_of("NTS Sessions"), row_of("KFJC 89.7 FM"), row_of("Sweden"));
        assert!(alb < st && st < song, "album, then stations, then songs: {alb:?} {st:?} {song:?}");
    }

    #[test]
    fn find_states_an_absent_kind_in_the_title_and_spends_no_row_on_it() {
        use crate::find::FindKind;
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Find;
        s.find.phase = crate::find::Phase::Done;
        s.find.hits.tallies = vec![(FindKind::Artist, 0, Some(20)), (FindKind::Song, 47, None)];
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("no artists"), "absence is stated, not silent:\n{out}");
    }

    #[test]
    fn find_in_flight_keeps_the_previous_rows_visible() {
        use crate::find::{FindKind, FindRow};
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Find;
        s.find.phase = crate::find::Phase::Loading("boards".into());
        s.find.hits.rows = vec![FindRow {
            kind: FindKind::Song, label: "Sweden".into(), uri: "song/3".into(),
            trailer: "3:03".into(), song_count: None, album_uri: None, artist_uri: None,
            artist: None,
        }];
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("searching"), "the in-flight title:\n{out}");
        assert!(
            out.contains("Sweden"),
            "the stale answer stays legible and actionable rather than blanking:\n{out}"
        );
    }

    #[test]
    fn find_ack_renders_in_the_pane_not_the_transient_bottom_bar() {
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Find;
        s.find.phase = crate::find::Phase::Failed("no such server".into());
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("search failed"), "the ACK is targeted at the screen:\n{out}");
    }

    #[test]
    fn find_survives_a_short_terminal_without_panicking() {
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Find;
        s.find.query = "c418".into();
        let out = render_to_lines_sized(&s, 60, 20).join("\n");
        assert!(out.contains("find> c418"), "the query line survives a 20-row terminal:\n{out}");
    }

    #[test]
    fn render_dj_view_draws_title_phase_and_input() {
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Dj;
        s.dj_phase = Some("thinking...".into());
        s.dj_input = "fade out".into();
        s.push_dj_log("> fade out".into());
        let out = render_to_lines(&s).join("\n");
        // The DJ pane title, the phase line, the ask> input, and the logged query.
        assert!(out.contains("DJ - ask in words"), "DJ view titled:\n{out}");
        assert!(out.contains("thinking..."), "phase line drawn:\n{out}");
        assert!(out.contains("ask>"), "input prompt drawn:\n{out}");
        assert!(out.contains("> fade out"), "scrollback shows the query:\n{out}");
        // The DJ view is on the view line, and the queue it used to share the
        // region with is one Tab away (its name is on the same line).
        assert!(out.contains(" DJ "), "DJ on the view line:\n{out}");
        assert!(out.contains("Queue"), "Queue named on the view line:\n{out}");
    }

    #[test]
    fn render_dj_shows_ambient_hint_above_input() {
        // The "btw, DJ knows" ambient surface: a just-finished hint rides its own faint
        // row directly above the ask> input, in the full DJ voice.
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Dj;
        s.now.hint = Some(AmbientHint {
            kind: HintKind::JustFinished,
            title: "303 (Ninajirachi Remix)".into(),
        });
        // A wide terminal so the DJ pane (half the width) fits the full phrase.
        let out = render_to_lines_sized(&s, 160, 40).join("\n");
        assert!(
            out.contains("btw, DJ knows: just finished 303 (Ninajirachi Remix)"),
            "ambient hint drawn in the DJ voice:\n{out}"
        );
        // The input row is still present (the hint did not displace it).
        assert!(out.contains("ask>"), "ask> input still drawn:\n{out}");
    }

    #[test]
    fn render_dj_no_hint_row_when_absent() {
        // No hint -> no "btw, DJ knows" line AND the layout is unperturbed (the pane
        // reserves no row for an absent hint, mirroring the armed/field HUD discipline).
        let mut base = TuiState::new();
        base.screen = crate::state::Screen::Dj;
        let no_hint = render_to_lines_sized(&base, 100, 40);
        assert!(
            !no_hint.join("\n").contains("btw, DJ knows"),
            "no hint line when absent:\n{}",
            no_hint.join("\n")
        );
    }

    #[test]
    fn now_playing_pane_shows_ambient_hint_when_stopped() {
        // A stopped deck surfaces the just-finished hint as the faintest line under
        // "nothing playing" (the bare phrase, no btw prefix in the card register).
        let mut s = TuiState::new();
        s.now.state = Some("stop".into());
        s.now.hint = Some(AmbientHint {
            kind: HintKind::JustFinished,
            title: "303 (Ninajirachi Remix)".into(),
        });
        let out = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(out.contains("nothing playing"), "stopped deck labelled:\n{out}");
        assert!(
            out.contains("just finished 303 (Ninajirachi Remix)"),
            "ambient hint drawn on the stopped pane:\n{out}"
        );
    }

    #[test]
    fn a_failed_play_says_why_instead_of_just_finished() {
        // The deck stopped because the song could not be fetched: the daemon's
        // `error` sentence is on the card, and "just finished" (a claim it played) is not.
        let mut s = TuiState::new();
        s.now.state = Some("stop".into());
        s.now.hint = Some(AmbientHint { kind: HintKind::JustFinished, title: "Ridge Line".into() });
        s.now.error =
            Some("could not play Ridge Line: not on disk and the server is unreachable".into());
        let out = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(out.contains("could not play Ridge Line: not on disk"), "{out}");
        assert!(!out.contains("just finished"), "{out}");
    }

    #[test]
    fn now_playing_pane_never_duplicates_current_track_with_hint() {
        // While a track plays the Now Playing pane shows the track by title; it must
        // NOT also render a hint (which would duplicate the pane). In practice the
        // daemon suppresses the now-playing hint, but even a leaked hint is ignored by
        // the playing branch.
        let mut s = TuiState::new();
        s.now.state = Some("play".into());
        s.now.title = Some("Blue in Green".into());
        s.now.artist = Some("Miles Davis".into());
        s.now.hint = Some(AmbientHint {
            kind: HintKind::JustFinished,
            title: "Some Other Track".into(),
        });
        let out = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(out.contains("Blue in Green"), "current track shown:\n{out}");
        assert!(!out.contains("just finished"), "no hint line while playing:\n{out}");
        assert!(!out.contains("Some Other Track"), "hint never duplicates the pane:\n{out}");
    }

    #[test]
    fn dj_screen_draws_ambient_hint_in_exactly_one_region() {
        // On Screen::Dj both the DJ pane and the always-drawn Now Playing card could
        // surface the same just-finished hint. Enforce ONE faint line: the DJ pane owns
        // the full "btw, DJ knows" voice and the card stays silent, so the phrase renders
        // exactly once (a stopped deck, which would otherwise draw the card's bare hint).
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Dj;
        s.now.state = Some("stop".into());
        s.now.hint = Some(AmbientHint {
            kind: HintKind::JustFinished,
            title: "Alpha One".into(),
        });
        let out = render_to_lines_sized(&s, 160, 40).join("\n");
        assert_eq!(
            out.matches("just finished Alpha One").count(),
            1,
            "the just-finished hint is drawn in exactly one region on Screen::Dj:\n{out}"
        );
        assert!(
            out.contains("btw, DJ knows: just finished Alpha One"),
            "the DJ pane is the single owner on Screen::Dj:\n{out}"
        );
    }

    #[test]
    fn up_next_hint_never_duplicates_the_up_next_pane() {
        // An up-next hint names the first-queued track, which the always-visible "Up
        // Next" preview pane already lists. It must NOT also draw as a faint hint line -
        // only the pane shows it (off the DJ screen, the card is the would-be owner).
        let mut s = TuiState::new();
        s.now.state = Some("stop".into());
        s.queue = vec![hypodj_client::model::QueueItem {
            pos: 0,
            title: "Track Zed".into(),
            artist: None,
            uri: Some("song/9".into()),
            album_uri: None,
            artist_uri: None,
            details: Vec::new(),
        }];
        s.now.hint = Some(AmbientHint {
            kind: HintKind::UpNext,
            title: "Track Zed".into(),
        });
        let out = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(out.contains("Track Zed"), "the Up Next pane lists the track:\n{out}");
        assert!(
            !out.contains("up next Track Zed"),
            "no up-next hint line duplicating the always-visible Up Next pane:\n{out}"
        );
    }

    #[test]
    fn queue_shows_continuation_tail_hint_only_when_armed() {
        // ARMED: the daemon names the station, so the Queue draws a faint
        // "then: <station>" row after the last real entry.
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Queue;
        s.queue = vec![hypodj_client::model::QueueItem {
            pos: 0,
            title: "Track Zed".into(),
            artist: None,
            uri: Some("song/9".into()),
            album_uri: None,
            artist_uri: None,
            details: Vec::new(),
        }];
        s.now.continuation = Some("NTS 1".into());
        let out = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(out.contains("then: NTS 1"), "armed continuation draws the tail hint:\n{out}");
        // DISARMED / unconfigured: no continuation, no tail hint (lean render).
        s.now.continuation = None;
        let out = render_to_lines_sized(&s, 100, 40).join("\n");
        assert!(!out.contains("then:"), "no tail hint when continuation is disarmed:\n{out}");
    }

    #[test]
    fn pressing_r_draws_its_own_confirmation_at_60x24() {
        // A successful `Req::Command` produces no banner (worker.rs banners only
        // ACKs), so the gesture's ONLY immediate feedback is the status line it sets
        // itself. Prove it actually lands on the smallest supported surface.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Queue;
        s.queue = vec![hypodj_client::model::QueueItem {
            pos: 0,
            title: "Roygbiv".into(),
            artist: None,
            uri: Some("song/9".into()),
            album_uri: None,
            artist_uri: None,
            details: Vec::new(),
        }];
        let intent = s.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert_eq!(
            intent,
            Some(crate::state::Intent::Command("radio song/9".into())),
        );
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("radio from Roygbiv"), "the gesture says what it did:\n{out}");
        // An unseedable row draws its REASON on the same line, never nothing.
        s.queue[0].uri = Some("http://stream.example/live".into());
        assert_eq!(s.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)), None);
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("can't start a radio"), "the reason is visible:\n{out}");
    }

    #[test]
    fn an_armed_walk_renders_its_seed_as_the_tail_hint_at_60x24() {
        // The STANDING confirmation (the status line is gone on the next keypress):
        // an armed autofill walk emits `X-hypodj-continuation-station: more like
        // <seed>` with no station configured, which the existing tail hint renders
        // with zero client change. Pinned at the smallest supported surface.
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Queue;
        s.queue = vec![hypodj_client::model::QueueItem {
            pos: 0,
            title: "Roygbiv".into(),
            artist: None,
            uri: Some("song/9".into()),
            album_uri: None,
            artist_uri: None,
            details: Vec::new(),
        }];
        s.now.continuation = Some("more like Roygbiv".into());
        let out = render_to_lines(&s).join("\n");
        assert!(out.contains("then: more like"), "the walk is visible in the queue:\n{out}");
    }

    #[test]
    fn dj_pane_suppresses_up_next_hint() {
        // On Screen::Dj the "btw, DJ knows" line is reserved for a hint that earns a
        // line; an up-next hint (already shown by the Up Next pane) draws nothing and
        // reserves no row, so the pane never jumps for a redundant hint.
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Dj;
        s.now.hint = Some(AmbientHint {
            kind: HintKind::UpNext,
            title: "Track Zed".into(),
        });
        let out = render_to_lines_sized(&s, 160, 40).join("\n");
        assert!(!out.contains("btw, DJ knows"), "no btw line for an up-next hint:\n{out}");
        assert!(!out.contains("up next Track Zed"), "up-next hint never drawn in the DJ pane:\n{out}");
    }

    #[test]
    fn a_canned_plan_renders_echo_note_and_armed_through_the_real_ui() {
        use hypodj_client::nl::split_echo;
        use hypodj_client::spoken::plan_echo;
        use hypodj_style::State;
        use ratatui::style::Modifier;
        let parts = split_echo("via rules | [1] fade out | NOTE: a wake also needs a play");
        let spoken = plan_echo(&parts);
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Dj;
        s.truecolor = true;
        s.enter_confirm(crate::state::Pending {
            trust: parts.trust,
            steps: parts.steps,
            note: parts.note,
            ..Default::default()
        });
        let flat = render_to_lines_sized(&s, 100, 40).join("\n");
        for item in &spoken {
            for line in &item.lines {
                assert!(flat.contains(line), "missing {line}:\n{flat}");
                let style = style_of_text(&s, 100, 40, line);
                if item.state == State::Echo {
                    assert!(style.add_modifier.contains(Modifier::DIM), "{line}");
                    assert!(style.add_modifier.contains(Modifier::UNDERLINED), "{line}");
                }
                if item.state == State::Note {
                    assert!(style.add_modifier.contains(Modifier::DIM), "{line}");
                    assert!(!style.add_modifier.contains(Modifier::UNDERLINED), "{line}");
                }
            }
        }
        // Off the DJ screen the same arm result is the status-bar banner, in ember.
        let armed = hypodj_client::spoken::from_reply(&[("plan_id".into(), "4".into())], None);
        s.screen = crate::state::Screen::Queue;
        s.mode = crate::state::Mode::Normal;
        s.set_reply(armed.clone().into());
        let bar = style_of_text(&s, 80, 24, &armed.text());
        assert_eq!(bar.fg, Some(hypodj_style::ratatui::ember_color(s.term_bg, true)));
        // A stopped deck still shows the wake/sleep hold, and it is ember too.
        s.clear_status();
        s.now.state = Some("stop".into());
        s.now.armed.sleep_remaining = Some(720);
        let hud = style_of_text(&s, 100, 40, "sleep in 12m");
        assert_eq!(hud.fg, Some(hypodj_style::ratatui::ember_color(s.term_bg, true)));
    }

    #[test]
    fn render_dj_confirm_prompt_is_differentiated() {
        // A confirm on the DJ screen pushes "confirm? [y/N]" into the chat; it must
        // render DIFFERENTIATED (bold + the theme accent), not as plain chat, so it
        // reads as an interactive prompt.
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Dj;
        s.enter_confirm(crate::state::Pending {
            command: Some("plan add x".into()),
            token: None,
            steps: vec!["add 5 calmer tracks".into()],
            note: None,
            trust: None,
        });
        assert!(
            s.dj_log.iter().any(|l| l.contains(hypodj_client::spoken::CONFIRM_LINE)),
            "prompt pushed to chat"
        );
        // Echo: quiet ink, underlined. No second colour.
        use ratatui::style::Modifier;
        let style = style_of_text(&s, 80, 30, hypodj_client::spoken::CONFIRM_LINE);
        assert!(style.add_modifier.contains(Modifier::UNDERLINED), "echo is underlined");
        assert!(style.add_modifier.contains(Modifier::DIM), "echo is quiet");
        assert_eq!(style.fg, Some(ratatui::style::Color::Reset), "echo does not paint a colour");
        // The caret stays alive during the confirm (the pane must not read as dead).
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| super::render(f, &s)).unwrap();
        assert!(terminal.get_cursor_position().is_ok());
    }

    #[test]
    fn render_dj_armed_line_visible_after_y() {
        // The command-arm banner ("armed: ...") folds into the DJ chat (main.rs routes
        // a Screen::Dj Banner to push_dj_log); it must be visible in the pane.
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Dj;
        s.push_dj_log("> y".into());
        s.push_dj_log("armed: add 5 calmer tracks".into());
        let out = render_to_lines_sized(&s, 80, 30).join("\n");
        assert!(out.contains("> y"), "choice echo visible:\n{out}");
        assert!(out.contains("armed: add 5 calmer tracks"), "armed line visible:\n{out}");
    }

    #[test]
    fn render_dj_cancelled_line_visible_after_n() {
        let mut s = TuiState::new();
        s.screen = crate::state::Screen::Dj;
        s.push_dj_log("> n".into());
        s.push_dj_log("cancelled".into());
        let out = render_to_lines_sized(&s, 80, 30).join("\n");
        assert!(out.contains("cancelled"), "cancelled line visible:\n{out}");
    }

    #[test]
    fn wave_row_length_matches_width() {
        // The row is exactly `width` glyphs, including the degenerate 0/1 widths.
        for w in [0usize, 1, 5, 20, 79] {
            assert_eq!(wave_row(w, 3.0, 42, true).chars().count(), w);
            assert_eq!(wave_row(w, 3.0, 42, false).chars().count(), w);
        }
    }

    #[test]
    fn wave_row_glyphs_all_in_allowed_ramp() {
        let allowed = wave_glyphs();
        let row = wave_row(64, 7.5, 1234, true);
        assert!(row.chars().all(|c| allowed.contains(&c)), "every glyph is on the ramp");
    }

    #[test]
    fn wave_row_deterministic_for_same_inputs() {
        // Same (width, t, seed, animate) => identical string (drives off wall-clock,
        // no hidden state or randomness).
        assert_eq!(wave_row(40, 12.25, 99, true), wave_row(40, 12.25, 99, true));
    }

    #[test]
    fn wave_row_frozen_baseline_when_not_animating() {
        // Paused/stopped => a flat baseline row, all the lowest glyph.
        let base = wave_glyphs()[0];
        let row = wave_row(24, 5.0, 7, false);
        assert!(row.chars().all(|c| c == base), "frozen row is all baseline glyph");
        // And it is time-independent while frozen.
        assert_eq!(wave_row(24, 5.0, 7, false), wave_row(24, 999.0, 7, false));
    }

    #[test]
    fn level_wave_row_length_and_caps() {
        // Exactly `width` glyphs at every width, including degenerate 0/1.
        for w in [0usize, 1, 5, 20, 80] {
            assert_eq!(level_wave_row(w, 3.0, 42, 1.0, true).chars().count(), w);
        }
        // The ramp NEVER reaches the full block (U+2588 = BLOCK_GLYPHS[7]): even at
        // maximum level the loudest glyph is at most U+2587 (index 6), leaving the
        // top breathing sliver.
        let full = BLOCK_GLYPHS[7];
        let loud = level_wave_row(120, 5.0, 7, 1.0, true);
        assert!(loud.chars().all(|c| c != full), "full block is banned: {loud}");
        // Every glyph is on the allowed ramp.
        let allowed = wave_glyphs();
        assert!(loud.chars().all(|c| allowed.contains(&c)));
    }

    #[test]
    fn level_wave_row_hairline_when_paused_or_silent() {
        let base = wave_glyphs()[0];
        // Not playing -> a flat resting hairline, no motion, time-independent.
        let row = level_wave_row(30, 5.0, 7, 0.8, false);
        assert!(row.chars().all(|c| c == base), "paused row is the hairline");
        assert_eq!(row, level_wave_row(30, 999.0, 7, 0.8, false));
        // Playing but silent (a == 0) also rests on the hairline (round(0) == 0).
        let quiet = level_wave_row(30, 5.0, 7, 0.0, true);
        assert!(quiet.chars().all(|c| c == base), "silence rests on the hairline");
    }

    #[test]
    fn wave_row_different_seeds_diverge() {
        // Two tracks (seeds from real file hashes) at the same instant have distinct
        // textures. Use realistic hashed seeds - adjacent tiny integers fold to
        // near-identical phases (the real seeds are full DefaultHasher outputs).
        let a = wave_row(48, 4.0, track_seed(&NowPlaying { file: Some("song/1".into()), ..NowPlaying::default() }), true);
        let b = wave_row(48, 4.0, track_seed(&NowPlaying { file: Some("song/2".into()), ..NowPlaying::default() }), true);
        assert_ne!(a, b, "per-track seed gives each track its own texture");
    }

    #[test]
    fn track_seed_stable_and_track_dependent() {
        let mut np = NowPlaying { file: Some("song/1".into()), ..NowPlaying::default() };
        let s1 = track_seed(&np);
        assert_eq!(s1, track_seed(&np), "stable for one track");
        np.file = Some("song/2".into());
        assert_ne!(s1, track_seed(&np), "changes with the track");
    }

    #[test]
    fn thumb_position_monotonic_non_decreasing() {
        let mut last = 0usize;
        for v in 0..=100u8 {
            let s = volume_slider(v, 12);
            let pos = s.find('#').unwrap();
            assert!(pos >= last, "thumb never moves left as vol rises (v={v})");
            last = pos;
        }
    }
}

fn render_queue(f: &mut Frame, area: ratatui::layout::Rect, state: &TuiState) {
    let (head, area) = body_header(area);
    let n = state.queue.len();
    let title = match n {
        // The deck above already says how to fill it; the header says only what is.
        0 => "Queue - empty".to_string(),
        1 => "Queue - 1 song".to_string(),
        n => format!("Queue - {n} songs"),
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(title, Style::default().quiet()))), head);
    let current = state.now.song;
    let query = state.highlight_query();
    let width = area.width as usize;
    let mut items: Vec<ListItem> = state
        .queue
        .iter()
        .enumerate()
        .map(|(i, it)| {
            // Offline, a row the mirror does not hold is greyed and the cursor skips
            // it (`TuiState::queue_active`).
            let base = if state.queue_active(i) { Style::default() } else { inactive_style() };
            // The searchable label (matches active_labels for the Queue screen).
            let label = match &it.artist {
                Some(a) => format!("{} - {}", it.title, a),
                None => it.title.clone(),
            };
            // Mark the current song row.
            let marker = if current == Some(it.pos) { "> " } else { "  " };
            let lead = format!("{marker}{}. ", it.pos + 1);
            // Availability in words (decision b) as a right-aligned column, from the
            // daemon's per-row `X-Offline`; absent when the daemon did not say.
            let avail = it.details.iter().find(|(k, _)| k == "X-Offline").map(|(_, v)| {
                if v == "1" { "on disk" } else { "stream only" }
            });
            let mut spans = vec![Span::styled(lead.clone(), base)];
            spans.extend(match_spans(&label, query, base, hit_style()));
            if let Some(w) = avail {
                push_column(&mut spans, lead.chars().count() + label.chars().count(), w, width);
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    // Standing CONTINUATION-radio queue-tail hint: when continuation is ARMED the
    // daemon names the station, so draw a faint "then: <station>" row AFTER the last
    // real queue entry - the future made visible BEFORE the drain handoff.
    if let Some(station) = &state.now.continuation {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("   then: {station}"),
            Style::default().quiet(),
        ))));
    }
    let list = List::new(items).highlight_style(Style::default().reverse());
    let mut ls = ListState::default();
    if !state.queue.is_empty() {
        let h = area.height as usize;
        let off = crate::state::scroll_offset(state.selected, state.queue.len(), h, state.offset.get());
        // Persist the derived offset for the next frame, then force it onto the
        // ListState (ratatui would otherwise recompute its own scroll).
        state.offset.set(off);
        *ls.offset_mut() = off;
        ls.select(Some(state.selected));
    }
    f.render_stateful_widget(list, area, &mut ls);
}

/// A row that cannot play right now (offline and not on disk): quiet ink, dimmed.
fn inactive_style() -> Style {
    Style::default().quiet().add_modifier(ratatui::style::Modifier::DIM)
}

/// Split a body region into its one-line header and the rows under it. The body is
/// borderless: a box costs two rows of a 24-row terminal and says only "this is a
/// list", which the rows already say.
fn body_header(area: Rect) -> (Rect, Rect) {
    let head = Rect { height: area.height.min(1), ..area };
    let rest = Rect { y: area.y + head.height, height: area.height - head.height, ..area };
    (head, rest)
}

/// Right-align `word` as a quiet column after a row whose text already used `used`
/// cells, two cells of air at least. When the row is too long the column still
/// follows it, so the edge clips the words before it clips the title.
fn push_column(spans: &mut Vec<Span<'static>>, used: usize, word: &str, width: usize) {
    let pad = width.saturating_sub(used + word.chars().count() + 1).max(2);
    spans.push(Span::styled(format!("{}{word}", " ".repeat(pad)), Style::default().quiet()));
}

/// Render a browse screen (Albums/Playlists): the active `Browse.rows` in a List,
/// dirs marked with a trailing `/`, driven by the browse's own cursor + scrolloff
/// offset. Reuses the same List+ListState mechanics as render_queue.
fn render_browse(
    f: &mut Frame,
    area: ratatui::layout::Rect,
    browse: &Browse,
    state: &TuiState,
    markers: bool,
) {
    let (head, area) = body_header(area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(browse.title.clone(), Style::default().quiet()))),
        head,
    );
    let query = state.highlight_query();
    let width = area.width as usize;
    // Queue-marker lookups (Albums screen only): album/<id> -> distinct queued song
    // ids for the full/partial gutter, plus the flat queued-uri set for the song
    // rows of an opened album.
    let album_map = if markers { state.queued_by_album() } else { Default::default() };
    let queued_uris = if markers { state.queued_uris() } else { Default::default() };
    let down = state.server_down();
    let items: Vec<ListItem> = browse
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            // Offline, a row the mirror does not hold is greyed and the cursor skips
            // it (`Browse::active`, the rule djg and djq share).
            let base = if browse.active(i, down) { Style::default() } else { inactive_style() };
            // Gutter glyph: for an album dir row, full/partial from queued vs total;
            // for a song row (opened album), `#` when already queued. Two-char gutter
            // (glyph + space) so it never collides with the REVERSED cursor bar.
            let glyph = if !markers {
                ' '
            } else if r.is_dir {
                let queued = album_map.get(&r.uri).map(|s| s.len()).unwrap_or(0);
                queue_mark_glyph(album_mark(queued, r.song_count))
            } else if queued_uris.contains(&r.uri) {
                '#'
            } else {
                ' '
            };
            let mut spans = vec![Span::styled(format!("{glyph} "), base)];
            spans.extend(match_spans(&r.label, query, base, hit_style()));
            // An album row says whether it plays offline, as a right-aligned column.
            // No trailing `/` on directories: that was the filesystem speaking.
            if let Some(words) = availability_words(browse, r) {
                push_column(&mut spans, 2 + r.label.chars().count(), &words, width);
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    // LOADING is a state, not an empty list: the Albums root can take seconds when
    // the server is slow or gone, and an empty pane meanwhile reads "no albums".
    if browse.rows.is_empty() && !browse.loaded && markers {
        let p = Paragraph::new(Line::from(Span::styled(
            "loading albums...",
            Style::default().quiet(),
        )));
        f.render_widget(p, area);
        return;
    }
    let list = List::new(items).highlight_style(Style::default().reverse());
    let mut ls = ListState::default();
    if !browse.rows.is_empty() {
        let h = area.height as usize;
        let off = crate::state::scroll_offset(browse.selected, browse.rows.len(), h, browse.offset.get());
        browse.offset.set(off);
        *ls.offset_mut() = off;
        ls.select(Some(browse.selected));
    }
    f.render_stateful_widget(list, area, &mut ls);
}

/// Cover rows per grid cell for a body `h` rows tall (`extra` caption rows beyond the
/// name): two grid rows on screen where
/// there is room, never under 3 (a sigil needs that much to read as one) nor over 8
/// (past that a cell is a poster and the grid stops being a grid).
fn grid_cover_rows(h: u16, extra: u16) -> u16 {
    (h / 2).saturating_sub(2 + extra).clamp(3, 8)
}

/// The Albums tab as a COVER GRID: each album a square cover with its name under it,
/// the cursor a reversed caption, and the grid scrolled by whole rows.
///
/// The cover is the decoded grid thumbnail in cells (sextants, or half-blocks under
/// `HYPODJ_ART_CELLS=half`), which every terminal draws, so the grid needs no image
/// protocol. Until a cover arrives, and for good when there is none or the server is
/// gone, the cell shows the album's SIGIL: deterministic per album, so a coverless grid
/// is still a grid of things you can tell apart rather than a wall of placeholders.
fn render_album_grid(f: &mut Frame, area: Rect, state: &TuiState) {
    let browse = &state.albums;
    let (head, area) = body_header(area);
    // The header names the album under the cursor IN FULL, because a caption is cut
    // to its cell and the one album he is about to act on must never be a guess.
    let mut title = browse.title.clone();
    if let Some(r) = browse.rows.get(browse.selected) {
        title.push_str(" \u{00b7} ");
        title.push_str(&r.label);
    }
    let hint = "  arrows move \u{00b7} l open \u{00b7} Enter play";
    if (area.width as usize) > title.chars().count() + hint.chars().count() + 4 {
        title.push_str(hint);
    }
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(title, Style::default().quiet()))),
        head,
    );
    let avail_rows = u16::from(browse.held_known);
    let rows = grid_cover_rows(area.height, avail_rows);
    let cover_w = square_art_cols(rows, state.sixel_cell_px).max(1);
    // A caption narrower than this names almost nothing, so a small cover sits
    // centred in a cell at least this wide rather than the name shrinking with it.
    let cap_w = cover_w.max(GRID_CAPTION_MIN);
    let cell_w = cap_w + 2;
    // Cover, caption, availability (when the store answered), one row of air.
    let cell_h = rows + 2 + avail_rows;
    let cols = ((area.width + 2) / cell_w).max(1) as usize;
    browse.grid_cols.set(cols);
    let n = browse.rows.len();
    let grid_rows = n.div_ceil(cols);
    let fit = ((area.height / cell_h) as usize).max(1);
    let sel_row = browse.selected / cols;
    let off = crate::state::scroll_offset(sel_row, grid_rows, fit, browse.offset.get());
    browse.offset.set(off);

    let bottom = area.y + area.height;
    let album_map = state.queued_by_album();
    let mut visible = Vec::new();
    for gr in off..(off + fit).min(grid_rows) {
        for gc in 0..cols {
            let i = gr * cols + gc;
            let Some(r) = browse.rows.get(i) else { break };
            let x = area.x + gc as u16 * cell_w;
            let y = area.y + (gr - off) as u16 * cell_h;
            if x + cap_w.min(area.width) > area.x + area.width || y >= bottom {
                continue;
            }
            let cap_w = cap_w.min(area.width);
            let cover = Rect {
                x: x + cap_w.saturating_sub(cover_w) / 2,
                y,
                width: cover_w.min(cap_w),
                height: rows.min(bottom - y),
            };
            let lines = match state.grid_art.get(&r.uri).and_then(|a| a.as_ref()) {
                Some(a) => a.lines(cover.width as usize, cover.height as usize),
                None => crate::sigil::Sigil::build(&r.uri, None, state.term_bg, state.truecolor)
                    .lines(cover.width as usize, cover.height as usize),
            };
            f.render_widget(Paragraph::new(lines), cover);
            if r.uri.starts_with("album/") {
                visible.push(r.uri.clone());
            }
            let cap_y = y + rows;
            if cap_y >= bottom {
                continue;
            }
            let glyph = if r.uri.starts_with("album/") {
                let queued = album_map.get(&r.uri).map(|s| s.len()).unwrap_or(0);
                queue_mark_glyph(album_mark(queued, r.song_count))
            } else {
                ' '
            };
            let name = if glyph == ' ' { r.label.clone() } else { format!("{glyph} {}", r.label) };
            let cap = ellipsize_words(&name, cap_w as usize);
            let (text, style) = if i == browse.selected {
                let pad = (cap_w as usize).saturating_sub(cap.chars().count());
                (format!("{cap}{}", " ".repeat(pad)), Style::default().reverse())
            } else if !browse.active(i, state.server_down()) {
                (cap, inactive_style())
            } else {
                (cap, Style::default())
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(text, style))),
                Rect { x, y: cap_y, width: cap_w, height: 1 },
            );
            // Whether it plays offline, in the same words the list used: the grid must
            // not lose what the list said just because it draws covers.
            if let Some(words) = availability_words(browse, r) {
                if cap_y + 1 < bottom {
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            ellipsize_words(&words, cap_w as usize),
                            Style::default().quiet(),
                        ))),
                        Rect { x, y: cap_y + 1, width: cap_w, height: 1 },
                    );
                }
            }
        }
    }
    *state.grid_visible.borrow_mut() = visible;
}

/// Does the body have room for the cover grid? Below this a grid shows a handful of
/// albums where the list shows a screenful (the redesign's list-yield bar: at least
/// 16 album rows at 80x24), so a small terminal keeps the dense list and a real-size
/// one gets covers.
fn grid_fits(body: Rect) -> bool {
    body.width >= GRID_MIN_W && body.height >= GRID_MIN_H
}

/// Minimum body size for the cover grid, in cells.
const GRID_MIN_W: u16 = 90;
const GRID_MIN_H: u16 = 20;

/// The narrowest a grid caption gets, in cells (see [`render_album_grid`]).
const GRID_CAPTION_MIN: u16 = 14;

/// An album dir row's availability in words (decision b): "on disk", "2 of 5 on
/// disk" or "stream only". `None` for anything that is not an album row, or when
/// the store did not answer - unknown is never drawn as "stream only".
fn availability_words(browse: &Browse, r: &crate::state::BrowseRow) -> Option<String> {
    if !browse.held_known || !r.is_dir || !r.uri.starts_with("album/") {
        return None;
    }
    let held = browse.held.get(&r.uri).copied().unwrap_or(0);
    Some(match r.song_count {
        _ if held == 0 => "stream only".to_string(),
        Some(n) if n > held => format!("{held} of {n} on disk"),
        _ => "on disk".to_string(),
    })
}

/// The DJ View pane (right of Queue on Screen::Dj): the Claude Code intelligence
/// surface. Bottom-pinned scrollback of coarse CC progress + result lines, a
/// spinner + phase row while a call is in flight, and the "ask>" NL input row.
/// The spinner rides the shared anim_secs clock; no token typewriter in the MVP.
fn render_dj(f: &mut Frame, area: Rect, state: &TuiState) {
    let (head, inner) = body_header(area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled("DJ - ask in words, Esc leaves", Style::default().quiet()))),
        head,
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    // The ambient context hint ("btw, DJ knows") rides its OWN row directly above the
    // ask> input - the "about to ask" position - but ONLY when a hint that earns a line
    // is present, so a resting deck (or an up-next hint the Up Next pane already shows)
    // reserves no row and the pane never jumps (mirrors the armed/field HUD discipline).
    // The row order is scrollback, spinner/phase, [hint], ask> input.
    let has_hint = state.now.hint.as_ref().is_some_and(|h| hint_earns_line(&h.kind));
    let mut constraints = vec![
        Constraint::Min(1),    // scrollback log
        Constraint::Length(1), // spinner + phase
    ];
    if has_hint {
        constraints.push(Constraint::Length(1)); // ambient hint
    }
    constraints.push(Constraint::Length(1)); // "ask>" input
    let rows = Layout::vertical(constraints).split(inner);
    let log_row = rows[0];
    let phase_row = rows[1];
    // The hint row (when present) sits between the phase row and the input row; the
    // input row is always the LAST row, whatever the dynamic insert did to the indices.
    let hint_row = if has_hint { Some(rows[2]) } else { None };
    let input_row = rows[rows.len() - 1];

    // Scrollback, bottom-pinned: show the last rows that fit.
    let log_h = log_row.height as usize;
    let mut visual: Vec<Line> = Vec::new();
    for entry in &state.dj_log {
        match entry {
            crate::state::DjEntry::Text(s) => visual.push(Line::from(s.clone())),
            crate::state::DjEntry::Spoken(spoken) => {
                visual.extend(hypodj_style::ratatui::render(
                    spoken,
                    state.term_bg,
                    state.truecolor,
                ));
            }
        }
    }
    let start = visual.len().saturating_sub(log_h);
    let log_lines: Vec<Line> = if visual.is_empty() {
        vec![Line::from(Span::styled(
            "ask me to DJ - e.g. \"fade out over 30s\"",
            Style::default().quiet(),
        ))]
    } else {
        visual[start..].to_vec()
    };
    f.render_widget(Paragraph::new(log_lines), log_row);

    // Spinner + phase line (only while a call is in flight).
    let phase_line = match &state.dj_phase {
        Some(phase) if !phase.is_empty() => {
            let frames = ['|', '/', '-', '\\'];
            // spin_secs is free-running (advances even when paused), so the spinner
            // keeps turning while a CC call is in flight regardless of play state.
            let spin = frames[hypodj_style::chrome_frame(state.spin_secs, frames.len())];
            Line::from(Span::styled(
                format!("{spin} {phase}"),
                Style::default().quiet(),
            ))
        }
        _ => Line::from(""),
    };
    f.render_widget(Paragraph::new(phase_line), phase_row);

    // The ambient "btw, DJ knows" hint row (present only when a hint exists). Plain
    // DIM - it must not compete with the confirm accent or the ask> caret; it is the
    // quietest, out-of-the-way voice, just above where the user is about to type.
    if let (Some(row), Some(hint)) = (hint_row, state.now.hint.as_ref()) {
        let line = Line::from(Span::styled(
            format!("btw, DJ knows: {}", hint.phrase()),
            Style::default().quiet(),
        ));
        f.render_widget(Paragraph::new(line), row);
    }

    // The "ask>" input row; place the caret when the DJ View has focus.
    let input_line = Line::from(vec![
        Span::styled("ask> ", Style::default().bold()),
        Span::raw(state.dj_input.clone()),
    ]);
    f.render_widget(Paragraph::new(input_line), input_row);
    // Keep the caret alive during the y/N confirm too (not just Normal): during
    // Mode::Confirm the pane must not read as dead while it waits for the keypress.
    if state.screen == Screen::Dj && matches!(state.mode, Mode::Normal | Mode::Confirm) {
        let caret_x = input_row.x + 5 + state.dj_input.chars().count() as u16;
        f.set_cursor_position((caret_x.min(input_row.x + input_row.width.saturating_sub(1)), input_row.y));
    }
}

/// Whether a DJ scrollback line is an interactive-prompt line that should render
/// DIFFERENTIATED (the `confirm? [y/N]` prompt and the y/n choice echo), so the
/// confirm reads as a live prompt rather than plain chat. Pure - unit-tested.
/// Whether to use unicode block glyphs for the ambient wave. Kept as a const so a
/// terminal without good unicode can flip to the ASCII fallback ramp at build time.
const USE_BLOCK_GLYPHS: bool = true;

/// The eight vertical block glyphs (U+2581 lower one-eighth .. U+2588 full block),
/// the wave's rest ramp - it reads as a soft equalizer/oscilloscope at idle.
const BLOCK_GLYPHS: [char; 8] = ['\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}'];

/// An ASCII fallback ramp (low -> high) for terminals without block glyphs.
const ASCII_GLYPHS: [char; 8] = ['.', ':', '-', '=', '+', '*', '#', '@'];

/// The active wave glyph ramp (block or ASCII fallback).
fn wave_glyphs() -> &'static [char; 8] {
    if USE_BLOCK_GLYPHS { &BLOCK_GLYPHS } else { &ASCII_GLYPHS }
}

/// Fold a per-track seed into a stable phase in `[0, TAU)` so each track gets its
/// own standing-wave texture (its wave looks distinct but never random per frame).
fn seed_phase(seed: u64) -> f64 {
    (seed % 100_000) as f64 / 100_000.0 * std::f64::consts::TAU
}

/// A cheap per-track seed: a hash of the current `file` (fallback `title`), so the
/// wave texture is stable for one track and changes when the track does. `0` when
/// nothing is playing (the caller freezes the wave there anyway).
fn track_seed(np: &NowPlaying) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match np.file.as_deref().or(np.title.as_deref()) {
        Some(s) => s.hash(&mut h),
        None => return 0,
    }
    h.finish()
}

/// Build one borderless ambient wave row `width` glyphs wide: a soft standing wave
/// of vertical block glyphs, driven ONLY by wall-clock `t_secs`, a per-track `seed`,
/// and whether to `animate`. Two summed incommensurate sines give a slowly-morphing,
/// never-repeating "breathing" envelope; `seed` shifts its phase per track. When
/// `animate` is false (paused/stopped/nothing) the row FREEZES to a flat baseline
/// (all the lowest glyph) - honest: it signals music is flowing + which track, never
/// pretends to know the beat or the position. Pure and deterministic - unit-tested.
fn wave_row(width: usize, t_secs: f64, seed: u64, animate: bool) -> String {
    let glyphs = wave_glyphs();
    if width == 0 {
        return String::new();
    }
    if !animate {
        return std::iter::repeat(glyphs[0]).take(width).collect();
    }
    let phase = seed_phase(seed);
    // Spatial wave numbers (per column) and temporal rates (rad/s). w1,w2 stay in
    // ~0.6-1.0 rad/s so the field drifts, never strobes.
    let (k1, k2, w1, w2) = (0.35_f64, 0.17_f64, 0.9_f64, 0.6_f64);
    let mut out = String::with_capacity(width);
    for x in 0..width {
        let xf = x as f64;
        let s = 0.6 * (xf * k1 + t_secs * w1 + phase).sin()
            + 0.4 * (xf * k2 - t_secs * w2 + phase * 1.7).sin();
        // s in [-1, 1]; a base + amp keeps the level off the row edges (never slams
        // 0 or 7 constantly). Round to a glyph index and clamp to the ramp.
        let level = (3.5 + 3.0 * s).round().clamp(0.0, 7.0) as usize;
        out.push(glyphs[level]);
    }
    out
}

/// Build one borderless bottom-bar row driven by the REAL post-gain audio level
/// `a` in `[0, 1]` (the smoothed ballistics envelope). The shipped two-sine standing
/// wave becomes the SPATIAL texture (so each track keeps its own look via `seed`),
/// now amplitude-DRIVEN instead of constant: a loud chorus rolls a full field, a
/// quiet verse a low ripple, a fade settles it toward the hairline. The ramp is
/// CAPPED at index 6 (`U+2587`), never the full block `U+2588`, so a permanent 1/8
/// sliver of background sits above the loudest wave (top breathing room). When
/// `playing` is false the row is a flat resting `▁` hairline with no motion (a frozen
/// animated wave reads as broken). Pure and deterministic - unit-tested.
fn level_wave_row(width: usize, t_secs: f64, seed: u64, a: f32, playing: bool) -> String {
    let glyphs = wave_glyphs();
    if width == 0 {
        return String::new();
    }
    // Not playing: a resting hairline (the lowest glyph), no motion.
    if !playing {
        return std::iter::repeat(glyphs[0]).take(width).collect();
    }
    let a = (a.clamp(0.0, 1.0)) as f64;
    let phase = seed_phase(seed);
    let (k1, k2, w1, w2) = (0.35_f64, 0.17_f64, 0.9_f64, 0.6_f64);
    let mut out = String::with_capacity(width);
    for x in 0..width {
        let xf = x as f64;
        let s = 0.6 * (xf * k1 + t_secs * w1 + phase).sin()
            + 0.4 * (xf * k2 - t_secs * w2 + phase * 1.7).sin();
        // Remap the texture s in [-1,1] -> [0.15, 1.0] so no column dies to zero
        // while playing (the field breathes, never gaps).
        let shape01 = 0.15 + (s + 1.0) / 2.0 * 0.85;
        // Height CAPPED at 6 (U+2587): the loudest music tops at 7/8, leaving the
        // top-of-row breathing sliver. The floor is index 0 (U+2581 hairline).
        let level = (6.0 * a * shape01).round().clamp(0.0, 6.0) as usize;
        out.push(glyphs[level]);
    }
    out
}

/// The thin, borderless bottom bar: the command/search prompt while typing, a status
/// banner when one is set, else the dim ambient wave when truly idle. Confirm renders
/// its own popup (see [`render_confirm_popup`]), so the bar stays blank there.
fn render_command(f: &mut Frame, area: ratatui::layout::Rect, state: &TuiState) {
    // Caret only in Command/Search mode. The row is borderless now, so the caret
    // math drops the old +1 border offsets: prompt_len = 2 for "> ", 1 for "/";
    // chars().count() (not len()) so multibyte input is not mis-placed.
    let caret: Option<(u16, usize)> = match state.mode {
        Mode::Command => Some((2, state.input.chars().count())),
        Mode::Search => Some((1, state.input.chars().count())),
        _ => None,
    };
    if let Some((prompt_len, input_chars)) = caret {
        f.set_cursor_position((area.x + prompt_len + input_chars as u16, area.y));
    }
    let line: Line = match state.mode {
        Mode::Command => Line::from(format!("> {}", state.input)),
        Mode::Search => Line::from(format!("/{}", state.input)),
        // Confirm's detail lives in the popup; keep the bar blank so it never grows.
        Mode::Confirm => Line::from(""),
        Mode::Normal => match &state.status_msg {
            // WRAPPED to the rows this area actually has (one, or two when the caller
            // lent it the blank breathing row), with any remaining truncation marked in
            // the text. `render` sizes the area; this only ever fills what it was given.
            Some(msg) => {
                if !state.status_parts.is_empty() {
                    let lines = styled_status_lines(
                        &state.status_parts,
                        area.width as usize,
                        area.height as usize,
                        state.term_bg,
                        state.truecolor,
                    );
                    f.render_widget(Paragraph::new(lines), area);
                    return;
                }
                let rows = status_lines(msg, area.width as usize, area.height as usize);
                let style = match state.status_state {
                    Some(st) => {
                        hypodj_style::ratatui::style_for_state(st, state.term_bg, state.truecolor)
                    }
                    None => Style::default(),
                };
                let lines: Vec<Line> =
                    rows.into_iter().map(|row| Line::from(Span::styled(row, style))).collect();
                f.render_widget(Paragraph::new(lines), area);
                return;
            }
            None => {
                // Truly idle: the bottom-bar wave. When the viz socket is live
                // (viz_active) draw the REAL post-gain level field; otherwise fall
                // back to the decorative wall-clock wave, animating only while
                // playing. Both are DIM so the row stays the least-prominent surface.
                // Reserve the right edge for the subtle "? help" hint, drawing the
                // wave into the remaining width so it never covers the hint.
                // Two leading spaces, matching the badge's own gap: with one, the badge
                // and the hint ran together into `...paused while streaming ? help`,
                // which reads as a single phrase ending in a question.
                const HINT: &str = "  ? help";
                let full = area.width as usize;
                let hint_len = if full > HINT.len() + 2 { HINT.len() } else { 0 };
                // THE STORE BADGE, left of the help hint: the passive answer to "is
                // the mirror running, held, or done?". Drawn only when the daemon
                // published one AND the bar keeps a real stretch of wave (a badge that
                // eats the whole row would be a status bar, not a badge). It is never
                // truncated into nonsense - it STEPS DOWN instead, to the short form
                // and then to nothing. `dj status` always carries the full sentence.
                //
                // Two forms rather than one, because dropping the badge outright made
                // a held mirror and a finished one look identical on a narrow terminal,
                // and silence reads as "nothing to report".
                //
                // Prefixed `offline:` and unbracketed: the numbers are meaningless
                // without a SUBJECT (a bare `80 of 360` on the same screen as
                // `track 1 of 5` invites exactly the wrong reading), and the brackets
                // added a frame to a row that is already dim and spatially separated
                // from the wave.
                let fits = |b: &String| full > hint_len + b.chars().count() + MIN_WAVE_CELLS;
                let store = state.now.store.as_deref();
                // THE RETRACTION OFFER takes the badge slot whenever there is one,
                // ahead of the store badge. It is the only badge here that EXPIRES -
                // the store's numbers will still be true in a minute, this offer will
                // not - and it is the one surface where the human decides whether to
                // keep what an agent just did. Its text is the daemon's own sentence,
                // rendered by the shared formatter, so nothing a model said can reach
                // it. When it does not fit it steps down to the bare key hint rather
                // than vanishing: a retraction the human never learns about is the
                // whole failure this badge exists to prevent.
                let badge = state
                    .undo_offer
                    .as_ref()
                    .map(|b| format!("  {b}"))
                    .filter(fits)
                    .or_else(|| Some("  u to undo".to_string()).filter(|_| state.undo_offer.is_some()).filter(fits))
                    .or_else(|| {
                        store
                            .and_then(store_badge)
                            .map(|b| format!("  offline: {b}"))
                            .filter(fits)
                    })
                    .or_else(|| {
                        store
                            .and_then(store_badge_short)
                            .map(|b| format!("  offline: {b}"))
                            .filter(fits)
                    });
                let badge_len = badge.as_ref().map_or(0, |b| b.chars().count());
                let width = full.saturating_sub(hint_len + badge_len);
                let seed = track_seed(&state.now);
                // ONE fact decides motion on BOTH paths: the transport state the card
                // itself is showing. The level path used to trust `viz_playing` alone -
                // a flag derived from the daemon's frames - so a stale or optimistic
                // slot could keep the bars bobbing under a deck that reads `stopped`.
                // Motion on this row means "music is flowing"; nothing but the deck's
                // own state gets to make that claim, and disagreeing with the line
                // directly above it is never the honest answer.
                let playing = state.now.state.as_deref() == Some("play");
                let wave = if state.viz_active {
                    level_wave_row(
                        width,
                        state.anim_secs,
                        seed,
                        state.viz_env,
                        state.viz_playing && playing,
                    )
                } else {
                    wave_row(width, hypodj_style::chrome_secs(state.anim_secs), seed, playing)
                };
                // Waveform color: an album swatch run through the INFO policy (>= 3:1
                // vs the detected bg, hue-preserving), replacing the flat DIM styling.
                // No cover -> stay DIM (no album hue to honor).
                let wave_style = match state.art_for_now() {
                    Some(a) => {
                        let c = hypodj_style::ratatui::info_color(
                            a.palette.vibrant,
                            state.term_bg,
                            state.truecolor,
                        );
                        Style::default().fg(c)
                    }
                    None => Style::default().quiet(),
                };
                let mut spans = vec![Span::styled(wave, wave_style)];
                if let Some(badge) = badge {
                    let quiet = hypodj_style::ratatui::style_for(
                        hypodj_style::Role::InkQuiet,
                        state.term_bg,
                        state.truecolor,
                    );
                    spans.push(Span::styled(badge, quiet));
                }
                if hint_len > 0 {
                    spans.push(Span::styled(
                        HINT,
                        hypodj_style::ratatui::style_for(
                            hypodj_style::Role::InkQuiet,
                            state.term_bg,
                            state.truecolor,
                        ),
                    ));
                }
                Line::from(spans)
            }
        },
    };
    f.render_widget(Paragraph::new(line), area);
}

/// The confirm surface: a small centered, bordered popup over the list/now region
/// (trust footnote / steps / note + the `confirm? [y/N]` prompt). A popup instead of
/// growing the thin bottom bar, so toggling into Confirm never jumps the layout.
fn render_confirm_popup(f: &mut Frame, region: Rect, state: &TuiState) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(p) = &state.pending {
        for spoken in p.utterances() {
            lines.extend(hypodj_style::ratatui::render(
                &spoken,
                state.term_bg,
                state.truecolor,
            ));
        }
    }

    // Size the popup to the content, clamped inside the region (with borders).
    let popup = centered_popup(region, &lines);
    let block = Block::default().borders(Borders::ALL).title("Confirm");
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// The Find (library query) screen: a borderless query line above a bordered
/// results list.
///
/// The band this owns is `height - 16` rows, because a `Length` constraint outranks
/// a `Min` in the outer layout, so at the 60x24 the test harness uses there are
/// exactly five content rows here. That arithmetic is why kind is a one-character
/// gutter sigil and the tallies live in the block title: per-kind header rows would
/// have spent three of those five on chrome.
fn render_find(f: &mut Frame, area: Rect, state: &TuiState) {
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(2)]).split(area);
    let (query_row, list_row) = (rows[0], rows[1]);
    let find = &state.find;

    // The query line, with a right-aligned hint that says what the keys do HERE.
    let hint = if find.drilling {
        "esc / h: back to hits".to_string()
    } else if let Some(indicator) = find.history_indicator() {
        indicator
    } else if find.focus == Focus::Query {
        "enter: search   ^v: history".to_string()
    } else {
        "tab: edit".to_string()
    };
    let prompt = format!("find> {}", find.query);
    let pad = (query_row.width as usize)
        .saturating_sub(prompt.chars().count() + hint.chars().count() + 1);
    let query_line = Line::from(vec![
        Span::styled("find> ", Style::default().bold()),
        Span::raw(find.query.clone()),
        Span::raw(" ".repeat(pad)),
        Span::styled(hint, Style::default().quiet()),
    ]);
    f.render_widget(Paragraph::new(query_line), query_row);

    // The drill borrows the ordinary browse renderer, so a drilled-in Find is
    // indistinguishable from the Albums tab.
    if find.drilling {
        // A drill with nothing yet would otherwise render an empty bordered box
        // carrying the artist's name for the whole round trip, which reads as "this
        // artist has no albums" rather than "still loading".
        if find.drill_loading && find.drill.rows.is_empty() {
            let frames = ['|', '/', '-', '\\'];
            let spin = frames[hypodj_style::chrome_frame(state.spin_secs, frames.len())];
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!("{spin} {}", find.drill.title));
            f.render_widget(block, list_row);
            return;
        }
        render_browse(f, list_row, &find.drill, state, true);
        return;
    }

    let title = match &find.phase {
        Phase::Loading(q) => {
            // spin_secs is free-running (it advances even on a stopped deck), so the
            // spinner turns while a query is in flight regardless of play state.
            let frames = ['|', '/', '-', '\\'];
            let spin = frames[hypodj_style::chrome_frame(state.spin_secs, frames.len())];
            format!("{spin} searching {q:?}")
        }
        Phase::Failed(reason) => format!("search failed: {reason}"),
        _ => find.tally_title(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(list_row);
    f.render_widget(block, list_row);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // The empty states are three DIFFERENT things and must not read alike: nothing
    // asked yet, a query that genuinely matched nothing, and a query still in
    // flight. The no-results line echoes the query verbatim because the protocol
    // cannot distinguish an empty result from an empty filter - the client knows
    // what it sent, so it says so.
    if find.hits.rows.is_empty() {
        let msg = match &find.phase {
            Phase::Cold => "type a query and press enter".to_string(),
            Phase::Loading(_) => String::new(),
            Phase::Failed(_) => String::new(),
            Phase::Done => format!("no results for {:?}", find.submitted),
        };
        if !msg.is_empty() {
            let line = Line::from(Span::styled(msg, Style::default().quiet()));
            f.render_widget(Paragraph::new(line).centered(), inner);
        }
        return;
    }

    // Rows go DIM while a refine is in flight: the stale answer stays legible and
    // still actionable rather than blanking the screen between keystrokes.
    let stale = matches!(find.phase, Phase::Loading(_));
    let height = inner.height as usize;
    let start =
        crate::state::scroll_offset(find.selected, find.hits.rows.len(), height, find.offset.get());
    find.offset.set(start);
    let lines: Vec<Line> = find
        .hits
        .rows
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(i, row)| {
            find_row_line(row, i == find.selected, stale, inner.width, state.highlight_query())
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// One hit row: a three-column gutter (`<kind sigil><queue mark><space>`), the
/// label, and a right-aligned trailer. The label is what truncates when the row is
/// too narrow, so the trailer never eats the name the user is reading.
fn find_row_line(
    row: &FindRow,
    selected: bool,
    stale: bool,
    width: u16,
    query: &str,
) -> Line<'static> {
    let gutter = format!("{}{} ", row.kind.sigil(), ' ');
    let width = width as usize;
    let trailer = row.trailer.clone();
    let room = width.saturating_sub(gutter.chars().count() + trailer.chars().count() + 1);
    let label: String = row.label.chars().take(room).collect();
    let pad = room.saturating_sub(label.chars().count()) + 1;
    let mut style = Style::default();
    if selected {
        style = style.reverse();
    }
    if stale {
        style = style.quiet();
    }
    // Underline the query inside the label, so the eye lands on WHY this row
    // matched - the same treatment the browse lists already give a `/` search.
    let hit = style.underline();
    let mut spans = vec![Span::styled(gutter, style)];
    spans.extend(match_spans(&label, query, style, hit));
    spans.push(Span::styled(format!("{}{trailer}", " ".repeat(pad)), style));
    Line::from(spans)
}

/// Draw `art` into `rect` as a SIXEL image, or return false to let the caller fall back
/// to the cell renderers.
///
/// THE REDRAW POLICY LIVES HERE. The payload is written as the SYMBOL of the rect's
/// top-left cell and every other cell of the rect is blank and SKIPPED. Two things fall
/// out of that, and both are the point:
///
///   - It is re-sent exactly when that cell's value changes, which means when the cover
///     changes or the pane geometry changes, and never otherwise. The draw loop runs at
///     up to 20 fps with an animated wave, so the frame diff is never empty; writing the
///     payload out of band would push it every frame at a terminal that deliberately
///     shortens its processing slice after decoding an image.
///
///   - Skipped cells emit NOTHING on the wire, so the rest of the frame cannot print
///     into the image. That is not an optimisation here: images are cell-anchored in
///     this terminal, so one printed character permanently takes that cell away from
///     the image and eats a hole in the cover.
///
/// Skip has to be re-applied every frame because the buffer is reset between frames.
fn render_sixel_art(
    f: &mut Frame,
    rect: Rect,
    art: &crate::art::AlbumArt,
    cell_px: Option<(u16, u16)>,
    gen: bool,
    held: &std::cell::RefCell<Option<crate::state::SixelHeld>>,
    covered: bool,
) -> bool {
    let Some(payload) = sixel_payload(f, rect, art, cell_px) else {
        return false;
    };
    let cw_ch = cell_px.unwrap_or((0, 0));

    // A trailing SGR reset when the generation is flipped: it renders as nothing at all,
    // and its only job is to make this frame's symbol differ from the one the diff
    // already sent, which is what forces a covered image to be re-sent (see
    // `TuiState::sixel_gen`).
    // While an overlay is drawn, PIN the symbol to what is already on screen: a cover
    // that changes under a popup would otherwise re-send the image straight over it (see
    // `TuiState::sixel_held`). Otherwise take the current payload, with the generation
    // mixed in as a trailing SGR reset that renders as nothing and exists only to differ
    // from what the diff already sent.
    //
    // The pin is only valid for the geometry it was encoded for. A resize while the
    // overlay is up moves or rescales the pane, and the pinned payload would then be
    // emitted at a new anchor with the OLD pixel size - a mis-sized image, and drawn
    // over the popup to boot. There is no correct sixel to send under a popup, so fall
    // back to the cell renderer until it closes; the close frame flips the generation
    // and re-sends a payload encoded for the new geometry.
    let geometry = (rect, cw_ch);
    let pinned = match (covered, held.borrow().as_ref()) {
        (true, Some(h)) if h.geometry == geometry => Some(h.payload.clone()),
        (true, Some(_)) => return false,
        _ => None,
    };
    let symbol: String = match pinned {
        Some(p) => p,
        None if gen => format!("{payload}\x1b[0m"),
        None => payload.to_string(),
    };
    if !covered {
        *held.borrow_mut() = Some(crate::state::SixelHeld { geometry, payload: symbol.clone() });
    }
    write_sixel_cells(f, rect, symbol);
    true
}

/// The sixel payload for `art` at `rect`'s native pixel size, or `None` when sixel is
/// off, the cell size is unknown, or the rect would be truncated or scroll the screen.
fn sixel_payload(
    f: &Frame,
    rect: Rect,
    art: &crate::art::AlbumArt,
    cell_px: Option<(u16, u16)>,
) -> Option<std::sync::Arc<str>> {
    let (cw, ch) = sixel_payload_gate(f, rect, cell_px)?;
    let payload = art.sixel(rect.width as u32 * cw as u32, rect.height as u32 * ch as u32);
    (!payload.is_empty()).then_some(payload)
}

/// The cell pixel size when a sixel may be drawn into `rect` at all: the protocol is
/// on, the cell size is known, and the rect neither truncates nor scrolls the screen.
fn sixel_payload_gate(f: &Frame, rect: Rect, cell_px: Option<(u16, u16)>) -> Option<(u16, u16)> {
    use crate::art::{art_mode, ArtMode};

    if matches!(art_mode(), ArtMode::Sextant | ArtMode::Half) {
        return None;
    }
    let (cw, ch) = cell_px?;
    if cw == 0 || ch == 0 || rect.width < 2 || rect.height < 1 {
        return None;
    }
    // An image wider than the screen is TRUNCATED by the terminal rather than scaled,
    // and one whose last row is the bottom row scrolls the whole alternate screen.
    // Refusing is better than either.
    let area = f.area();
    if rect.x + rect.width > area.width || rect.y + rect.height >= area.height {
        return None;
    }
    Some((cw, ch))
}

/// Draw an unpinned sixel (the info card's cover) for a cover of `full_rows` cell rows of which only `rect` is in
/// view, the first `skip_rows` having scrolled off above it: encodes just that band.
fn paint_sixel_rows(
    f: &mut Frame,
    rect: Rect,
    art: &crate::art::AlbumArt,
    cell_px: Option<(u16, u16)>,
    skip_rows: u16,
    full_rows: u16,
) -> bool {
    let full = Rect { height: full_rows, ..rect };
    // The geometry gate (protocol on, cell size known, the visible rect on screen) is
    // the same as an unscrolled cover's; only the encoded band differs.
    if sixel_payload_gate(f, rect, cell_px).is_none() {
        return false;
    }
    let (cw, ch) = cell_px.unwrap_or((0, 0));
    let payload = art.sixel_rows(
        full.width as u32 * cw as u32,
        full.height as u32 * ch as u32,
        skip_rows as u32 * ch as u32,
        rect.height as u32 * ch as u32,
    );
    if payload.is_empty() {
        return false;
    }
    write_sixel_cells(f, rect, payload.to_string());
    true
}

/// Put `payload` in `rect`'s top-left cell and blank-and-skip every other cell of it.
fn write_sixel_cells(f: &mut Frame, rect: Rect, payload: String) {
    let buf = f.buffer_mut();
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            let cell = &mut buf[(x, y)];
            if x == rect.x && y == rect.y {
                // The one cell that carries the payload. Not skipped, so the diff sees
                // it change when the cover or the geometry does.
                cell.set_symbol(&payload);
            } else {
                cell.reset();
                cell.set_skip(true);
            }
        }
    }
}

#[cfg(test)]
mod sixel_render_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn payloads(t: &Terminal<TestBackend>) -> Vec<(u16, u16, String)> {
        let buf = t.backend().buffer();
        let w = buf.area.width;
        buf.content()
            .iter()
            .enumerate()
            .filter(|(_, c)| c.symbol().starts_with("\x1bP"))
            .map(|(i, c)| ((i as u16) % w, (i as u16) / w, c.symbol().to_string()))
            .collect()
    }

    /// The info card's cover must be drawn at the SAME fidelity as Now Playing.
    ///
    /// Reported from a live screenshot: the `i` card on "Loro" showed the Junjo cover
    /// as coarse blocks while the Now Playing cover beside it was sharp, because the
    /// card always used the sextant cell renderer (20x10 cells, ~40x30 effective
    /// pixels). With sixel available the card now carries its own native-pixel sixel,
    /// encoded from the card's art, placed inside the popup - and the Now Playing
    /// image stays pinned to the payload already on screen.
    #[test]
    fn the_info_card_cover_is_a_sixel_when_the_terminal_draws_sixel() {
        let mut terminal = Terminal::new(TestBackend::new(129, 40)).unwrap();
        let mut s = TuiState::new();
        s.sixel_cell_px = Some((8, 17));
        s.art = Some(crate::art::AlbumArt::for_test_solid([10, 20, 30]));
        s.now.file = Some("song/1".into());
        s.art_album = Some(s.album_identity());
        s.now.title = Some("The Peacocks".into());
        s.now.state = Some("play".into());
        terminal.draw(|f| render(f, &s)).unwrap();
        let np = payloads(&terminal);
        assert_eq!(np.len(), 1, "only the Now Playing cover before the card opens");

        s.card = Some(vec![("title".into(), "Loro".into())]);
        s.card_art = Some(crate::art::AlbumArt::for_test_solid([200, 40, 40]));
        terminal.draw(|f| render(f, &s)).unwrap();
        let all = payloads(&terminal);
        assert_eq!(all.len(), 2, "the card adds its own sixel: {:?}", all.iter().map(|p| (p.0, p.1)).collect::<Vec<_>>());
        let card = all.iter().find(|p| (p.0, p.1) != (np[0].0, np[0].1)).expect("a second anchor");
        assert_ne!(card.2, np[0].2, "encoded from the card's art, not the playing cover");
        let still = all.iter().find(|p| (p.0, p.1) == (np[0].0, np[0].1)).unwrap();
        assert_eq!(still.2, np[0].2, "Now Playing stays pinned while the card is up");
        // Square on real cell pixels: 10 rows of 17px wants 21 columns of 8px, and the
        // payload is encoded at exactly that native size.
        assert!(card.2.contains("\"1;1;168;170"), "a square 168x170 px card cover, even on a one-row card");
    }

    /// Scrolled, the cover is partly out of view. A cell-anchored sixel cannot be
    /// clipped by the terminal, so the card sends ONLY the visible band of the cover,
    /// at native pixels and anchored at the first inner row - never the pixelated cell
    /// art, which is only for a terminal with no image protocol.
    #[test]
    fn a_scrolled_info_card_keeps_the_visible_part_of_the_cover_sharp() {
        let mut terminal = Terminal::new(TestBackend::new(129, 16)).unwrap();
        let mut s = TuiState::new();
        s.sixel_cell_px = Some((8, 17));
        s.card = Some((0..12).map(|i| (format!("k{i}"), format!("v{i}"))).collect());
        s.card_art = Some(crate::art::AlbumArt::for_test_solid([200, 40, 40]));
        terminal.draw(|f| render(f, &s)).unwrap();
        assert!(s.card_scroll.max.get() > 0, "the card scrolls at this height");
        let top = payloads(&terminal);
        assert_eq!(top.len(), 1, "unscrolled: the cover is a sixel");
        // A 129x16 terminal: the 14-row inner box shows all 10 cover rows.
        assert!(top[0].2.contains("\"1;1;168;170"), "the whole cover at native size");

        s.card_scroll.scroll = 3;
        terminal.draw(|f| render(f, &s)).unwrap();
        let p = payloads(&terminal);
        assert_eq!(p.len(), 1, "scrolled: still one sharp sixel, not cell art");
        assert_eq!((p[0].0, p[0].1), (top[0].0, top[0].1), "anchored at the first inner row");
        // 7 of 10 rows visible: 7 x 17 = 119 px tall, same 168 px width.
        assert!(p[0].2.contains("\"1;1;168;119"), "only the visible band is encoded");
        // No sextant cell art painted where the cover is.
        let row = (top[0].1..top[0].1 + 7)
            .map(|y| terminal.backend().buffer()[(top[0].0 + 1, y)].symbol().to_string())
            .collect::<String>();
        assert!(row.trim().is_empty(), "the band is skipped cells under the image: {row:?}");

        // Scrolled past the cover entirely: nothing to draw.
        s.card_scroll.scroll = 10;
        terminal.draw(|f| render(f, &s)).unwrap();
        assert!(payloads(&terminal).is_empty(), "no cover rows in view, no sixel");
    }

    /// Without an image protocol (no cell pixel size) the card falls back to cell art.
    #[test]
    fn an_info_card_without_an_image_protocol_uses_cell_art() {
        let mut terminal = Terminal::new(TestBackend::new(129, 16)).unwrap();
        let mut s = TuiState::new();
        s.sixel_cell_px = None;
        s.card = Some((0..12).map(|i| (format!("k{i}"), format!("v{i}"))).collect());
        s.card_art = Some(crate::art::AlbumArt::for_test_solid([200, 40, 40]));
        s.card_scroll.scroll = 3;
        terminal.draw(|f| render(f, &s)).unwrap();
        assert!(payloads(&terminal).is_empty(), "no protocol, no sixel");
    }

    fn art() -> crate::art::AlbumArt {
        crate::art::AlbumArt::for_test_solid([200, 60, 60])
    }

    /// An overlay that covers the cover must leave the image REPAINTED, not holed.
    ///
    /// A sixel image is cell-anchored: a popup drawn over it prints text into cells the
    /// terminal is showing pixels in, and those pixels are destroyed. Closing the popup
    /// restores the buffer to blank-and-skipped, and the backend never draws a skipped
    /// cell - so unless the PAYLOAD cell changes, ratatui's diff concludes there is
    /// nothing to send and the cover keeps the popup's silhouette punched out of it
    /// until the track changes.
    ///
    /// This drives three real frames through a real `Terminal`, so it is ratatui's own
    /// diff answering, not an assertion about our intent. Without the fix the frame-3
    /// payload is byte-identical to frame 1 and this fails on the last assert.
    #[test]
    fn an_overlay_over_the_cover_forces_the_image_to_be_resent_when_it_closes() {
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        let mut s = TuiState::new();
        s.sixel_cell_px = Some((8, 17));
        s.art = Some(art());
        s.now.file = Some("song/1".into());
        s.art_album = Some(s.album_identity());
        s.now.title = Some("Future Proof".into());
        s.now.state = Some("play".into());

        let payload_of = |t: &Terminal<TestBackend>| -> Option<String> {
            let buf = t.backend().buffer();
            buf.content()
                .iter()
                .map(|c| c.symbol().to_string())
                .find(|sym| sym.starts_with("\x1bP"))
        };

        // Frame 1: no overlay. The image is on screen.
        terminal.draw(|f| render(f, &s)).unwrap();
        let first = payload_of(&terminal).expect("the cover is drawn as a sixel payload");

        // Frame 2: the menu is open over it, which is what destroys the pixels.
        s.menu = Some(crate::menu::Menu::new(
            crate::menu::Target {
                kind: crate::menu::TargetKind::LibrarySong,
                origin: crate::menu::Origin::Queue { pos: 0 },
                label: "Future Proof".into(),
                uri: Some("song/s1".into()),
                album_uri: Some("album/a1".into()),
                artist: Some("Massive Attack".into()),
                artist_uri: None,
                match_uri: None,
            },
            1,
        ));
        terminal.draw(|f| render(f, &s)).unwrap();
        assert!(s.sixel_covered.get(), "the frame recorded that an overlay covered it");

        // Frame 3: the menu closes. The payload must DIFFER from frame 1, because that
        // difference is the only thing that makes the diff re-send the image.
        s.menu = None;
        terminal.draw(|f| render(f, &s)).unwrap();
        let third = payload_of(&terminal).expect("the cover is still drawn");
        assert_ne!(
            first, third,
            "the payload cell must change on the frame the overlay closes, or the diff \
             never re-sends the image and the holes stay"
        );
        // And the change is invisible: same image, plus an SGR reset that renders as
        // nothing. Anything else would be a different picture, not a repaint.
        assert!(third.starts_with(&first) || first.starts_with(&third));
    }

    /// A cover that changes UNDER an open popup must not re-send the image.
    ///
    /// The failure this pins is silent and self-healing, which is what makes it easy to
    /// dismiss: a track advances while the menu is open, the payload cell is not under
    /// the popup so its symbol changes, the diff re-sends the image, and the terminal
    /// paints the new cover's pixels straight over the popup. ratatui never repairs it -
    /// the popup's own cells did not change, so it does not redraw them - and the box
    /// stays erased until some later keypress happens to dirty those rows.
    ///
    /// Measured at the buffer, which is what the diff reads: while an overlay is drawn
    /// the payload symbol must be IDENTICAL frame to frame, whatever the cover does.
    #[test]
    fn a_cover_that_changes_under_an_open_popup_is_not_resent_over_it() {
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        let mut s = TuiState::new();
        s.sixel_cell_px = Some((8, 17));
        s.now.state = Some("play".into());
        s.now.title = Some("A".into());
        s.art = Some(art());
        s.now.file = Some("song/1".into());
        s.art_album = Some(s.album_identity());

        let payload_of = |t: &Terminal<TestBackend>| -> Option<String> {
            t.backend().buffer().content().iter()
                .map(|c| c.symbol().to_string())
                .find(|sym| sym.starts_with("\x1bP"))
        };

        terminal.draw(|f| render(f, &s)).unwrap();
        assert!(payload_of(&terminal).is_some(), "the cover is drawn as a sixel payload");

        s.menu = Some(crate::menu::Menu::new(
            crate::menu::Target {
                kind: crate::menu::TargetKind::LibrarySong,
                origin: crate::menu::Origin::NowPlaying,
                label: "A".into(),
                uri: Some("song/s1".into()),
                album_uri: None,
                artist: None,
                artist_uri: None,
                match_uri: None,
            },
            0,
        ));
        terminal.draw(|f| render(f, &s)).unwrap();
        let under_popup = payload_of(&terminal);

        // The track advances while the menu is still up: a DIFFERENT cover.
        s.art = Some(crate::art::AlbumArt::for_test_solid([20, 40, 220]));
        s.now.file = Some("song/1".into());
        s.art_album = Some(s.album_identity());
        terminal.draw(|f| render(f, &s)).unwrap();
        assert_eq!(
            under_popup,
            payload_of(&terminal),
            "the payload must be pinned while a popup is drawn, or the image is re-sent \
             over it"
        );

        // Closing the popup re-sends, and now it is the CURRENT cover, not the pinned one.
        s.menu = None;
        terminal.draw(|f| render(f, &s)).unwrap();
        let after = payload_of(&terminal);
        assert_ne!(under_popup, after, "the close frame re-sends the image");
        assert!(after.is_some(), "and there is still a cover on screen");
    }

    /// m2zhfld: the pin is keyed on GEOMETRY too. Resizing while a popup is open must
    /// never emit the held payload, which was encoded for the old pixel size; the pane
    /// falls back to cells under the popup, and the close re-sends at the new size.
    #[test]
    fn a_resize_under_an_open_popup_never_emits_the_stale_geometry_payload() {
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        let mut s = TuiState::new();
        s.sixel_cell_px = Some((8, 17));
        s.now.state = Some("play".into());
        s.now.title = Some("A".into());
        s.art = Some(art());
        s.now.file = Some("song/1".into());
        s.art_album = Some(s.album_identity());
        let payload_of = |t: &Terminal<TestBackend>| -> Option<String> {
            t.backend().buffer().content().iter()
                .map(|c| c.symbol().to_string())
                .find(|sym| sym.starts_with("\x1bP"))
        };
        terminal.draw(|f| render(f, &s)).unwrap();
        let before = payload_of(&terminal).expect("the cover is drawn as a sixel payload");

        s.menu = Some(crate::menu::Menu::new(
            crate::menu::Target {
                kind: crate::menu::TargetKind::LibrarySong,
                origin: crate::menu::Origin::NowPlaying,
                label: "A".into(),
                uri: Some("song/s1".into()),
                album_uri: None,
                artist: None,
                artist_uri: None,
                match_uri: None,
            },
            0,
        ));
        terminal.draw(|f| render(f, &s)).unwrap();

        // The terminal is resized with the popup still up, wide enough that the deck
        // becomes the rail and the cover changes size (a strip-to-strip resize keeps
        // the strip's cover geometry, so its pin would rightly still hold).
        terminal.backend_mut().resize(129, 40);
        terminal.resize(Rect::new(0, 0, 129, 40)).unwrap();
        terminal.draw(|f| render(f, &s)).unwrap();
        assert_ne!(
            payload_of(&terminal).as_deref(),
            Some(before.as_str()),
            "the payload encoded for the old geometry must not be emitted at the new one"
        );

        s.menu = None;
        terminal.draw(|f| render(f, &s)).unwrap();
        let after = payload_of(&terminal).expect("the close frame re-sends the cover");
        assert_ne!(after, before, "re-encoded for the new geometry");
    }

    /// The payload goes in ONE cell and every other cell of the rect is skipped.
    ///
    /// Both halves matter. The payload cell is what the frame diff watches, so it
    /// must not be skipped; the rest must be, because skipped cells emit nothing and
    /// images are cell-anchored in this terminal - a single character printed into
    /// the rect permanently takes that cell away from the image.
    #[test]
    fn payload_occupies_one_cell_and_the_rest_are_skipped() {
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        let a = art();
        let rect = Rect { x: 2, y: 1, width: 8, height: 4 };

        // Assert on RATATUI's buffer, inside the closure. The backend's buffer is
        // what was actually drawn, and `skip` exists precisely to stop a cell being
        // drawn, so a skipped cell never appears there at all.
        terminal
            .draw(|f| {
                assert!(render_sixel_art(f, rect, &a, Some((8, 17)), false, &std::cell::RefCell::new(None), false));

                let buf = f.buffer_mut();

                let payload = buf[(rect.x, rect.y)].symbol().to_string();
                assert!(payload.starts_with("\x1bP"), "top-left cell must carry the DCS");
                assert!(!buf[(rect.x, rect.y)].skip, "the payload cell must NOT be skipped");

                for y in rect.y..rect.y + rect.height {
                    for x in rect.x..rect.x + rect.width {
                        if x == rect.x && y == rect.y {
                            continue;
                        }
                        assert!(buf[(x, y)].skip, "cell {x},{y} in the art rect must be skipped");
                    }
                }
            })
            .unwrap();

        // And the consequence that actually matters: the skipped cells emitted
        // nothing, so the backend never received anything in the art rect beyond
        // the payload cell.
        let drawn = terminal.backend().buffer().clone();
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                if x == rect.x && y == rect.y {
                    continue;
                }
                assert_eq!(
                    drawn[(x, y)].symbol(),
                    " ",
                    "cell {x},{y} was drawn into the image and would eat a hole in it"
                );
            }
        }
    }

    /// The redraw policy: an unchanged cover at an unchanged geometry must produce
    /// an IDENTICAL payload, so the frame diff has nothing to send.
    ///
    /// This is the property the whole design rests on. The draw loop runs
    /// unconditionally at up to 20 fps with an animated wave, so if the payload
    /// string varied per frame the terminal would be handed a fresh image twenty
    /// times a second.
    #[test]
    fn an_unchanged_cover_yields_an_identical_payload() {
        let a = art();
        let first = a.sixel(64, 68);
        let second = a.sixel(64, 68);
        assert_eq!(first, second, "same cover and geometry must give the same bytes");
        assert!(std::sync::Arc::ptr_eq(&first, &second), "and it must be cached, not re-encoded");

        // A different geometry is a different image and must re-encode.
        let resized = a.sixel(80, 68);
        assert_ne!(first, resized);
    }


    /// A terminal that answers DA1 but has not published a pixel geometry yet
    /// must still get images once it does.
    ///
    /// A freshly mapped window reports correct character dimensions at once but
    /// takes 20 to 100 ms to report pixel dimensions, and 4 cold launches in 6
    /// were measured reporting 0x0 at first. The startup probe lands inside
    /// that window, so a session that asked once and never asked again lost
    /// images for its whole lifetime - which is exactly the intermittent
    /// "no cover in a new window" report.
    ///
    /// This pins the RENDER side of that contract: sixel is refused while the
    /// geometry is missing, and drawn once it arrives, with no other state
    /// having changed.
    #[test]
    fn a_late_pixel_geometry_still_yields_images() {
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        let a = art();
        let rect = Rect { x: 0, y: 0, width: 8, height: 4 };

        // The window has not been allocated yet: no geometry, so no image.
        terminal
            .draw(|f| {
                assert!(
                    !render_sixel_art(f, rect, &a, None, false, &std::cell::RefCell::new(None), false),
                    "must refuse while the geometry is unknown"
                );
            })
            .unwrap();

        // The compositor allocates it and a cell size appears. Nothing else
        // changed, and the image must now draw.
        terminal
            .draw(|f| {
                assert!(
                    render_sixel_art(f, rect, &a, Some((8, 17)), false, &std::cell::RefCell::new(None), false),
                    "must draw once the geometry arrives"
                );
                assert!(f.buffer_mut()[(0, 0)].symbol().starts_with("\x1bP"));
            })
            .unwrap();
    }

    /// Refusals fall back rather than drawing something wrong.
    #[test]
    fn refuses_when_it_cannot_draw_safely() {
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        let a = art();

        terminal
            .draw(|f| {
                // No cell geometry: the terminal never told us how big a cell is.
                assert!(!render_sixel_art(f, Rect { x: 0, y: 0, width: 8, height: 4 }, &a, None, false, &std::cell::RefCell::new(None), false));

                // One column wide: the backend only emits MoveTo when the next cell
                // is not (x+1, y), so the cell right of the payload has to exist.
                assert!(!render_sixel_art(f, Rect { x: 0, y: 0, width: 1, height: 4 }, &a, Some((8, 17)), false, &std::cell::RefCell::new(None), false));

                // Past the right edge: this terminal TRUNCATES a too-wide image
                // rather than scaling it.
                let w = f.area().width;
                assert!(!render_sixel_art(f, Rect { x: w - 2, y: 0, width: 8, height: 4 }, &a, Some((8, 17)), false, &std::cell::RefCell::new(None), false));

                // Last row: an image whose bottom lands on the bottom row scrolls
                // the whole alternate screen.
                let h = f.area().height;
                assert!(!render_sixel_art(f, Rect { x: 0, y: h - 1, width: 8, height: 1 }, &a, Some((8, 17)), false, &std::cell::RefCell::new(None), false));
            })
            .unwrap();
    }
}
