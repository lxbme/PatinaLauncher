use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::download::{DownloadJob, mirror::canonical_url};
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
    /// Classpath libraries produced by loader processors (empty artifact url):
    /// never downloaded, but expected on disk.
    pub generated: Vec<PathBuf>,
}

pub fn resolve_libraries(
    libs: &[Library],
    env: &RuleEnv,
    libraries_dir: &Path,
) -> ResolvedLibraries {
    let mut out = ResolvedLibraries::default();
    for lib in libs {
        if !lib.is_client_side() || !rules_allow(&lib.rules, env) {
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
                    if artifact.url.is_empty() {
                        // Produced by a loader processor; never downloaded.
                        out.generated.push(dest.clone());
                    } else {
                        out.jobs.push(job_from(artifact, dest.clone()));
                    }
                    out.classpath.push(dest);
                }
            }
            // An old-style natives library has no plain jar, only the
            // classifier jar `resolve_old_native` handled.
            None if lib.natives.is_some() => {}
            None => {
                let Some((rel, dest)) = maven_dest(&lib.name, libraries_dir) else {
                    continue;
                };
                out.jobs.push(DownloadJob {
                    url: maven_url(lib, &rel),
                    dest: dest.clone(),
                    sha1: lib.sha1.clone(),
                    size: lib.size,
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

/// Maven path of coordinate `name` and its destination under `libraries_dir`.
fn maven_dest(name: &str, libraries_dir: &Path) -> Option<(String, PathBuf)> {
    let rel = maven_path(name)?;
    let dest = safe_join(libraries_dir, &rel)?;
    Some((rel, dest))
}

/// `rel` under the library's own maven base (`url`), else Mojang's.
fn maven_url(lib: &Library, rel: &str) -> String {
    let mut base = lib.url.as_deref().unwrap_or(LIBRARIES_URL).to_string();
    if !base.ends_with('/') {
        base.push('/');
    }
    canonical_url(&format!("{base}{rel}"))
}

fn job_from(file: &FileRef, dest: PathBuf) -> DownloadJob {
    DownloadJob {
        url: canonical_url(&file.url),
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
    let (job, dest) = match &lib.downloads {
        Some(downloads) => {
            let Some(file) = downloads.classifiers.get(&classifier) else {
                return;
            };
            let Some(dest) = artifact_dest(file, &lib.name, libraries_dir) else {
                return;
            };
            (job_from(file, dest.clone()), dest)
        }
        // Old-style entry (e.g. Forge 1.6.4's `versionInfo`): the classifier
        // jar sits next to the library at its maven base.
        // TODO(forge-1.6/1.7.2): these entries also shadow vanilla's lwjgl
        // natives by group:artifact; launching stays best effort.
        None => {
            let Some((rel, dest)) =
                maven_dest(&format!("{}:{classifier}", lib.name), libraries_dir)
            else {
                return;
            };
            let job = DownloadJob {
                url: maven_url(lib, &rel),
                dest: dest.clone(),
                sha1: None,
                size: None,
            };
            (job, dest)
        }
    };
    out.jobs.push(job);
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

    fn libs(json: &str) -> Vec<Library> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn library_level_sha1_is_used() {
        let l = libs(
            r#"[{"name":"net.fabricmc:intermediary:1.21.1","url":"https://maven.fabricmc.net/","sha1":"abc","size":42}]"#,
        );
        let r = resolve_libraries(&l, &linux(), Path::new("/L"));
        assert_eq!(r.jobs.len(), 1);
        assert_eq!(r.jobs[0].sha1.as_deref(), Some("abc"));
        assert_eq!(r.jobs[0].size, Some(42));
    }

    #[test]
    fn url_without_trailing_slash_is_normalized() {
        let l = libs(
            r#"[{"name":"net.fabricmc:intermediary:1.21.1","url":"https://maven.fabricmc.net"}]"#,
        );
        let r = resolve_libraries(&l, &linux(), Path::new("/L"));
        assert_eq!(
            r.jobs[0].url,
            "https://maven.fabricmc.net/net/fabricmc/intermediary/1.21.1/intermediary-1.21.1.jar"
        );
    }

    #[test]
    fn empty_url_artifact_is_generated_not_downloaded() {
        let l = libs(
            r#"[{"name":"net.minecraftforge:forge:1.20.1:client","downloads":{"artifact":{"url":"","sha1":"","size":0,"path":"net/minecraftforge/forge/1.20.1/forge-1.20.1-client.jar"}}}]"#,
        );
        let r = resolve_libraries(&l, &linux(), Path::new("/L"));
        let p =
            Path::new("/L/net/minecraftforge/forge/1.20.1/forge-1.20.1-client.jar").to_path_buf();
        assert_eq!(r.classpath, vec![p.clone()]);
        assert_eq!(r.generated, vec![p]);
        assert!(r.jobs.is_empty());
    }

    /// Old-style natives libraries (no `downloads`, as in Forge 1.6.4's
    /// `versionInfo`) have no plain jar: only `<name>-<classifier>.jar`
    /// exists, at the library's maven base.
    #[test]
    fn natives_library_without_downloads_fetches_only_the_classifier() {
        let l = libs(
            r#"[{"name":"org.lwjgl.lwjgl:lwjgl-platform:2.9.0","natives":{"linux":"natives-linux","windows":"natives-windows-${arch}"},"extract":{"exclude":["META-INF/"]}},
            {"name":"net.java.jinput:jinput-platform:2.0.5","url":"https://example.net/m","natives":{"linux":"natives-linux"}},
            {"name":"org.lwjgl.lwjgl:lwjgl:2.9.0"}]"#,
        );
        let r = resolve_libraries(&l, &linux(), Path::new("/L"));
        let urls: Vec<&str> = r.jobs.iter().map(|j| j.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://libraries.minecraft.net/org/lwjgl/lwjgl/lwjgl-platform/2.9.0/lwjgl-platform-2.9.0-natives-linux.jar",
                "https://example.net/m/net/java/jinput/jinput-platform/2.0.5/jinput-platform-2.0.5-natives-linux.jar",
                "https://libraries.minecraft.net/org/lwjgl/lwjgl/lwjgl/2.9.0/lwjgl-2.9.0.jar",
            ]
        );
        assert_eq!(
            r.natives,
            vec![
                NativeJar {
                    path: Path::new("/L/org/lwjgl/lwjgl/lwjgl-platform/2.9.0/lwjgl-platform-2.9.0-natives-linux.jar").into(),
                    exclude: vec!["META-INF/".into()],
                },
                NativeJar {
                    path: Path::new("/L/net/java/jinput/jinput-platform/2.0.5/jinput-platform-2.0.5-natives-linux.jar").into(),
                    exclude: vec![],
                },
            ]
        );
        assert_eq!(
            r.classpath,
            vec![Path::new("/L/org/lwjgl/lwjgl/lwjgl/2.9.0/lwjgl-2.9.0.jar").to_path_buf()]
        );
        // No classifier for this OS: nothing at all.
        let mut env = linux();
        env.os_name = "freebsd".into();
        let r = resolve_libraries(&l[..1], &env, Path::new("/L"));
        assert!(r.jobs.is_empty() && r.natives.is_empty() && r.classpath.is_empty());
    }

    #[test]
    fn server_only_library_is_skipped() {
        let l = libs(r#"[{"name":"a:b:1","clientreq":false},{"name":"a:c:1","clientreq":true}]"#);
        let r = resolve_libraries(&l, &linux(), Path::new("/L"));
        assert_eq!(r.classpath.len(), 1);
        assert_eq!(r.jobs.len(), 1);
        assert!(r.classpath[0].ends_with("c-1.jar"));
    }

    #[test]
    fn old_forge_maven_url_is_canonicalized() {
        let l = libs(
            r#"[{"name":"a:b:1","url":"http://files.minecraftforge.net/maven/"},
            {"name":"a:c:1","downloads":{"artifact":{"url":"http://files.minecraftforge.net/maven/a/c/1/c-1.jar","sha1":"00","size":1}}},
            {"name":"a:n:1","natives":{"linux":"natives-linux"},"downloads":{"classifiers":{"natives-linux":{"url":"https://files.minecraftforge.net/maven/a/n/1/n-1-natives-linux.jar","sha1":"00","size":1,"path":"a/n/1/n-1-natives-linux.jar"}}}}]"#,
        );
        let r = resolve_libraries(&l, &linux(), Path::new("/L"));
        assert_eq!(r.jobs.len(), 3);
        for j in &r.jobs {
            assert!(
                j.url.starts_with("https://maven.minecraftforge.net/"),
                "{}",
                j.url
            );
        }
    }
}
