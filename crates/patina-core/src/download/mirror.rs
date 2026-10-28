/// A download source that rewrites official URLs by prefix.
#[derive(Clone, Debug, PartialEq)]
pub struct Mirror {
    pub id: String,
    pub rules: Vec<(String, String)>,
}

impl Mirror {
    pub fn official() -> Mirror {
        Mirror {
            id: "official".into(),
            rules: Vec::new(),
        }
    }

    pub fn bmclapi() -> Mirror {
        const BASE: &str = "https://bmclapi2.bangbang93.com";
        let rules = [
            ("https://piston-meta.mojang.com", BASE.to_string()),
            ("https://launchermeta.mojang.com", BASE.to_string()),
            ("https://piston-data.mojang.com", BASE.to_string()),
            ("https://launcher.mojang.com", BASE.to_string()),
            ("https://libraries.minecraft.net", format!("{BASE}/maven")),
            (
                "https://resources.download.minecraft.net",
                format!("{BASE}/assets"),
            ),
        ];
        Mirror {
            id: "bmclapi".into(),
            rules: rules.into_iter().map(|(a, b)| (a.to_string(), b)).collect(),
        }
    }

    pub fn by_id(id: &str) -> Option<Mirror> {
        match id {
            "official" => Some(Mirror::official()),
            "bmclapi" => Some(Mirror::bmclapi()),
            _ => None,
        }
    }

    /// First matching prefix wins; unmatched URLs are returned unchanged.
    pub fn rewrite(&self, url: &str) -> String {
        for (from, to) in &self.rules {
            if let Some(rest) = url.strip_prefix(from.as_str()) {
                return format!("{to}{rest}");
            }
        }
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bmclapi_rewrites() {
        let m = Mirror::bmclapi();
        assert_eq!(
            m.rewrite("https://piston-meta.mojang.com/mc/game/version_manifest_v2.json"),
            "https://bmclapi2.bangbang93.com/mc/game/version_manifest_v2.json"
        );
        assert_eq!(
            m.rewrite("https://libraries.minecraft.net/org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3.jar"),
            "https://bmclapi2.bangbang93.com/maven/org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3.jar"
        );
        assert_eq!(
            m.rewrite("https://resources.download.minecraft.net/bd/bdf4"),
            "https://bmclapi2.bangbang93.com/assets/bd/bdf4"
        );
        assert_eq!(m.rewrite("https://example.com/x"), "https://example.com/x");
        assert_eq!(
            Mirror::official().rewrite("https://piston-meta.mojang.com/a"),
            "https://piston-meta.mojang.com/a"
        );
    }

    #[test]
    fn by_id_lookup() {
        assert_eq!(Mirror::by_id("bmclapi"), Some(Mirror::bmclapi()));
        assert_eq!(Mirror::by_id("nope"), None);
    }
}
