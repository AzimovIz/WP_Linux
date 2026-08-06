import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import QtQuick.Dialogs
import org.kde.kirigami as Kirigami

ColumnLayout {
    id: root

    property string cfg_ProjectPath
    property string cfg_ProjectPathDefault: ""

    // Plasma's wallpaper config dialog always tries to set these on every
    // wallpaper's config page, regardless of whether it uses them -- without
    // them declared, it logs "Setting initial properties failed".
    property var wallpaperConfiguration
    property var configDialog

    Kirigami.FormLayout {
        RowLayout {
            Kirigami.FormData.label: "Project folder:"

            Controls.TextField {
                id: pathField
                Layout.fillWidth: true
                text: root.cfg_ProjectPath
                onTextEdited: root.cfg_ProjectPath = text
            }

            Controls.Button {
                text: "Browse…"
                onClicked: folderDialog.open()
            }
        }
    }

    FolderDialog {
        id: folderDialog
        title: "Choose wallpaper project folder"
        onAccepted: {
            root.cfg_ProjectPath = decodeURIComponent(
                selectedFolder.toString().replace("file://", "")
            );
        }
    }
}
