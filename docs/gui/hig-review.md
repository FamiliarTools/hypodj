# djg against the GNOME HIG

Reviewed 2026-11-26 against the primary docs: developer.gnome.org/hig (patterns:
header bars, menus, search, view switchers, placeholders, boxed lists, toasts,
banners; reference: keyboard) and the libadwaita 1.6 / GTK 4.16 API docs the app
builds on. Severity is the cost to a GNOME user: **high** breaks a platform
expectation every session, **medium** is a visible deviation or an accessibility
gap, **low** is polish.

## Fixed in this change

| # | Sev | Finding | HIG source | Fix |
|---|-----|---------|-----------|-----|
| 1 | high | No primary menu, so no About dialog. "Every primary menu should include" About App. | patterns/controls/menus | `open-menu-symbolic` MenuButton at the header bar end, tooltip and accessible label "Main Menu", `primary` (F10). Items: Keyboard Shortcuts, About HypoDJ (`AdwAboutDialog`). No Quit item, per the HIG. |
| 2 | high | No type-to-search; Ctrl+F did nothing. | patterns/nav/search ("type to search", "Ctrl+F: the keyboard shortcut for search") | `GtkSearchEntry::set_key_capture_widget(window)`: typing outside a text field brings Search forward with the key in the entry; Escape clears it and returns to the page and the widget focus came from. `win.search` on Ctrl+F. |
| 3 | high | No keyboard shortcuts window (Ctrl+?), and none of the standard accelerators. | reference/keyboard (Ctrl+?, Ctrl+Q, Ctrl+W, Ctrl+F, F10) | `GtkShortcutsWindow` as the window help overlay on Ctrl+?, listing Search, Close Search, Back, Main Menu, Close Window, Quit. Ctrl+W `window.close`, Ctrl+Q `app.quit`, Alt+Left back. `AdwShortcutsDialog` needs libadwaita 1.8; the crate targets 1.6. |
| 4 | medium | The album page's back button sat inside the content, under the header bar. Back is a header bar start control; with hierarchical navigation the primary menu shows only on the top level. | patterns/containers/header-bars, patterns/controls/menus | Back moved to the header bar start (`win.back`), visible only on a pushed album page; the primary menu hides there. |
| 5 | medium | Icon-only buttons (previous, next, reply dismiss) had tooltips but no accessible label. | patterns/containers/header-bars ("controls should all have tooltips"); GTK accessibility docs | `accessible::Property::Label` on each, matching the tooltip. |
| 6 | medium | An empty or failed Albums page was a one-line dim label above an empty boxed list. Albums are named as a symbolic-placeholder case. | patterns/feedback/placeholders | `AdwStatusPage` (symbolic icon, title "No Albums" / "No Albums Available", the reason as description) when there are no rows; the note label stays for the non-empty offline case. |
| 7 | medium | Offline, rows that cannot play were active and focusable; activating one could only fail. | (product rule; boxed lists keep insensitive rows visible) | With the server down, only rows the mirror holds stay sensitive. Insensitive rows are greyed and skipped by keyboard focus (proof below). |

## Kept, with the reason

| # | Sev | Finding | Why it stays |
|---|-----|---------|--------------|
| 8 | low | Replies use a card above the ask entry, not a toast. | HIG toasts are for brief event notices; an answer runs to several lines and must be readable and selectable. A toast would cut it. |
| 9 | low | Server trouble uses an `AdwBanner`. | Correct per patterns/feedback/banners: a persistent state that affects the whole window. |

## Listed, not fixed (low)

| # | Sev | Finding |
|---|-----|---------|
| 10 | low | The search entry sits in the Search page, not in a `GtkSearchBar` sliding under the header bar. Search is a page of the view switcher, so the page itself is the search UI; a slide-down bar would duplicate it. |
| 11 | low | "Up Next" is a bare `heading` label over a boxed list; `AdwPreferencesGroup` would give the standard group title spacing. |
| 12 | low | The ask entry's `user-available-symbolic` icon reads as presence, not "ask"; no stock icon fits better yet. |
| 13 | low | The album page title duplicates the `AdwNavigationPage` title and the header bar stays the switcher's, so the album name is not in the header bar. A per-page `AdwToolbarView` would fix it but changes the window shape (deferred; agents/w26-find is reworking Albums). |
| 14 | low | No Preferences or Help item: the app has neither yet, and the HIG lists them "if it has one". |
| 15 | low | Album rows have no cover thumbnails (agents/w26-find adds a grid). |
| 16 | low | The volume icon is static (`audio-volume-medium-symbolic`) regardless of level. |

## Proof

`scripts/djg-screens.sh` renders on a private Xvfb against a silent probe daemon
(audio null, throwaway store) and a second daemon with an unreachable server.
Screens in `screens/`: `type-to-search`, `type-to-search-escape`, `about`,
`shortcuts-window`, `unreachable-playing`, `unreachable-albums`,
`unreachable-album`, `unreachable-narrow`. The Qt shell (`screens/qt/`) gets the
same type-to-search and offline gating.
