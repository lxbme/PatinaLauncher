//! Asset source: gpui-kit's default component icons plus the extra Lucide
//! icons Patina draws (same composition as crypto_pro's `assets.rs`).

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        Download, Boxes, Puzzle, Image, SunMedium, Earth, Trash, Package
    ]
);

/// The default `Assets` with `ExtraIcons` on top.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match ExtraIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut all = gpui_kit::assets::Assets.list(path)?;
        all.extend(ExtraIcons.list(path)?);
        all.sort();
        all.dedup();
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::assets::IconName;

    #[test]
    fn extra_and_default_icons_load() {
        for icon in [
            IconName::Download,
            IconName::Boxes,
            IconName::Puzzle,
            IconName::Image,
            IconName::SunMedium,
            IconName::Earth,
            IconName::Trash,
            IconName::Package,
            IconName::Close,
        ] {
            let path = icon.path();
            assert!(AppAssets.load(&path).unwrap().is_some(), "{path}");
        }
        assert!(!matches!(
            AppAssets.load("icons/no-such-icon.svg"),
            Ok(Some(_))
        ));
    }
}
