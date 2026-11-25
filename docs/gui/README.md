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
The words come from `hypodj-client` and `view.rs`, the same functions as the TUI.

## Proof

`scripts/djg-screens.sh <djg> <hypodj>` renders every view against a SILENT probe
daemon (audio null, MPRIS off, throwaway store capped at ~120 MB, port 6690) on a
private Xvfb `:93`, with `env -i`, a private HOME / XDG_RUNTIME_DIR and its own
D-Bus. Screens: `screens/`.

## Not yet

- Flatpak: `flatpak/tech.familiartools.HypoDJ.yml` is an untested draft.
- The Qt shell.
- Album track rows show availability only once the daemon emits `X-Offline` on
  `lsinfo album/<id>`; the queue already does.
- MPRIS / media keys come from the daemon; app-side breathing motion, AT-SPI audit,
  appstream metadata and an app icon are follow-ups.
