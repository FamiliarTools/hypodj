// The two Qt calls cxx-qt-lib does not wrap. No behaviour lives here.
#pragma once
#include <QtCore/QString>
#include <QtQuick/QQuickWindow>

// Application identity: the desktop file id (the Wayland app id) and the theme icon
// `hypodj`, so the window and the task switcher show the HypoDJ mark.
void djq_setup_app();
// The whole window (chrome, overlays, dialogs) to a PNG. False when it failed.
bool djq_grab(QQuickWindow *w, const QString &path);
