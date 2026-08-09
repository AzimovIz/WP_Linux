import QtQuick
import QtQuick.Window
import org.kde.plasma.plasmoid

WallpaperItem {
    id: root

    // render-server now tracks one independently-loaded project per
    // monitor rather than one shared by the whole desktop (see its own
    // module doc comment), so every request needs to say which monitor
    // it's for. `Screen.name` is resolved per wallpaper-item-instance
    // already -- it's the same attached property `Screen.virtualX`/
    // `Screen.virtualY` below already read correctly per screen.
    readonly property string monitorId: Screen.name || ""

    // Appends this item's monitor id (as `?monitor=` or `&monitor=`,
    // whichever `url` still needs) to a render-server URL -- every route
    // takes one now, see main.rs's module doc comment. Centralized here
    // so a missing `monitorId` (Screen not resolved yet) is a single
    // guard instead of one per call site. Returns "" when there's no
    // monitor id yet, which every call site treats as "don't send".
    function withMonitorParam(url) {
        if (!root.monitorId) {
            return "";
        }
        const separator = url.indexOf("?") === -1 ? "?" : "&";
        return url + separator + "monitor=" + encodeURIComponent(root.monitorId);
    }

    // Tells render-server where this wallpaper item actually sits on the
    // virtual desktop. render-server gets the cursor's GLOBAL position
    // directly over D-Bus (from the KWin script, see kwin-script/package
    // -- no HTTP round-trip through QML needed for that anymore), but it
    // has no Wayland/Qt connection of its own, so only QML can supply
    // this piece: without it there'd be no way to turn a global position
    // into "is the cursor over this monitor, and where."
    function pushGeometry() {
        const url = root.withMonitorParam("http://127.0.0.1:47824/geometry");
        if (!url) {
            return;
        }
        const xhr = new XMLHttpRequest();
        xhr.open("POST", url);
        xhr.send(Screen.virtualX + "," + Screen.virtualY + "," + root.width + "," + root.height);
    }

    Connections {
        target: Screen
        function onVirtualXChanged() { root.pushGeometry(); }
        function onVirtualYChanged() { root.pushGeometry(); }
        // A screen can be (re-)identified by the compositor after this
        // item already existed (e.g. hotplug/reconfiguration) -- resend
        // once `monitorId` actually has a value to key on, same as
        // Component.onCompleted does for the normal startup case.
        function onNameChanged() { root.pushGeometry(); }
    }

    onWidthChanged: root.pushGeometry()
    onHeightChanged: root.pushGeometry()

    Component.onCompleted: {
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
            property var activeXhr: null
            // Whether render-server currently has geometry on file for
            // this monitor. `pushGeometry()` normally only fires
            // reactively (screen moved/resized) with no retry of its own
            // -- if its one push at Component.onCompleted lands before
            // render-server has even bound its HTTP port yet (e.g. right
            // after a reboot, while it's still initializing wgpu), that
            // POST is just lost, and nothing would ever resend it since
            // the screen itself isn't going to move again on its own.
            // Polling this flag closes that gap (see the resend below).
            property bool hasGeometry: false

            onFrameIdChanged: framePoll.refresh()

            // Aborts a still-in-flight /meta request rather than letting it
            // finish on its own time: if this item is torn down mid-request
            // (screen reconfig, wallpaper type switched away), the response
            // would otherwise arrive later and run its callback against a
            // scope whose properties are gone, keeping the underlying
            // network reply alive for no reason in the meantime.
            Component.onDestruction: {
                if (activeXhr) {
                    activeXhr.abort();
                    activeXhr = null;
                }
            }

            function poll() {
                if (requestInFlight || !root.monitorId) {
                    return;
                }
                requestInFlight = true;

                const xhr = new XMLHttpRequest();
                activeXhr = xhr;
                xhr.onreadystatechange = function () {
                    if (xhr.readyState !== XMLHttpRequest.DONE) {
                        return;
                    }
                    requestInFlight = false;
                    activeXhr = null;

                    if (xhr.status !== 200) {
                        ready = false;
                        return;
                    }

                    try {
                        const meta = JSON.parse(xhr.responseText);
                        ready = !!meta.ready;
                        needsCursor = !!meta.needs_cursor;
                        frameId = meta.frame_id;
                        hasGeometry = !!meta.has_geometry;
                        if (meta.fps) {
                            fps = meta.fps;
                        }
                    } catch (e) {
                        ready = false;
                    }

                    // render-server may have been restarted (losing
                    // whatever geometry we last pushed) since we last
                    // pushed it -- resend until it sticks, see
                    // `hasGeometry`'s own doc comment. `/project` doesn't
                    // need this dance: its assignment is persisted to disk
                    // and reloaded by render-server itself on every
                    // startup (see the WP Linux app's monitors_config
                    // module), so there's nothing for this item to resend
                    // at all.
                    if (!hasGeometry) {
                        root.pushGeometry();
                    }
                };
                xhr.open("GET", root.withMonitorParam("http://127.0.0.1:47824/meta"));
                xhr.send();
            }
        }

        // Matches render-server's per-project target fps (configured in
        // the editor) so it can keep up with frame_id changes -- static
        // projects just bump frame_id once and this settles into a
        // cheap no-op poll regardless of the interval.
        Timer {
            interval: Math.max(8, Math.round(1000 / sceneMeta.fps))
            running: root.monitorId !== ""
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

            // Guards against piling up frame requests faster than they can
            // be fetched+decoded. Without this, a frame_id that bumps
            // faster than the HTTP GET + decode of the previous frame
            // completes (easy at fps 30 with a full-size frame) kept
            // overwriting the target Image's source mid-load, forcing Qt's
            // pixmap loader to abort-and-restart on every single tick --
            // under sustained load that churn backs up inside plasmashell's
            // native pixmap loader queue and is only ever reclaimed by
            // killing the process. Dropping a refresh when the target
            // buffer is still mid-load just means the next poll tick offers
            // the (by-then-newer) frame again -- nothing is lost but a
            // couple of stale intermediate frames.
            function refresh() {
                const url = root.withMonitorParam("http://127.0.0.1:47824/frame?t=" + Date.now());
                if (!url) {
                    return;
                }
                if (activeBuffer === 0) {
                    if (imgB.status === Image.Loading) {
                        return;
                    }
                    urlB = url;
                } else {
                    if (imgA.status === Image.Loading) {
                        return;
                    }
                    urlA = url;
                }
            }
        }

        Image {
            id: imgA
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
            id: imgB
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
