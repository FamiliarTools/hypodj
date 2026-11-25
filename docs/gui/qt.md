# djq - HypoDJ on Qt

`djq` is the Qt 6 / QML app (`crates/hypodj-qt`). It is the same feature slice as
`djg` (README.md here): the now-playing instrument with transport and volume, Up
Next with on disk / stream only, Albums with the `store albums` offline fallback,
Search, the server status in words, and the ask entry that answers questions and
asks before it acts. Icon id `hypodj`; desktop id `tech.familiartools.HypoDJ.Qt`.

## Look

Decided (Memoria t:01m8678): the chrome is the host's, the content is HypoDJ's.
djq never sets a Qt Quick Controls style, a `QPalette` or an accent, so the tool
bar, tabs, lists, fields and dialogs are whatever the host picked (the Breeze /
Kirigami desktop style on Plasma, the platform theme's palette with Fusion
elsewhere, `QT_QUICK_CONTROLS_STYLE` when set). The one HypoDJ-owned surface is the
instrument: its ground, inks and ember come from `hypodj_style::qt::instrument`,
which is the GTK adapter's fitting code (a test asserts the two shells produce the
same four colours), so the instrument overrides the host accent and never uses
red. The room's lightness is read from the host palette (`palette.window`), so the
instrument follows a dark host to its dark ground.

## Shared core

`view.rs` and `worker.rs` moved out of `hypodj-gtk` into `crates/hypodj-app`, a
toolkit-free crate over `hypodj-client`. Both shells run the same socket worker
(one persistent command connection, so `nl confirm` lands on the connection that
translated), the same ask routing and the same words; each shell only folds
`worker::Msg` into its own widgets. Neither depends on `hypodj-core`.

## Binding: sketch-compare

Judged on Nix buildability, maintenance, and staying on hypodj-client only.

- **cxx-qt 0.10 + QML (chosen).** KDAB-maintained, Qt 6 first, regular releases.
  Already proven on this machine's nixpkgs by Spirivia's shipped Qt frontend,
  including the one Nix trap: cxx-qt-build finds Qt through ONE `qmake -query`
  prefix while nixpkgs splits qtbase and qtdeclarative, so `QMAKE` must point at a
  `qt6.env` joining them, re-exported in `preConfigure` because the qtbase setup
  hook overwrites it (Memoria m:01m7tc3; `nix/qt.nix`). The Rust side is a single
  QObject with JSON properties; QML is declarative and leaves the style to the
  host. The only C++ is a 15-line shim (desktop id + window icon, window grab).
- **qmetaobject-rs.** Pure-Rust QObject macros, but finds Qt via `qmake` and
  compiles C++ through the `cpp` crate's inline macros at build time (the same Nix
  prefix problem, without an escape hatch as clean). Its Qt 6 support trails, and
  it is one maintainer with slow releases. More risk for no gain here.
- **Minimal C++ shell over a Rust lib (CMake + a C ABI).** Most control, but two
  build systems, a hand-written FFI layer, a CMake derivation beside the cargo one,
  and the words would have to cross a C boundary as strings anyway. Highest
  maintenance for the same QML on top.

## Proof

`scripts/djq-screens.sh <djq> <hypodj>` renders every view against a SILENT probe
daemon (audio null, MPRIS off, throwaway store capped at ~120 MB, port 6691) on a
private Xvfb `:94` with Qt forced to xcb and the software scene graph, under
`env -i` with a private HOME / XDG_RUNTIME_DIR and its own D-Bus. The grab is the
window's own frame (`QQuickWindow::grabWindow`), never the screen. Screens:
`screens/qt/`. The dark shot uses the Material style in its dark theme to stand in
for a dark host (the pale disc in that shot is Material's own tab ripple caught
mid-animation, chrome rather than content).

## Not yet

- High contrast: Qt Quick has no portable contrast preference before Qt 6.10's
  `styleHints.accessibility`; `setRoom` takes the flag, QML passes `false` for now.
- Glyph transport buttons (text, not themed icons) so they render on any host
  without an icon theme; switch to `icon.name` once a fallback theme ships.
- Same follow-ups as djg: AT-SPI audit, appstream metadata, breathing motion,
  album rows' `X-Offline`.
