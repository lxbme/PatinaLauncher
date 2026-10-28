//! Per-version JSON model.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::manifest::VersionKind;
use super::rules::Rule;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub url: String,
    pub sha1: String,
    pub size: u64,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgValue {
    Plain(String),
    Conditional { rules: Vec<Rule>, value: OneOrMany },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arguments {
    #[serde(default)]
    pub game: Vec<ArgValue>,
    #[serde(default)]
    pub jvm: Vec<ArgValue>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryDownloads {
    pub artifact: Option<FileRef>,
    #[serde(default)]
    pub classifiers: HashMap<String, FileRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extract {
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub name: String,
    #[serde(default)]
    pub downloads: Option<LibraryDownloads>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub natives: Option<HashMap<String, String>>,
    #[serde(default)]
    pub extract: Option<Extract>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetIndexRef {
    pub id: String,
    pub url: String,
    pub sha1: String,
    pub size: u64,
    #[serde(default)]
    pub total_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionDownloads {
    pub client: FileRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JavaVersionReq {
    pub component: String,
    pub major_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggingFile {
    pub id: String,
    pub url: String,
    pub sha1: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggingClient {
    pub argument: String,
    pub file: LoggingFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Logging {
    #[serde(default)]
    pub client: Option<LoggingClient>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionJson {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: VersionKind,
    pub main_class: String,
    #[serde(default)]
    pub arguments: Option<Arguments>,
    #[serde(default)]
    pub minecraft_arguments: Option<String>,
    #[serde(default)]
    pub libraries: Vec<Library>,
    pub asset_index: AssetIndexRef,
    pub assets: String,
    pub downloads: VersionDownloads,
    #[serde(default)]
    pub java_version: Option<JavaVersionReq>,
    #[serde(default)]
    pub logging: Option<Logging>,
}

impl VersionJson {
    /// Versions predating the `javaVersion` field run on Java 8.
    pub fn java_requirement(&self) -> JavaVersionReq {
        self.java_version.clone().unwrap_or(JavaVersionReq {
            component: "jre-legacy".to_string(),
            major_version: 8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::rules::{RuleEnv, rules_allow};
    use std::collections::HashSet;

    fn modern() -> VersionJson {
        serde_json::from_str(include_str!("../../tests/fixtures/version_modern.json")).unwrap()
    }
    fn legacy() -> VersionJson {
        serde_json::from_str(include_str!("../../tests/fixtures/version_legacy.json")).unwrap()
    }

    #[test]
    fn parses_fixtures() {
        let m = modern();
        assert_eq!(m.java_requirement().major_version, 25);
        assert_eq!(m.java_requirement().component, "java-runtime-epsilon");
        let l = legacy();
        let j = l.java_requirement();
        assert_eq!((j.component.as_str(), j.major_version), ("jre-legacy", 8));
        assert!(l.arguments.is_none() && l.minecraft_arguments.is_some());
    }

    #[test]
    fn modern_details() {
        let m = modern();
        assert_eq!(m.libraries.len(), 5);
        let args = m.arguments.as_ref().unwrap();
        assert!(args.game.iter().any(
            |a| matches!(a, ArgValue::Conditional { value: OneOrMany::One(v), .. } if v == "--demo")
        ));
        assert!(args.jvm.iter().any(|a| matches!(a, ArgValue::Conditional { value: OneOrMany::Many(v), .. } if v == &["-XstartOnFirstThread"])));
        let log = m.logging.unwrap().client.unwrap();
        assert_eq!(log.file.id, "client-1.12.xml");
        assert_eq!(log.argument, "-Dlog4j.configurationFile=${path}");
        let env = RuleEnv {
            os_name: "linux".into(),
            arch: "x86_64".into(),
            os_version: "6.0".into(),
            features: HashSet::new(),
        };
        let allowed: Vec<_> = m
            .libraries
            .iter()
            .filter(|l| rules_allow(&l.rules, &env))
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(
            allowed,
            [
                "com.mojang:brigadier:1.0.18",
                "org.lwjgl:lwjgl:3.3.3:natives-linux"
            ]
        );
    }

    #[test]
    fn legacy_details() {
        let l = legacy();
        assert_eq!(l.asset_index.id, "1.8");
        let lw = &l.libraries[0];
        assert_eq!(
            lw.natives.as_ref().unwrap()["windows"],
            "natives-windows-${arch}"
        );
        assert_eq!(lw.extract.as_ref().unwrap().exclude, ["META-INF/"]);
        let cls = &lw.downloads.as_ref().unwrap().classifiers;
        assert!(
            cls.contains_key("natives-linux")
                && cls.contains_key("natives-osx")
                && cls.contains_key("natives-windows-64")
        );
        assert!(l.libraries[1].downloads.is_none());
    }
}
