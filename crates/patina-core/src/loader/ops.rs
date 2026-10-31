//! Instance-level loader operations: install, change version, change kind,
//! remove, and changing the Minecraft version of an instance with a loader.
//!
//! Every operation takes the instance lock and refuses with
//! [`BusyKind::Running`](crate::locks::BusyKind::Running) while the
//! instance's game runs, so a running game's version is never changed.

use std::fmt;

use super::fabric::install_fabric;
use super::forge::installed::{installed_forge, is_installed};
use super::forge::installer::is_safe_version;
use super::forge::{
    InstallerProfile, fetch_installer, install_legacy, install_modern, read_installer,
};
use super::list::{InstallerProblem, LoaderError, LoaderVersion, VersionPick, list_versions};
use super::{InstalledProfile, LoaderKind};
use crate::instance::{Component, ComponentUid, Instance, InstanceStore};
use crate::launch::running::refuse_while_running;
use crate::meta::VersionJson;
use crate::prepare::{Launcher, PrepareError};
use crate::progress::{Phase, ProgressSink, Unit, report};

/// Something the user should know after a successful operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderNotice {
    /// The loader kind changed, so mods made for the old one may not load.
    ModsMayBeIncompatible,
}

impl fmt::Display for LoaderNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoaderNotice::ModsMayBeIncompatible => {
                f.write_str(&rust_i18n::t!("notices.loader.mods_may_be_incompatible"))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpOutcome {
    /// The instance as saved.
    pub instance: Instance,
    pub notices: Vec<LoaderNotice>,
}

/// What to do with a loader that does not support a new Minecraft version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderAction {
    /// Install the recommended version of the same loader for it.
    Replace,
    Remove,
}

/// Installs a `kind` loader (the version `pick` selects for the instance's
/// Minecraft version) into instance `id`. Installing another version of the
/// instance's current loader kind changes its version; replacing a loader of
/// another kind fails with [`LoaderError::Conflict`] unless
/// `replace_existing`, and then reports [`LoaderNotice::ModsMayBeIncompatible`].
///
/// The instance config is saved only after every install step succeeded:
/// on any error the instance is left as it was.
pub async fn install_loader(
    launcher: &Launcher,
    store: &InstanceStore,
    id: &str,
    kind: LoaderKind,
    pick: VersionPick,
    replace_existing: bool,
    progress: ProgressSink,
) -> Result<OpOutcome, LoaderError> {
    let _lock = store.lock(id)?;
    let inst = store.load(id)?;
    refuse_while_running(&inst.dir, &inst.id)?;
    let previous = inst.config.loader().map(|(k, c)| (k, c.version.clone()));
    if let Some((existing, existing_version)) = &previous
        && *existing != kind
        && !replace_existing
    {
        return Err(LoaderError::Conflict {
            existing: *existing,
            existing_version: existing_version.clone(),
        });
    }
    let mc = inst.config.mc_version();
    check_mc(mc)?;
    report(&progress, Phase::FetchMetadata, 0, 1, Unit::Items);
    let list = list_versions(&launcher.downloader, kind, mc).await?;
    let version = pick.pick(&list).ok_or_else(|| match &pick {
        VersionPick::Exact(v) => LoaderError::VersionNotFound {
            kind,
            version: v.clone(),
        },
        // `list_versions` never returns an empty list.
        VersionPick::Recommended | VersionPick::Latest => LoaderError::NoVersions {
            kind,
            mc: mc.to_string(),
        },
    })?;
    let vanilla = launcher.ensure_version(mc).await?;
    report(&progress, Phase::FetchMetadata, 1, 1, Unit::Items);
    let component = install_version(launcher, version, &vanilla, false, &progress).await?;

    let mut config = inst.config.clone();
    config.set_loader(Some(component))?;
    let instance = commit_async(store, id, config.components).await?;
    let notices = match previous {
        Some((old, _)) if old != kind => vec![LoaderNotice::ModsMayBeIncompatible],
        _ => Vec::new(),
    };
    Ok(OpOutcome { instance, notices })
}

/// Reinstalls the instance's current loader (same kind and version) even
/// when the shared install looks complete, repairing damaged files. The
/// "already installed" reuse check is skipped; the instance lock and the
/// version's exclusive lock are still taken. Fails with
/// [`LoaderError::NoLoader`] for a vanilla instance.
pub async fn reinstall_loader(
    launcher: &Launcher,
    store: &InstanceStore,
    id: &str,
    progress: ProgressSink,
) -> Result<OpOutcome, LoaderError> {
    let _lock = store.lock(id)?;
    let inst = store.load(id)?;
    refuse_while_running(&inst.dir, &inst.id)?;
    let (kind, current) = inst.config.loader().ok_or(LoaderError::NoLoader)?;
    let pick = VersionPick::Exact(current.version.clone());
    tracing::info!(instance = %id, loader = ?kind, version = %current.version, "reinstalling loader");
    let mc = inst.config.mc_version();
    check_mc(mc)?;
    report(&progress, Phase::FetchMetadata, 0, 1, Unit::Items);
    let list = list_versions(&launcher.downloader, kind, mc).await?;
    let version = pick
        .pick(&list)
        .ok_or_else(|| LoaderError::VersionNotFound {
            kind,
            version: current.version.clone(),
        })?;
    let vanilla = launcher.ensure_version(mc).await?;
    report(&progress, Phase::FetchMetadata, 1, 1, Unit::Items);
    let component = install_version(launcher, version, &vanilla, true, &progress).await?;
    let mut config = inst.config.clone();
    config.set_loader(Some(component))?;
    Ok(OpOutcome {
        instance: commit_async(store, id, config.components).await?,
        notices: Vec::new(),
    })
}

/// Removes the instance's loader (a no-op for a vanilla instance).
pub fn remove_loader(store: &InstanceStore, id: &str) -> Result<OpOutcome, LoaderError> {
    let _lock = store.lock(id)?;
    let inst = store.load(id)?;
    refuse_while_running(&inst.dir, &inst.id)?;
    let mut config = inst.config;
    config.set_loader(None)?;
    Ok(OpOutcome {
        instance: commit(store, id, config.components)?,
        notices: Vec::new(),
    })
}

/// Changes the instance's Minecraft version to `mc`.
///
/// - Without a loader only the version changes.
/// - [`LoaderAction::Remove`] always drops the loader, whatever its kind and
///   whether or not it supports `mc`; nothing is listed or fetched.
/// - Fabric loader versions work across Minecraft versions: when `mc`'s
///   Fabric list has the installed loader version, that version is
///   reinstalled for `mc` (Fabric profile ids include the Minecraft version).
/// - Otherwise (always for Forge and NeoForge, whose builds target one
///   Minecraft version) the loader is incompatible: with `action == None`
///   this fails with [`LoaderError::Incompatible`], and
///   [`LoaderAction::Replace`] installs the recommended version of the same
///   loader for `mc`. `Replace` is ignored when the loader is compatible.
///
/// The instance config is saved only once everything succeeded.
pub async fn set_mc_version(
    launcher: &Launcher,
    store: &InstanceStore,
    id: &str,
    mc: &str,
    action: Option<LoaderAction>,
    progress: ProgressSink,
) -> Result<OpOutcome, LoaderError> {
    let _lock = store.lock(id)?;
    let inst = store.load(id)?;
    refuse_while_running(&inst.dir, &inst.id)?;
    check_mc(mc)?;
    if inst.config.mc_version() == mc {
        return Ok(OpOutcome {
            instance: inst,
            notices: Vec::new(),
        });
    }
    let mut config = inst.config.clone();
    config.set_mc_version(mc);
    let (kind, current) = match inst.config.loader() {
        Some(loader) if action != Some(LoaderAction::Remove) => loader,
        // No loader, or an explicit Remove: no listing, no network.
        _ => {
            config.set_loader(None)?;
            return Ok(OpOutcome {
                instance: commit_async(store, id, config.components).await?,
                notices: Vec::new(),
            });
        }
    };

    report(&progress, Phase::FetchMetadata, 0, 1, Unit::Items);
    let list = match list_versions(&launcher.downloader, kind, mc).await {
        Ok(list) => list,
        Err(LoaderError::NoVersions { .. }) => Vec::new(),
        Err(e) => return Err(e),
    };
    let same = match kind {
        LoaderKind::Fabric => VersionPick::Exact(current.version.clone()).pick(&list),
        LoaderKind::Forge | LoaderKind::Neoforge => None,
    };
    let target =
        match (same, action) {
            (Some(v), _) => v,
            (None, Some(LoaderAction::Replace)) => VersionPick::Recommended
                .pick(&list)
                .ok_or_else(|| LoaderError::NoVersions {
                    kind,
                    mc: mc.to_string(),
                })?,
            // `Remove` returned above.
            (None, _) => {
                return Err(LoaderError::Incompatible {
                    kind,
                    version: current.version.clone(),
                    can_replace: !list.is_empty(),
                });
            }
        };
    let vanilla = launcher.ensure_version(mc).await?;
    report(&progress, Phase::FetchMetadata, 1, 1, Unit::Items);
    let component = install_version(launcher, target, &vanilla, false, &progress).await?;
    config.set_loader(Some(component))?;
    Ok(OpOutcome {
        instance: commit_async(store, id, config.components).await?,
        notices: Vec::new(),
    })
}

/// A Minecraft version that is safe in loader URLs and file names; anything
/// else cannot be a real version.
fn check_mc(mc: &str) -> Result<(), LoaderError> {
    if is_safe_version(mc) {
        Ok(())
    } else {
        Err(PrepareError::UnknownVersion(mc.to_string()).into())
    }
}

fn component_uid(kind: LoaderKind) -> ComponentUid {
    match kind {
        LoaderKind::Forge => ComponentUid::Forge,
        LoaderKind::Neoforge => ComponentUid::Neoforge,
        LoaderKind::Fabric => ComponentUid::Fabric,
    }
}

/// Installs loader version `v` (for `v.mc`, whose version JSON is
/// `vanilla`) into the shared data dirs and returns its instance component.
/// A version that is already completely installed (another instance uses
/// it) is reused as it is: no installer is fetched or run and no shared file
/// is rewritten, unless `force`, which always reinstalls. Touches no instance.
async fn install_version(
    launcher: &Launcher,
    v: &LoaderVersion,
    vanilla: &VersionJson,
    force: bool,
    progress: &ProgressSink,
) -> Result<Component, LoaderError> {
    let (dl, data, kind, mc) = (&launcher.downloader, &launcher.data, v.kind, v.mc.as_str());
    // Processors patch the vanilla client jar, and every loader launches on it.
    launcher.ensure_client_jar(mc, vanilla).await?;
    let installed = match kind {
        LoaderKind::Fabric => {
            for s in [mc, v.maven_version.as_str()] {
                if !is_safe_version(s) {
                    return Err(LoaderError::UnsafeMetadata(s.to_string()));
                }
            }
            // The id Fabric's metadata server gives its profiles.
            let id = format!("fabric-loader-{}-{mc}", v.maven_version);
            // The shared lock is dropped before an install takes the exclusive one.
            let present = !force && {
                let _shared = data.lock_version_shared(&id)?;
                is_installed(data, &id, false).await
            };
            if present {
                tracing::info!(profile = %id, "loader already installed; reusing");
                reused(progress, id)
            } else {
                install_fabric(dl, data, mc, &v.maven_version, progress).await?
            }
        }
        LoaderKind::Forge | LoaderKind::Neoforge => {
            let found = match installed_forge(data, kind, mc, v).await.filter(|_| !force) {
                Some(f) => {
                    let _shared = data.lock_version_shared(&f.profile_id)?;
                    Some(f)
                }
                None => None,
            };
            if let Some(found) = found {
                tracing::info!(profile = %found.profile_id, "loader already installed; reusing");
                reused(progress, found.profile_id)
            } else {
                install_forge(launcher, v, vanilla, progress).await?
            }
        }
    };
    Ok(Component {
        uid: component_uid(kind),
        version: v.version.clone(),
        profile: Some(installed.profile_id),
    })
}

/// An already installed profile, reported as finalized.
fn reused(progress: &ProgressSink, profile_id: String) -> InstalledProfile {
    report(progress, Phase::Finalize, 0, 1, Unit::Items);
    report(progress, Phase::Finalize, 1, 1, Unit::Items);
    InstalledProfile { profile_id }
}

/// Fetches and runs the Forge / NeoForge installer for `v`.
async fn install_forge(
    launcher: &Launcher,
    v: &LoaderVersion,
    vanilla: &VersionJson,
    progress: &ProgressSink,
) -> Result<InstalledProfile, LoaderError> {
    let (dl, data, kind, mc) = (&launcher.downloader, &launcher.data, v.kind, v.mc.as_str());
    let jar = fetch_installer(dl, data, kind, mc, v, progress).await?;
    let read_from = jar.clone();
    let profile = tokio::task::spawn_blocking(move || read_installer(&read_from))
        .await
        .map_err(LoaderError::from)??;
    match profile {
        InstallerProfile::Legacy { .. } if kind == LoaderKind::Forge => {
            install_legacy(dl, data, &jar, profile, progress).await
        }
        InstallerProfile::Legacy { .. } => Err(LoaderError::InstallerInvalid(
            InstallerProblem::NeoforgeLegacy,
        )),
        InstallerProfile::Modern { .. } => {
            // Processors run on the managed runtime for this version.
            let java = launcher.select_java(None, vanilla, progress).await?;
            install_modern(dl, data, kind, &jar, profile, vanilla, &java.path, progress).await
        }
    }
}

/// [`commit`] on the blocking pool: the config lock it takes may wait for
/// another writer, which must not stall a runtime worker.
async fn commit_async(
    store: &InstanceStore,
    id: &str,
    components: Vec<Component>,
) -> Result<Instance, LoaderError> {
    let (store, id) = (store.clone(), id.to_string());
    tokio::task::spawn_blocking(move || commit(&store, &id, components)).await?
}

/// Saves `components` into instance `id` as it is on disk now (under the
/// config lock, [`InstanceStore::update`]), so changes to its other fields
/// made during a long install (name, Java settings) are kept.
fn commit(
    store: &InstanceStore,
    id: &str,
    components: Vec<Component>,
) -> Result<Instance, LoaderError> {
    Ok(store.update(id, |config| config.components = components)?)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::download::{Downloader, Mirror};
    use crate::i18n::{Language, locale_lock, set_language};
    use crate::java::{ManagedRuntimes, RUNTIME_INDEX_URL};
    use crate::loader::TaskProgress;
    use crate::loader::forge::processors::test_java::{FakeJava, processor_jar};
    use crate::loader::forge::test_support::{OFFICIAL_HOSTS, sha1_hex, write_zip};
    use crate::paths::DataDirs;
    use serde_json::{Value, json};
    use std::net::SocketAddr;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FABRIC_HOSTS: [&str; 2] = ["https://meta.fabricmc.net", "https://maven.fabricmc.net"];
    const PISTON_META: &str = "https://piston-meta.mojang.com";
    const PISTON_DATA: &str = "https://piston-data.mojang.com";
    /// Vanilla versions the fake manifest lists.
    const MCS: [&str; 3] = ["1.20.1", "1.21.1", "1.12.2"];
    /// Fabric has these loader versions for 1.20.1 and 1.21.1 (none for 1.12.2).
    const FABRIC_MCS: [&str; 2] = ["1.20.1", "1.21.1"];
    const FABRIC_VERSIONS: [&str; 2] = ["0.16.10", "0.16.9"];
    /// Forge: a modern (processors) build for 1.20.1, a legacy one for 1.12.2.
    const FORGE_MODERN: &str = "47.1.0";
    const FORGE_MODERN_ID: &str = "1.20.1-forge-47.1.0";
    const FORGE_LEGACY: &str = "14.23.5.2859";
    const FORGE_LEGACY_ID: &str = "1.12.2-forge-14.23.5.2859";
    const PATCHED: &str = "net/minecraftforge/forge/1.20.1-47.1.0/forge-1.20.1-47.1.0-client.jar";
    const UNIVERSAL: &str =
        "net/minecraftforge/forge/1.12.2-14.23.5.2859/forge-1.12.2-14.23.5.2859.jar";

    /// Every official host maps to `server`; the real hosts resolve to a
    /// closed local port, so nothing can reach the network.
    fn downloader(server: &MockServer) -> Downloader {
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let hosts: Vec<&str> = OFFICIAL_HOSTS
            .iter()
            .chain(&FABRIC_HOSTS)
            .copied()
            .collect();
        let mut b = reqwest::Client::builder().no_proxy();
        for h in &hosts {
            let host = h.trim_start_matches("https://").split('/').next().unwrap();
            b = b.resolve(host, dead);
        }
        let rules = hosts
            .iter()
            .map(|h| (h.to_string(), server.uri()))
            .collect();
        Downloader::new(
            b.build().unwrap(),
            Mirror {
                id: "fake".into(),
                rules,
            },
        )
        .with_retry(1, Duration::ZERO)
    }

    async fn serve(server: &MockServer, p: &str, body: impl Into<Vec<u8>>) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.into()))
            .mount(server)
            .await;
    }

    fn fabric_id(version: &str, mc: &str) -> String {
        format!("fabric-loader-{version}-{mc}")
    }

    fn fabric_lib(version: &str) -> (String, Vec<u8>) {
        (
            format!("/net/fabricmc/fabric-loader/{version}/fabric-loader-{version}.jar"),
            format!("fabric-loader {version}").into_bytes(),
        )
    }

    struct Env {
        tmp: tempfile::TempDir,
        server: MockServer,
        launcher: Launcher,
        store: InstanceStore,
        java: FakeJava,
    }

    impl Env {
        async fn new() -> Env {
            let tmp = tempfile::tempdir().unwrap();
            let server = MockServer::start().await;
            let launcher = Launcher {
                data: DataDirs::new(tmp.path().join("data")),
                downloader: downloader(&server),
            };
            let store = InstanceStore::new(tmp.path().join("instances"));
            let java = FakeJava::new(tmp.path());
            let env = Env {
                tmp,
                server,
                launcher,
                store,
                java,
            };
            env.install_runtime().await;
            env.serve_all(None).await;
            env
        }

        /// Installs the managed runtime the vanilla fixture asks for, with
        /// the fake `java` as its `bin/java`.
        async fn install_runtime(&self) {
            let component = self.vanilla("1.20.1")["javaVersion"]["component"]
                .as_str()
                .unwrap()
                .to_string();
            let java = std::fs::read(&self.java.java).unwrap();
            let release = b"JAVA_VERSION=\"25.0.1\"\nOS_ARCH=\"x86_64\"\n".to_vec();
            let mut files = serde_json::Map::new();
            files.insert("bin".into(), json!({"type": "directory"}));
            for (name, body, exec) in [("bin/java", &java, true), ("release", &release, false)] {
                let sha1 = sha1_hex(body);
                files.insert(
                    name.into(),
                    json!({"type": "file", "executable": exec, "downloads": {"raw": {
                        "url": format!("{PISTON_DATA}/v1/objects/{sha1}/{name}"),
                        "sha1": sha1, "size": body.len(),
                    }}}),
                );
                serve(
                    &self.server,
                    &format!("/v1/objects/{sha1}/{name}"),
                    body.clone(),
                )
                .await;
            }
            let manifest = json!({"files": files}).to_string();
            let mut index = serde_json::Map::new();
            index.insert(
                crate::java::runtime::platform_key().unwrap().to_string(),
                json!({component.clone(): [{
                    "manifest": {
                        "url": format!("{PISTON_META}/v1/packages/runtime/manifest.json"),
                        "sha1": sha1_hex(manifest.as_bytes()),
                        "size": manifest.len(),
                    },
                    "version": {"name": "25.0.1"},
                }]}),
            );
            serve(
                &self.server,
                "/v1/packages/runtime/manifest.json",
                manifest.into_bytes(),
            )
            .await;
            serve(
                &self.server,
                RUNTIME_INDEX_URL.strip_prefix(PISTON_META).unwrap(),
                Value::Object(index).to_string().into_bytes(),
            )
            .await;
            let bin = ManagedRuntimes::new(self.launcher.data.runtimes())
                .install(&self.launcher.downloader, &component, &None)
                .await
                .unwrap();
            // A freshly written executable can briefly fail with ETXTBSY while
            // another test thread forks; run it once until it execs cleanly.
            for _ in 0..50 {
                match std::process::Command::new(&bin).arg("--probe").status() {
                    Ok(_) => break,
                    Err(e) if e.raw_os_error() == Some(26) => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    Err(e) => panic!("fake managed java: {e}"),
                }
            }
        }

        fn vanilla(&self, mc: &str) -> Value {
            let mut v: Value =
                serde_json::from_str(include_str!("../../tests/fixtures/version_modern.json"))
                    .unwrap();
            v["id"] = json!(mc);
            let client = format!("client jar {mc}").into_bytes();
            let sha1 = sha1_hex(&client);
            v["downloads"]["client"] = json!({
                "url": format!("{PISTON_DATA}/v1/objects/{sha1}/client.jar"),
                "sha1": sha1, "size": client.len(),
            });
            v
        }

        /// Serves vanilla, Fabric and Forge metadata; the Fabric loader jar
        /// of `fail_fabric` (a loader version) answers 500.
        async fn serve_all(&self, fail_fabric: Option<&str>) {
            let server = &self.server;
            // Vanilla: manifest, version JSONs, client jars.
            let mut entries = Vec::new();
            for mc in MCS {
                let client = format!("client jar {mc}").into_bytes();
                serve(
                    server,
                    &format!("/v1/objects/{}/client.jar", sha1_hex(&client)),
                    client,
                )
                .await;
                let bytes = serde_json::to_vec(&self.vanilla(mc)).unwrap();
                let sha1 = sha1_hex(&bytes);
                serve(server, &format!("/v1/packages/{sha1}/{mc}.json"), bytes).await;
                entries.push(json!({
                    "id": mc, "type": "release", "sha1": sha1,
                    "url": format!("{PISTON_META}/v1/packages/{sha1}/{mc}.json"),
                    "releaseTime": "2026-01-01T00:00:00+00:00",
                }));
            }
            let manifest = json!({
                "latest": {"release": "1.21.1", "snapshot": "1.21.1"},
                "versions": entries,
            });
            serve(
                server,
                "/mc/game/version_manifest_v2.json",
                serde_json::to_vec(&manifest).unwrap(),
            )
            .await;

            // Fabric: version lists, profiles, loader jars.
            for mc in FABRIC_MCS {
                let list: Vec<Value> = FABRIC_VERSIONS
                    .iter()
                    .map(|v| json!({"loader": {"version": v, "stable": true}}))
                    .collect();
                serve(
                    server,
                    &format!("/v2/versions/loader/{mc}"),
                    serde_json::to_vec(&list).unwrap(),
                )
                .await;
                for ver in FABRIC_VERSIONS {
                    let (_, body) = fabric_lib(ver);
                    let profile = json!({
                        "id": fabric_id(ver, mc),
                        "inheritsFrom": mc,
                        "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
                        "libraries": [{
                            "name": format!("net.fabricmc:fabric-loader:{ver}"),
                            "url": "https://maven.fabricmc.net/",
                            "sha1": sha1_hex(&body), "size": body.len(),
                        }],
                    });
                    serve(
                        server,
                        &format!("/v2/versions/loader/{mc}/{ver}/profile/json"),
                        serde_json::to_vec(&profile).unwrap(),
                    )
                    .await;
                }
            }
            // An empty list: Fabric has nothing for 1.12.2.
            serve(server, "/v2/versions/loader/1.12.2", b"[]".to_vec()).await;
            for ver in FABRIC_VERSIONS {
                let (p, body) = fabric_lib(ver);
                let resp = if fail_fabric == Some(ver) {
                    ResponseTemplate::new(500)
                } else {
                    ResponseTemplate::new(200).set_body_bytes(body)
                };
                Mock::given(method("GET"))
                    .and(path(p))
                    .respond_with(resp)
                    .mount(server)
                    .await;
            }

            // Forge: version list, promotions, installers, processor tool.
            let xml = format!(
                "<metadata><versioning><versions>\
                 <version>1.12.2-{FORGE_LEGACY}</version>\
                 <version>1.20.1-{FORGE_MODERN}</version>\
                 </versions></versioning></metadata>"
            );
            serve(server, "/net/minecraftforge/forge/maven-metadata.xml", xml).await;
            let promos = json!({"promos": {
                "1.12.2-recommended": FORGE_LEGACY,
                "1.20.1-recommended": FORGE_MODERN,
            }});
            serve(
                server,
                "/net/minecraftforge/forge/promotions_slim.json",
                serde_json::to_vec(&promos).unwrap(),
            )
            .await;
            let dir = self.tmp.path();
            let tool_jar = dir.join("tool.jar");
            processor_jar(&tool_jar, "net.minecraftforge.binarypatcher.ConsoleTool");
            let tool = std::fs::read(&tool_jar).unwrap();
            serve(server, "/x/tool/1/tool-1.jar", tool.clone()).await;
            let modern = json!({
                "spec": 1,
                "minecraft": "1.20.1",
                "json": "/version.json",
                "data": {"PATCHED": {
                    "client": format!("[net.minecraftforge:forge:1.20.1-{FORGE_MODERN}:client]"),
                    "server": format!("[net.minecraftforge:forge:1.20.1-{FORGE_MODERN}:server]"),
                }},
                "processors": [{"sides": ["client"], "jar": "x:tool:1",
                    "args": ["--clean", "{MINECRAFT_JAR}", "--out", "{PATCHED}"]}],
                "libraries": [{"name": "x:tool:1", "downloads": {"artifact": {
                    "path": "x/tool/1/tool-1.jar",
                    "url": "https://maven.minecraftforge.net/x/tool/1/tool-1.jar",
                    "sha1": sha1_hex(&tool), "size": tool.len(),
                }}}],
            });
            let version = json!({
                "id": FORGE_MODERN_ID,
                "inheritsFrom": "1.20.1",
                "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
                "libraries": [],
            });
            let legacy = json!({
                "install": {
                    "path": format!("net.minecraftforge:forge:1.12.2-{FORGE_LEGACY}"),
                    "filePath": format!("forge-1.12.2-{FORGE_LEGACY}-universal.jar"),
                    "minecraft": "1.12.2",
                },
                "versionInfo": {
                    "id": FORGE_LEGACY_ID,
                    "inheritsFrom": "1.12.2",
                    "mainClass": "net.minecraft.launchwrapper.Launch",
                    "libraries": [{"name": format!("net.minecraftforge:forge:1.12.2-{FORGE_LEGACY}")}],
                },
            });
            let universal = format!("forge-1.12.2-{FORGE_LEGACY}-universal.jar");
            let (modern, version, legacy) =
                (modern.to_string(), version.to_string(), legacy.to_string());
            let modern_entries: [(&str, &[u8]); 2] = [
                ("install_profile.json", modern.as_bytes()),
                ("version.json", version.as_bytes()),
            ];
            let legacy_entries: [(&str, &[u8]); 2] = [
                ("install_profile.json", legacy.as_bytes()),
                (&universal, b"universal jar"),
            ];
            for (maven, entries) in [
                (format!("1.20.1-{FORGE_MODERN}"), &modern_entries),
                (format!("1.12.2-{FORGE_LEGACY}"), &legacy_entries),
            ] {
                let jar = dir.join(format!("installer-{maven}.jar"));
                write_zip(&jar, entries);
                let bytes = std::fs::read(&jar).unwrap();
                let at = format!("/net/minecraftforge/forge/{maven}/forge-{maven}-installer.jar");
                serve(server, &format!("{at}.sha1"), sha1_hex(&bytes)).await;
                serve(server, &at, bytes).await;
            }
        }

        /// Requests the mock server received for path `p`.
        async fn hits(&self, p: &str) -> usize {
            let all = self.server.received_requests().await.unwrap();
            all.iter().filter(|r| r.url.path() == p).count()
        }

        /// Every request the mock server received.
        async fn requests(&self) -> usize {
            self.server.received_requests().await.unwrap().len()
        }

        fn load(&self, id: &str) -> Instance {
            self.store.load(id).unwrap()
        }

        fn config_text(&self, id: &str) -> String {
            std::fs::read_to_string(
                self.tmp
                    .path()
                    .join("instances")
                    .join(id)
                    .join("instance.toml"),
            )
            .unwrap()
        }

        fn profile_json(&self, id: &str) -> std::path::PathBuf {
            self.launcher
                .data
                .versions()
                .join(id)
                .join(format!("{id}.json"))
        }

        async fn install(
            &self,
            id: &str,
            kind: LoaderKind,
            pick: VersionPick,
            replace: bool,
        ) -> Result<OpOutcome, LoaderError> {
            install_loader(&self.launcher, &self.store, id, kind, pick, replace, None).await
        }

        async fn set_mc(
            &self,
            id: &str,
            mc: &str,
            action: Option<LoaderAction>,
        ) -> Result<OpOutcome, LoaderError> {
            set_mc_version(&self.launcher, &self.store, id, mc, action, None).await
        }
    }

    fn exact(v: &str) -> VersionPick {
        VersionPick::Exact(v.to_string())
    }

    /// `(kind, version, profile)` of the instance's loader.
    fn loader_of(inst: &Instance) -> Option<(LoaderKind, String, Option<String>)> {
        inst.config
            .loader()
            .map(|(k, c)| (k, c.version.clone(), c.profile.clone()))
    }

    fn minecraft_only(mc: &str) -> Vec<Component> {
        vec![Component {
            uid: ComponentUid::Minecraft,
            version: mc.into(),
            profile: None,
        }]
    }

    #[tokio::test]
    async fn install_then_change_fabric_version() {
        let env = Env::new().await;
        let id = env.store.create("fab", "1.20.1").unwrap().id;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out = install_loader(
            &env.launcher,
            &env.store,
            &id,
            LoaderKind::Fabric,
            exact("0.16.10"),
            false,
            Some(tx),
        )
        .await
        .unwrap();
        let first = fabric_id("0.16.10", "1.20.1");
        assert_eq!(
            loader_of(&out.instance),
            Some((LoaderKind::Fabric, "0.16.10".into(), Some(first.clone())))
        );
        assert!(out.notices.is_empty());
        assert_eq!(env.load(&id), out.instance);
        assert!(env.profile_json(&first).is_file());
        let client = env.launcher.data.versions().join("1.20.1/1.20.1.jar");
        assert_eq!(std::fs::read(client).unwrap(), b"client jar 1.20.1");
        assert!(rx.try_recv().is_ok(), "progress is forwarded");

        // Same kind, other version: a version change, not a conflict.
        let out = env
            .install(&id, LoaderKind::Fabric, exact("0.16.9"), false)
            .await
            .unwrap();
        let second = fabric_id("0.16.9", "1.20.1");
        assert_eq!(
            loader_of(&out.instance),
            Some((LoaderKind::Fabric, "0.16.9".into(), Some(second.clone())))
        );
        assert!(out.notices.is_empty());
        assert_eq!(env.load(&id), out.instance);
        assert_eq!(out.instance.config.mc_version(), "1.20.1");
        assert!(env.profile_json(&second).is_file());

        // An unknown exact version changes nothing.
        let before = env.config_text(&id);
        let r = env
            .install(&id, LoaderKind::Fabric, exact("9.9.9"), false)
            .await;
        assert!(
            matches!(&r, Err(LoaderError::VersionNotFound { kind: LoaderKind::Fabric, version }) if version == "9.9.9"),
            "{r:?}"
        );
        assert_eq!(env.config_text(&id), before);
    }

    #[tokio::test]
    async fn kind_change_requires_confirmation() {
        let env = Env::new().await;
        let id = env.store.create("kc", "1.20.1").unwrap().id;
        env.install(&id, LoaderKind::Fabric, exact("0.16.10"), false)
            .await
            .unwrap();
        let before = env.config_text(&id);

        let r = env
            .install(&id, LoaderKind::Forge, VersionPick::Recommended, false)
            .await;
        assert!(
            matches!(&r, Err(LoaderError::Conflict { existing: LoaderKind::Fabric, existing_version }) if existing_version == "0.16.10"),
            "{r:?}"
        );
        assert_eq!(env.config_text(&id), before);
        assert!(
            env.java.runs().is_empty(),
            "nothing is installed on conflict"
        );

        let out = env
            .install(&id, LoaderKind::Forge, VersionPick::Recommended, true)
            .await
            .unwrap();
        assert_eq!(
            loader_of(&out.instance),
            Some((
                LoaderKind::Forge,
                FORGE_MODERN.into(),
                Some(FORGE_MODERN_ID.into())
            ))
        );
        assert_eq!(out.notices, vec![LoaderNotice::ModsMayBeIncompatible]);
        assert_eq!(env.load(&id), out.instance);
        assert_eq!(out.instance.config.components.len(), 2);
        // The processor ran with the managed runtime and made its output.
        let runs = env.java.runs();
        assert_eq!(runs.len(), 1, "{runs:?}");
        let patched = env.launcher.data.libraries().join(PATCHED);
        assert_eq!(std::fs::read(&patched).unwrap(), b"generated");
        assert!(env.profile_json(FORGE_MODERN_ID).is_file());
    }

    #[tokio::test]
    async fn failed_install_leaves_instance_unchanged() {
        let env = Env::new().await;
        let id = env.store.create("fail", "1.20.1").unwrap().id;
        env.install(&id, LoaderKind::Fabric, exact("0.16.9"), false)
            .await
            .unwrap();
        let before = env.load(&id);
        let before_text = env.config_text(&id);

        env.server.reset().await;
        env.serve_all(Some("0.16.10")).await;
        let r = env
            .install(&id, LoaderKind::Fabric, exact("0.16.10"), false)
            .await;
        assert!(r.is_err(), "{r:?}");
        assert_eq!(env.load(&id), before);
        assert_eq!(env.config_text(&id), before_text);
        assert!(!env.profile_json(&fabric_id("0.16.10", "1.20.1")).exists());

        // The network is back: the same install now succeeds.
        env.server.reset().await;
        env.serve_all(None).await;
        let out = env
            .install(&id, LoaderKind::Fabric, exact("0.16.10"), false)
            .await
            .unwrap();
        assert_eq!(
            loader_of(&out.instance),
            Some((
                LoaderKind::Fabric,
                "0.16.10".into(),
                Some(fabric_id("0.16.10", "1.20.1"))
            ))
        );
        assert_eq!(env.load(&id), out.instance);
    }

    #[tokio::test]
    async fn concurrent_ops_on_one_instance_are_busy() {
        let env = Env::new().await;
        let id = env.store.create("busy-op-unique", "1.20.1").unwrap().id;
        let before = env.load(&id);
        let guard = env.store.lock(&id).unwrap();
        let r = env
            .install(&id, LoaderKind::Fabric, exact("0.16.10"), false)
            .await;
        assert!(matches!(r, Err(LoaderError::Busy(_))), "{r:?}");
        assert!(matches!(
            remove_loader(&env.store, &id),
            Err(LoaderError::Busy(_))
        ));
        let r = env.set_mc(&id, "1.21.1", None).await;
        assert!(matches!(r, Err(LoaderError::Busy(_))), "{r:?}");
        assert_eq!(env.load(&id), before);
        drop(guard);
        env.install(&id, LoaderKind::Fabric, exact("0.16.10"), false)
            .await
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn ops_refuse_while_the_game_runs() {
        use crate::launch::running::FakeGame;
        use crate::locks::BusyKind;
        let env = Env::new().await;
        let id = env.store.create("running-op", "1.20.1").unwrap().id;
        env.install(&id, LoaderKind::Fabric, exact("0.16.10"), false)
            .await
            .unwrap();
        let before = env.load(&id);
        let requests = env.requests().await;
        let game = FakeGame::start(&before.dir, &id);
        let running = |r: Result<OpOutcome, LoaderError>| matches!(&r, Err(LoaderError::Busy(b)) if b.what == BusyKind::Running(id.clone()));
        assert!(running(
            env.install(&id, LoaderKind::Fabric, exact("0.16.9"), false)
                .await
        ));
        assert!(running(
            reinstall_loader(&env.launcher, &env.store, &id, None).await
        ));
        assert!(running(env.set_mc(&id, "1.21.1", None).await));
        assert!(running(remove_loader(&env.store, &id)));
        assert_eq!(env.load(&id), before, "nothing changed");
        assert_eq!(env.requests().await, requests, "nothing fetched");
        drop(game);
        remove_loader(&env.store, &id).unwrap();
    }

    #[tokio::test]
    async fn install_of_version_in_use_is_busy() {
        let env = Env::new().await;
        for (name, kind, pick, pid) in [
            (
                "use-fabric",
                LoaderKind::Fabric,
                exact("0.16.10"),
                fabric_id("0.16.10", "1.20.1"),
            ),
            (
                "use-forge",
                LoaderKind::Forge,
                VersionPick::Recommended,
                FORGE_MODERN_ID.to_string(),
            ),
        ] {
            let id = env.store.create(name, "1.20.1").unwrap().id;
            let before = env.load(&id);
            let held = env.launcher.data.lock_version_shared(&pid).unwrap();
            let r = env.install(&id, kind, pick, false).await;
            assert!(matches!(r, Err(LoaderError::Busy(_))), "{r:?}");
            assert!(!env.profile_json(&pid).exists());
            assert_eq!(env.load(&id), before);
            drop(held);
        }
    }

    #[tokio::test]
    async fn remove_loader_returns_to_vanilla() {
        let env = Env::new().await;
        let id = env.store.create("rm", "1.20.1").unwrap().id;
        env.install(&id, LoaderKind::Fabric, exact("0.16.10"), false)
            .await
            .unwrap();
        let out = remove_loader(&env.store, &id).unwrap();
        assert_eq!(out.instance.config.components, minecraft_only("1.20.1"));
        assert!(out.instance.config.loader().is_none());
        assert!(out.notices.is_empty());
        assert_eq!(env.load(&id), out.instance);
        // Removing from a vanilla instance is a no-op.
        let again = remove_loader(&env.store, &id).unwrap();
        assert_eq!(again.instance, out.instance);
        assert!(matches!(
            remove_loader(&env.store, "missing"),
            Err(LoaderError::Instance(_))
        ));
    }

    #[tokio::test]
    async fn set_mc_version_rules() {
        let env = Env::new().await;

        // No loader: just the Minecraft version changes.
        let id = env.store.create("vanilla", "1.20.1").unwrap().id;
        let out = env.set_mc(&id, "1.12.2", None).await.unwrap();
        assert_eq!(out.instance.config.components, minecraft_only("1.12.2"));
        assert_eq!(env.load(&id), out.instance);

        // Fabric: the new version's list has the loader version → kept,
        // reinstalled for the new Minecraft version.
        let id = env.store.create("fabric", "1.20.1").unwrap().id;
        env.install(&id, LoaderKind::Fabric, exact("0.16.10"), false)
            .await
            .unwrap();
        let out = env.set_mc(&id, "1.21.1", None).await.unwrap();
        assert_eq!(out.instance.config.mc_version(), "1.21.1");
        assert_eq!(
            loader_of(&out.instance),
            Some((
                LoaderKind::Fabric,
                "0.16.10".into(),
                Some(fabric_id("0.16.10", "1.21.1"))
            ))
        );
        assert!(out.notices.is_empty());
        assert_eq!(env.load(&id), out.instance);
        // Fabric has nothing for 1.12.2 → incompatible, nothing to replace with.
        let before = env.config_text(&id);
        let r = env.set_mc(&id, "1.12.2", None).await;
        assert!(
            matches!(&r, Err(LoaderError::Incompatible { kind: LoaderKind::Fabric, version, can_replace: false }) if version == "0.16.10"),
            "{r:?}"
        );
        assert_eq!(env.config_text(&id), before);

        // An explicit Remove always removes, even a compatible Fabric
        // loader, and needs no network.
        let id = env.store.create("fabric-rm", "1.20.1").unwrap().id;
        env.install(&id, LoaderKind::Fabric, exact("0.16.10"), false)
            .await
            .unwrap();
        let before = env.requests().await;
        let out = env
            .set_mc(&id, "1.21.1", Some(LoaderAction::Remove))
            .await
            .unwrap();
        assert_eq!(out.instance.config.components, minecraft_only("1.21.1"));
        assert_eq!(env.load(&id), out.instance);
        assert_eq!(env.requests().await, before, "Remove made requests");

        // Forge: always incompatible with another Minecraft version.
        let a = env.store.create("forge-a", "1.20.1").unwrap().id;
        let b = env.store.create("forge-b", "1.20.1").unwrap().id;
        for id in [&a, &b] {
            env.install(id, LoaderKind::Forge, VersionPick::Recommended, false)
                .await
                .unwrap();
        }
        let before = env.config_text(&a);
        let r = env.set_mc(&a, "1.12.2", None).await;
        assert!(
            matches!(&r, Err(LoaderError::Incompatible { kind: LoaderKind::Forge, version, can_replace: true }) if version == FORGE_MODERN),
            "{r:?}"
        );
        let r = env.set_mc(&a, "1.21.1", None).await;
        assert!(
            matches!(
                &r,
                Err(LoaderError::Incompatible {
                    kind: LoaderKind::Forge,
                    can_replace: false,
                    ..
                })
            ),
            "{r:?}"
        );
        assert_eq!(env.config_text(&a), before);

        let out = env
            .set_mc(&a, "1.12.2", Some(LoaderAction::Remove))
            .await
            .unwrap();
        assert_eq!(out.instance.config.components, minecraft_only("1.12.2"));
        assert_eq!(env.load(&a), out.instance);

        let out = env
            .set_mc(&b, "1.12.2", Some(LoaderAction::Replace))
            .await
            .unwrap();
        assert_eq!(out.instance.config.mc_version(), "1.12.2");
        assert_eq!(
            loader_of(&out.instance),
            Some((
                LoaderKind::Forge,
                FORGE_LEGACY.into(),
                Some(FORGE_LEGACY_ID.into())
            ))
        );
        assert_eq!(env.load(&b), out.instance);
        let universal = env.launcher.data.libraries().join(UNIVERSAL);
        assert_eq!(std::fs::read(universal).unwrap(), b"universal jar");
        assert!(env.profile_json(FORGE_LEGACY_ID).is_file());
    }

    fn mtime(p: &std::path::Path) -> std::time::SystemTime {
        std::fs::metadata(p).unwrap().modified().unwrap()
    }

    const FORGE_INSTALLER: &str =
        "/net/minecraftforge/forge/1.20.1-47.1.0/forge-1.20.1-47.1.0-installer.jar";

    #[tokio::test]
    async fn installed_forge_version_is_reused_by_another_instance() {
        let env = Env::new().await;
        let a = env.store.create("a", "1.20.1").unwrap().id;
        env.install(&a, LoaderKind::Forge, VersionPick::Recommended, false)
            .await
            .unwrap();
        assert_eq!(env.java.runs().len(), 1);
        let dir = env.launcher.data.versions().join(FORGE_MODERN_ID);
        let shared = [
            env.profile_json(FORGE_MODERN_ID),
            dir.join(crate::loader::forge::GENERATED_FILE),
            env.launcher.data.libraries().join(PATCHED),
        ];
        let stamps: Vec<_> = shared.iter().map(|p| mtime(p)).collect();
        let installer_hits = env.hits(FORGE_INSTALLER).await;

        // A second instance on the same version: nothing is fetched or run.
        let b = env.store.create("b", "1.20.1").unwrap().id;
        let out = env
            .install(&b, LoaderKind::Forge, VersionPick::Recommended, false)
            .await
            .unwrap();
        assert_eq!(
            loader_of(&out.instance),
            Some((
                LoaderKind::Forge,
                FORGE_MODERN.into(),
                Some(FORGE_MODERN_ID.into())
            ))
        );
        assert_eq!(env.java.runs().len(), 1, "a processor ran again");
        assert_eq!(env.hits(FORGE_INSTALLER).await, installer_hits);
        let after: Vec<_> = shared.iter().map(|p| mtime(p)).collect();
        assert_eq!(after, stamps, "shared files were rewritten");

        // A generated file is gone: that install is incomplete and is redone.
        std::fs::remove_file(&shared[2]).unwrap();
        let c = env.store.create("c", "1.20.1").unwrap().id;
        env.install(&c, LoaderKind::Forge, VersionPick::Recommended, false)
            .await
            .unwrap();
        assert_eq!(env.java.runs().len(), 2);
        assert_eq!(std::fs::read(&shared[2]).unwrap(), b"generated");
    }

    #[tokio::test]
    async fn installed_fabric_and_legacy_forge_are_reused() {
        let env = Env::new().await;
        let fabric_profile = "/v2/versions/loader/1.20.1/0.16.10/profile/json";
        let legacy_installer = format!(
            "/net/minecraftforge/forge/1.12.2-{FORGE_LEGACY}/forge-1.12.2-{FORGE_LEGACY}-installer.jar"
        );
        for name in ["one", "two"] {
            let id = env.store.create(name, "1.20.1").unwrap().id;
            env.install(&id, LoaderKind::Fabric, exact("0.16.10"), false)
                .await
                .unwrap();
            let id = env
                .store
                .create(&format!("{name}-old"), "1.12.2")
                .unwrap()
                .id;
            let out = env
                .install(&id, LoaderKind::Forge, VersionPick::Recommended, false)
                .await
                .unwrap();
            assert_eq!(
                loader_of(&out.instance),
                Some((
                    LoaderKind::Forge,
                    FORGE_LEGACY.into(),
                    Some(FORGE_LEGACY_ID.into())
                ))
            );
        }
        assert_eq!(env.hits(fabric_profile).await, 1);
        assert_eq!(env.hits(&legacy_installer).await, 1);

        // A missing library means the install is redone.
        let (lib, _) = fabric_lib("0.16.10");
        std::fs::remove_file(env.launcher.data.libraries().join(&lib[1..])).unwrap();
        let id = env.store.create("three", "1.20.1").unwrap().id;
        env.install(&id, LoaderKind::Fabric, exact("0.16.10"), false)
            .await
            .unwrap();
        assert_eq!(env.hits(fabric_profile).await, 2);
        assert!(env.launcher.data.libraries().join(&lib[1..]).is_file());
    }

    #[tokio::test]
    async fn reinstall_bypasses_short_circuit() {
        let env = Env::new().await;
        let id = env.store.create("a", "1.20.1").unwrap().id;
        env.install(&id, LoaderKind::Forge, VersionPick::Recommended, false)
            .await
            .unwrap();
        assert_eq!(env.java.runs().len(), 1);
        let out = reinstall_loader(&env.launcher, &env.store, &id, None)
            .await
            .unwrap();
        assert_eq!(
            env.java.runs().len(),
            2,
            "reinstall did not run the installer"
        );
        assert_eq!(
            loader_of(&out.instance),
            Some((
                LoaderKind::Forge,
                FORGE_MODERN.into(),
                Some(FORGE_MODERN_ID.into())
            ))
        );
    }

    #[tokio::test]
    async fn reinstall_fabric_refetches_profile() {
        let env = Env::new().await;
        let id = env.store.create("f", "1.20.1").unwrap().id;
        env.install(&id, LoaderKind::Fabric, exact("0.16.10"), false)
            .await
            .unwrap();
        let profile = "/v2/versions/loader/1.20.1/0.16.10/profile/json";
        assert_eq!(env.hits(profile).await, 1);
        reinstall_loader(&env.launcher, &env.store, &id, None)
            .await
            .unwrap();
        assert_eq!(env.hits(profile).await, 2);
    }

    #[tokio::test]
    async fn reinstall_is_busy_while_version_is_in_use() {
        let env = Env::new().await;
        let id = env.store.create("a", "1.20.1").unwrap().id;
        env.install(&id, LoaderKind::Forge, VersionPick::Recommended, false)
            .await
            .unwrap();
        let config_before = env.config_text(&id);
        let profile_path = env.profile_json(FORGE_MODERN_ID);
        let profile_before = std::fs::read(&profile_path).unwrap();
        let runs = env.java.runs().len();
        {
            let _shared = env
                .launcher
                .data
                .lock_version_shared(FORGE_MODERN_ID)
                .unwrap();
            let err = reinstall_loader(&env.launcher, &env.store, &id, None)
                .await
                .unwrap_err();
            assert!(matches!(err, LoaderError::Busy(_)), "{err:?}");
        }
        assert_eq!(env.config_text(&id), config_before);
        assert_eq!(std::fs::read(&profile_path).unwrap(), profile_before);
        assert_eq!(env.java.runs().len(), runs);
    }

    #[tokio::test]
    async fn reinstall_without_loader_errors() {
        let env = Env::new().await;
        let id = env.store.create("v", "1.20.1").unwrap().id;
        let err = reinstall_loader(&env.launcher, &env.store, &id, None)
            .await
            .unwrap_err();
        assert!(matches!(err, LoaderError::NoLoader));
    }

    #[tokio::test]
    async fn metadata_progress_comes_first() {
        let env = Env::new().await;
        let id = env.store.create("p", "1.20.1").unwrap().id;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        install_loader(
            &env.launcher,
            &env.store,
            &id,
            LoaderKind::Forge,
            VersionPick::Recommended,
            false,
            Some(tx),
        )
        .await
        .unwrap();
        let mut events = Vec::new();
        while let Ok(p) = rx.try_recv() {
            events.push(p);
        }
        let meta = |done| TaskProgress {
            phase: Phase::FetchMetadata,
            done,
            total: 1,
            unit: Unit::Items,
        };
        assert_eq!(events[..2], [meta(0), meta(1)], "{events:?}");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        set_mc_version(
            &env.launcher,
            &env.store,
            &id,
            "1.12.2",
            Some(LoaderAction::Replace),
            Some(tx),
        )
        .await
        .unwrap();
        let mut events = Vec::new();
        while let Ok(p) = rx.try_recv() {
            events.push(p);
        }
        assert_eq!(events[..2], [meta(0), meta(1)], "{events:?}");
    }

    #[test]
    fn op_messages_are_localized() {
        let _locale = locale_lock();
        let conflict = LoaderError::Conflict {
            existing: LoaderKind::Neoforge,
            existing_version: "21.1.77".into(),
        };
        let incompatible = |can_replace| LoaderError::Incompatible {
            kind: LoaderKind::Forge,
            version: "47.1.0".into(),
            can_replace,
        };
        let notice = LoaderNotice::ModsMayBeIncompatible;
        set_language(Language::ZhCn);
        let zh = [
            conflict.to_string(),
            incompatible(true).to_string(),
            incompatible(false).to_string(),
            notice.to_string(),
        ];
        set_language(Language::En);
        let en = [
            conflict.to_string(),
            incompatible(true).to_string(),
            incompatible(false).to_string(),
            notice.to_string(),
        ];
        for s in &zh {
            assert!(
                !s.is_ascii() && !s.contains("notices.") && !s.contains("errors."),
                "{s}"
            );
        }
        for s in &en {
            assert!(
                s.is_ascii() && !s.contains("notices.") && !s.contains("errors."),
                "{s}"
            );
        }
        assert!(
            en[0].contains("NeoForge") && en[0].contains("21.1.77"),
            "{}",
            en[0]
        );
        assert!(
            zh[1].contains("Forge") && zh[1].contains("47.1.0"),
            "{}",
            zh[1]
        );
        assert_ne!(en[1], en[2]);
    }
}
