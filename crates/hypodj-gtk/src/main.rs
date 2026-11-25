//! djg - HypoDJ as a GNOME app (GTK4 + libadwaita).
//!
//! Shape: the room is the OS and HypoDJ owns one warm instrument. Every pixel of
//! chrome is stock libadwaita (header bar, view switcher, boxed lists, cards,
//! alert dialogs); the only HypoDJ-styled surface is the now-playing `.instrument`,
//! whose ground, ink and ember come from `hypodj_style::gtk_css`.
//!
//! Replies to the ask entry land in a dismissible card above it (answers run to several
//! lines). IO never runs on the main loop: `worker` owns the sockets on std threads and
//! sends `Msg` values back over an async channel, which `ui::App::apply` folds into
//! widgets. The words come from `view` (pure, tested) and from hypodj-client, the
//! same functions the TUI and `djc` use, so the three cannot disagree.

mod ui;
mod view;
mod worker;

use adw::prelude::*;

pub const APP_ID: &str = "tech.familiartools.HypoDJ";

fn main() -> gtk::glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(ui::activate);
    // GTK parses argv itself; djg takes no options of its own yet.
    app.run()
}
