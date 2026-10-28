use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::download::DownloadJob;
use crate::fsutil::safe_join;
use crate::meta::{FileRef, Library, RuleEnv, maven_path, rules_allow};

pub const LIBRARIES_URL: &str = "https://libraries.minecraft.net/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeJar {
    pub path: PathBuf,
    pub exclude: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ResolvedLibraries {
    pub classpath: Vec<PathBuf>,
    pub natives: Vec<NativeJar>,
    pub jobs: Vec<DownloadJob>,
}

pub fn resolve_libraries(
    libs: &[Library],
    env: &RuleEnv,
    libraries_dir: &Path,
) -> ResolvedLibraries {
    let mut out = ResolvedLibraries::default();
    for lib in libs {
        if !rules_allow(&lib.rules, env) {
            continue;
        }
        if let Some(natives) = &lib.natives {
            resolve_old_native(lib, natives, env, libraries_dir, &mut out);
        }
        match lib.downloads.as_ref() {
            Some(downloads) => {
                if let Some(artifact) = &downloads.artifact
                    && let Some(dest) = artifact_dest(artifact, &lib.name, libraries_dir)
                {
                    out.jobs.push(job_from(artifact, dest.clone()));
                    out.classpath.push(dest);
                }
            }
            None => {
                let Some(rel) = maven_path(&lib.name) else {
                    continue;
                };
                let Some(dest) = safe_join(libraries_dir, &rel) else {
                    continue;
                };
                let base = lib.url.as_deref().unwrap_or(LIBRARIES_URL);
                out.jobs.push(DownloadJob {
                    url: format!("{base}{rel}"),
                    dest: dest.clone(),
                    sha1: None,
                    size: None,
                });
                out.classpath.push(dest);
            }
        }
    }
    out
}

/// Destination of `file` under `libraries_dir`: its own `path`, else the maven
/// path of `name`. `None` (skip the library) when the path would escape.
fn artifact_dest(file: &FileRef, name: &str, libraries_dir: &Path) -> Option<PathBuf> {
    let rel = file.path.clone().or_else(|| maven_path(name))?;
    safe_join(libraries_dir, &rel)
}

fn job_from(file: &FileRef, dest: PathBuf) -> DownloadJob {
    DownloadJob {
        url: file.url.clone(),
        dest,
        sha1: Some(file.sha1.clone()),
        size: Some(file.size),
    }
}

fn resolve_old_native(
    lib: &Library,
    natives: &HashMap<String, String>,
    env: &RuleEnv,
    libraries_dir: &Path,
    out: &mut ResolvedLibraries,
) {
    let Some(template) = natives.get(&env.os_name) else {
        return;
    };
    let bits = if env.arch == "x86" { "32" } else { "64" };
    let classifier = template.replace("${arch}", bits);
    let Some(file) = lib
        .downloads
        .as_ref()
        .and_then(|d| d.classifiers.get(&classifier))
    else {
        return;
    };
    let Some(dest) = artifact_dest(file, &lib.name, libraries_dir) else {
        return;
    };
    out.jobs.push(job_from(file, dest.clone()));
    out.natives.push(NativeJar {
        path: dest,
        exclude: lib
            .extract
            .as_ref()
            .map(|e| e.exclude.clone())
            .unwrap_or_default(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::VersionJson;

    fn linux() -> RuleEnv {
        RuleEnv {
            os_name: "linux".into(),
            arch: "x86_64".into(),
            os_version: "6.0".into(),
            features: Default::default(),
        }
    }

    fn modern() -> VersionJson {
        serde_json::from_str(include_str!("../../tests/fixtures/version_modern.json")).unwrap()
    }
    fn legacy() -> VersionJson {
        serde_json::from_str(include_str!("../../tests/fixtures/version_legacy.json")).unwrap()
    }

    #[test]
    fn modern_filters_foreign_os_libraries() {
        let v = modern();
        let r = resolve_libraries(&v.libraries, &linux(), Path::new("/L"));
        let cp: Vec<String> = r
            .classpath
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert!(cp.iter().any(|p| p.ends_with("brigadier-1.0.18.jar")));
        assert!(
            cp.iter()
                .any(|p| p.ends_with("lwjgl-3.3.3-natives-linux.jar"))
        );
        assert!(
            !cp.iter()
                .any(|p| p.contains("natives-macos") || p.contains("java-objc-bridge"))
        );
        assert_eq!(cp.len(), 2);
        assert!(r.natives.is_empty());
        assert_eq!(r.jobs.len(), 2);
    }

    #[test]
    fn legacy_natives_go_to_extraction_not_classpath() {
        let v = legacy();
        let r = resolve_libraries(&v.libraries, &linux(), Path::new("/L"));
        assert_eq!(r.natives.len(), 1);
        assert!(
            r.natives[0]
                .path
                .display()
                .to_string()
                .ends_with("lwjgl-platform-2.9.4-nightly-20150209-natives-linux.jar")
        );
        assert_eq!(r.natives[0].exclude, vec!["META-INF/".to_string()]);
        // the plain artifact stays on the classpath; the natives classifier never does
        assert!(
            r.classpath
                .iter()
                .any(|p| p.ends_with("lwjgl-platform-2.9.4-nightly-20150209.jar"))
        );
        assert!(
            !r.classpath
                .iter()
                .any(|p| p.display().to_string().contains("natives"))
        );
    }

    #[test]
    fn windows_arch_placeholder_is_replaced() {
        let v = legacy();
        let mut env = linux();
        env.os_name = "windows".into();
        let r = resolve_libraries(&v.libraries, &env, Path::new("/L"));
        assert!(
            r.natives[0]
                .path
                .display()
                .to_string()
                .ends_with("natives-windows-64.jar")
        );
    }

    #[test]
    fn missing_classifier_is_skipped() {
        let v = legacy();
        let mut env = linux();
        env.os_name = "freebsd".into();
        let r = resolve_libraries(&v.libraries, &env, Path::new("/L"));
        assert!(r.natives.is_empty());
    }

    #[test]
    fn library_without_downloads_uses_maven_default() {
        let v = legacy();
        let r = resolve_libraries(&v.libraries, &linux(), Path::new("/L"));
        let job = r
            .jobs
            .iter()
            .find(|j| j.url.contains("launchwrapper"))
            .expect("launchwrapper job");
        assert_eq!(
            job.url,
            "https://libraries.minecraft.net/net/minecraft/launchwrapper/1.12/launchwrapper-1.12.jar"
        );
        assert_eq!(job.sha1, None);
        assert!(
            r.classpath
                .iter()
                .any(|p| p.ends_with("net/minecraft/launchwrapper/1.12/launchwrapper-1.12.jar"))
        );
    }

    #[test]
    fn unsafe_library_paths_are_skipped() {
        let libs: Vec<Library> = serde_json::from_str(
            r#"[
            {"name":"a:rel:1","downloads":{"artifact":{"url":"https://x/e","sha1":"00","size":1,"path":"../../evil.jar"}}},
            {"name":"a:abs:1","downloads":{"artifact":{"url":"https://x/f","sha1":"00","size":1,"path":"/etc/evil.jar"}}},
            {"name":"a:b:../../x"},
            {"name":"a:nat:1","natives":{"linux":"natives-linux"},
             "downloads":{"classifiers":{"natives-linux":{"url":"https://x/n","sha1":"00","size":1,"path":"../../n.jar"}}}},
            {"name":"a:nat2:../../y","natives":{"linux":"natives-linux"},
             "downloads":{"classifiers":{"natives-linux":{"url":"https://x/m","sha1":"00","size":1}}}},
            {"name":"ok:good:1"}
        ]"#,
        )
        .unwrap();
        let root = Path::new("/L");
        let r = resolve_libraries(&libs, &linux(), root);
        assert_eq!(r.classpath, vec![root.join("ok/good/1/good-1.jar")]);
        assert!(r.natives.is_empty(), "{:?}", r.natives);
        assert_eq!(r.jobs.len(), 1, "{:?}", r.jobs);
        assert_eq!(r.jobs[0].dest, root.join("ok/good/1/good-1.jar"));
        for p in r.classpath.iter().chain(r.jobs.iter().map(|j| &j.dest)) {
            assert!(p.starts_with(root), "{} escapes the root", p.display());
        }
    }
}
