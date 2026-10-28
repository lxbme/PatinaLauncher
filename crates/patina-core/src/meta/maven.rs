//! Maven coordinate to repository path conversion.

/// Converts `group:artifact:version[:classifier][@ext]` to a repository-relative path.
pub fn maven_path(coord: &str) -> Option<String> {
    let (main, ext) = match coord.split_once('@') {
        Some((m, e)) => (m, e),
        None => (coord, "jar"),
    };
    if ext.is_empty() {
        return None;
    }
    let parts: Vec<&str> = main.split(':').collect();
    if !(3..=4).contains(&parts.len()) || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let (group, artifact, version) = (parts[0], parts[1], parts[2]);
    let file = match parts.get(3) {
        Some(c) => format!("{artifact}-{version}-{c}.{ext}"),
        None => format!("{artifact}-{version}.{ext}"),
    };
    Some(format!(
        "{}/{artifact}/{version}/{file}",
        group.replace('.', "/")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maven_paths() {
        assert_eq!(
            maven_path("org.lwjgl:lwjgl:3.3.3:natives-linux").unwrap(),
            "org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-linux.jar"
        );
        assert_eq!(
            maven_path("net.minecraft:launchwrapper:1.12").unwrap(),
            "net/minecraft/launchwrapper/1.12/launchwrapper-1.12.jar"
        );
        assert_eq!(maven_path("a.b:c:1.0@zip").unwrap(), "a/b/c/1.0/c-1.0.zip");
        assert_eq!(
            maven_path("a.b:c:1.0:cls@zip").unwrap(),
            "a/b/c/1.0/c-1.0-cls.zip"
        );
        assert!(maven_path("broken").is_none());
        assert!(maven_path("a:b:").is_none());
    }
}
