//! What the shaders tab lists: the zips and folders of `shaderpacks`
//! (option files such as `<name>.zip.txt` are not shaders). The tab itself
//! is [`LocalTab`](super::LocalTab).

use std::io;
use std::path::PathBuf;

use patina_core::content::packs::scan_shader_packs;
use patina_core::instance::Instance;

use super::local_tab::LocalItem;

/// The instance's shader packs folder.
pub fn dir(instance: &Instance) -> PathBuf {
    instance.game_dir().join("shaderpacks")
}

/// Lists the shader packs (blocking).
pub(super) fn load(instance: &Instance) -> io::Result<Vec<LocalItem>> {
    Ok(scan_shader_packs(&dir(instance))?
        .into_iter()
        .map(|pack| LocalItem {
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
            description: None,
            last_played: None,
            is_dir: pack.is_dir,
            has_icon: false,
            icon_size: 0,
            icon_modified: None,
        })
        .collect())
}
