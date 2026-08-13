//! Launch-render-server-at-login toggle, backed by the XDG Desktop
//! Application Autostart Specification (`$XDG_CONFIG_HOME/autostart/*.desktop`)
//! instead of a systemd --user unit -- every major DE session (GNOME, KDE,
//! Cinnamon, XFCE, MATE, ...) launches whatever `.desktop` files it finds
//! there at login, regardless of which init system (or none at all) the
//! distro uses underneath. State lives entirely in whether the file
//! exists: no separate on/off flag to keep in sync with it.

use std::io;
use std::path::PathBuf;

const DESKTOP_FILE_NAME: &str = "dev.wplinux.render-server.desktop";

fn desktop_file_path() -> PathBuf {
    dirs::config_dir()
        .expect("no config dir (HOME unset?)")
        .join("autostart")
        .join(DESKTOP_FILE_NAME)
}

/// `render-server` is assumed to sit next to this process's own
/// executable -- true of every shipped layout (install.sh's
/// `~/.local/bin`, the AUR package's `/usr/bin`), since both binaries are
/// always installed side by side. Resolved fresh on every enable rather
/// than cached, so reinstalling to a new location and re-ticking the box
/// picks up the new path.
fn render_server_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let candidate = exe
        .parent()
        .ok_or_else(|| {
            io::Error::other("wp_linux_editor's own executable path has no parent directory")
        })?
        .join("render-server");
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no render-server binary next to wp_linux_editor at {candidate:?}"),
        ))
    }
}

pub fn is_enabled() -> bool {
    desktop_file_path().is_file()
}

pub fn set_enabled(enabled: bool) -> io::Result<()> {
    if enabled { enable() } else { disable() }
}

fn enable() -> io::Result<()> {
    let render_server = render_server_path()?;
    let path = desktop_file_path();
    std::fs::create_dir_all(path.parent().expect("just joined a file name onto a dir"))?;
    // NoDisplay: this is a background service, not something a user
    // should ever see in an app launcher/menu. X-GNOME-Autostart-enabled
    // is redundant with the file simply existing, but some GNOME-derived
    // autostart implementations still check it out of habit -- costs
    // nothing to set.
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=WP Linux Render Server\n\
         Comment=Renders the WP Linux animated wallpaper in the background\n\
         Exec={}\n\
         Terminal=false\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
        render_server.display()
    );
    std::fs::write(path, contents)
}

fn disable() -> io::Result<()> {
    match std::fs::remove_file(desktop_file_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
