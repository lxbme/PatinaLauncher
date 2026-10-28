//! Path containment for paths that come from downloaded metadata.

use std::path::{Component, Path, PathBuf};

/// `rel` is a non-empty relative path made only of normal components: no
/// root, no `.` or `..`, no empty segment, no backslash.
pub fn is_safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && rel
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != ".." && !s.contains('\\'))
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// `name` is a single safe path component (a file or directory name).
pub fn is_safe_name(name: &str) -> bool {
    is_safe_rel(name) && !name.contains('/')
}

/// `root.join(rel)` when `rel` is safe (see [`is_safe_rel`]), so the result
/// always stays inside `root`.
pub fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    is_safe_rel(rel).then(|| root.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_accepts_plain_relative_paths() {
        let root = Path::new("/root");
        assert_eq!(
            safe_join(root, "org/lwjgl/lwjgl-3.3.3.jar"),
            Some(PathBuf::from("/root/org/lwjgl/lwjgl-3.3.3.jar"))
        );
        assert_eq!(safe_join(root, "1.8.9"), Some(PathBuf::from("/root/1.8.9")));
        assert_eq!(safe_join(root, "a..b"), Some(PathBuf::from("/root/a..b")));
        assert_eq!(
            safe_join(root, ".hidden"),
            Some(PathBuf::from("/root/.hidden"))
        );
    }

    #[test]
    fn safe_join_rejects_escapes() {
        let root = Path::new("/root");
        for bad in [
            "",
            ".",
            "..",
            "./a",
            "a/.",
            "a/./b",
            "../x",
            "a/../../x",
            "a/..",
            "/abs",
            "/",
            "a//b",
            "a/",
            "..\\x",
            "a\\b",
        ] {
            assert_eq!(safe_join(root, bad), None, "{bad:?} was accepted");
            assert!(!is_safe_rel(bad), "{bad:?}");
        }
    }

    #[test]
    fn safe_names_are_single_components() {
        assert!(is_safe_name("1.8.9"));
        assert!(is_safe_name("client-1.12.xml"));
        assert!(!is_safe_name("a/b"));
        assert!(!is_safe_name(".."));
        assert!(!is_safe_name(""));
        assert!(!is_safe_name("/abs"));
    }
}
