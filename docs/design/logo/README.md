# HypoDJ app icon

Icon id: **`hypodj`** (plus `hypodj-symbolic`). Every HypoDJ desktop surface uses
this one id - the `dj-gui` desktop entry, the GNOME search provider, and the GTK
app `djg` (branch `agents/w16-gtk`): set `Icon=hypodj` in its `.desktop` file and
`gtk::Window::set_default_icon_name("hypodj")`, and install nothing new - the
icons ship from `nix/clients.nix` into `share/icons/hicolor/{scalable,symbolic}/apps/`.

Sources: `crates/hypodj-search/data/icons/hicolor/`.

## The pick: the ember disc (`candidates/a-ember-disc.svg`)

A warm-dark record (`substrate` #1a120a, thin lower depth edge per the GNOME HIG)
whose label is the one owned colour, ember (oklch 0.74 0.15 60, graded into the
Familiar Tools copper #C17A2F). It is the token decision made literal: the room
(the record, the OS) stays dark and quiet, HypoDJ owns one warm instrument - the
furnace underneath. At 16 px it still reads as a glowing coal in a dark ring,
which no generic player icon does.

Rejected:

- `b-falling-wave` - descending bars on paper (wind-down). Reads as an equaliser
  or an analytics chart; at 16 px it is a bar graph, and the cream tile is lost on
  light panels.
- `c-ember-note` - an ember-headed note on a dark squircle. Legible, but a note
  is the generic music glyph every player already uses.

Renders at 128/32/16 px sit beside each SVG in `candidates/`. The symbolic
variant (`symbolic-16.png`) is the disc's grammar in one colour: ring, groove, spindle.
