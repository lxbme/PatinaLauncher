//! Installing modern-format Forge / NeoForge (1.13+: `install_profile.json`
//! with processors + `version.json`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::installer::InstallerProfile;
use super::processors::{
    CLIENT, Processor, ProcessorEnv, extract_entries, referenced_library_files, resolve_data,
    run_processors,
};
use super::{checked_profile_id, download_libraries, fill_sha1_from_sidecars, write_profile};
use crate::download::Downloader;
use crate::fsutil::{is_safe_name, safe_join, write_atomic};
use crate::loader::list::{InstallerProblem, LoaderError};
use crate::loader::{InstalledProfile, LoaderKind};
use crate::meta::{Library, RuleEnv, VersionJson};
use crate::paths::DataDirs;
use crate::progress::{Phase, ProgressSink, Unit, report};
use crate::resolve::resolve_libraries;

/// File in `versions/<profile id>/` listing the processor-generated files
/// the profile needs at launch: a JSON array of `/`-separated paths relative
/// to `libraries/`. Forge 1.17+ and NeoForge find the patched client through
/// launch arguments, not through `url: ""` libraries, so this is the only
/// record of what must exist.
pub const GENERATED_FILE: &str = "patina-generated.json";

/// Per-install scratch dir under `cache/`, removed on drop (success or not).
struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn invalid(problem: InstallerProblem) -> LoaderError {
    LoaderError::InstallerInvalid(problem)
}

/// Installs a modern Forge / NeoForge profile: downloads the installer and
/// profile libraries (`url: ""` ones are taken from the installer's `maven/`
/// when it has them, else a processor makes them), runs the client
/// processors with `java`, records the generated files in
/// [`GENERATED_FILE`] and, as the very last step, writes
/// `versions/<id>/<id>.json` (NeoForge ids get their `neoforge-` prefix).
///
/// `vanilla` is the Minecraft version the profile inherits from; its client
/// jar must already be at `versions/<mc>/<mc>.jar`.
// Each argument is a distinct input of the install; a struct would only
// rename them.
#[allow(clippy::too_many_arguments)]
pub async fn install_modern(
    dl: &Downloader,
    data: &DataDirs,
    kind: LoaderKind,
    jar: &Path,
    profile: InstallerProfile,
    vanilla: &VersionJson,
    java: &Path,
    progress: &ProgressSink,
) -> Result<InstalledProfile, LoaderError> {
    let InstallerProfile::Modern { profile, version } = profile else {
        return Err(invalid(InstallerProblem::NotModern));
    };
    let id = checked_profile_id(kind, &version.id)?;
    // Held until the profile json is written.
    let _version_lock = data.lock_version_exclusive(&id)?;
    let mc = vanilla.id.as_str();
    if !is_safe_name(mc) {
        return Err(LoaderError::UnsafeMetadata(mc.to_string()));
    }
    if let Some(wanted) = profile.get("minecraft").and_then(Value::as_str)
        && wanted != mc
    {
        return Err(invalid(InstallerProblem::WrongMinecraft {
            expected: mc.to_string(),
            found: wanted.to_string(),
        }));
    }
    let minecraft_jar = data.versions().join(mc).join(format!("{mc}.jar"));
    if !minecraft_jar.is_file() {
        return Err(invalid(InstallerProblem::MinecraftJarMissing {
            path: minecraft_jar.display().to_string(),
        }));
    }
    let procs: Vec<Processor> = match profile.get("processors") {
        Some(v) => serde_json::from_value(v.clone())?,
        None => Vec::new(),
    };
    let profile_libs: Vec<Library> = match profile.get("libraries") {
        Some(v) => serde_json::from_value(v.clone())?,
        None => Vec::new(),
    };
    let libraries = data.libraries();
    // Cheap pre-check so a Busy comes before the long library download; the
    // lock is really taken before the processors run (step 3).
    drop(data.lock_processors(mc)?);

    // 1. Libraries.
    let env_rules = RuleEnv::current();
    let mut jobs = Vec::new();
    let mut bundled = Vec::new();
    for libs in [&profile_libs, &version.libraries] {
        let r = resolve_libraries(libs, &env_rules, &libraries);
        jobs.extend(r.jobs);
        for path in r.generated {
            // Forge up to 1.16 ships these inside the installer.
            if let Some(rel) = path.strip_prefix(&libraries).ok().and_then(Path::to_str) {
                bundled.push((format!("maven/{rel}"), path.clone()));
            }
        }
    }
    let (jar_owned, wanted) = (jar.to_path_buf(), bundled);
    let bundled = tokio::task::spawn_blocking(move || extract_entries(&jar_owned, &wanted, false))
        .await
        .map_err(LoaderError::from)??;
    let mut provided: HashSet<PathBuf> = jobs.iter().map(|j| j.dest.clone()).collect();
    provided.extend(bundled);
    fill_sha1_from_sidecars(dl, &mut jobs, |j| j.sha1.is_none()).await;
    download_libraries(dl, jobs, progress, 0).await?;

    // A previous install's list must not outlive a reinstall that fails from
    // here on: without it, launch reports that the loader needs reinstalling.
    let dir =
        safe_join(&data.versions(), &id).ok_or_else(|| LoaderError::UnsafeMetadata(id.clone()))?;
    let generated_file = dir.join(GENERATED_FILE);
    match std::fs::remove_file(&generated_file) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }

    // 2. Data and environment.
    let scratch = ScratchDir(
        data.cache()
            .join("processor-tmp")
            .join(uuid::Uuid::new_v4().to_string()),
    );
    let raw_data = profile.get("data").cloned().unwrap_or(Value::Null);
    let (lib_dir, jar_owned, scratch_path) =
        (libraries.clone(), jar.to_path_buf(), scratch.0.clone());
    let resolved = tokio::task::spawn_blocking(move || {
        resolve_data(&raw_data, CLIENT, &lib_dir, &jar_owned, &scratch_path)
    })
    .await
    .map_err(LoaderError::from)??;
    let env = ProcessorEnv {
        data: resolved,
        minecraft_jar,
        minecraft_version: mc.to_string(),
        root: data.root(),
        installer: jar.to_path_buf(),
        library_dir: libraries.clone(),
    };

    // 3. Processors.
    // MC_SRG / MC_EXTRA / MC_SLIM outputs are shared by every build of this
    // Minecraft version. Lock order: profile first (above), then processors.
    let _processors_lock = data.lock_processors(mc)?;
    run_processors(java, &procs, &env, dl, vanilla, progress).await?;

    // 4. Generated-file list, then the profile JSON last.
    report(progress, Phase::Finalize, 0, 1, Unit::Items);
    let generated: Vec<String> = referenced_library_files(&procs, &env)?
        .into_iter()
        .filter_map(|rel| {
            let path = safe_join(&libraries, &rel)?;
            (!provided.contains(&path) && path.is_file()).then_some(rel)
        })
        .collect();
    write_atomic(&generated_file, &serde_json::to_vec_pretty(&generated)?)?;
    drop(scratch);
    let profile_id = write_profile(data, kind, version)?;
    report(progress, Phase::Finalize, 1, 1, Unit::Items);
    Ok(InstalledProfile { profile_id })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::loader::forge::processors::test_java::{
        FakeJava, OUTPUT, PISTON_DATA, downloader, processor_jar, vanilla,
    };
    use crate::loader::forge::read_installer;
    use crate::loader::forge::test_support::{sha1_hex, write_zip};
    use crate::loader::{Phase, TaskProgress};
    use crate::meta::PartialVersionJson;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ID: &str = "1.20.1-forge-47.1.106";
    const STORED_ID: &str = "neoforge-1.20.1-forge-47.1.106";
    const NEO: &str = "https://maven.neoforged.net/releases";
    const BUNDLED: &str = "net/neoforged/forge/1.20.1-47.1.106/forge-1.20.1-47.1.106-universal.jar";
    const PATCHED: &str = "net/neoforged/forge/1.20.1-47.1.106/forge-1.20.1-47.1.106-client.jar";
    const MOJMAPS: &str =
        "net/minecraft/client/1.20.1-20230612.114412/client-1.20.1-20230612.114412-mappings.txt";
    const MAPPINGS: &[u8] = b"net.minecraft.Foo -> a:";

    /// A library served from the NeoForge maven (`body` → sha1/size).
    fn lib(name: &str, rel: &str, body: &[u8]) -> Value {
        json!({"name": name, "downloads": {"artifact": {
            "path": rel, "url": format!("{NEO}/{rel}"),
            "sha1": sha1_hex(body), "size": body.len()
        }}})
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        server: MockServer,
        java: FakeJava,
        data: DataDirs,
        installer: std::path::PathBuf,
    }

    impl Fixture {
        fn tmp_dir(&self) -> std::path::PathBuf {
            self.data.cache().join("processor-tmp")
        }
    }

    async fn serve(server: &MockServer, p: &str, body: &[u8]) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
            .mount(server)
            .await;
    }

    /// `extra_args` are appended to the patching processor's arguments.
    async fn fixture(extra_args: &[&str]) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let java = FakeJava::new(tmp.path());
        let data = DataDirs::new(tmp.path().join("data"));

        let tool_jar = tmp.path().join("tool.jar");
        processor_jar(&tool_jar, "net.neoforged.binarypatcher.ConsoleTool");
        let tool = std::fs::read(&tool_jar).unwrap();
        let files: [(&str, &str, &[u8]); 4] = [
            ("x:tool:1", "x/tool/1/tool-1.jar", &tool),
            ("x:dep:1", "x/dep/1/dep-1.jar", b"dep"),
            (
                "de.oceanlabs.mcp:mcp_config:1@zip",
                "de/oceanlabs/mcp/mcp_config/1/mcp_config-1.zip",
                b"mcp",
            ),
            (
                "net.neoforged.fancymodloader:loader:1",
                "net/neoforged/fancymodloader/loader/1/loader-1.jar",
                b"fml",
            ),
        ];
        for (_, rel, body) in &files {
            serve(&server, &format!("/{rel}"), body).await;
        }
        let v = vanilla(Some(MAPPINGS));
        let url = &v.downloads.client_mappings.as_ref().unwrap().url;
        serve(&server, url.strip_prefix(PISTON_DATA).unwrap(), MAPPINGS).await;

        let patch_args: Vec<Value> = [
            "--maps",
            "{MOJMAPS}",
            "--mcp",
            "[de.oceanlabs.mcp:mcp_config:1@zip]",
            "--apply",
            "{BINPATCH}",
            "--clean",
            "{MINECRAFT_JAR}",
            "--out",
            "{PATCHED}",
        ]
        .iter()
        .chain(extra_args)
        .map(|a| json!(a))
        .collect();
        let profile = json!({
            "spec": 1,
            "minecraft": "1.20.1",
            "json": "/version.json",
            "data": {
                "MOJMAPS": {
                    "client": "[net.minecraft:client:1.20.1-20230612.114412:mappings@txt]",
                    "server": "[net.minecraft:server:1.20.1-20230612.114412:mappings@txt]"
                },
                "PATCHED": {
                    "client": "[net.neoforged:forge:1.20.1-47.1.106:client]",
                    "server": "[net.neoforged:forge:1.20.1-47.1.106:server]"
                },
                "PATCHED_SHA": {"client": format!("'{}'", sha1_hex(OUTPUT)), "server": "'00'"},
                "BINPATCH": {"client": "/data/client.lzma", "server": "/data/server.lzma"}
            },
            "processors": [
                {"sides": ["server"], "jar": "x:tool:1", "args": ["--out", "{ROOT}/server.jar"]},
                {"jar": "x:tool:1", "args": ["--task", "DOWNLOAD_MOJMAPS", "--version", "1.20.1",
                  "--side", "{SIDE}", "--output", "{MOJMAPS}"]},
                {"sides": ["client"], "jar": "x:tool:1", "classpath": ["x:dep:1"],
                 "args": patch_args, "outputs": {"{PATCHED}": "{PATCHED_SHA}"}}
            ],
            "libraries": [
                lib(files[0].0, files[0].1, files[0].2),
                lib(files[1].0, files[1].1, files[1].2),
                lib(files[2].0, files[2].1, files[2].2),
                {"name": "net.neoforged:forge:1.20.1-47.1.106:universal", "downloads": {"artifact": {
                    "path": BUNDLED, "url": "", "sha1": sha1_hex(b"universal"), "size": 9
                }}}
            ]
        });
        let version = json!({
            "id": ID,
            "inheritsFrom": "1.20.1",
            "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
            "libraries": [lib(files[3].0, files[3].1, files[3].2)]
        });
        let installer = tmp.path().join("installer.jar");
        write_zip(
            &installer,
            &[
                ("install_profile.json", profile.to_string().as_bytes()),
                ("version.json", version.to_string().as_bytes()),
                ("data/client.lzma", b"client patches"),
                ("data/server.lzma", b"server patches"),
                (&format!("maven/{BUNDLED}"), b"universal"),
            ],
        );
        let mc_jar = data.versions().join("1.20.1/1.20.1.jar");
        std::fs::create_dir_all(mc_jar.parent().unwrap()).unwrap();
        std::fs::write(&mc_jar, b"vanilla client").unwrap();
        Fixture {
            _tmp: tmp,
            server,
            java,
            data,
            installer,
        }
    }

    fn vanilla_1201() -> VersionJson {
        let mut v = vanilla(Some(MAPPINGS));
        v.id = "1.20.1".into();
        v
    }

    async fn install(
        f: &Fixture,
        profile: InstallerProfile,
        progress: &ProgressSink,
    ) -> Result<InstalledProfile, LoaderError> {
        install_modern(
            &downloader(&f.server),
            &f.data,
            LoaderKind::Neoforge,
            &f.installer,
            profile,
            &vanilla_1201(),
            &f.java.java,
            progress,
        )
        .await
    }

    fn assert_no_scratch_left(f: &Fixture) {
        let left: Vec<_> = std::fs::read_dir(f.tmp_dir())
            .map(|d| d.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
    }

    #[tokio::test]
    async fn modern_install_runs_processors_and_writes_profile_last() {
        let f = fixture(&[]).await;
        let profile = read_installer(&f.installer).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out = install(&f, profile, &Some(tx)).await.unwrap();
        assert_eq!(out.profile_id, STORED_ID);

        let libs = f.data.libraries();
        assert_eq!(std::fs::read(libs.join(BUNDLED)).unwrap(), b"universal");
        assert_eq!(
            std::fs::read(libs.join("net/neoforged/fancymodloader/loader/1/loader-1.jar")).unwrap(),
            b"fml"
        );
        assert_eq!(std::fs::read(libs.join(MOJMAPS)).unwrap(), MAPPINGS);
        assert_eq!(std::fs::read(libs.join(PATCHED)).unwrap(), OUTPUT);

        // only the client patching processor ran (mojmaps came from the launcher)
        let runs = f.java.runs();
        assert_eq!(runs.len(), 1, "{runs:?}");
        let run = &runs[0];
        assert_eq!(run[2], "net.neoforged.binarypatcher.ConsoleTool");
        let binpatch = &run[run.iter().position(|a| a == "--apply").unwrap() + 1];
        assert!(
            Path::new(binpatch).starts_with(f.tmp_dir()),
            "{binpatch} is not in the per-install scratch dir"
        );
        assert!(binpatch.ends_with("data/client.lzma"));
        let clean = &run[run.iter().position(|a| a == "--clean").unwrap() + 1];
        assert_eq!(
            Path::new(clean),
            f.data.versions().join("1.20.1/1.20.1.jar")
        );
        assert!(!f.data.root().join("server.jar").exists());
        assert_no_scratch_left(&f);

        let dir = f.data.versions().join(STORED_ID);
        let written: PartialVersionJson =
            serde_json::from_slice(&std::fs::read(dir.join(format!("{STORED_ID}.json"))).unwrap())
                .unwrap();
        assert_eq!(written.id, STORED_ID);
        assert_eq!(written.inherits_from.as_deref(), Some("1.20.1"));

        // R5: processor outputs (not downloaded libraries) are recorded
        let generated: Vec<String> =
            serde_json::from_slice(&std::fs::read(dir.join(GENERATED_FILE)).unwrap()).unwrap();
        assert_eq!(generated, vec![MOJMAPS.to_string(), PATCHED.to_string()]);

        let mut last: Option<TaskProgress> = None;
        let mut ran = false;
        while let Ok(p) = rx.try_recv() {
            ran |= p.phase == Phase::RunProcessor && p.done == 2 && p.total == 2;
            last = Some(p);
        }
        assert!(ran);
        assert_eq!(last.unwrap().phase, Phase::Finalize);
    }

    /// Another install running the processors of the same Minecraft version
    /// makes this one Busy (with its own message, not the internal lock key),
    /// and before any library is downloaded.
    #[tokio::test]
    async fn held_processors_lock_makes_install_busy_before_downloads() {
        let f = fixture(&[]).await;
        let held = f.data.lock_processors("1.20.1").unwrap();
        let profile = read_installer(&f.installer).unwrap();
        let r = install(&f, profile, &None).await;
        match &r {
            Err(LoaderError::Busy(b)) => {
                assert_eq!(b.what, crate::locks::BusyKind::Processors("1.20.1".into()))
            }
            other => panic!("{other:?}"),
        }
        assert!(f.server.received_requests().await.unwrap().is_empty());
        assert!(f.java.runs().is_empty());
        // The profile lock was released again.
        assert!(f.data.lock_version_exclusive(STORED_ID).is_ok());
        drop(held);
        let profile = read_installer(&f.installer).unwrap();
        install(&f, profile, &None).await.unwrap();
    }

    #[tokio::test]
    async fn reinstall_skips_processors_with_valid_outputs() {
        let f = fixture(&[]).await;
        for _ in 0..2 {
            let profile = read_installer(&f.installer).unwrap();
            install(&f, profile, &None).await.unwrap();
        }
        assert_eq!(f.java.runs().len(), 1);
    }

    #[tokio::test]
    async fn failed_processor_writes_no_profile() {
        let f = fixture(&["--fail", "2"]).await;
        let profile = read_installer(&f.installer).unwrap();
        let r = install(&f, profile, &None).await;
        assert!(
            matches!(
                r,
                Err(LoaderError::ProcessorFailed {
                    index: 2,
                    step: 2,
                    exit_code: Some(2),
                    ..
                })
            ),
            "{r:?}"
        );
        assert!(crate::locks::only_lock_files(
            &f.data.versions().join(STORED_ID)
        ));
        assert_no_scratch_left(&f);
    }

    #[tokio::test]
    async fn failed_reinstall_removes_the_old_generated_list() {
        let f = fixture(&["--fail", "2"]).await;
        let dir = f.data.versions().join(STORED_ID);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(GENERATED_FILE), format!("[\"{PATCHED}\"]")).unwrap();
        let profile = read_installer(&f.installer).unwrap();
        let r = install(&f, profile, &None).await;
        assert!(
            matches!(r, Err(LoaderError::ProcessorFailed { .. })),
            "{r:?}"
        );
        assert!(!dir.join(GENERATED_FILE).exists());
        assert!(!dir.join(format!("{STORED_ID}.json")).exists());
    }

    #[tokio::test]
    async fn modern_rejects_bad_input() {
        let f = fixture(&[]).await;
        let InstallerProfile::Modern { profile, version } = read_installer(&f.installer).unwrap()
        else {
            panic!("expected Modern");
        };

        let legacy = InstallerProfile::Legacy {
            install_path: "a:b:1".into(),
            file_path: "x.jar".into(),
            version: version.clone(),
        };
        let r = install(&f, legacy, &None).await;
        assert!(matches!(r, Err(LoaderError::InstallerInvalid(_))), "{r:?}");

        let mut other_mc = profile.clone();
        other_mc["minecraft"] = json!("1.21.1");
        let r = install(
            &f,
            InstallerProfile::Modern {
                profile: other_mc,
                version: version.clone(),
            },
            &None,
        )
        .await;
        assert!(matches!(r, Err(LoaderError::InstallerInvalid(_))), "{r:?}");

        let mut evil = version.clone();
        evil.id = "../evil".into();
        let r = install(
            &f,
            InstallerProfile::Modern {
                profile: profile.clone(),
                version: evil,
            },
            &None,
        )
        .await;
        assert!(matches!(r, Err(LoaderError::UnsafeMetadata(_))), "{r:?}");

        std::fs::remove_file(f.data.versions().join("1.20.1/1.20.1.jar")).unwrap();
        let r = install(&f, InstallerProfile::Modern { profile, version }, &None).await;
        assert!(r.is_err(), "{r:?}");

        assert!(crate::locks::only_lock_files(
            &f.data.versions().join(STORED_ID)
        ));
        assert!(f.java.runs().is_empty());
    }
}
