//! State for the "Discover" tab: browses the store server (`store-client`)
//! and downloads a wallpaper straight into the local library.
//!
//! Every network call runs on its own `std::thread::spawn` background
//! thread rather than on the UI thread -- this app has no async runtime,
//! and a blocking `ureq` call on the UI thread would freeze the whole
//! window for as long as the request takes (and, with a flaky or
//! unreachable server, `store_client::Client`'s connect/request timeouts
//! are the only thing bounding that). Each thread reports back over an
//! `mpsc` channel and pokes the given `egui::Context` to repaint, since
//! egui otherwise only repaints in response to input.
//!
//! `main.rs`'s `show_discover_tab`/`show_discover_tile` own the actual
//! egui layout; this module only owns state and the background-thread
//! plumbing around it, same split as `library` (data) vs. `show_wallpapers_tab`
//! (layout) already have.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use eframe::egui;

use crate::library;

/// Where a downloaded wallpaper lands in the local library -- prefixed so
/// it can never collide with a `library::new_project_id()` id (a plain hex
/// timestamp) and so re-downloading the same store item always lands in
/// the same place instead of piling up duplicates.
fn store_library_dir(wallpaper_id: &str) -> PathBuf {
    library::project_dir(&format!("store-{wallpaper_id}"))
}

enum Msg {
    ListLoaded {
        /// Matched against `DiscoverState::generation` in `drain` so a
        /// slow response to an earlier search/refresh can't clobber the
        /// result of a newer one that finished first -- `reload` (unlike
        /// the other calls here) can fire again before its previous call
        /// has answered, since both the "Refresh" button and a tag search
        /// call it, back to back, from the same UI.
        generation: u64,
        result: Result<store_client::WallpaperListOut, String>,
    },
    PreviewLoaded {
        id: String,
        bytes: Option<Vec<u8>>,
    },
    DownloadFinished {
        id: String,
        result: Result<(), String>,
    },
}

/// One background download's outcome, handed back to `main.rs` from
/// [`DiscoverState::drain`] so it can update the shared status line and
/// rescan the library -- both of those are `EditorApp`-wide concerns this
/// module has no access to (and shouldn't: `library`'s on-disk scan is
/// already the one place that owns turning "a directory landed under
/// `library_root()`" into a `LibraryEntry`).
pub struct DownloadOutcome {
    pub id: String,
    pub result: Result<(), String>,
}

pub struct DiscoverState {
    client: store_client::Client,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    /// Bumped by every `reload` call; see `Msg::ListLoaded`.
    generation: u64,

    pub items: Vec<store_client::WallpaperOut>,
    pub status: String,
    pub loading: bool,

    /// Raw text of the tag-search box, edited directly by the text field
    /// widget. Only takes effect once [`Self::search`] parses it into
    /// `filter_tags` and reloads -- not on every keystroke.
    pub tags_input: String,
    filter_tags: Vec<String>,

    /// Id of the item whose detail overlay is open, if any.
    pub overlay: Option<String>,

    /// `None` means "requested, but the fetch or decode failed" -- kept
    /// distinct from "never requested" (absent key) so a failed preview
    /// doesn't get re-requested every single frame. Same shape as
    /// `EditorApp::shader_previews`.
    previews: HashMap<String, Option<egui::TextureHandle>>,
    preview_pending: HashSet<String>,
    downloading: HashSet<String>,
    downloaded: HashSet<String>,
}

impl DiscoverState {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            client: store_client::Client::default(),
            tx,
            rx,
            generation: 0,
            items: Vec::new(),
            status: String::new(),
            loading: false,
            tags_input: String::new(),
            filter_tags: Vec::new(),
            overlay: None,
            previews: HashMap::new(),
            preview_pending: HashSet::new(),
            downloading: HashSet::new(),
            downloaded: HashSet::new(),
        }
    }

    /// Parses `tags_input` (comma/whitespace-separated) into `filter_tags`
    /// and reloads with it applied. Called from the Discover tab's Search
    /// button/Enter key -- typing alone doesn't refetch.
    pub fn search(&mut self, ctx: &egui::Context) {
        self.filter_tags = self
            .tags_input
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        self.reload(ctx);
    }

    /// Re-fetches the wallpaper list with the current `filter_tags`. Safe
    /// to call while a previous `reload`/`search` is still in flight --
    /// `generation` makes sure only the latest one's answer is ever
    /// applied, so an in-progress request never wins a race against a
    /// newer one and shows stale results.
    pub fn reload(&mut self, ctx: &egui::Context) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.status.clear();

        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let params = store_client::ListParams {
            tags: self.filter_tags.clone(),
            ..Default::default()
        };
        std::thread::spawn(move || {
            let result = client.list_wallpapers(&params).map_err(|e| e.to_string());
            let _ = tx.send(Msg::ListLoaded { generation, result });
            ctx.request_repaint();
        });
    }

    /// Re-derives which items are already in the local library, by
    /// checking `store_library_dir` on disk for each -- cheap (a `dozens
    /// of entries` `stat()` loop, same order of magnitude as `library::scan`)
    /// and only run when entering the tab or right after a list reload,
    /// not every frame. Needed because a download's success is recorded
    /// directly into `downloaded` as it happens, but nothing here would
    /// otherwise notice if the user later deletes that entry from the
    /// Wallpapers tab.
    pub fn refresh_downloaded_state(&mut self) {
        self.downloaded = self
            .items
            .iter()
            .filter(|item| store_library_dir(&item.id).is_dir())
            .map(|item| item.id.clone())
            .collect();
    }

    pub fn is_downloaded(&self, id: &str) -> bool {
        self.downloaded.contains(id)
    }

    pub fn is_downloading(&self, id: &str) -> bool {
        self.downloading.contains(id)
    }

    /// Egui texture for `id`'s preview, if one has been fetched and
    /// decoded -- `None` both before it's been requested and if it
    /// failed. Call [`Self::ensure_preview_requested`] alongside this to
    /// actually kick off the fetch the first time a tile asks.
    pub fn preview_texture(&self, id: &str) -> Option<egui::TextureId> {
        self.previews.get(id)?.as_ref().map(|t| t.id())
    }

    /// Same texture as [`Self::preview_texture`], plus its native pixel
    /// size -- needed to show it at the right aspect ratio in the detail
    /// overlay (the grid tile doesn't need this: it always stretches the
    /// preview to fill a fixed tile size).
    pub fn preview_image(&self, id: &str) -> Option<(egui::TextureId, egui::Vec2)> {
        let texture = self.previews.get(id)?.as_ref()?;
        Some((texture.id(), texture.size_vec2()))
    }

    /// Starts fetching `id`'s preview image in the background if nothing
    /// has asked for it yet (and nothing is already in flight for it).
    pub fn ensure_preview_requested(&mut self, ctx: &egui::Context, id: &str) {
        if self.previews.contains_key(id) {
            return;
        }
        if !self.preview_pending.insert(id.to_string()) {
            return;
        }

        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let id = id.to_string();
        std::thread::spawn(move || {
            let bytes = client.download_preview(&id).ok();
            let _ = tx.send(Msg::PreviewLoaded { id, bytes });
            ctx.request_repaint();
        });
    }

    /// Downloads `id`'s archive and unpacks it into its library slot. A
    /// no-op if a download for `id` is already running.
    pub fn download(&mut self, ctx: &egui::Context, id: String) {
        if !self.downloading.insert(id.clone()) {
            return;
        }

        let client = self.client.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let dest = store_library_dir(&id);
        std::thread::spawn(move || {
            let result = client
                .download_archive(&id)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    store_client::extract_archive(&bytes, &dest).map_err(|e| e.to_string())
                });
            let _ = tx.send(Msg::DownloadFinished { id, result });
            ctx.request_repaint();
        });
    }

    /// Applies every message that has arrived since the last call, and
    /// returns any downloads that just finished (both success and
    /// failure) for the caller to fold into its own status line and, on
    /// success, rescan the library. Must be called every frame the tab is
    /// shown -- nothing else drains the channel.
    pub fn drain(&mut self, ctx: &egui::Context) -> Vec<DownloadOutcome> {
        let mut outcomes = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::ListLoaded { generation, result } => {
                    if generation != self.generation {
                        // Superseded by a newer reload/search -- drop it.
                        continue;
                    }
                    self.loading = false;
                    match result {
                        Ok(list) => {
                            self.status = format!("{} wallpaper(s)", list.total);
                            self.items = list.items;
                            self.refresh_downloaded_state();
                        }
                        Err(e) => self.status = format!("Failed to load: {e}"),
                    }
                }
                Msg::PreviewLoaded { id, bytes } => {
                    self.preview_pending.remove(&id);
                    let texture = bytes.and_then(|b| decode_preview(ctx, &id, &b));
                    self.previews.insert(id, texture);
                }
                Msg::DownloadFinished { id, result } => {
                    self.downloading.remove(&id);
                    if result.is_ok() {
                        self.downloaded.insert(id.clone());
                    }
                    outcomes.push(DownloadOutcome { id, result });
                }
            }
        }
        outcomes
    }
}

/// Decodes a preview image fetched over the network -- `None` on a
/// decode failure rather than erroring the whole tab out, same fallback
/// philosophy as `load_thumbnail_texture` uses for on-disk previews.
fn decode_preview(ctx: &egui::Context, id: &str, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let rgba = image::load_from_memory(bytes).ok()?.into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    Some(ctx.load_texture(
        format!("discover-preview-{id}"),
        color_image,
        egui::TextureOptions::default(),
    ))
}
