//! What the resource packs tab lists: the zips and folders of
//! `resourcepacks` with `pack.mcmeta`'s description and whether there is a
//! `pack.png`. The tab itself is [`LocalTab`](super::LocalTab).

use std::io;
use std::path::PathBuf;

use patina_core::content::packs::list_resource_packs;
use patina_core::instance::Instance;

use super::local_tab::{LocalItem, icon_stamp};

/// The instance's resource packs folder.
pub fn dir(instance: &Instance) -> PathBuf {
    instance.game_dir().join("resourcepacks")
}

/// Lists the packs (blocking). Icons are not read.
pub(super) fn load(instance: &Instance) -> io::Result<Vec<LocalItem>> {
    Ok(list_resource_packs(&dir(instance))?
        .into_iter()
        .map(|pack| {
            // A folder's icon is its pack.png; a zip's changes with the zip.
            let (icon_size, icon_modified) = match (pack.has_icon, pack.is_dir) {
                (false, _) => (0, None),
                (true, true) => icon_stamp(&pack.path.join("pack.png")),
                (true, false) => icon_stamp(&pack.path),
            };
            LocalItem {
                key: pack.name.clone(),
                title: if pack.is_dir {
                    pack.name.clone()
                } else {
                    std::path::Path::new(&pack.name).file_stem().map_or_else(
                        || pack.name.clone(),
                        |name| name.to_string_lossy().into_owned(),
                    )
                },
                path: pack.path,
                // One line: the row cuts it with an ellipsis.
                description: pack
                    .description
                    .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" ")),
                last_played: None,
                is_dir: pack.is_dir,
                has_icon: pack.has_icon,
                icon_size,
                icon_modified,
            }
        })
        .collect())
}
