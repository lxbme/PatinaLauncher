//! Original local grayscale heads; UUID parity is identity, never gender.
use patina_core::accounts::{Account, AccountKind};

pub(crate) const ASSETS: [&str; 3] = [
    "account-heads/steve.png",
    "account-heads/alex.png",
    "account-heads/LICENSE.md",
];

pub(crate) fn asset_bytes(path: &str) -> Option<&'static [u8]> {
    match path {
        "account-heads/steve.png" => Some(include_bytes!("../../assets/account-heads/steve.png")),
        "account-heads/alex.png" => Some(include_bytes!("../../assets/account-heads/alex.png")),
        "account-heads/LICENSE.md" => Some(include_bytes!("../../assets/account-heads/LICENSE.md")),
        _ => None,
    }
}

pub(crate) fn asset_for(account: &Account) -> Option<&'static str> {
    if account.kind != AccountKind::Offline {
        return None;
    }
    // Defined byte-wise rather than DefaultHasher (whose algorithm is not a
    // persistence contract). No preference or network lookup is involved.
    let parity = account
        .uuid
        .as_bytes()
        .iter()
        .fold(0, |parity, byte| parity ^ byte)
        & 1;
    Some(if parity == 0 { ASSETS[0] } else { ASSETS[1] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_selects_the_same_head_after_reload_for_both_offline_origins() {
        for origin in ["entitled", "development"] {
            for (uuid, expected) in [
                (
                    "00000000-0000-0000-0000-000000000001",
                    "account-heads/alex.png",
                ),
                (
                    "00000000-0000-0000-0000-000000000002",
                    "account-heads/steve.png",
                ),
            ] {
                let mut account: Account = toml::from_str(&format!("id = 'offline'\nkind = 'offline'\nusername = 'Player'\nuuid = '{uuid}'\noffline_origin = '{origin}'")).unwrap();
                assert_eq!(asset_for(&account), Some(expected));
                account.username = "DifferentName".into();
                let reloaded = toml::from_str(&toml::to_string(&account).unwrap()).unwrap();
                assert_eq!(asset_for(&reloaded), Some(expected));
                account.kind = AccountKind::Msa;
                assert_eq!(
                    asset_for(&account),
                    None,
                    "MSA retains its cached head / letter path"
                );
            }
        }
    }

    #[test]
    fn bundled_heads_are_distinct_normalized_grayscale_rasters_with_credits() {
        let heads: Vec<_> = ASSETS[..2]
            .iter()
            .map(|path| {
                let image = image::load_from_memory(asset_bytes(path).unwrap())
                    .unwrap()
                    .into_rgba8();
                assert_eq!(image.dimensions(), (64, 64));
                assert!(
                    image
                        .pixels()
                        .all(|p| p[0] == p[1] && p[1] == p[2] && p[3] == 255)
                );
                image
            })
            .collect();
        assert_ne!(heads[0], heads[1]);
        assert!(asset_bytes(ASSETS[2]).is_some());
    }
}
