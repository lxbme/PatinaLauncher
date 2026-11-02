//! Embedded Patina branding, account/instance images, and component icons.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        Download,
        Boxes,
        Puzzle,
        Image,
        SunMedium,
        Earth,
        Trash,
        Package,
        Cpu,
        MemoryStick,
        SlidersHorizontal
    ]
);

/// The full-color artwork is an image, not a monochrome component icon.
pub(crate) const BRAND_LOGO: &str = "branding/patina-logo-2048.png";

/// The app's embedded images plus the default and extra component icons.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == BRAND_LOGO {
            return Ok(Some(Cow::Borrowed(include_bytes!(
                "../../../branding/patina-logo-2048.png"
            ))));
        }
        if let Some(bytes) = crate::accounts::offline_heads::asset_bytes(path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        if let Some(bytes) = crate::instances::asset_bytes(path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        match ExtraIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut all = gpui_kit::assets::Assets.list(path)?;
        all.extend(ExtraIcons.list(path)?);
        if BRAND_LOGO.starts_with(path) {
            all.push(BRAND_LOGO.into());
        }
        all.extend(
            crate::instances::BUILTIN_ICONS
                .iter()
                .map(|name| SharedString::from(format!("instance-icons/{name}.png")))
                .filter(|asset| asset.starts_with(path)),
        );
        all.extend(
            crate::accounts::offline_heads::ASSETS
                .iter()
                .copied()
                .filter(|asset| asset.starts_with(path))
                .map(SharedString::from),
        );
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
    fn brand_logo_is_embedded_and_decodes_in_full_color() {
        let path = "branding/patina-logo-2048.png";
        let bytes = AppAssets.load(path).unwrap().expect("embedded brand logo");
        let logo = image::load_from_memory(&bytes).unwrap().into_rgba8();
        assert_eq!(logo.dimensions(), (2048, 2048));
        assert_eq!(logo.get_pixel(0, 0).0[3], 0, "transparent background");
        assert!(logo.pixels().any(|p| p.0[3] == 255 && p.0[0] > p.0[1]));
        assert!(logo.pixels().any(|p| p.0[3] == 255 && p.0[1] > p.0[0]));
        assert_eq!(
            AppAssets.list("branding/").unwrap(),
            vec![SharedString::from(path)]
        );
        assert!(AppAssets.list("icons/").unwrap().iter().all(|p| p != path));
        assert!(!matches!(
            AppAssets.load("branding/no-such-logo.png"),
            Ok(Some(_))
        ));
    }

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
            IconName::Cpu,
            IconName::MemoryStick,
            IconName::SlidersHorizontal,
            IconName::Close,
        ] {
            let path = icon.path();
            assert!(AppAssets.load(&path).unwrap().is_some(), "{path}");
        }
        for name in crate::instances::BUILTIN_ICONS {
            let path = format!("instance-icons/{name}.png");
            assert!(AppAssets.load(&path).unwrap().is_some(), "{path}");
        }
        for path in crate::accounts::offline_heads::ASSETS {
            assert!(AppAssets.load(path).unwrap().is_some(), "{path}");
            assert!(
                AppAssets
                    .list("account-heads/")
                    .unwrap()
                    .contains(&path.into())
            );
        }
        assert!(!matches!(
            AppAssets.load("icons/no-such-icon.svg"),
            Ok(Some(_))
        ));
    }
}
