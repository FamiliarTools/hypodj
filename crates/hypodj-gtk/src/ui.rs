//! The window and its three pages. Widgets are built once; `App::apply` folds each
//! worker message into them. No IO here.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::Sender;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use hypodj_client::config::{self, Env};
use hypodj_client::model::{NowPlaying, QueueItem};

use crate::view::{self, AlbumRow, Hit, HitKind, SongRow};
use crate::worker::{self, Msg, Req};

/// Cover edge on the instrument, in logical pixels.
const COVER: i32 = 220;

struct App {
    tx: Sender<Req>,
    window: adw::ApplicationWindow,
    /// HypoDJ's last reply, above the ask entry. Answers run to several lines
    /// ("2 albums play offline" and the list), which a one-line toast would cut.
    reply: gtk::Revealer,
    reply_text: gtk::Label,
    stack: adw::ViewStack,
    banner: adw::Banner,
    server: gtk::Label,
    server_icon: gtk::Image,
    server_details: gtk::Label,
    // The instrument.
    instrument: gtk::Box,
    instrument_css: gtk::CssProvider,
    cover: gtk::Picture,
    title: gtk::Label,
    artist: gtk::Label,
    album: gtk::Label,
    held: gtk::Label,
    play: gtk::Button,
    /// The verb the play button sends, set by each snapshot.
    play_verb: Rc<Cell<&'static str>>,
    volume: gtk::Scale,
    up_next: gtk::ListBox,
    // Albums.
    albums_nav: adw::NavigationView,
    albums_list: gtk::ListBox,
    albums_note: gtk::Label,
    albums_status: adw::StatusPage,
    // Search.
    search_list: gtk::ListBox,
    search_status: adw::StatusPage,
    search_entry: gtk::SearchEntry,
    // State the folds need.
    art_file: RefCell<Option<String>>,
    /// The plan dialog on screen, if any. The worker keeps ONE plan pending and
    /// cancels the one a newer plan supersedes, so the older dialog is closed here
    /// rather than left answering for a token that is already gone.
    plan_dialog: RefCell<Option<adw::AlertDialog>>,
    cover_avg: Cell<Option<[u8; 3]>>,
    /// The music server is down: only rows the mirror holds stay active.
    down: Cell<bool>,
    /// Every browse row with its on-disk answer, so a server state change re-gates
    /// them in place (Up Next is rebuilt on every snapshot and gates itself).
    gated: RefCell<Vec<(glib::WeakRef<adw::ActionRow>, Option<bool>)>>,
    /// Where type-to-search came from: the page and the focused widget, restored
    /// on Escape.
    search_return: RefCell<Option<(String, Option<glib::WeakRef<gtk::Widget>>)>>,
}

pub fn activate(app: &adw::Application) {
    // The HypoDJ icon id (docs/design/logo); windows and the task switcher use it.
    gtk::Window::set_default_icon_name("hypodj");
    let env = Env { get: &|k| std::env::var(k).ok() };
    let (host, port) = config::resolve(None, None, &env);
    let (out_tx, out_rx) = async_channel::unbounded::<Msg>();
    let handles = worker::spawn(host, port, out_tx);

    let ui = Rc::new(build(app, handles.cmd));
    ui.restyle();
    {
        let ui = ui.clone();
        let sm = adw::StyleManager::default();
        sm.connect_dark_notify(move |_| ui.restyle());
    }
    {
        let ui = ui.clone();
        adw::StyleManager::default().connect_high_contrast_notify(move |_| ui.restyle());
    }
    {
        let ui = ui.clone();
        glib::spawn_future_local(async move {
            while let Ok(msg) = out_rx.recv().await {
                ui.apply(msg);
            }
        });
    }
    wire_search(&ui);
    ui.window.present();
    screenshot_hook(&ui);
}

/// Type-to-search and its way back: the first captured key brings the Search page
/// forward (remembering the page and the focused widget), Escape clears the entry
/// and returns to both. Escape on a Search page reached by its tab just clears.
fn wire_search(ui: &Rc<App>) {
    {
        let ui = ui.clone();
        ui.search_entry.clone().connect_search_started(move |e| {
            let page = ui.stack.visible_child_name().map(|n| n.to_string()).unwrap_or_default();
            if page != "search" {
                let focus = gtk::prelude::GtkWindowExt::focus(&ui.window).map(|w| w.downgrade());
                *ui.search_return.borrow_mut() = Some((page, focus));
                ui.stack.set_visible_child_name("search");
            }
            if !e.has_focus() {
                e.grab_focus();
                e.set_position(-1);
            }
        });
    }
    {
        let ui = ui.clone();
        ui.search_entry.clone().connect_stop_search(move |e| {
            e.set_text("");
            if let Some((page, focus)) = ui.search_return.take() {
                ui.stack.set_visible_child_name(&page);
                match focus.and_then(|w| w.upgrade()) {
                    Some(w) => {
                        w.grab_focus();
                    }
                    None => gtk::prelude::GtkWindowExt::set_focus(&ui.window, None::<&gtk::Widget>),
                }
            }
        });
    }
}

fn label(classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .build();
    for c in classes {
        l.add_css_class(c);
    }
    l
}

fn boxed_list() -> gtk::ListBox {
    let l = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
    l.add_css_class("boxed-list");
    l
}

fn clamp(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let c = adw::Clamp::builder().maximum_size(760).child(child).build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&c)
        .build()
}

fn build(app: &adw::Application, tx: Sender<Req>) -> App {
    // The instrument: cover, words, transport, volume.
    let cover = gtk::Picture::builder()
        .width_request(COVER)
        .height_request(COVER)
        .content_fit(gtk::ContentFit::Cover)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .build();
    cover.add_css_class("card");
    cover.set_overflow(gtk::Overflow::Hidden);
    let title = label(&["title-1"]);
    let artist = label(&["title-4"]);
    let album = label(&["quiet"]);
    let held = label(&["ember", "caption-heading"]);
    held.set_visible(false);

    let prev = gtk::Button::from_icon_name("media-skip-backward-symbolic");
    prev.set_tooltip_text(Some("Previous"));
    prev.update_property(&[gtk::accessible::Property::Label("Previous")]);
    let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
    play.set_tooltip_text(Some("Play"));
    play.add_css_class("pill");
    let next = gtk::Button::from_icon_name("media-skip-forward-symbolic");
    next.set_tooltip_text(Some("Next"));
    next.update_property(&[gtk::accessible::Property::Label("Next")]);
    for b in [&prev, &next] {
        b.add_css_class("circular");
        b.add_css_class("flat");
    }
    let transport = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    transport.append(&prev);
    transport.append(&play);
    transport.append(&next);

    let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
    volume.set_hexpand(true);
    volume.update_property(&[gtk::accessible::Property::Label("Volume")]);
    let vol_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    vol_row.append(&gtk::Image::from_icon_name("audio-volume-medium-symbolic"));
    vol_row.append(&volume);

    let words = gtk::Box::new(gtk::Orientation::Vertical, 6);
    words.set_hexpand(true);
    words.set_valign(gtk::Align::Center);
    words.append(&title);
    words.append(&artist);
    words.append(&album);
    words.append(&held);
    words.append(&transport);
    words.append(&vol_row);

    let instrument = gtk::Box::new(gtk::Orientation::Horizontal, 24);
    instrument.add_css_class("instrument");
    instrument.set_margin_top(12);
    let inner_pad = gtk::Box::new(gtk::Orientation::Horizontal, 24);
    inner_pad.set_margin_top(24);
    inner_pad.set_margin_bottom(24);
    inner_pad.set_margin_start(24);
    inner_pad.set_margin_end(24);
    inner_pad.set_hexpand(true);
    inner_pad.append(&cover);
    inner_pad.append(&words);
    instrument.append(&inner_pad);

    let up_next = boxed_list();
    let up_next_head = label(&["heading"]);
    up_next_head.set_text("Up Next");
    let playing_col = gtk::Box::new(gtk::Orientation::Vertical, 18);
    playing_col.set_margin_start(12);
    playing_col.set_margin_end(12);
    playing_col.set_margin_bottom(24);
    playing_col.append(&instrument);
    playing_col.append(&up_next_head);
    playing_col.append(&up_next);
    let playing_page = clamp(&playing_col);

    // Albums: a navigation view, list then tracks.
    let albums_list = boxed_list();
    let albums_note = label(&["dim-label"]);
    albums_note.set_visible(false);
    let albums_col = gtk::Box::new(gtk::Orientation::Vertical, 12);
    albums_col.set_margin_top(12);
    albums_col.set_margin_bottom(24);
    albums_col.set_margin_start(12);
    albums_col.set_margin_end(12);
    let albums_status = adw::StatusPage::builder()
        .icon_name("media-optical-cd-audio-symbolic")
        .title("Loading Albums")
        .vexpand(true)
        .build();
    albums_col.append(&albums_note);
    albums_col.append(&albums_status);
    albums_col.append(&albums_list);
    let albums_root = adw::NavigationPage::builder()
        .title("Albums")
        .tag("albums")
        .child(&clamp(&albums_col))
        .build();
    let albums_nav = adw::NavigationView::new();
    albums_nav.add(&albums_root);

    // Search.
    let search_entry = gtk::SearchEntry::builder()
        .placeholder_text("Artists, albums, songs")
        .hexpand(true)
        .build();
    let search_list = boxed_list();
    search_list.set_visible(false);
    let search_status = adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title("Search the library")
        .description("Type to find artists, albums and songs.")
        .vexpand(true)
        .build();
    let search_col = gtk::Box::new(gtk::Orientation::Vertical, 12);
    search_col.set_margin_top(12);
    search_col.set_margin_bottom(24);
    search_col.set_margin_start(12);
    search_col.set_margin_end(12);
    search_col.append(&search_entry);
    search_col.append(&search_status);
    search_col.append(&search_list);
    let search_page = clamp(&search_col);

    let stack = adw::ViewStack::new();
    stack.add_titled_with_icon(&playing_page, Some("playing"), "Playing", "media-playback-start-symbolic");
    stack.add_titled_with_icon(&albums_nav, Some("albums"), "Albums", "media-optical-cd-audio-symbolic");
    stack.add_titled_with_icon(&search_page, Some("search"), "Search", "system-search-symbolic");

    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    let header = adw::HeaderBar::builder().title_widget(&switcher).build();
    // Back lives at the start of the header bar (HIG), shown only on a pushed album
    // page; the primary menu only on the top level.
    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.set_tooltip_text(Some("Back to Albums"));
    back.update_property(&[gtk::accessible::Property::Label("Back to Albums")]);
    back.set_action_name(Some("win.back"));
    back.set_visible(false);
    header.pack_start(&back);
    let primary = gio::Menu::new();
    let standard = gio::Menu::new();
    standard.append(Some("_Keyboard Shortcuts"), Some("win.show-help-overlay"));
    standard.append(Some("_About HypoDJ"), Some("app.about"));
    primary.append_section(None, &standard);
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&primary)
        .tooltip_text("Main Menu")
        .primary(true)
        .build();
    menu_button.update_property(&[gtk::accessible::Property::Label("Main Menu")]);
    header.pack_end(&menu_button);
    // The status: icon + short words in a flat menu button; the popover carries
    // the full server and store sentences the label had to drop.
    let server = gtk::Label::new(None);
    server.add_css_class("caption");
    server.set_ellipsize(gtk::pango::EllipsizeMode::End);
    server.set_max_width_chars(40);
    let server_icon = gtk::Image::from_icon_name("network-server-symbolic");
    let server_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    server_content.append(&server_icon);
    server_content.append(&server);
    let server_details = label(&[]);
    server_details.set_selectable(true);
    server_details.set_max_width_chars(48);
    server_details.set_margin_start(6);
    server_details.set_margin_end(6);
    server_details.set_margin_top(6);
    server_details.set_margin_bottom(6);
    let server_pop = gtk::Popover::builder().child(&server_details).build();
    let server_button = gtk::MenuButton::builder()
        .child(&server_content)
        .popover(&server_pop)
        .tooltip_text("Server and offline status")
        .build();
    server_button.add_css_class("flat");

    let banner = adw::Banner::new("");
    banner.set_revealed(false);

    // The ask entry: routes like djc.
    let ask = gtk::Entry::builder()
        .placeholder_text("Ask or tell HypoDJ - \"what is this\", \"play something calmer\"")
        .hexpand(true)
        .primary_icon_name("user-available-symbolic")
        .build();
    ask.update_property(&[gtk::accessible::Property::Label("Ask or tell HypoDJ")]);
    let ask_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    ask_bar.set_margin_top(6);
    ask_bar.set_margin_bottom(6);
    ask_bar.set_margin_start(12);
    ask_bar.set_margin_end(12);
    ask_bar.append(&ask);
    ask_bar.append(&server_button);
    let switcher_bar = adw::ViewSwitcherBar::builder().stack(&stack).build();
    let reply_text = label(&[]);
    reply_text.set_selectable(true);
    reply_text.set_hexpand(true);
    let reply_close = gtk::Button::from_icon_name("window-close-symbolic");
    reply_close.add_css_class("flat");
    reply_close.add_css_class("circular");
    reply_close.set_valign(gtk::Align::Start);
    reply_close.set_tooltip_text(Some("Dismiss"));
    reply_close.update_property(&[gtk::accessible::Property::Label("Dismiss")]);
    let reply_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    reply_box.add_css_class("card");
    reply_box.set_margin_start(12);
    reply_box.set_margin_end(12);
    reply_box.set_margin_top(12);
    let reply_inner = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    reply_inner.set_margin_start(12);
    reply_inner.set_margin_end(6);
    reply_inner.set_margin_top(10);
    reply_inner.set_margin_bottom(10);
    reply_inner.set_hexpand(true);
    reply_inner.append(&reply_text);
    reply_inner.append(&reply_close);
    reply_box.append(&reply_inner);
    let reply = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .transition_duration(hypodj_style::tokens().motion.pause_ms as u32)
        .child(&reply_box)
        .build();
    {
        let reply = reply.clone();
        reply_close.connect_clicked(move |_| reply.set_reveal_child(false));
    }
    let bottom = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bottom.append(&reply);
    bottom.append(&ask_bar);
    bottom.append(&switcher_bar);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&stack));
    toolbar.add_bottom_bar(&bottom);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("HypoDJ")
        .default_width(920)
        .default_height(760)
        .width_request(360)
        .height_request(294)
        .content(&toolbar)
        .build();

    // Narrow: the switcher moves to the bottom bar and the instrument stacks.
    let bp = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 600sp").expect("static condition"));
    bp.add_setter(&switcher_bar, "reveal", Some(&true.to_value()));
    bp.add_setter(&header, "title-widget", Some(&None::<gtk::Widget>.to_value()));
    // Narrow: the icon alone; the words stay one click away in the popover.
    bp.add_setter(&server, "visible", Some(&false.to_value()));
    bp.add_setter(&inner_pad, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
    window.add_breakpoint(bp);
    window.set_help_overlay(Some(&shortcuts_window()));
    install_actions(app, &window, &stack, &albums_nav, &search_entry);
    {
        // Back and the primary menu follow the page: back on a pushed album page,
        // the menu only on the top level.
        let sync = {
            let (stack, nav, back, menu) = (stack.clone(), albums_nav.clone(), back.clone(), menu_button.clone());
            move || {
                let deep = stack.visible_child_name().as_deref() == Some("albums")
                    && nav.visible_page().and_then(|p| p.tag()).as_deref() != Some("albums");
                back.set_visible(deep);
                menu.set_visible(!deep);
            }
        };
        let s2 = sync.clone();
        stack.connect_visible_child_notify(move |_| s2());
        albums_nav.connect_visible_page_notify(move |_| sync());
    }

    let css = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("a display"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // Signals. Each one only enqueues a request.
    let play_verb = Rc::new(Cell::new("play"));
    {
        let (tx, verb) = (tx.clone(), play_verb.clone());
        play.connect_clicked(move |_| {
            let _ = tx.send(Req::Command(verb.get().into()));
        });
    }
    {
        let tx = tx.clone();
        prev.connect_clicked(move |_| {
            let _ = tx.send(Req::Command("previous".into()));
        });
    }
    {
        let tx = tx.clone();
        next.connect_clicked(move |_| {
            let _ = tx.send(Req::Command("next".into()));
        });
    }
    {
        let tx = tx.clone();
        // `change-value` fires only for the user's own drag, never for our refresh.
        volume.connect_change_value(move |_, _, v| {
            let _ = tx.send(Req::Command(format!("setvol {}", v.round().clamp(0.0, 100.0) as i64)));
            glib::Propagation::Proceed
        });
    }
    {
        let tx = tx.clone();
        ask.connect_activate(move |e| {
            let text = e.text().trim().to_string();
            if !text.is_empty() {
                let _ = tx.send(Req::Ask(text));
                e.set_text("");
            }
        });
    }
    {
        let tx = tx.clone();
        search_entry.connect_search_changed(move |e| {
            let q = e.text().trim().to_string();
            if !q.is_empty() {
                let _ = tx.send(Req::Search(q));
            }
        });
    }

    // Type to search (HIG): typing anywhere not already taken by a text field
    // lands in the search entry, which brings its page forward.
    search_entry.set_key_capture_widget(Some(&window));

    App {
        tx,
        window,
        reply,
        reply_text,
        stack,
        banner,
        server,
        server_icon,
        server_details,
        instrument,
        instrument_css: css,
        cover,
        title,
        artist,
        album,
        held,
        play,
        play_verb,
        volume,
        up_next,
        albums_nav,
        albums_list,
        albums_note,
        albums_status,
        search_list,
        search_status,
        search_entry,
        art_file: RefCell::new(None),
        plan_dialog: RefCell::new(None),
        cover_avg: Cell::new(None),
        down: Cell::new(false),
        gated: RefCell::new(Vec::new()),
        search_return: RefCell::new(None),
    }
}

/// The window actions and the standard accelerators: Ctrl+F search, Ctrl+? the
/// shortcuts window, Ctrl+W close, Ctrl+Q quit, Alt+Left back.
fn install_actions(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    stack: &adw::ViewStack,
    nav: &adw::NavigationView,
    search_entry: &gtk::SearchEntry,
) {
    let back = gio::ActionEntry::builder("back")
        .activate({
            let nav = nav.clone();
            move |_: &adw::ApplicationWindow, _, _| {
                nav.pop();
            }
        })
        .build();
    let search = gio::ActionEntry::builder("search")
        .activate({
            let (stack, entry) = (stack.clone(), search_entry.clone());
            move |_: &adw::ApplicationWindow, _, _| {
                stack.set_visible_child_name("search");
                entry.grab_focus();
            }
        })
        .build();
    window.add_action_entries([back, search]);
    let about = gio::ActionEntry::builder("about")
        .activate({
            let window = window.clone();
            move |_: &adw::Application, _, _| about_dialog().present(Some(&window))
        })
        .build();
    let quit = gio::ActionEntry::builder("quit")
        .activate(|a: &adw::Application, _, _| a.quit())
        .build();
    app.add_action_entries([about, quit]);
    app.set_accels_for_action("win.search", &["<Control>f"]);
    app.set_accels_for_action("win.show-help-overlay", &["<Control>question"]);
    app.set_accels_for_action("window.close", &["<Control>w"]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
    app.set_accels_for_action("win.back", &["<Alt>Left"]);
}

fn about_dialog() -> adw::AboutDialog {
    adw::AboutDialog::builder()
        .application_name("HypoDJ")
        .application_icon("hypodj")
        .developer_name("Guilherme Fontes")
        .version(env!("CARGO_PKG_VERSION"))
        .website("https://github.com/FamiliarTools/hypodj")
        .issue_url("https://github.com/FamiliarTools/hypodj/issues")
        .license_type(gtk::License::Custom)
        .license("Functional Source License, Version 1.1, MIT Future License (FSL-1.1-MIT)")
        .comments("A listening companion: your Subsonic library, played by a daemon you can talk to.")
        .build()
}

/// The Ctrl+? window. GtkShortcutsWindow is the GTK 4.16 way (AdwShortcutsDialog
/// needs libadwaita 1.8).
fn shortcuts_window() -> gtk::ShortcutsWindow {
    let ui = r#"<interface>
  <object class="GtkShortcutsWindow" id="w">
    <property name="modal">1</property>
    <child>
      <object class="GtkShortcutsSection">
        <property name="section-name">main</property>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">General</property>
            <child><object class="GtkShortcutsShortcut"><property name="title">Search</property><property name="accelerator">&lt;Control&gt;f</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Close Search</property><property name="accelerator">Escape</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Back</property><property name="accelerator">&lt;Alt&gt;Left</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Keyboard Shortcuts</property><property name="accelerator">&lt;Control&gt;question</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Main Menu</property><property name="accelerator">F10</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Close Window</property><property name="accelerator">&lt;Control&gt;w</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Quit</property><property name="accelerator">&lt;Control&gt;q</property></object></child>
          </object>
        </child>
      </object>
    </child>
  </object>
</interface>"#;
    gtk::Builder::from_string(ui).object("w").expect("static shortcuts ui")
}

fn clear(list: &gtk::ListBox) {
    while let Some(c) = list.first_child() {
        list.remove(&c);
    }
}

fn row(title: &str, subtitle: Option<&str>) -> adw::ActionRow {
    let r = adw::ActionRow::builder()
        .title(glib::markup_escape_text(title).as_str())
        .build();
    if let Some(s) = subtitle.filter(|s| !s.is_empty()) {
        r.set_subtitle(glib::markup_escape_text(s).as_str());
    }
    r
}

fn join(a: Option<&str>, b: Option<&str>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a} · {b}")),
        (Some(x), None) | (None, Some(x)) => Some(x.to_string()),
        (None, None) => None,
    }
}

impl App {
    fn send(&self, r: Req) {
        let _ = self.tx.send(r);
    }

    fn say(&self, text: &str) {
        self.reply_text.set_text(text);
        self.reply.set_reveal_child(true);
    }

    /// Register a browse row and gate it now: insensitive rows are skipped by
    /// keyboard focus and cannot be activated.
    fn gate(&self, r: &adw::ActionRow, on_disk: Option<bool>) {
        r.set_sensitive(view::row_active(self.down.get(), on_disk));
        self.gated.borrow_mut().push((r.downgrade(), on_disk));
    }

    fn regate(&self) {
        self.prune_gated();
        let down = self.down.get();
        for (w, on_disk) in self.gated.borrow().iter() {
            if let Some(r) = w.upgrade() {
                r.set_sensitive(view::row_active(down, *on_disk));
            }
        }
    }

    /// Drop rows that left the tree (a cleared list, a popped album page).
    fn prune_gated(&self) {
        self.gated.borrow_mut().retain(|(w, _)| w.upgrade().is_some_and(|r| r.parent().is_some()));
    }

    fn restyle(&self) {
        let sm = adw::StyleManager::default();
        self.instrument_css.load_from_string(&hypodj_style::gtk_css::instrument_css(
            self.cover_avg.get(),
            sm.is_dark(),
            sm.is_high_contrast(),
        ));
    }

    fn apply(self: &Rc<Self>, msg: Msg) {
        match msg {
            Msg::Snapshot { now, queue } => self.now_playing(&now, &queue),
            Msg::Down(why) => {
                eprintln!("djg: down - {why}");
                self.banner.set_title(&glib::markup_escape_text(&why));
                self.banner.set_revealed(true);
                self.server.set_text("Not connected");
                self.server_icon.set_icon_name(Some("network-offline-symbolic"));
                self.server_details.set_text(&why);
            }
            Msg::Albums { rows, note } => self.albums(rows, note),
            Msg::Album { uri, title, rows } => self.album_page(&uri, &title, rows),
            Msg::Search { query, result } => {
                if self.search_entry.text().trim() == query {
                    self.search(&query, result);
                }
            }
            Msg::Art { file, bytes } => {
                if self.art_file.borrow().as_deref() != Some(file.as_str()) {
                    return; // a late answer for a track that has since changed
                }
                let tex = bytes.and_then(|b| gdk::Texture::from_bytes(&glib::Bytes::from_owned(b)).ok());
                self.cover_avg.set(tex.as_ref().map(average));
                self.cover.set_paintable(tex.as_ref());
                self.restyle();
            }
            Msg::Say(text) => self.say(&text),
            Msg::Confirm { token, steps, note } => {
                let mut body = steps.join("\n");
                if let Some(n) = note {
                    body.push_str("\n\n");
                    body.push_str(&n);
                }
                let d = adw::AlertDialog::new(Some("Do this?"), Some(&body));
                d.add_responses(&[("cancel", "Cancel"), ("confirm", "Do It")]);
                d.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
                d.set_default_response(Some("confirm"));
                d.set_close_response("cancel");
                let tx = self.tx.clone();
                d.connect_response(None, move |_, resp| {
                    let _ = tx.send(if resp == "confirm" {
                        Req::Confirm(token.clone())
                    } else {
                        Req::Cancel(token.clone())
                    });
                });
                if let Some(old) = self.plan_dialog.replace(Some(d.clone())) {
                    old.force_close();
                }
                d.present(Some(&self.window));
            }
            Msg::ConfirmCommand { command, question } => {
                let d = adw::AlertDialog::new(Some(&question), None);
                d.add_responses(&[("cancel", "Cancel"), ("go", "Clear")]);
                d.set_response_appearance("go", adw::ResponseAppearance::Destructive);
                d.set_close_response("cancel");
                let tx = self.tx.clone();
                d.connect_response(Some("go"), move |_, _| {
                    let _ = tx.send(Req::Command(command.clone()));
                });
                d.present(Some(&self.window));
            }
            Msg::ShowPlaying => self.stack.set_visible_child_name("playing"),
        }
    }

    fn now_playing(&self, now: &NowPlaying, queue: &[QueueItem]) {
        let st = view::status(now.server.as_deref(), now.store.as_deref());
        self.server.set_text(&st.label);
        self.server_icon.set_icon_name(Some(st.icon));
        self.server_details.set_text(&st.details.join("\n"));
        let problem = st.problem;
        let down = view::server_down(now.server.as_deref());
        if down != self.down.replace(down) {
            self.regate();
        }
        self.banner.set_revealed(problem);
        if problem {
            self.banner.set_title(&glib::markup_escape_text(&format!(
                "{} - songs on disk still play",
                hypodj_client::model::server_words(now.server.as_deref().unwrap_or("")).0
            )));
        }

        let nothing = now.title.is_none() && now.name.is_none() && now.file.is_none();
        self.title.set_text(if nothing {
            "Nothing playing"
        } else {
            now.title.as_deref().or(now.name.as_deref()).unwrap_or("Unknown")
        });
        self.artist.set_text(now.artist.as_deref().unwrap_or(""));
        self.artist.set_visible(now.artist.is_some());
        self.album.set_text(now.album.as_deref().unwrap_or(if nothing {
            "Ask for something below, or pick an album."
        } else {
            ""
        }));
        // Ember marks only what HypoDJ is holding for you: kept, or armed.
        let mut held: Vec<String> = Vec::new();
        if now.starred {
            held.push("kept".into());
        }
        for s in hypodj_client::spoken::armed_hold(&now.armed) {
            held.push(s.text());
        }
        self.held.set_text(&held.join(" · "));
        self.held.set_visible(!held.is_empty());

        let playing = now.state.as_deref() == Some("play");
        self.play.set_icon_name(if playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        });
        self.play.set_tooltip_text(Some(if playing { "Pause" } else { "Play" }));
        // Resume is `pause 0` (startle-safe, from the playhead), never `play`,
        // which restarts the track.
        self.play_verb.set(match now.state.as_deref() {
            Some("play") => "pause 1",
            Some("pause") => "pause 0",
            _ => "play",
        });
        match now.volume.filter(|v| *v >= 0) {
            Some(v) => {
                self.volume.set_sensitive(true);
                self.volume.set_value(v as f64);
            }
            None => self.volume.set_sensitive(false),
        }

        // Cover: ask once per track.
        let file = now.file.clone();
        if *self.art_file.borrow() != file {
            *self.art_file.borrow_mut() = file.clone();
            self.cover.set_paintable(None::<&gdk::Paintable>);
            self.cover_avg.set(None);
            self.restyle();
            if let Some(f) = file {
                let wants = f.starts_with("song/") || now.cover.is_some();
                if wants {
                    self.send(Req::Art(f));
                }
            }
        }

        clear(&self.up_next);
        let start = now.song.map(|s| s + 1).unwrap_or(0);
        let rest: Vec<&QueueItem> = queue.iter().skip(start).take(8).collect();
        if rest.is_empty() {
            self.up_next.append(&row("Nothing queued", Some("The queue ends after this track.")));
        }
        for it in rest {
            let r = row(&it.title, join(it.artist.as_deref(), view::queue_availability(it)).as_deref());
            let tx = self.tx.clone();
            let pos = it.pos;
            r.set_activatable(true);
            r.set_sensitive(view::row_active(down, view::queue_on_disk(it)));
            r.connect_activated(move |_| {
                let _ = tx.send(Req::Command(format!("play {pos}")));
            });
            self.up_next.append(&r);
        }
        self.instrument.queue_draw();
    }

    fn albums(self: &Rc<Self>, rows: Vec<AlbumRow>, note: Option<String>) {
        clear(&self.albums_list);
        self.prune_gated();
        // No rows is an empty state (HIG placeholder), with the reason as its text.
        let empty = rows.is_empty();
        self.albums_status.set_visible(empty);
        self.albums_list.set_visible(!empty);
        if empty {
            self.albums_status.set_title(if note.is_some() { "No Albums Available" } else { "No Albums" });
            self.albums_status.set_description(note.as_deref().map(|n| glib::markup_escape_text(n)).as_deref());
        }
        self.albums_note.set_text(note.as_deref().unwrap_or(""));
        self.albums_note.set_visible(note.is_some() && !empty);
        for a in rows {
            let r = row(&a.title, join(a.artist.as_deref(), a.availability().as_deref()).as_deref());
            r.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            r.set_activatable(true);
            let (tx, uri, title) = (self.tx.clone(), a.uri.clone(), a.title.clone());
            r.connect_activated(move |_| {
                let _ = tx.send(Req::Album { uri: uri.clone(), title: title.clone() });
            });
            self.gate(&r, a.on_disk());
            self.albums_list.append(&r);
        }
    }

    fn album_page(self: &Rc<Self>, uri: &str, title: &str, rows: Vec<SongRow>) {
        let list = boxed_list();
        let play_all = gtk::Button::with_label("Play Album");
        play_all.add_css_class("pill");
        play_all.add_css_class("suggested-action");
        play_all.set_valign(gtk::Align::Center);
        {
            let (tx, uri) = (self.tx.clone(), uri.to_string());
            play_all.connect_clicked(move |_| {
                let _ = tx.send(Req::PlayUri(uri.clone()));
            });
        }
        for s in &rows {
            let r = row(&s.title, join(s.artist.as_deref(), s.availability).as_deref());
            r.set_activatable(true);
            let (tx, u) = (self.tx.clone(), s.uri.clone());
            r.connect_activated(move |_| {
                let _ = tx.send(Req::PlayUri(u.clone()));
            });
            self.gate(&r, s.on_disk());
            list.append(&r);
        }
        let col = gtk::Box::new(gtk::Orientation::Vertical, 18);
        col.set_margin_top(12);
        col.set_margin_bottom(24);
        col.set_margin_start(12);
        col.set_margin_end(12);
        // Back is the header bar's (win.back); the page carries its title.
        let heading = label(&["title-2"]);
        heading.set_text(title);
        heading.set_hexpand(true);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        top.append(&heading);
        top.append(&play_all);
        col.append(&top);
        col.append(&list);
        let page = adw::NavigationPage::builder().title(title).child(&clamp(&col)).build();
        self.albums_nav.push(&page);
    }

    fn search(&self, query: &str, result: Result<Vec<Hit>, String>) {
        clear(&self.search_list);
        match result {
            Err(why) => {
                self.search_list.set_visible(false);
                self.search_status.set_visible(true);
                self.search_status.set_title("Search did not work");
                self.search_status.set_description(Some(&glib::markup_escape_text(&why)));
            }
            Ok(hits) if hits.is_empty() => {
                self.search_list.set_visible(false);
                self.search_status.set_visible(true);
                self.search_status.set_title("No results");
                self.search_status
                    .set_description(Some(&glib::markup_escape_text(&format!("Nothing matches \"{query}\"."))));
            }
            Ok(hits) => {
                self.search_status.set_visible(false);
                self.search_list.set_visible(true);
                for h in hits {
                    let r = row(&h.title, h.subtitle.as_deref());
                    r.add_prefix(&gtk::Image::from_icon_name(match h.kind {
                        HitKind::Artist => "avatar-default-symbolic",
                        HitKind::Album => "media-optical-cd-audio-symbolic",
                        HitKind::Song => "audio-x-generic-symbolic",
                    }));
                    r.set_activatable(true);
                    let (tx, uri) = (self.tx.clone(), h.uri.clone());
                    let kind = h.kind;
                    r.connect_activated(move |_| {
                        let _ = tx.send(match kind {
                            HitKind::Song | HitKind::Album | HitKind::Artist => Req::PlayUri(uri.clone()),
                        });
                    });
                    // A hit carries no on-disk answer, so offline it is inactive.
                    self.gate(&r, None);
                    self.search_list.append(&r);
                }
            }
        }
    }
}

/// The cover's average colour, sampled on a coarse grid.
fn average(t: &gdk::Texture) -> [u8; 3] {
    let (w, h) = (t.width().max(1) as usize, t.height().max(1) as usize);
    let stride = w * 4;
    let mut buf = vec![0u8; stride * h];
    t.download(&mut buf, stride);
    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
    let step = (w.max(h) / 32).max(1);
    for y in (0..h).step_by(step) {
        for x in (0..w).step_by(step) {
            let i = y * stride + x * 4;
            // GDK_MEMORY_DEFAULT is B8G8R8A8 premultiplied.
            b += buf[i] as u64;
            g += buf[i + 1] as u64;
            r += buf[i + 2] as u64;
            n += 1;
        }
    }
    let n = n.max(1);
    [(r / n) as u8, (g / n) as u8, (b / n) as u8]
}

/// Headless proof hook: with `DJG_SCREENSHOT=<png>` the app shows `DJG_PAGE`
/// (`playing` / `albums` / `search` / `album`), optionally runs `DJG_SEARCH` or
/// `DJG_ASK`, renders the window to the PNG after a settle delay and quits. It
/// draws through the window's own renderer, so it needs a display but never reads
/// the screen.
fn screenshot_hook(ui: &Rc<App>) {
    let Ok(path) = std::env::var("DJG_SCREENSHOT") else { return };
    if let Ok(w) = std::env::var("DJG_WIDTH").map(|v| v.parse::<i32>().unwrap_or(920)) {
        ui.window.set_default_size(w, 760);
    }
    let page = std::env::var("DJG_PAGE").unwrap_or_else(|_| "playing".into());
    let stack_page = if page == "album" { "albums" } else { page.as_str() };
    ui.stack.set_visible_child_name(stack_page);
    if let Ok(q) = std::env::var("DJG_SEARCH") {
        ui.search_entry.set_text(&q);
    }
    if let Ok(a) = std::env::var("DJG_ASK") {
        ui.send(Req::Ask(a));
    }
    // Keyboard proof: log every focus move as the focused row's title (or widget
    // type), so a scripted Tab / Down run shows which rows focus landed on.
    if std::env::var("DJG_FOCUS_LOG").is_ok() {
        ui.window.connect_focus_widget_notify(|w| {
            let Some(f) = gtk::prelude::GtkWindowExt::focus(w) else { return };
            let row = f.ancestor(adw::ActionRow::static_type()).and_downcast::<adw::ActionRow>();
            match row {
                Some(r) => eprintln!("djg-focus: row {:?} sensitive={}", r.title(), r.is_sensitive()),
                None => eprintln!("djg-focus: {}", f.type_().name()),
            }
        });
    }
    if let Ok(a) = std::env::var("DJG_ACTION") {
        let ui2 = ui.clone();
        glib::timeout_add_seconds_local_once(2, move || {
            let (group, name) = a.split_once('.').unwrap_or(("win", a.as_str()));
            let _ = match group {
                "app" => ui2.window.application().map(|ap| ap.activate_action(name, None)).is_some(),
                _ => gtk::prelude::WidgetExt::activate_action(&ui2.window, &a, None).is_ok(),
            };
        });
    }
    let settle = std::env::var("DJG_SETTLE").ok().and_then(|v| v.parse().ok()).unwrap_or(6u32);
    if page == "album" {
        let ui2 = ui.clone();
        glib::timeout_add_seconds_local_once(settle.saturating_sub(4).max(2), move || {
            if let Some(r) = ui2.albums_list.row_at_index(0) {
                r.activate();
            }
        });
    }
    let ui = ui.clone();
    glib::timeout_add_seconds_local_once(settle, move || {
        let win = ui.window.clone();
        let (w, h) = (win.width(), win.height());
        let paintable = gtk::WidgetPaintable::new(Some(&win));
        let snap = gtk::Snapshot::new();
        paintable.snapshot(&snap, w as f64, h as f64);
        if let (Some(node), Some(native)) = (snap.to_node(), win.native()) {
            let tex = native.renderer().expect("realized").render_texture(&node, None);
            if let Err(e) = tex.save_to_png(&path) {
                eprintln!("djg: screenshot failed: {e}");
            }
        }
        win.application().map(|a| a.quit());
    });
}
