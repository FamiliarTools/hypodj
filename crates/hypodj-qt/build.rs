//! Builds the QML module (qmlcachegen, qmltyperegistrar, rcc) and the C++ shim with
//! cxx-qt-build. Qt is located through `qmake -query` (the QMAKE variable first): in
//! Nix that must be a `qt6.env` prefix holding qtdeclarative next to qtbase, because
//! nixpkgs splits them into separate store paths (see nix/qt.nix).
use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(QmlModule::new("tech.familiartools.hypodj").qml_files(["qml/Main.qml", "qml/Rows.qml"]))
        .qt_module("Quick")
        .files(["src/bridge.rs"])
        .cpp_file("cpp/shim.cpp")
        .build();
}
