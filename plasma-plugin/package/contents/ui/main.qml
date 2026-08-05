import QtQuick
import QtQuick.Window
import org.kde.plasma.plasmoid

WallpaperItem {
    id: root

    Rectangle {
        id: background
        anchors.fill: parent
        color: "#0d1428"

        SequentialAnimation on color {
            loops: Animation.Infinite
            ColorAnimation { to: "#2a1740"; duration: 4000 }
            ColorAnimation { to: "#0d1428"; duration: 4000 }
        }

        // Folder View's icon layer sits above WallpaperItem and consumes
        // hover before it gets here, so a plain HoverHandler never fires
        // while desktop icons are shown. Instead we poll a tiny local
        // HTTP endpoint (crates/cursor-bridge) that a companion KWin
        // script keeps updated with the real, compositor-level cursor
        // position -- see kwin-script/package for the other half.
        Item {
            id: cursorPoll

            property bool requestInFlight: false
            property bool haveCursor: false
            property real localX: 0
            property real localY: 0

            function poll() {
                if (requestInFlight) {
                    return;
                }
                requestInFlight = true;

                const xhr = new XMLHttpRequest();
                xhr.onreadystatechange = function () {
                    if (xhr.readyState !== XMLHttpRequest.DONE) {
                        return;
                    }
                    requestInFlight = false;

                    if (xhr.status !== 200) {
                        haveCursor = false;
                        return;
                    }

                    try {
                        const pos = JSON.parse(xhr.responseText);
                        localX = pos.x - Screen.virtualX;
                        localY = pos.y - Screen.virtualY;
                        haveCursor = true;
                    } catch (e) {
                        haveCursor = false;
                    }
                };
                xhr.open("GET", "http://127.0.0.1:47823/cursor");
                xhr.send();
            }
        }

        Timer {
            interval: 16
            running: true
            repeat: true
            onTriggered: cursorPoll.poll()
        }

        Rectangle {
            width: 220
            height: 220
            radius: width / 2
            color: "#e8a23a"
            x: cursorPoll.localX - width / 2
            y: cursorPoll.localY - height / 2
            opacity: (cursorPoll.haveCursor
                      && cursorPoll.localX >= 0 && cursorPoll.localX <= background.width
                      && cursorPoll.localY >= 0 && cursorPoll.localY <= background.height)
                     ? 0.35 : 0.0

            Behavior on opacity {
                NumberAnimation { duration: 150 }
            }
        }
    }
}
