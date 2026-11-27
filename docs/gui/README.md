# djg - HypoDJ on GNOME

`djg` is the GTK4 + libadwaita app (`crates/hypodj-gtk`). `dj-gui` stays the TUI.

## Shape

The room is the OS and HypoDJ owns one warm instrument (`docs/design/tokens.md`,
section 0). Chrome is stock libadwaita: header bar with a view switcher (Playing,
Albums, Search), boxed lists, alert dialogs, a bottom switcher bar under 600sp. The
only HypoDJ-styled surface is the now-playing `.instrument`, styled by
`hypodj_style::gtk_css` through private `--hypodj-*` properties.

Two shapes were compared before code:

- A. One persistent command socket on a std thread, plus idle / art / search /
  albums side threads, folding typed messages into widgets on the main loop.
- B. Each view fetches for itself on short-lived sockets via `gio` blocking tasks.

A won: the daemon stamps a per-connection owner key, so `nl confirm` only works on
the socket that ran the translate, and B would scatter IO (and its error words)
across every view. Slow, stateless reads (art, search, the whole-library album
listing) still get their own sockets so they can never delay a transport press.
The words come from `hypodj-client` and `hypodj_app::view`, the same functions as the TUI.

## Proof

`scripts/djg-screens.sh <djg> <hypodj>` renders every view against a SILENT probe
daemon (audio null, MPRIS off, throwaway store capped at ~120 MB, port 6690) on a
private Xvfb `:93`, with `env -i`, a private HOME / XDG_RUNTIME_DIR and its own
D-Bus. Screens: `screens/`.

## Keyboard and offline

- Type to search: typing anywhere outside a text field opens Search with the key
  already in the entry; Escape clears it and returns to the page and widget focus
  came from. Ctrl+F, Ctrl+? (shortcuts window), Ctrl+W, Ctrl+Q, Alt+Left, F10.
  djq does the same by hand (Qt Quick has no key-capture widget).
- Offline rows: with the server down (`view::server_down`), only rows the mirror
  holds stay active (`view::row_active`); the rest are greyed, insensitive and
  skipped by keyboard focus. Album tracks carry `X-Offline` from the daemon.
- HIG review and its fixes: `hig-review.md`.

## Not yet

- Flatpak: `flatpak/tech.familiartools.HypoDJ.yml` is an untested draft.
- The Qt shell is `djq` (`qt.md`); both shells share `crates/hypodj-app`.
- MPRIS / media keys come from the daemon; app-side breathing motion, AT-SPI audit,
  appstream metadata are follow-ups. The icon id is `hypodj` (agents/w16-logo).
