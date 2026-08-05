import QtQuick
import QtQuick.Window
import org.kde.plasma.plasmoid

WallpaperItem {
    id: root

    // While no project is configured yet, don't touch the network at all --
    // just show the plain background below. Avoids spamming "Connection
    // refused" (render-server may not even be running yet) and gives a
    // clean, obviously-unconfigured state instead.
    readonly property bool hasProject: root.configuration.ProjectPath !== ""

    function pushProjectPath() {
        const path = root.configuration.ProjectPath;
        if (!path) {
            return;
        }
        const xhr = new XMLHttpRequest();
        xhr.open("POST", "http://127.0.0.1:47824/project");
        xhr.send(path);
    }

    Connections {
        target: root.configuration
        function onProjectPathChanged() {
            root.pushProjectPath();
        }
    }

    Component.onCompleted: root.pushProjectPath()

    Rectangle {
        id: background
        anchors.fill: parent
        color: "#0d1428"

        // Polls render-server (crates/render-server) for whether a scene is
        // loaded and what it wants from the host -- right now just whether
        // to draw the cursor glow on top.
        Item {
            id: sceneMeta

            property bool requestInFlight: false
            property bool ready: false
            property bool cursorGlow: false

            onReadyChanged: {
                console.log("wplinux: sceneMeta.ready =", ready);
                if (ready) {
                    framePoll.refresh();
                }
            }

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
                        ready = false;
                        return;
                    }

                    try {
                        const meta = JSON.parse(xhr.responseText);
                        ready = !!meta.ready;
                        cursorGlow = !!meta.cursor_glow;
                    } catch (e) {
                        ready = false;
                    }

                    // render-server may have been restarted (lost its
                    // in-memory project) after we already pushed the path
                    // once, e.g. on load. Resend it until it sticks instead
                    // of requiring a manual re-poke.
                    if (!ready && root.hasProject) {
                        root.pushProjectPath();
                    }
                };
                xhr.open("GET", "http://127.0.0.1:47824/meta");
                xhr.send();
            }
        }

        Timer {
            interval: 1000
            running: root.hasProject
            repeat: true
            triggeredOnStart: true
            onTriggered: sceneMeta.poll()
        }

        // Fetches the current rendered frame from render-server. Only
        // re-fetched when the scene actually becomes ready (see
        // sceneMeta.onReadyChanged above) -- NOT on a fixed timer. A 1080p
        // PNG can take a while to decode, and re-requesting a fresh
        // cache-busted URL every second was aborting the in-flight load
        // before it ever reached Image.Ready, so the picture never showed.
        Item {
            id: framePoll

            property string url: ""

            function refresh() {
                url = "http://127.0.0.1:47824/frame?t=" + Date.now();
            }
        }

        Image {
            id: sceneImage
            anchors.fill: parent
            fillMode: Image.PreserveAspectCrop
            asynchronous: true
            cache: false
            source: framePoll.url
            onStatusChanged: console.log("wplinux: sceneImage.status =", status, "source =", source)
            visible: sceneMeta.ready && status === Image.Ready
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
            opacity: (sceneMeta.cursorGlow
                      && cursorPoll.haveCursor
                      && cursorPoll.localX >= 0 && cursorPoll.localX <= background.width
                      && cursorPoll.localY >= 0 && cursorPoll.localY <= background.height)
                     ? 0.35 : 0.0

            Behavior on opacity {
                NumberAnimation { duration: 150 }
            }
        }
    }
}
