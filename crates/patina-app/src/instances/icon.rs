//! Instance icons: the built-in set (24 CC0 pixel icons, see
//! `assets/instance-icons/LICENSE.md`), a custom uploaded image, and the
//! letter tile that stands in when neither applies.
//!
//! The built-in PNGs are compiled in and served to `img("instance-icons/..")`
//! through [`AppAssets`](crate::assets::AppAssets) ([`asset_bytes`]). gpui
//! samples images with a fixed linear filter, so nearest-neighbour scaling
//! is not available. Instead the icons are drawn at integer divisors of the
//! 64 px source: 32 px in the list and the picker, 64 px in the detail
//! header, at UI scale 1.0 (other scales resample and may look softer).

use std::path::{Path, PathBuf};

use gpui_kit::{
    AnyElement, App, Entity, ImageSource, IntoElement, ObjectFit, ParentElement, Rems,
    SharedString, Styled, StyledImage, Task, img, rems,
};
use patina_core::instance::{Instance, InstanceIcon};

use super::model::InstancesModel;
use super::tile;
use crate::core_bridge::{Core, run_blocking};
use crate::theme::ProductColors;

macro_rules! builtin_icons {
    ($($name:literal),+ $(,)?) => {
        /// Names of the built-in icons; each is `assets/instance-icons/<name>.png`.
        pub const BUILTIN_ICONS: &[&str] = &[$($name),+];

        /// The PNG of built-in icon `name`.
        fn builtin_png(name: &str) -> Option<&'static [u8]> {
            match name {
                $($name => Some(
                    include_bytes!(concat!("../../assets/instance-icons/", $name, ".png"))
                        .as_slice(),
                ),)+
                _ => None,
            }
        }
    };
}

builtin_icons!(
    "grass-block",
    "dirt",
    "stone",
    "sand",
    "log",
    "leaves",
    "bricks",
    "diamond-ore",
    "water",
    "lava",
    "ice",
    "snow",
    "pickaxe",
    "sword",
    "axe",
    "shovel",
    "bow",
    "apple",
    "diamond",
    "gold",
    "cooked-fish",
    "hoe",
    "minecart",
    "stew",
);

const ASSET_DIR: &str = "instance-icons/";
const ASSET_EXT: &str = ".png";

/// The bytes behind asset path `path` (`instance-icons/<name>.png`), for the
/// app's asset source.
pub fn asset_bytes(path: &str) -> Option<&'static [u8]> {
    let name = path.strip_prefix(ASSET_DIR)?.strip_suffix(ASSET_EXT)?;
    builtin_png(name)
}

/// Where an instance's icon comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconSource {
    Builtin(&'static str),
    Custom(PathBuf),
    /// The first letter of the name on a tile of tint `tint` (0..8).
    Letter {
        ch: char,
        tint: usize,
    },
}

/// What a picker or a save asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconChoice {
    Builtin(String),
    Custom(PathBuf),
    /// Back to the letter tile.
    None,
}

/// The tint of the letter tile of instance `id`: a stable hash modulo 8.
pub fn tint_for(id: &str) -> usize {
    // u64 so the tint does not depend on the platform's pointer width.
    let hash = id
        .bytes()
        .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b.into()));
    (hash % 8) as usize
}

fn letter_for(inst: &Instance) -> IconSource {
    let ch = inst
        .config
        .name
        .chars()
        .next()
        .and_then(|c| c.to_uppercase().next())
        .unwrap_or('?');
    IconSource::Letter {
        ch,
        tint: tint_for(&inst.id),
    }
}

/// The icon to draw for `inst`. An unknown built-in name, and a custom icon
/// whose file is gone, fall back to the letter tile.
pub fn icon_for(inst: &Instance) -> IconSource {
    match &inst.config.icon {
        Some(InstanceIcon::Builtin(name)) => BUILTIN_ICONS
            .iter()
            .find(|known| **known == name)
            .map_or_else(|| letter_for(inst), |known| IconSource::Builtin(known)),
        Some(InstanceIcon::Custom) => inst
            .icon_path()
            .map_or_else(|| letter_for(inst), IconSource::Custom),
        None => letter_for(inst),
    }
}

/// `src` as a square of side `size`.
pub fn instance_icon(src: &IconSource, size: Rems, cx: &App) -> AnyElement {
    let radius = rems(size.0 * 0.22);
    match src {
        IconSource::Builtin(name) => img(format!("{ASSET_DIR}{name}{ASSET_EXT}"))
            .flex_none()
            .size(size)
            .rounded(radius)
            .object_fit(ObjectFit::Cover)
            .into_any_element(),
        IconSource::Custom(path) => img(path.as_path())
            .flex_none()
            .size(size)
            .rounded(radius)
            .object_fit(ObjectFit::Cover)
            .into_any_element(),
        IconSource::Letter { ch, tint } => {
            let color = ProductColors::get(cx).tint(*tint);
            tile(color, size, cx)
                .child(SharedString::from(ch.to_string()))
                .into_any_element()
        }
    }
}

/// Makes the image at `path` load again from disk. gpui caches decoded
/// images by path, and a re-uploaded custom icon keeps its path.
pub fn forget_cached_image(path: &Path, cx: &mut App) {
    ImageSource::from(path).remove_asset(cx);
}

/// Saves `choice` as the icon of instance `id` in the background, then
/// refreshes `model`. Resolves to the localized error when saving fails (an
/// unreadable or unsupported image); the instance keeps its old icon then.
pub fn save_icon(
    model: &Entity<InstancesModel>,
    id: String,
    choice: IconChoice,
    cx: &mut App,
) -> Task<Result<(), SharedString>> {
    let store = Core::global(cx).store.clone();
    let job = run_blocking(cx, move || {
        // The custom image the instance has now, to evict from gpui's cache
        // (it is replaced or deleted by the save).
        let old = store.load(&id).ok().and_then(|inst| inst.icon_path());
        let saved = match choice {
            IconChoice::Builtin(name) => store.set_builtin_icon(&id, &name),
            IconChoice::Custom(path) => store.set_custom_icon(&id, &path),
            IconChoice::None => store.clear_icon(&id),
        };
        saved.map(|inst| (old, inst))
    });
    let model = model.clone();
    cx.spawn(async move |cx| {
        let (old, saved) = match job.await {
            Ok(Ok(done)) => done,
            Ok(Err(error)) => {
                tracing::warn!(%error, "saving the instance icon failed");
                return Err(error.to_string().into());
            }
            Err(error) => return Err(error.to_string().into()),
        };
        cx.update(|cx| {
            for path in old.into_iter().chain(saved.icon_path()) {
                forget_cached_image(&path, cx);
            }
            model.update(cx, |model, cx| model.refresh(cx));
        });
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use patina_core::instance::InstanceConfig;

    use super::*;

    fn instance(id: &str, name: &str, icon: Option<InstanceIcon>) -> Instance {
        Instance {
            id: id.to_string(),
            dir: std::env::temp_dir()
                .join("patina-icon-test-no-such-dir")
                .join(id),
            config: InstanceConfig {
                name: name.to_string(),
                icon,
                ..Default::default()
            },
        }
    }

    fn assets_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/instance-icons")
    }

    #[test]
    fn builtin_names_match_files() {
        assert_eq!(BUILTIN_ICONS.len(), 24);
        let mut files: Vec<String> = std::fs::read_dir(assets_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter_map(|n| n.strip_suffix(".png").map(str::to_owned))
            .collect();
        files.sort();
        let mut names: Vec<String> = BUILTIN_ICONS.iter().map(|n| (*n).to_owned()).collect();
        names.sort();
        assert_eq!(files, names);
        for name in BUILTIN_ICONS {
            assert!(
                name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'),
                "{name}"
            );
            assert!(asset_bytes(&format!("instance-icons/{name}.png")).is_some());
        }
        assert!(asset_bytes("instance-icons/nope.png").is_none());
        assert!(asset_bytes("icons/stone.png").is_none());
    }

    #[test]
    fn every_builtin_is_64px_png() {
        for name in BUILTIN_ICONS {
            let bytes = std::fs::read(assets_dir().join(format!("{name}.png"))).unwrap();
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{name}: signature");
            // IHDR: length, "IHDR", width, height.
            assert_eq!(&bytes[12..16], b"IHDR", "{name}");
            let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
            let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
            assert_eq!((width, height), (64, 64), "{name}");
            assert_eq!(
                builtin_png(name),
                Some(bytes.as_slice()),
                "{name}: embedded copy"
            );
        }
    }

    #[test]
    fn license_file_names_source_and_cc0() {
        let text = std::fs::read_to_string(assets_dir().join("LICENSE.md")).unwrap();
        assert!(text.contains("kenney.nl"));
        assert!(text.contains("CC0"));
        assert!(text.contains("opengameart.org/content/voxel-pack"));
    }

    #[test]
    fn known_builtin_resolves() {
        let inst = instance("a", "Alpha", Some(InstanceIcon::Builtin("sword".into())));
        assert_eq!(icon_for(&inst), IconSource::Builtin("sword"));
    }

    #[test]
    fn unknown_builtin_falls_back_to_letter() {
        let inst = instance("a", "alpha", Some(InstanceIcon::Builtin("no-such".into())));
        assert_eq!(
            icon_for(&inst),
            IconSource::Letter {
                ch: 'A',
                tint: tint_for("a")
            }
        );
    }

    #[test]
    fn missing_custom_file_falls_back_to_letter() {
        let inst = instance("b", "Beta", Some(InstanceIcon::Custom));
        assert!(matches!(
            icon_for(&inst),
            IconSource::Letter { ch: 'B', .. }
        ));
        assert!(matches!(
            icon_for(&instance("c", "", None)),
            IconSource::Letter { ch: '?', .. }
        ));
    }

    #[test]
    fn existing_custom_file_is_used() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("icon.png"), b"x").unwrap();
        let mut inst = instance("d", "Delta", Some(InstanceIcon::Custom));
        inst.dir = dir.path().to_path_buf();
        assert_eq!(
            icon_for(&inst),
            IconSource::Custom(dir.path().join("icon.png"))
        );
    }

    #[test]
    fn tint_is_stable_per_id() {
        for id in ["Alpha", "my pack", "", "整合包"] {
            assert_eq!(tint_for(id), tint_for(id));
            assert!(tint_for(id) < 8);
        }
        // Pinned: changing the hash recolours every tile.
        assert_eq!(tint_for("Alpha"), 6);
        let spread: std::collections::HashSet<_> =
            (0..32).map(|i| tint_for(&format!("inst-{i}"))).collect();
        assert!(spread.len() > 1);
    }
}
