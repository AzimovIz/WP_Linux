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
        // loaded, whether it needs cursor input (an xray layer), and a
        // frame_id that bumps every time render-server produces a new
        // composited frame -- static projects bump it once; projects with
        // a gif/xray layer bump it continuously, capped at ~30fps.
        Item {
            id: sceneMeta

            property bool requestInFlight: false
            property bool ready: false
            property bool needsCursor: false
            property real frameId: -1
            property int fps: 30

            onFrameIdChanged: framePoll.refresh()

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
                        needsCursor = !!meta.needs_cursor;
                        frameId = meta.frame_id;
                        if (meta.fps) {
                            fps = meta.fps;
                        }
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

        // Matches render-server's per-project target fps (configured in
        // the editor) so it can keep up with frame_id changes -- static
        // projects just bump frame_id once and this settles into a
        // cheap no-op poll regardless of the interval.
        Timer {
            interval: Math.max(8, Math.round(1000 / sceneMeta.fps))
            running: root.hasProject
            repeat: true
            triggeredOnStart: true
            onTriggered: sceneMeta.poll()
        }

        // Fetches the current rendered frame from render-server. Only
        // re-fetched when frame_id actually changes (see
        // sceneMeta.onFrameIdChanged above) -- not on its own timer, so a
        // slow-to-decode frame is never aborted mid-load by a fresher
        // cache-busted request racing in behind it.
        //
        // Loaded into a hidden "back buffer" Image and only swapped in
        // once fully decoded, using two Image elements ping-ponged via
        // activeBuffer. Turns out Qt Quick does NOT keep the previous
        // pixmap on screen while a new `source` decodes (confirmed via
        // logging status transitions: it goes Ready -> Loading on every
        // single frame) -- it briefly shows nothing, letting the
        // background color flash through. A single reloading Image can't
        // avoid that gap; two of them, swapped only when the new one is
        // actually ready, can.
        Item {
            id: framePoll

            property int activeBuffer: 0
            property string urlA: ""
            property string urlB: ""

            function refresh() {
                const url = "http://127.0.0.1:47824/frame?t=" + Date.now();
                if (activeBuffer === 0) {
                    urlB = url;
                } else {
                    urlA = url;
                }
            }
        }

        Image {
            anchors.fill: parent
            fillMode: Image.PreserveAspectCrop
            asynchronous: true
            cache: false
            source: framePoll.urlA
            visible: sceneMeta.ready && framePoll.activeBuffer === 0
            onStatusChanged: {
                if (status === Image.Ready && framePoll.activeBuffer !== 0) {
                    framePoll.activeBuffer = 0;
                }
            }
        }

        Image {
            anchors.fill: parent
            fillMode: Image.PreserveAspectCrop
            asynchronous: true
            cache: false
            source: framePoll.urlB
            visible: sceneMeta.ready && framePoll.activeBuffer === 1
            onStatusChanged: {
                if (status === Image.Ready && framePoll.activeBuffer !== 1) {
                    framePoll.activeBuffer = 1;
                }
            }
        }

        // Folder View's icon layer sits above WallpaperItem and consumes
        // hover before it gets here, so a plain HoverHandler never fires
        // while desktop icons are shown. Instead we poll a tiny local HTTP
        // endpoint (crates/cursor-bridge) that a companion KWin script
        // keeps updated with the real, compositor-level cursor position --
        // see kwin-script/package for the other half. All the actual
        // cursor-reactive rendering (xray mask, etc.) now happens in
        // render-server; this only relays where the pointer is, in
        // normalized item-local coordinates, since only QML knows this
        // item's on-screen placement and size.
        Item {
            id: cursorRelay

            property bool requestInFlight: false

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
                        return;
                    }

                    try {
                        const pos = JSON.parse(xhr.responseText);
                        const localX = pos.x - Screen.virtualX;
                        const localY = pos.y - Screen.virtualY;
                        const inside = localX >= 0 && localX <= background.width
                                       && localY >= 0 && localY <= background.height;
                        pushCursor(inside ? (localX / background.width) : null,
                                   inside ? (localY / background.height) : null);
                    } catch (e) {
                        // ignore, try again next tick
                    }
                };
                xhr.open("GET", "http://127.0.0.1:47823/cursor");
                xhr.send();
            }

            function pushCursor(u, v) {
                const xhr = new XMLHttpRequest();
                xhr.open("POST", "http://127.0.0.1:47824/cursor");
                xhr.send(u === null ? "none" : (u + "," + v));
            }
        }

        Timer {
            interval: 16
            running: sceneMeta.needsCursor
            repeat: true
            onTriggered: cursorRelay.poll()
        }
    }
}
