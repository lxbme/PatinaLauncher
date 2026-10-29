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
            ("https://meta.fabricmc.net", format!("{BASE}/fabric-meta")),
            ("https://maven.fabricmc.net", format!("{BASE}/maven")),
            ("https://maven.minecraftforge.net", format!("{BASE}/maven")),
            (
                "https://files.minecraftforge.net/maven",
                format!("{BASE}/maven"),
            ),
            (
                "http://files.minecraftforge.net/maven",
                format!("{BASE}/maven"),
            ),
            (
                "https://maven.neoforged.net/releases",
                format!("{BASE}/maven"),
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

    /// First matching prefix wins; unmatched URLs are returned unchanged. A
    /// prefix only matches at a host boundary: it must be followed by `/`,
    /// `?`, `#` or the end of the URL.
    pub fn rewrite(&self, url: &str) -> String {
        for (from, to) in &self.rules {
            if let Some(rest) = url.strip_prefix(from.as_str())
                && (rest.is_empty() || rest.starts_with(['/', '?', '#']))
            {
                return format!("{to}{rest}");
            }
        }
        url.to_string()
    }
}

/// Rewrites known legacy URLs to their canonical form. Every download job URL
/// goes through this before it is created.
pub fn canonical_url(url: &str) -> String {
    for old in [
        "http://files.minecraftforge.net/maven/",
        "https://files.minecraftforge.net/maven/",
    ] {
        if let Some(rest) = url.strip_prefix(old) {
            return format!("https://maven.minecraftforge.net/{rest}");
        }
    }
    url.to_string()
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
    fn host_boundary_is_enforced() {
        let m = Mirror::bmclapi();
        assert_eq!(
            m.rewrite("https://piston-meta.mojang.com.evil/x"),
            "https://piston-meta.mojang.com.evil/x"
        );
        assert_eq!(
            m.rewrite("https://piston-meta.mojang.com"),
            "https://bmclapi2.bangbang93.com"
        );
        assert_eq!(
            m.rewrite("https://piston-meta.mojang.com?q"),
            "https://bmclapi2.bangbang93.com?q"
        );
    }

    #[test]
    fn loader_hosts_rewrite() {
        let m = Mirror::bmclapi();
        assert_eq!(
            m.rewrite("https://meta.fabricmc.net/v2/versions/loader/1.21.1"),
            "https://bmclapi2.bangbang93.com/fabric-meta/v2/versions/loader/1.21.1"
        );
        assert_eq!(
            m.rewrite(
                "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml"
            ),
            "https://bmclapi2.bangbang93.com/maven/net/neoforged/neoforge/maven-metadata.xml"
        );
        assert_eq!(
            m.rewrite("https://maven.minecraftforge.net/net/minecraftforge/forge/x.jar"),
            "https://bmclapi2.bangbang93.com/maven/net/minecraftforge/forge/x.jar"
        );
    }

    #[test]
    fn canonicalizes_old_forge_maven() {
        assert_eq!(
            canonical_url("http://files.minecraftforge.net/maven/a/b.jar"),
            "https://maven.minecraftforge.net/a/b.jar"
        );
        assert_eq!(
            canonical_url("https://files.minecraftforge.net/maven/a/b.jar"),
            "https://maven.minecraftforge.net/a/b.jar"
        );
        assert_eq!(
            canonical_url("https://example.com/x"),
            "https://example.com/x"
        );
    }

    #[test]
    fn by_id_lookup() {
        assert_eq!(Mirror::by_id("bmclapi"), Some(Mirror::bmclapi()));
        assert_eq!(Mirror::by_id("nope"), None);
    }
}
