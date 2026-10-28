//! Mojang version manifest (`version_manifest_v2.json`).

use serde::{Deserialize, Serialize};

pub const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionKind {
    Release,
    Snapshot,
    OldBeta,
    OldAlpha,
}

impl VersionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            VersionKind::Release => "release",
            VersionKind::Snapshot => "snapshot",
            VersionKind::OldBeta => "old_beta",
            VersionKind::OldAlpha => "old_alpha",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Latest {
    pub release: String,
    pub snapshot: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: VersionKind,
    pub url: String,
    pub sha1: String,
    pub release_time: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionManifest {
    pub latest: Latest,
    pub versions: Vec<ManifestEntry>,
}

impl VersionManifest {
    pub fn find(&self, id: &str) -> Option<&ManifestEntry> {
        self.versions.iter().find(|v| v.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest_and_finds() {
        let json = r#"{"latest":{"release":"1.0","snapshot":"s"},"versions":[
          {"id":"1.0","type":"release","url":"u","time":"t","releaseTime":"r","sha1":"h","complianceLevel":1},
          {"id":"b1.0","type":"old_beta","url":"u2","releaseTime":"r2","sha1":"h2"}]}"#;
        let m: VersionManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.find("b1.0").unwrap().kind, VersionKind::OldBeta);
        assert_eq!(m.find("1.0").unwrap().kind.as_str(), "release");
        assert!(m.find("nope").is_none());
    }
}
