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
    // lives in the WP Linux app's own "Обои" tab now (see that app's
    // library/monitors_config modules), which is the only way assignments
    // stay valid: it's the app that scans the library and knows what
    // actually exists, rather than this page accepting any typed/browsed
    // path with no validation. This page is just a launcher for it.
    //
    // Shelling out via the "executable" data engine is the standard
    // Plasma-applet pattern for launching an external command from QML --
    // ⚠ unverified in this environment (no KDE/Wayland session to test
    // against): confirm the import above resolves on the target Plasma/KF6
    // version, and that `kioclient` (going through the installed .desktop
    // file, dev.wplinux.editor.desktop, rather than a bare binary name) is
    // actually on PATH for the plasmashell process. install.sh installs
    // the binary itself to ~/.local/bin, which is NOT guaranteed to be on
    // plasmashell's PATH -- see its own comment about the user needing to
    // add it to their shell's PATH -- so launching through the desktop
    // file is the more robust choice here, not a stylistic preference.
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
            onClicked: launcher.run("kioclient exec applications:dev.wplinux.editor.desktop")
        }
    }
}
