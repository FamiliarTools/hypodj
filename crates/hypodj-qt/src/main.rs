//! djq - HypoDJ as a Qt 6 / QML app.
//!
//! Shape: the room is the OS and HypoDJ owns one warm instrument. The chrome is the
//! host's: djq never sets a Controls style, a palette or an accent, so Qt Quick
//! Controls pick the platform's (Breeze / Kirigami desktop style on Plasma, the
//! platform theme's palette elsewhere). The only HypoDJ-coloured surface is the
//! now-playing instrument, painted from `hypodj_style::qt`.
//!
//! The sockets, routing and words are `hypodj_app` - the very code djg runs - so the
//! two shells cannot disagree. `bridge` folds the worker's messages into JSON
//! properties on the GUI thread; `qml/Main.qml` draws them.

mod bridge;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QUrl};

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("djq {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    cxx_qt::init_crate!(cxx_qt_lib);
    cxx_qt::init_qml_module!("tech.familiartools.hypodj");

    let mut app = QGuiApplication::new();
    bridge::qobject::djq_setup_app();
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        // A QML error ends the run; an idle event loop would look alive.
        engine
            .as_mut()
            .on_object_creation_failed(|_, url| {
                eprintln!("djq: QML failed to load {url}");
                std::process::exit(3);
            })
            .release();
        engine.load(&QUrl::from("qrc:/qt/qml/tech/familiartools/hypodj/qml/Main.qml"));
    }
    if let Some(app) = app.as_mut() {
        std::process::exit(app.exec());
    }
}
