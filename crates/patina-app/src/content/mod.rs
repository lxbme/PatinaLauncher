//! The building blocks of the four local-content tabs (Mods, resource
//! packs, shaders, saves) of an instance's detail:
//!
//! - [`ContentTab`]: one model per tab. It lists a directory of the
//!   instance on the blocking pool, keeps the old items while it reloads and
//!   drops results that a newer reload has superseded. Reload when the tab is
//!   shown ([`ContentTab::shown`]), when the window becomes active
//!   ([`ContentTab::reload_on_activation`]) and after this app changed the
//!   directory ([`ContentTab::mutated`]). There is no file watcher.
//! - [`ContentToolbar`] (filter, open folder, refresh) and [`EmptyState`].
//! - [`ImageCache`]: icon bytes as ready-to-draw images, 512 entries, LRU.
//! - [`with_instance_lock`]: how every mutation of an instance's files runs.
//!
//! The tabs built on them: [`ModsTab`].

mod image_cache;
mod list;
mod mods_tab;
mod refresh;

pub use image_cache::{
    ImageCache, ImageKey, MAX_ICON_BYTES, icon_tile, read_icon_file, request_image,
};
pub use list::{
    ContentLoader, ContentTab, ContentToolbar, EmptyKind, EmptyState, LoadState, Named, Reloaded,
    filter_input, matches_filter,
};
pub use mods_tab::{
    HIGHLIGHT_FOR, ModInfo, ModItem, ModsChanged, ModsEvents, ModsTab, mod_delete_id,
    mod_disabled_id, mod_row_id, mod_switch_id,
};
pub use refresh::{lock_blocking, open_folder, open_folder_with, with_instance_lock};

/// Registers the app-wide state of the content tabs (the image cache, the
/// mods-changed events) and the Mods list's keys.
pub fn init(cx: &mut gpui_kit::App) {
    cx.set_global(ImageCache::new());
    ModsEvents::init(cx);
    mods_tab::init(cx);
}
