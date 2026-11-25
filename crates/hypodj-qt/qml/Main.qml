// djq's window. Chrome (tool bar, tabs, lists, dialogs, fields) is stock Qt Quick
// Controls in whatever style the host chose - djq never sets a style, a palette or
// an accent. The one HypoDJ-owned surface is the instrument, whose colours come
// from the backend (hypodj_style::qt) and never from the palette.
import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import tech.familiartools.hypodj

ApplicationWindow {
    id: win
    title: "HypoDJ"
    width: Number(backend.env("DJQ_WIDTH")) || 920
    height: 760
    minimumWidth: 360
    minimumHeight: 294
    visible: true

    readonly property bool narrow: width < 600
    readonly property var nowv: backend.now ? JSON.parse(backend.now) : ({ "title": "Connecting", "artist": "", "album": "", "held": "", "playing": false, "verb": "play", "volume": -1, "upNext": [], "status": { "label": "Connecting", "problem": false, "details": "", "icon": "" }, "banner": "" })
    readonly property var inst: backend.instrument ? JSON.parse(backend.instrument) : null
    readonly property var albumsv: backend.albums ? JSON.parse(backend.albums) : ({ "rows": [], "note": "Loading albums" })
    readonly property var albumv: backend.album ? JSON.parse(backend.album) : ({ "title": "", "uri": "", "rows": [] })
    readonly property var searchv: backend.search ? JSON.parse(backend.search) : null
    // The room's lightness, read from the colour the host style actually paints the
    // window with (Fusion and Breeze take it from the palette; Material sets it from
    // its own theme without touching the palette): the instrument follows it.
    readonly property color roomColor: win.color
    readonly property bool roomDark: roomColor.hslLightness < 0.5
    onRoomDarkChanged: backend.setRoom(roomDark, false)

    Backend {
        id: backend
        onShowPlaying: tabs.currentIndex = 0
        onSaid: replyCard.visible = true
        onAlbumOpened: albumsStack.depth < 2 ? albumsStack.push(albumPage) : null
        // One plan waits at a time: the worker has already cancelled the plan this
        // one supersedes, so the open dialog simply takes the newer token and body.
        onConfirmPlan: (token, body) => { planDialog.token = token; planDialog.body = body; planDialog.open() }
        onConfirmCommand: (command, question) => { clearDialog.command = command; clearDialog.title = question; clearDialog.open() }
    }

    Component.onCompleted: {
        backend.setRoom(roomDark, false)
        backend.start(win)
        proof.begin()
    }

    header: ToolBar {
        ColumnLayout {
            anchors.fill: parent
            spacing: 0
            TabBar {
                id: tabs
                visible: !win.narrow
                Layout.alignment: Qt.AlignHCenter
                Layout.preferredWidth: Math.min(480, win.width - 24)
                TabButton { text: "Playing" }
                TabButton { text: "Albums" }
                TabButton { text: "Search" }
            }
            Label {
                visible: win.narrow
                text: ["Playing", "Albums", "Search"][tabs.currentIndex]
                font.bold: true
                Layout.alignment: Qt.AlignHCenter
                Layout.margins: 8
            }
        }
    }

    footer: ColumnLayout {
        spacing: 0
        // HypoDJ's last reply. Answers run to several lines, so a card, not a toast.
        Frame {
            id: replyCard
            visible: false
            Layout.fillWidth: true
            Layout.margins: 12
            Layout.bottomMargin: 0
            RowLayout {
                anchors.fill: parent
                Label {
                    text: backend.reply
                    wrapMode: Text.Wrap
                    textFormat: Text.PlainText
                    Layout.fillWidth: true
                }
                ToolButton {
                    text: "✕"
                    Accessible.name: "Dismiss"
                    ToolTip.visible: hovered
                    ToolTip.text: "Dismiss"
                    Layout.alignment: Qt.AlignTop
                    onClicked: replyCard.visible = false
                }
            }
        }
        RowLayout {
            Layout.fillWidth: true
            Layout.margins: 12
            Layout.topMargin: 6
            Layout.bottomMargin: 6
            spacing: 6
            TextField {
                id: askField
                placeholderText: "Ask or tell HypoDJ - \"what is this\", \"play something calmer\""
                Accessible.name: "Ask or tell HypoDJ"
                Layout.fillWidth: true
                onAccepted: { backend.ask(text); text = "" }
            }
            // The status in words; the popup carries what the label had to drop.
            ToolButton {
                id: statusButton
                text: win.narrow ? (win.nowv.status.problem ? "Offline" : "Status") : win.nowv.status.label
                Accessible.name: "Server and offline status: " + win.nowv.status.label
                Layout.maximumWidth: 320
                contentItem: Label {
                    text: statusButton.text
                    elide: Text.ElideRight
                    verticalAlignment: Text.AlignVCenter
                }
                onClicked: statusPopup.open()
                Popup {
                    id: statusPopup
                    y: -height - 6
                    x: parent.width - width
                    width: 360
                    Label {
                        width: parent.width
                        text: win.nowv.status.details
                        wrapMode: Text.Wrap
                        textFormat: Text.PlainText
                    }
                }
            }
        }
        TabBar {
            visible: win.narrow
            Layout.fillWidth: true
            currentIndex: tabs.currentIndex
            onCurrentIndexChanged: tabs.currentIndex = currentIndex
            TabButton { text: "Playing" }
            TabButton { text: "Albums" }
            TabButton { text: "Search" }
        }
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: 0
        // The server banner: a problem, said in words, with what still works.
        Pane {
            visible: win.nowv.banner !== ""
            Layout.fillWidth: true
            Label {
                anchors.fill: parent
                text: win.nowv.banner
                wrapMode: Text.Wrap
                horizontalAlignment: Text.AlignHCenter
                font.bold: true
            }
        }

        StackLayout {
            currentIndex: tabs.currentIndex
            Layout.fillWidth: true
            Layout.fillHeight: true

            // Playing: the instrument, then Up Next.
            ScrollView {
                id: playingScroll
                contentWidth: availableWidth
                ColumnLayout {
                    width: Math.min(760, playingScroll.availableWidth - 24)
                    x: (playingScroll.availableWidth - width) / 2
                    spacing: 18

                    Item { Layout.preferredHeight: 0 }
                    Rectangle {
                        id: instrument
                        objectName: "instrument"
                        Layout.fillWidth: true
                        Layout.preferredHeight: instGrid.implicitHeight + 48
                        radius: 12
                        // High contrast (inst null): the plain room.
                        color: win.inst ? win.inst.ground : palette.window
                        readonly property color ink: win.inst ? win.inst.ink : palette.windowText
                        readonly property color quiet: win.inst ? win.inst.quiet : palette.windowText
                        readonly property color ember: win.inst ? win.inst.ember : palette.windowText

                        GridLayout {
                            id: instGrid
                            anchors.fill: parent
                            anchors.margins: 24
                            columns: win.narrow ? 1 : 2
                            columnSpacing: 24
                            rowSpacing: 18

                            Rectangle {
                                Layout.preferredWidth: 220
                                Layout.preferredHeight: 220
                                Layout.alignment: Qt.AlignHCenter | Qt.AlignTop
                                radius: 8
                                color: Qt.rgba(instrument.ink.r, instrument.ink.g, instrument.ink.b, 0.08)
                                clip: true
                                Image {
                                    anchors.fill: parent
                                    source: backend.cover
                                    fillMode: Image.PreserveAspectCrop
                                    asynchronous: true
                                    Accessible.name: "Cover"
                                }
                            }
                            ColumnLayout {
                                Layout.fillWidth: true
                                Layout.alignment: Qt.AlignVCenter
                                spacing: 6
                                Label {
                                    text: win.nowv.title
                                    color: instrument.ink
                                    font.pointSize: Qt.application.font.pointSize * 1.8
                                    font.bold: true
                                    wrapMode: Text.Wrap
                                    Layout.fillWidth: true
                                }
                                Label {
                                    visible: text !== ""
                                    text: win.nowv.artist
                                    color: instrument.ink
                                    font.pointSize: Qt.application.font.pointSize * 1.2
                                    wrapMode: Text.Wrap
                                    Layout.fillWidth: true
                                }
                                Label {
                                    visible: text !== ""
                                    text: win.nowv.album
                                    color: instrument.quiet
                                    wrapMode: Text.Wrap
                                    Layout.fillWidth: true
                                }
                                // Ember: only what HypoDJ is holding for you.
                                Label {
                                    visible: text !== ""
                                    text: win.nowv.held
                                    color: instrument.ember
                                    font.bold: !win.inst
                                    font.pointSize: Math.max(1, Qt.application.font.pointSize - 1)
                                    Layout.fillWidth: true
                                }
                                RowLayout {
                                    spacing: 12
                                    Layout.topMargin: 6
                                    RoundButton {
                                        text: "⏮"
                                        font.pointSize: Qt.application.font.pointSize * 1.6
                                        flat: true
                                        palette.buttonText: instrument.ink
                                        Accessible.name: "Previous"
                                        ToolTip.visible: hovered
                                        ToolTip.text: "Previous"
                                        onClicked: backend.command("previous")
                                    }
                                    Button {
                                        text: win.nowv.playing ? "Pause" : "Play"
                                        Accessible.name: text
                                        onClicked: backend.command(win.nowv.verb)
                                    }
                                    RoundButton {
                                        text: "⏭"
                                        font.pointSize: Qt.application.font.pointSize * 1.6
                                        flat: true
                                        palette.buttonText: instrument.ink
                                        Accessible.name: "Next"
                                        ToolTip.visible: hovered
                                        ToolTip.text: "Next"
                                        onClicked: backend.command("next")
                                    }
                                }
                                RowLayout {
                                    Layout.fillWidth: true
                                    Label { text: "Volume"; color: instrument.quiet }
                                    Slider {
                                        id: volume
                                        from: 0; to: 100; stepSize: 1
                                        enabled: win.nowv.volume >= 0
                                        value: Math.max(0, win.nowv.volume)
                                        Accessible.name: "Volume"
                                        Layout.fillWidth: true
                                        // Only the user's own move sends; a refresh never echoes.
                                        onMoved: backend.command("setvol " + Math.round(value))
                                    }
                                }
                            }
                        }
                    }

                    Label { text: "Up Next"; font.bold: true }
                    Rows {
                        rows: win.nowv.upNext
                        empty: "Nothing queued"
                        emptySub: "The queue ends after this track."
                        onActivated: (r) => backend.command("play " + r.pos)
                    }
                    Item { Layout.preferredHeight: 12 }
                }
            }

            // Albums: the list, then an album's tracks.
            StackView {
                id: albumsStack
                clip: true
                initialItem: ScrollView {
                    id: albumsScroll
                    contentWidth: availableWidth
                    ColumnLayout {
                        width: Math.min(760, albumsScroll.availableWidth - 24)
                        x: (albumsScroll.availableWidth - width) / 2
                        spacing: 12
                        Item { Layout.preferredHeight: 0 }
                        Label {
                            visible: text !== ""
                            text: win.albumsv.note
                            opacity: 0.7
                            wrapMode: Text.Wrap
                            Layout.fillWidth: true
                        }
                        Rows {
                            id: albumsList
                            objectName: "albumsList"
                            rows: win.albumsv.rows
                            chevron: true
                            onActivated: (r) => backend.openAlbum(r.uri, r.title)
                        }
                        Item { Layout.preferredHeight: 12 }
                    }
                }
            }

            // Search.
            ScrollView {
                id: searchScroll
                contentWidth: availableWidth
                ColumnLayout {
                    width: Math.min(760, searchScroll.availableWidth - 24)
                    x: (searchScroll.availableWidth - width) / 2
                    spacing: 12
                    Item { Layout.preferredHeight: 0 }
                    TextField {
                        id: searchField
                        placeholderText: "Artists, albums, songs"
                        Accessible.name: "Search the library"
                        Layout.fillWidth: true
                        onTextChanged: searchDelay.restart()
                        Timer { id: searchDelay; interval: 250; onTriggered: backend.searchFor(searchField.text) }
                    }
                    ColumnLayout {
                        visible: !win.searchv || win.searchv.error !== "" || win.searchv.hits.length === 0
                        Layout.fillWidth: true
                        Layout.topMargin: 60
                        spacing: 8
                        Label {
                            text: !win.searchv ? "Search the library" : win.searchv.error !== "" ? "Search did not work" : "No results"
                            font.pointSize: Qt.application.font.pointSize * 1.5
                            font.bold: true
                            Layout.alignment: Qt.AlignHCenter
                        }
                        Label {
                            text: !win.searchv ? "Type to find artists, albums and songs." : win.searchv.error !== "" ? win.searchv.error : "Nothing matches \"" + win.searchv.query + "\"."
                            opacity: 0.7
                            wrapMode: Text.Wrap
                            horizontalAlignment: Text.AlignHCenter
                            Layout.fillWidth: true
                        }
                    }
                    Rows {
                        visible: !!win.searchv && win.searchv.hits.length > 0
                        rows: win.searchv ? win.searchv.hits.map(h => ({ "title": h.title, "sub": h.sub, "uri": h.uri, "glyph": h.kind === "artist" ? "☺" : h.kind === "album" ? "◎" : "♪" })) : []
                        onActivated: (r) => backend.playUri(r.uri)
                    }
                    Item { Layout.preferredHeight: 12 }
                }
            }
        }
    }

    Component {
        id: albumPage
        ScrollView {
            id: albumScroll
            contentWidth: availableWidth
            ColumnLayout {
                width: Math.min(760, albumScroll.availableWidth - 24)
                x: (albumScroll.availableWidth - width) / 2
                spacing: 18
                Item { Layout.preferredHeight: 0 }
                RowLayout {
                    Layout.fillWidth: true
                    ToolButton {
                        text: "‹"
                        Accessible.name: "Back to Albums"
                        ToolTip.visible: hovered
                        ToolTip.text: "Back to Albums"
                        onClicked: albumsStack.pop()
                    }
                    Label {
                        text: win.albumv.title
                        font.pointSize: Qt.application.font.pointSize * 1.5
                        font.bold: true
                        elide: Text.ElideRight
                        Layout.fillWidth: true
                    }
                    Button {
                        text: "Play Album"
                        highlighted: true
                        onClicked: backend.playUri(win.albumv.uri)
                    }
                }
                Rows {
                    rows: win.albumv.rows
                    onActivated: (r) => backend.playUri(r.uri)
                }
                Item { Layout.preferredHeight: 12 }
            }
        }
    }

    Dialog {
        id: planDialog
        property string token: ""
        property string body: ""
        title: "Do this?"
        modal: true
        anchors.centerIn: parent
        width: Math.min(440, win.width - 24)
        Label {
            width: parent.width
            text: planDialog.body
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
        }
        footer: DialogButtonBox {
            Button { text: "Cancel"; DialogButtonBox.buttonRole: DialogButtonBox.RejectRole }
            Button { text: "Do It"; highlighted: true; DialogButtonBox.buttonRole: DialogButtonBox.AcceptRole }
        }
        onAccepted: backend.confirm(token)
        onRejected: backend.cancel(token)
    }

    Dialog {
        id: clearDialog
        property string command: ""
        modal: true
        anchors.centerIn: parent
        footer: DialogButtonBox {
            Button { text: "Cancel"; DialogButtonBox.buttonRole: DialogButtonBox.RejectRole }
            Button { text: "Clear"; DialogButtonBox.buttonRole: DialogButtonBox.DestructiveRole; onClicked: { backend.command(clearDialog.command); clearDialog.close() } }
        }
    }

    // Headless proof hook: with DJQ_SCREENSHOT=<png> show DJQ_PAGE (playing, albums,
    // album, search), optionally run DJQ_SEARCH or DJQ_ASK, grab the whole window
    // after DJQ_SETTLE seconds and quit. It reads the window's own frame, never the
    // screen.
    QtObject {
        id: proof
        readonly property string path: backend.env("DJQ_SCREENSHOT")
        readonly property string page: backend.env("DJQ_PAGE") || "playing"
        readonly property int settle: Number(backend.env("DJQ_SETTLE")) || 6
        function begin() {
            if (path === "") return
            tabs.currentIndex = ({ "playing": 0, "albums": 1, "album": 1, "search": 2 })[page] || 0
            const s = backend.env("DJQ_SEARCH")
            if (s !== "") searchField.text = s
            const a = backend.env("DJQ_ASK")
            if (a !== "") backend.ask(a)
            if (page === "album") openFirst.start()
            shoot.start()
        }
    }
    Timer {
        id: openFirst
        interval: Math.max(2, proof.settle - 4) * 1000
        onTriggered: if (win.albumsv.rows.length > 0) backend.openAlbum(win.albumsv.rows[0].uri, win.albumsv.rows[0].title)
    }
    Timer {
        id: shoot
        interval: proof.settle * 1000
        onTriggered: {
            if (!backend.screenshot(proof.path)) console.error("djq: screenshot failed")
            Qt.quit()
        }
    }
}
