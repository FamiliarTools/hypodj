#include "hypodj-qt/cpp/shim.h"

#include <QtGui/QGuiApplication>
#include <QtGui/QIcon>
#include <QtGui/QImage>

void djq_setup_app() {
    QGuiApplication::setDesktopFileName(QStringLiteral("tech.familiartools.HypoDJ.Qt"));
    QGuiApplication::setApplicationDisplayName(QStringLiteral("HypoDJ"));
    QGuiApplication::setWindowIcon(QIcon::fromTheme(QStringLiteral("hypodj")));
}

bool djq_grab(QQuickWindow *w, const QString &path) {
    if (!w) return false;
    return w->grabWindow().save(path, "PNG");
}
