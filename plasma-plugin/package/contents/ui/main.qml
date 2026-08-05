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
    // Path we last actually POSTed to render-server. Compared against
    // the live config every tick (see the Timer below) instead of
    // relying solely on onProjectPathChanged firing -- clicking Apply in
    // the wallpaper settings dialog can replace `configuration` itself,
    // and a Connections target re-binding to a new object can miss the
    // one signal that mattered. Polling the actual value is slower by
    // at most one tick, but can't miss a change.
    property string lastPushedProjectPath: ""

    function pushProjectPath() {
        const path = root.configuration.ProjectPath;
        if (!path) {
            return;
        }
        root.lastPushedProjectPath = path;
        const xhr = new XMLHttpRequest();
        xhr.open("POST", "http://127.0.0.1:47824/project");
        xhr.send(path);
    }

    // Tells render-server where this wallpaper item actually sits on the
    // virtual desktop. render-server gets the cursor's GLOBAL position
    // directly over D-Bus (from the KWin script, see kwin-script/package
    // -- no HTTP round-trip through QML needed for that anymore), but it
    // has no Wayland/Qt connection of its own, so only QML can supply
    // this piece: without it there'd be no way to turn a global position
    // into "is the cursor over this monitor, and where."
    function pushGeometry() {
        const xhr = new XMLHttpRequest();
        xhr.open("POST", "http://127.0.0.1:47824/geometry");
        xhr.send(Screen.virtualX + "," + Screen.virtualY + "," + root.width + "," + root.height);
    }

    Connections {
        target: root.configuration
        function onProjectPathChanged() {
            root.pushProjectPath();
        }
    }

    Connections {
        target: Screen
        function onVirtualXChanged() { root.pushGeometry(); }
        function onVirtualYChanged() { root.pushGeometry(); }
    }

    onWidthChanged: root.pushGeometry()
    onHeightChanged: root.pushGeometry()

    Component.onCompleted: {
        root.pushProjectPath();
        root.pushGeometry();
    }

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
            onTriggered: {
                if (root.configuration.ProjectPath !== root.lastPushedProjectPath) {
                    root.pushProjectPath();
                }
                sceneMeta.poll();
            }
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

        // Cursor-reactive rendering (xray mask, etc.) happens entirely in
        // render-server now: it gets the true, compositor-level pointer
        // position directly from a KWin script over D-Bus (see
        // kwin-script/package), and this item's placement via
        // root.pushGeometry() above. Folder View's icon layer sits above
        // WallpaperItem and would consume hover before it reached any
        // QML-side cursor tracking here anyway -- that's the whole
        // reason cursor handling doesn't live in this file.
    }
}
