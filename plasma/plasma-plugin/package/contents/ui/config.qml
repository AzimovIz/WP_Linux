import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.plasma.plasma5support as Plasma5Support

ColumnLayout {
    id: root

    // Plasma's wallpaper config dialog always tries to set these on every
    // wallpaper's config page, regardless of whether it uses them -- without
    // them declared, it logs "Setting initial properties failed".
    property var wallpaperConfiguration
    property var configDialog

    // Wallpaper selection no longer happens on this page at all -- it
    // lives in the WP Linux app's own "Wallpapers" tab now (see that app's
    // library/monitors_config modules), which is the only way assignments
    // stay valid: it's the app that scans the library and knows what
    // actually exists, rather than this page accepting any typed/browsed
    // path with no validation. This page is just a launcher for it.
    //
    // Shelling out via the "executable" data engine is the standard
    // Plasma-applet pattern for launching an external command from QML.
    // Launching through the installed .desktop file (dev.wplinux.editor.desktop)
    // rather than a bare binary name is deliberate: install.sh puts the
    // binary itself in ~/.local/bin, which is NOT guaranteed to be on
    // plasmashell's PATH (see its own comment about the user needing to
    // add it to their shell's PATH).
    //
    // `kioclient exec` wants the .desktop file's real filesystem path here,
    // not a KIO `applications:` URL -- that scheme is a virtual tree of
    // menu *categories* (`applications:/Graphics/`, etc.), and treats a
    // bare `applications:<id>.desktop` as an unresolvable folder name
    // ("Unknown application folder"), not a lookup by desktop-file id. The
    // desktop file lands in one of two places depending on how WP Linux
    // was installed -- ~/.local/share/applications (install.sh) or
    // /usr/share/applications (PKGBUILD) -- so try the user path first and
    // fall back to the system one.
    Plasma5Support.DataSource {
        id: launcher
        engine: "executable"
        connectedSources: []
        onNewData: (sourceName) => disconnectSource(sourceName)
        function run(cmd) {
            connectSource(cmd);
        }
    }

    Kirigami.FormLayout {
        Controls.Label {
            Kirigami.FormData.label: "Wallpaper:"
            text: "Configured via the WP Linux app"
        }
        Controls.Button {
            text: "Open WP Linux…"
            onClicked: launcher.run(
                "f=\"$HOME/.local/share/applications/dev.wplinux.editor.desktop\"; " +
                "[ -f \"$f\" ] || f=\"/usr/share/applications/dev.wplinux.editor.desktop\"; " +
                "kioclient exec \"$f\""
            )
        }
    }
}
