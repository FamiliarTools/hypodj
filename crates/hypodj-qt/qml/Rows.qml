// A boxed list of rows: title, optional subtitle, optional leading glyph, optional
// trailing chevron. Every colour is the host palette's; this is chrome, not content.
import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

Frame {
    id: box
    property var rows: []
    property bool chevron: false
    property string empty: ""
    property string emptySub: ""
    signal activated(var row)
    Layout.fillWidth: true
    padding: 0

    ColumnLayout {
        anchors.left: parent.left
        anchors.right: parent.right
        spacing: 0
        Repeater {
            model: box.rows.length === 0 && box.empty !== "" ? [{ "title": box.empty, "sub": box.emptySub, "inert": true }] : box.rows
            delegate: ItemDelegate {
                required property var modelData
                required property int index
                Layout.fillWidth: true
                hoverEnabled: !modelData.inert
                Accessible.name: modelData.title + (modelData.sub ? ", " + modelData.sub : "")
                contentItem: RowLayout {
                    spacing: 12
                    Label {
                        visible: !!modelData.glyph
                        text: modelData.glyph || ""
                        color: palette.placeholderText
                        Layout.preferredWidth: 20
                    }
                    ColumnLayout {
                        spacing: 2
                        Layout.fillWidth: true
                        Label {
                            text: modelData.title
                            elide: Text.ElideRight
                            Layout.fillWidth: true
                        }
                        Label {
                            visible: !!modelData.sub
                            text: modelData.sub || ""
                            elide: Text.ElideRight
                            opacity: 0.7
                            font.pointSize: Math.max(1, Qt.application.font.pointSize - 1)
                            Layout.fillWidth: true
                        }
                    }
                    Label {
                        visible: box.chevron
                        text: "›"
                        opacity: 0.6
                    }
                }
                onClicked: if (!modelData.inert) box.activated(modelData)
                Rectangle {
                    visible: index > 0
                    anchors.top: parent.top
                    width: parent.width
                    height: 1
                    color: palette.mid
                    opacity: 0.4
                }
            }
        }
    }
}
