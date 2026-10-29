//! Merging a loader's partial version JSON over the vanilla version JSON.

use std::collections::HashSet;

use super::version::{Arguments, Library, PartialVersionJson, VersionJson};

/// `group:artifact[:classifier]` of a maven coordinate; version and `@ext` are dropped.
pub fn library_key(name: &str) -> Option<String> {
    let no_ext = name.split('@').next().unwrap_or(name);
    let mut parts = no_ext.split(':');
    let group = parts.next().filter(|s| !s.is_empty())?;
    let artifact = parts.next().filter(|s| !s.is_empty())?;
    let _version = parts.next();
    Some(match parts.next().filter(|s| !s.is_empty()) {
        Some(classifier) => format!("{group}:{artifact}:{classifier}"),
        None => format!("{group}:{artifact}"),
    })
}

/// Merge `child` (a loader profile) over `parent` (the vanilla version).
///
/// Argument handling: `arguments` and `minecraft_arguments` are merged
/// independently. Child `arguments` are appended to the parent's `arguments`
/// (if any); the legacy string is replaced only when the child provides one.
/// If the two sides use different styles, both fields are kept as merged;
/// `build_command` already prefers `arguments` when present.
pub fn merge_versions(parent: &VersionJson, child: &PartialVersionJson) -> VersionJson {
    let arguments = match (&parent.arguments, &child.arguments) {
        (Some(p), Some(c)) => Some(Arguments {
            game: p.game.iter().chain(&c.game).cloned().collect(),
            jvm: p.jvm.iter().chain(&c.jvm).cloned().collect(),
        }),
        (Some(a), None) | (None, Some(a)) => Some(a.clone()),
        (None, None) => None,
    };

    // All child libraries untouched, then parent libraries not shadowed by a child key.
    // Parent-internal duplicates are kept: vanilla lists the same group:artifact more
    // than once with different OS rules. Unparseable names are always kept.
    let child_keys: HashSet<String> = child
        .libraries
        .iter()
        .filter_map(|l| library_key(&l.name))
        .collect();
    let libraries: Vec<Library> = child
        .libraries
        .iter()
        .chain(
            parent
                .libraries
                .iter()
                .filter(|l| library_key(&l.name).is_none_or(|k| !child_keys.contains(&k))),
        )
        .cloned()
        .collect();

    VersionJson {
        id: child.id.clone(),
        kind: child.kind.unwrap_or(parent.kind),
        main_class: child
            .main_class
            .clone()
            .unwrap_or_else(|| parent.main_class.clone()),
        arguments,
        minecraft_arguments: child
            .minecraft_arguments
            .clone()
            .or_else(|| parent.minecraft_arguments.clone()),
        libraries,
        asset_index: child
            .asset_index
            .clone()
            .unwrap_or_else(|| parent.asset_index.clone()),
        assets: child
            .assets
            .clone()
            .unwrap_or_else(|| parent.assets.clone()),
        // The client jar is always the vanilla one.
        downloads: parent.downloads.clone(),
        java_version: child
            .java_version
            .clone()
            .or_else(|| parent.java_version.clone()),
        logging: child.logging.clone().or_else(|| parent.logging.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::version::{ArgValue, JavaVersionReq};

    fn modern() -> VersionJson {
        serde_json::from_str(include_str!("../../tests/fixtures/version_modern.json")).unwrap()
    }
    fn legacy() -> VersionJson {
        serde_json::from_str(include_str!("../../tests/fixtures/version_legacy.json")).unwrap()
    }
    fn fabric() -> PartialVersionJson {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/fabric_profile_1.21.1.json"
        ))
        .unwrap()
    }
    fn lib(name: &str) -> Library {
        serde_json::from_value(serde_json::json!({ "name": name })).unwrap()
    }

    #[test]
    fn library_keys() {
        assert_eq!(
            library_key("org.ow2.asm:asm:9.7").as_deref(),
            Some("org.ow2.asm:asm")
        );
        assert_eq!(
            library_key("org.lwjgl:lwjgl:3.3.3:natives-linux").as_deref(),
            Some("org.lwjgl:lwjgl:natives-linux")
        );
        assert_eq!(library_key("a:b:1@zip").as_deref(), Some("a:b"));
        assert_eq!(library_key("a:b:1:c@zip").as_deref(), Some("a:b:c"));
        assert_eq!(library_key("nonsense"), None);
    }

    #[test]
    fn child_libraries_win_and_come_first() {
        let mut parent = modern();
        parent.libraries = vec![lib("org.ow2.asm:asm:9.6"), lib("com.google:guava:33")];
        let mut child = fabric();
        child.libraries = vec![lib("org.ow2.asm:asm:9.7")];
        let m = merge_versions(&parent, &child);
        let names: Vec<_> = m.libraries.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["org.ow2.asm:asm:9.7", "com.google:guava:33"]);
    }

    fn plain(v: &str) -> ArgValue {
        ArgValue::Plain(v.to_string())
    }

    #[test]
    fn arguments_append_and_main_class_overrides() {
        let parent = modern();
        let mut child = fabric();
        child.arguments = Some(Arguments {
            game: vec![plain("--childGame")],
            jvm: vec![plain("-DchildJvm")],
        });
        let m = merge_versions(&parent, &child);
        assert_eq!(m.id, child.id);
        assert_eq!(m.main_class, child.main_class.clone().unwrap());
        let pa = parent.arguments.as_ref().unwrap();
        let ma = m.arguments.as_ref().unwrap();
        assert_eq!(ma.game.len(), pa.game.len() + 1);
        assert_eq!(ma.jvm.len(), pa.jvm.len() + 1);
        assert_eq!(ma.game[..pa.game.len()], pa.game[..]);
        assert_eq!(ma.jvm[..pa.jvm.len()], pa.jvm[..]);
        assert_eq!(ma.game.last().unwrap(), &plain("--childGame"));
        assert_eq!(ma.jvm.last().unwrap(), &plain("-DchildJvm"));
    }

    #[test]
    fn parent_duplicates_survive_when_child_lacks_key() {
        let mut parent = modern();
        let mut a = lib("org.lwjgl.lwjgl:lwjgl-platform:2.9.4");
        let mut b = lib("org.lwjgl.lwjgl:lwjgl-platform:2.9.2");
        a.rules = vec![];
        b.sha1 = Some("marker".into());
        parent.libraries = vec![a, b];
        let mut child = fabric();
        child.libraries = vec![lib("net.fabricmc:fabric-loader:0.16")];
        let m = merge_versions(&parent, &child);
        let names: Vec<_> = m.libraries.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "net.fabricmc:fabric-loader:0.16",
                "org.lwjgl.lwjgl:lwjgl-platform:2.9.4",
                "org.lwjgl.lwjgl:lwjgl-platform:2.9.2"
            ]
        );
    }

    #[test]
    fn natives_survive_next_to_child_main_jar() {
        let mut parent = modern();
        parent.libraries = vec![lib("org.lwjgl:lwjgl:3.3.3:natives-linux")];
        let mut child = fabric();
        child.libraries = vec![lib("org.lwjgl:lwjgl:3.3.4")];
        let m = merge_versions(&parent, &child);
        let names: Vec<_> = m.libraries.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "org.lwjgl:lwjgl:3.3.4",
                "org.lwjgl:lwjgl:3.3.3:natives-linux"
            ]
        );
    }

    #[test]
    fn mixed_argument_styles_keep_both_fields() {
        // parent modern `arguments`, child legacy string only
        let mut child = fabric();
        child.arguments = None;
        child.minecraft_arguments = Some("--legacy".into());
        let m = merge_versions(&modern(), &child);
        assert_eq!(m.arguments, modern().arguments);
        assert_eq!(m.minecraft_arguments.as_deref(), Some("--legacy"));
        // parent legacy string, child `arguments` only
        let mut child = fabric();
        child.minecraft_arguments = None;
        child.arguments = Some(Arguments {
            game: vec![plain("--g")],
            jvm: vec![],
        });
        let m = merge_versions(&legacy(), &child);
        assert_eq!(m.minecraft_arguments, legacy().minecraft_arguments);
        assert_eq!(m.arguments, child.arguments);
    }

    #[test]
    fn child_java_version_overrides_parent() {
        let mut child = fabric();
        child.java_version = Some(JavaVersionReq {
            component: "java-runtime-delta".into(),
            major_version: 21,
        });
        let m = merge_versions(&modern(), &child);
        assert_eq!(m.java_version, child.java_version);
        child.java_version = None;
        assert_eq!(
            merge_versions(&modern(), &child).java_version,
            modern().java_version
        );
    }

    #[test]
    fn legacy_minecraft_arguments_replace() {
        let ip: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/forge_1.7.10_install_profile.json"
        ))
        .unwrap();
        let child: PartialVersionJson = serde_json::from_value(ip["versionInfo"].clone()).unwrap();
        let m = merge_versions(&legacy(), &child);
        assert_eq!(m.minecraft_arguments, child.minecraft_arguments);
        assert!(m.minecraft_arguments.unwrap().contains("--tweakClass"));
    }

    #[test]
    fn downloads_and_assets_come_from_parent() {
        let parent = modern();
        let m = merge_versions(&parent, &fabric());
        assert_eq!(m.downloads.client.url, parent.downloads.client.url);
        assert_eq!(m.asset_index.id, parent.asset_index.id);
        assert_eq!(m.assets, parent.assets);
    }
}
