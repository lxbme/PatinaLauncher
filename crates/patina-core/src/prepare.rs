//! Launch orchestration: version metadata, Java, memory, downloads, command.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use crate::config::GlobalConfig;
use crate::download::{
    DownloadError, DownloadJob, Downloader, Mirror, PROBE_URL, Progress, SPEEDTEST_TIMEOUT,
    pick_fastest, resolve_choice, sha1_file,
};
use crate::fsutil::{is_safe_name, safe_join};
use crate::fsutil::{replace_atomic, write_atomic};
use crate::instance::{Instance, effective_java};
use crate::java::{JavaInstall, ManagedRuntimes, RuntimeError, probe};
use crate::jvm::{
    GcPreset, JvmArgsError, MemoryCheck, check_available_memory, clamp_for_arch, split_args,
    validate_extra_jvm_args,
};
use crate::launch::{
    LAUNCHER_NAME, LAUNCHER_VERSION, LaunchAccount, LaunchCommand, LaunchSpec, build_command,
};
use crate::loader::LoaderKind;
use crate::loader::forge::GENERATED_FILE;
use crate::meta::{
    ArgValue, MANIFEST_URL, OneOrMany, PartialVersionJson, RuleEnv, VersionJson, VersionManifest,
    merge_versions,
};
use crate::paths::DataDirs;
use crate::platform;
use crate::resolve::{
    AssetIndex, asset_jobs, asset_layout, extract_natives, materialize_legacy, resolve_libraries,
};

/// Max heap used when physical memory cannot be read.
const UNKNOWN_TOTAL_MAX_MEM_MIB: u32 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareWarning {
    LowMemory {
        requested_mib: u32,
        available_mib: u64,
    },
    /// The GC preset needs a newer Java; launched without GC arguments.
    GcPresetIgnored { preset: GcPreset, java_major: u32 },
}

impl std::fmt::Display for PrepareWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrepareWarning::LowMemory {
                requested_mib,
                available_mib,
            } => f.write_str(&rust_i18n::t!(
                "warnings.low_memory",
                requested = requested_mib,
                available = available_mib
            )),
            PrepareWarning::GcPresetIgnored { preset, java_major } => f.write_str(&rust_i18n::t!(
                "warnings.gc_preset_ignored",
                preset = preset.display_name(),
                required = preset.min_java_major(),
                found = java_major
            )),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("{}", rust_i18n::t!("errors.prepare.unknown_version", version = .0))]
    UnknownVersion(String),
    #[error("{}", rust_i18n::t!("errors.prepare.java_too_old", required = .required, found = .found))]
    JavaTooOld { required: u32, found: u32 },
    #[error("{}", rust_i18n::t!("errors.prepare.java_not_found", path = .0.display()))]
    JavaNotFound(PathBuf),
    #[error("{}", rust_i18n::t!("errors.prepare.invalid_jvm_args", error = .0))]
    InvalidJvmArgs(#[from] JvmArgsError),
    #[error("{}", rust_i18n::t!("errors.prepare.runtime", error = .0))]
    Runtime(#[from] RuntimeError),
    #[error("{}", rust_i18n::t!("errors.prepare.download", error = .0))]
    Download(#[from] DownloadError),
    #[error("{}", rust_i18n::t!("errors.prepare.io", error = .0))]
    Io(#[from] io::Error),
    #[error("{}", rust_i18n::t!("errors.prepare.json", error = .0))]
    Json(#[from] serde_json::Error),
    /// A file name from downloaded metadata would escape its directory.
    #[error("{}", rust_i18n::t!("errors.prepare.unsafe_metadata", name = .0))]
    UnsafeMetadata(String),
    /// The loader profile or a file its install generated is gone.
    #[error("{}", rust_i18n::t!("errors.prepare.loader_needs_reinstall", kind = .kind.display_name(), version = .version))]
    LoaderNeedsReinstall { kind: LoaderKind, version: String },
}

#[derive(Debug)]
pub struct Prepared {
    pub command: LaunchCommand,
    pub warnings: Vec<PrepareWarning>,
}

pub struct Launcher {
    pub data: DataDirs,
    pub downloader: Downloader,
}

/// The shared HTTP client: bounded connect/read stalls, no overall timeout
/// (large files must be allowed to take as long as they need).
pub fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .user_agent(format!("{LAUNCHER_NAME}/{LAUNCHER_VERSION}"))
        .build()
}

/// Download sources for one run; see [`Downloader::with_fallback`].
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSource {
    pub mirror: Mirror,
    /// Tried after `mirror` and the original URL. Always the other built-in
    /// mirror: every source setting is a preference, not a lock.
    pub fallback: Option<Mirror>,
}

impl ResolvedSource {
    pub fn downloader(self, client: reqwest::Client) -> Downloader {
        Downloader::new(client, self.mirror).with_fallback(self.fallback)
    }
}

/// Source from the config. With Auto and no stored pick, speed-tests the
/// built-in mirrors and stores the winner in `cfg.download.auto_pick`; if
/// every probe fails, uses official for this run and stores nothing, so the
/// next run tests again. Whatever the setting, the other built-in mirror is the
/// fallback.
pub async fn resolve_mirror(client: &reqwest::Client, cfg: &mut GlobalConfig) -> ResolvedSource {
    let builtin = [Mirror::official(), Mirror::bmclapi()];
    resolve_mirror_with(client, cfg, &builtin, PROBE_URL, SPEEDTEST_TIMEOUT).await
}

async fn resolve_mirror_with(
    client: &reqwest::Client,
    cfg: &mut GlobalConfig,
    candidates: &[Mirror],
    probe_url: &str,
    timeout: Duration,
) -> ResolvedSource {
    let mirror = match resolve_choice(&cfg.download) {
        Some(m) => m,
        None => match pick_fastest(client, candidates, probe_url, timeout).await {
            Some(m) => {
                cfg.download.auto_pick = Some(m.id.clone());
                m
            }
            None => Mirror::official(),
        },
    };
    let fallback = other_builtin(&mirror.id);
    ResolvedSource { mirror, fallback }
}

fn other_builtin(id: &str) -> Option<Mirror> {
    match id {
        "official" => Some(Mirror::bmclapi()),
        "bmclapi" => Some(Mirror::official()),
        _ => None,
    }
}

/// `name` from downloaded metadata, if it is a single safe path component.
fn checked_name(name: &str) -> Result<&str, PrepareError> {
    if is_safe_name(name) {
        Ok(name)
    } else {
        Err(PrepareError::UnsafeMetadata(name.to_string()))
    }
}

fn read_cached(path: &Path) -> Option<VersionJson> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

impl Launcher {
    pub async fn manifest(&self) -> Result<VersionManifest, PrepareError> {
        let bytes = self.downloader.fetch_bytes(MANIFEST_URL, None).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Version JSON cached at `versions/<id>/<id>.json`. The cache is reused
    /// when its sha1 matches the manifest, or when the manifest is unreachable.
    pub async fn ensure_version(&self, id: &str) -> Result<VersionJson, PrepareError> {
        if !is_safe_name(id) {
            return Err(PrepareError::UnknownVersion(id.to_string()));
        }
        let path = self.data.versions().join(id).join(format!("{id}.json"));
        let manifest = match self.manifest().await {
            Ok(m) => m,
            Err(e) => return read_cached(&path).ok_or(e),
        };
        let entry = manifest
            .find(id)
            .ok_or_else(|| PrepareError::UnknownVersion(id.to_string()))?;
        if sha1_file(&path).is_ok_and(|h| h.eq_ignore_ascii_case(&entry.sha1))
            && let Some(v) = read_cached(&path)
        {
            return Ok(v);
        }
        let bytes = self
            .downloader
            .fetch_bytes(&entry.url, Some(&entry.sha1))
            .await?;
        let v: VersionJson = serde_json::from_slice(&bytes)?;
        write_atomic(&path, &bytes)?;
        Ok(v)
    }

    /// The vanilla client jar at `versions/<mc>/<mc>.jar` (keyed on the
    /// requested id, like [`Launcher::ensure_version`]), downloaded and
    /// verified unless a matching file is already there. Returns its
    /// absolute path.
    pub(crate) async fn ensure_client_jar(
        &self,
        mc: &str,
        version: &VersionJson,
    ) -> Result<PathBuf, PrepareError> {
        let job = self.client_jar_job(mc, version)?;
        let dest = job.dest.clone();
        self.downloader.run(vec![job], None).await?;
        Ok(dest)
    }

    /// Download job for the vanilla client jar at `versions/<mc>/<mc>.jar`
    /// (absolute destination).
    fn client_jar_job(&self, mc: &str, version: &VersionJson) -> Result<DownloadJob, PrepareError> {
        let mc = checked_name(mc)?;
        let dest = std::path::absolute(self.data.versions().join(mc).join(format!("{mc}.jar")))?;
        let client = &version.downloads.client;
        Ok(DownloadJob {
            url: client.url.clone(),
            dest,
            sha1: Some(client.sha1.clone()),
            size: Some(client.size),
        })
    }

    /// Vanilla `vanilla` merged with the instance's loader profile (if any).
    /// A missing or unreadable profile means the loader must be reinstalled.
    async fn merged_version(
        &self,
        inst: &Instance,
        vanilla: VersionJson,
    ) -> Result<VersionJson, PrepareError> {
        let Some((kind, comp)) = inst.config.loader() else {
            return Ok(vanilla);
        };
        let reinstall = || PrepareError::LoaderNeedsReinstall {
            kind,
            version: comp.version.clone(),
        };
        let id = comp.profile.as_deref().ok_or_else(reinstall)?;
        let id = checked_name(id)?;
        let dir = self.data.versions().join(id);
        let bytes = tokio::fs::read(dir.join(format!("{id}.json")))
            .await
            .map_err(|_| reinstall())?;
        let profile: PartialVersionJson =
            serde_json::from_slice(&bytes).map_err(|_| reinstall())?;
        let merged = merge_versions(&vanilla, &profile);

        // Modern Forge/NeoForge installs (1.17+) record the processor-generated
        // libraries in GENERATED_FILE as their last install step. Such a
        // profile is recognised by `${library_directory}` in its JVM arguments
        // (module-path launch); without the file the install is incomplete.
        let list = dir.join(GENERATED_FILE);
        let is_modern = kind != LoaderKind::Fabric && uses_library_directory(&profile);
        match std::fs::read(&list) {
            Ok(b) => {
                let rels: Vec<String> = serde_json::from_slice(&b).map_err(|_| reinstall())?;
                for rel in rels {
                    let ok = safe_join(&self.data.libraries(), &rel).is_some_and(|p| p.is_file());
                    if !ok {
                        return Err(reinstall());
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && !is_modern => {}
            Err(_) => return Err(reinstall()),
        }
        Ok(merged)
    }

    /// `java_path`, or (when `None`) the managed runtime `version` asks for,
    /// installed on first use.
    pub(crate) async fn select_java(
        &self,
        java_path: Option<&Path>,
        version: &VersionJson,
    ) -> Result<JavaInstall, PrepareError> {
        let req = version.java_requirement();
        match java_path {
            Some(p) => {
                let java = probe_blocking(p.to_path_buf()).await?;
                if java.major < req.major_version {
                    return Err(PrepareError::JavaTooOld {
                        required: req.major_version,
                        found: java.major,
                    });
                }
                Ok(java)
            }
            None => {
                let runtimes = ManagedRuntimes::new(self.data.runtimes());
                let bin = if runtimes.is_installed(&req.component) {
                    runtimes.java_bin(&req.component)
                } else {
                    runtimes.install(&self.downloader, &req.component).await?
                };
                probe_blocking(bin).await
            }
        }
    }

    pub async fn prepare(
        &self,
        inst: &Instance,
        global: &GlobalConfig,
        account: &LaunchAccount,
        progress: Option<UnboundedSender<Progress>>,
    ) -> Result<Prepared, PrepareError> {
        // The game runs with cwd = game dir, so every path must be absolute.
        let data = DataDirs::new(std::path::absolute(self.data.root())?);
        let game_dir = std::path::absolute(inst.game_dir())?;
        let natives_dir = std::path::absolute(inst.natives_dir())?;
        let mut warnings = Vec::new();

        // 1. Settings and version metadata.
        let js = effective_java(&global.java, &inst.config);
        validate_extra_jvm_args(&js.extra_jvm_args)?;
        let extra_jvm_args = split_args(&js.extra_jvm_args);
        // The client jar is keyed on the requested id, the same id
        // `ensure_version` caches the JSON under (may differ from `version.id`).
        let jar_id = inst.config.mc_version();
        let vanilla = self.ensure_version(jar_id).await?;
        checked_name(&vanilla.id)?;
        let version = self.merged_version(inst, vanilla).await?;
        checked_name(&version.id)?;

        // 2. Java.
        let java = self.select_java(js.java_path.as_deref(), &version).await?;

        // 3. Memory.
        let max = match (js.max_mem_mib, platform::total_memory_mib()) {
            (Some(m), _) => m,
            (None, Some(total)) => js.effective_max_mem(total),
            (None, None) => UNKNOWN_TOTAL_MAX_MEM_MIB,
        };
        let max_mem_mib = clamp_for_arch(max, java.is_64bit);
        let min_mem_mib = js.min_mem_mib.min(max_mem_mib);
        if js.low_mem_warning
            && let Some(available) = platform::available_memory_mib()
            && let MemoryCheck::Low {
                requested_mib,
                available_mib,
            } = check_available_memory(min_mem_mib, max_mem_mib, available)
        {
            warnings.push(PrepareWarning::LowMemory {
                requested_mib,
                available_mib,
            });
        }

        // 4. GC arguments.
        let gc_args = if js.gc_preset.is_compatible(java.major) {
            js.gc_preset.args(java.major)
        } else {
            let w = PrepareWarning::GcPresetIgnored {
                preset: js.gc_preset,
                java_major: java.major,
            };
            warnings.push(w);
            Vec::new()
        };

        // 5. Downloads. The asset index must be on disk before its objects are known.
        let assets_dir = data.assets();
        let ai = &version.asset_index;
        let index_file = format!("{}.json", checked_name(&ai.id)?);
        let index_path = safe_join(&assets_dir.join("indexes"), &index_file)
            .ok_or_else(|| PrepareError::UnsafeMetadata(ai.id.clone()))?;
        self.downloader
            .run(
                vec![DownloadJob {
                    url: ai.url.clone(),
                    dest: index_path.clone(),
                    sha1: Some(ai.sha1.clone()),
                    size: Some(ai.size),
                }],
                None,
            )
            .await?;
        let index: AssetIndex = serde_json::from_slice(&tokio::fs::read(&index_path).await?)?;

        let env = RuleEnv::current();
        let libs = resolve_libraries(&version.libraries, &env, &data.libraries());
        // Libraries a loader install generated are never downloaded.
        if let Some((kind, comp)) = inst.config.loader()
            && libs.generated.iter().any(|p| !p.is_file())
        {
            return Err(PrepareError::LoaderNeedsReinstall {
                kind,
                version: comp.version.clone(),
            });
        }
        let mut jobs = libs.jobs.clone();
        jobs.push(self.client_jar_job(jar_id, &version)?);
        jobs.extend(asset_jobs(&index, &assets_dir));
        let logging = version.logging.as_ref().and_then(|l| l.client.as_ref());
        let logging_config = match logging {
            Some(l) if is_safe_name(&l.file.id) => {
                let dest = assets_dir.join("log_configs").join(&l.file.id);
                jobs.push(DownloadJob {
                    url: l.file.url.clone(),
                    dest: dest.clone(),
                    sha1: Some(l.file.sha1.clone()),
                    size: Some(l.file.size),
                });
                Some(dest)
            }
            _ => None,
        };
        self.downloader.run(jobs, progress).await?;
        let vanilla_jar = self.client_jar_job(jar_id, &version)?.dest;
        let client_jar = if inst.config.loader().is_some() {
            let (versions, id) = (data.versions(), version.id.clone());
            let sha1 = version.downloads.client.sha1.clone();
            tokio::task::spawn_blocking(move || {
                loader_client_jar(&vanilla_jar, &versions, &id, &sha1)
            })
            .await
            .map_err(io::Error::other)??
        } else {
            vanilla_jar
        };

        // 6. Natives, legacy asset layout, game directory.
        extract_natives(&libs.natives, &natives_dir)?;
        let layout = asset_layout(&index, &ai.id, &assets_dir, &game_dir);
        materialize_legacy(&index, &assets_dir, &layout)?;
        std::fs::create_dir_all(&game_dir)?;

        // 7. Command line.
        let mut classpath = libs.classpath;
        classpath.push(client_jar);
        let command = build_command(&LaunchSpec {
            version: &version,
            data: &data,
            game_dir: &game_dir,
            natives_dir: &natives_dir,
            classpath: &classpath,
            asset_layout: &layout,
            java: &java,
            account,
            min_mem_mib,
            max_mem_mib,
            gc_args: &gc_args,
            extra_jvm_args: &extra_jvm_args,
            logging_config: logging_config.as_deref(),
            env: &env,
        });
        Ok(Prepared { command, warnings })
    }
}

/// The client jar a loader profile `id` launches on: a copy of the verified
/// vanilla jar at `versions/<id>/<id>.jar`, as the official launcher makes.
/// NeoForge and Forge 1.17+ rely on that name: `-DignoreList=...,${version_name}.jar`
/// keeps the classpath entry with that file name out of the module layer,
/// and a jar named after the Minecraft version would clash with the
/// patched `minecraft` module. A copy whose SHA-1 differs is replaced.
/// `id` must already be a safe name.
fn loader_client_jar(vanilla: &Path, versions: &Path, id: &str, sha1: &str) -> io::Result<PathBuf> {
    let dir = versions.join(id);
    let dest = dir.join(format!("{id}.jar"));
    if sha1_file(&dest).is_ok_and(|h| h.eq_ignore_ascii_case(sha1)) {
        return Ok(dest);
    }
    // Two launches of instances on one loader may copy at once.
    replace_atomic(&dest, |tmp| std::fs::copy(vanilla, tmp).map(drop))?;
    Ok(dest)
}

/// Whether a loader profile's JVM arguments use `${library_directory}`.
fn uses_library_directory(profile: &PartialVersionJson) -> bool {
    let has = |s: &String| s.contains("${library_directory}");
    profile
        .arguments
        .iter()
        .flat_map(|a| &a.jvm)
        .any(|a| match a {
            ArgValue::Plain(s) => has(s),
            ArgValue::Conditional { value, .. } => match value {
                OneOrMany::One(s) => has(s),
                OneOrMany::Many(v) => v.iter().any(has),
            },
        })
}

async fn probe_blocking(bin: PathBuf) -> Result<JavaInstall, PrepareError> {
    let p = bin.clone();
    tokio::task::spawn_blocking(move || probe(&p))
        .await
        .ok()
        .flatten()
        .ok_or(PrepareError::JavaNotFound(bin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_ids() {
        assert!(checked_name("1.8.9").is_ok());
        assert!(checked_name("26.3").is_ok());
        for bad in ["", "../x", "a/b", ".", "..", "/abs"] {
            assert!(
                matches!(checked_name(bad), Err(PrepareError::UnsafeMetadata(n)) if n == bad),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn concurrent_loader_client_jar_copies_all_succeed() {
        let tmp = tempfile::tempdir().unwrap();
        let vanilla = tmp.path().join("vanilla.jar");
        let body = vec![7u8; 256 * 1024];
        std::fs::write(&vanilla, &body).unwrap();
        let sha1 = sha1_file(&vanilla).unwrap();
        let versions = tmp.path().join("versions");
        let barrier = std::sync::Barrier::new(16);
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        loader_client_jar(&vanilla, &versions, "fabric-x", &sha1)
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap().unwrap();
            }
        });
        let dir = versions.join("fabric-x");
        assert_eq!(std::fs::read(dir.join("fabric-x.jar")).unwrap(), body);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn unsafe_metadata_message_is_localized() {
        use crate::i18n::{Language, locale_lock, set_language};
        let _locale = locale_lock();
        let e = PrepareError::UnsafeMetadata("../evil".into());
        set_language(Language::En);
        assert!(e.to_string().contains("../evil") && e.to_string().is_ascii());
        set_language(Language::ZhCn);
        assert!(e.to_string().contains("../evil") && e.to_string().contains("不安全"));
    }

    #[test]
    fn reinstall_error_is_localized() {
        use crate::i18n::{Language, locale_lock, set_language};
        let _locale = locale_lock();
        let e = PrepareError::LoaderNeedsReinstall {
            kind: LoaderKind::Fabric,
            version: "0.16.10".into(),
        };
        set_language(Language::En);
        assert!(e.to_string().contains("Fabric") && e.to_string().contains("0.16.10"));
        set_language(Language::ZhCn);
        assert!(e.to_string().contains("重新安装") && e.to_string().contains("0.16.10"));
    }

    #[test]
    fn warnings_render_with_values() {
        use crate::i18n::{Language, locale_lock, set_language};
        let _locale = locale_lock();
        set_language(Language::ZhCn);
        let w = PrepareWarning::GcPresetIgnored {
            preset: GcPreset::GenerationalZgc,
            java_major: 8,
        };
        assert!(w.to_string().contains("Java 21"));
        assert!(w.to_string().contains("分代 ZGC"), "{w}");
        set_language(Language::En);
        assert!(w.to_string().contains("Generational ZGC"), "{w}");
        assert!(!w.to_string().contains("GenerationalZgc"), "{w}");
        set_language(Language::ZhCn);
        let w = PrepareWarning::LowMemory {
            requested_mib: 4096,
            available_mib: 1000,
        };
        assert!(w.to_string().contains("4096"));
        set_language(Language::En);
        assert!(w.to_string().contains("Not enough free memory"));
        set_language(Language::ZhCn);
    }

    #[tokio::test]
    async fn resolve_mirror_respects_explicit_choice() {
        let mut cfg = GlobalConfig::default();
        cfg.download.source = crate::config::SourceChoice::Bmclapi;
        let src = resolve_mirror(&reqwest::Client::new(), &mut cfg).await;
        assert_eq!(src.mirror.id, "bmclapi");
        assert_eq!(
            src.fallback.as_ref().map(|m| m.id.as_str()),
            Some("official")
        );
        assert_eq!(cfg.download.auto_pick, None);

        // Explicit official is a preference: bmclapi is the fallback.
        cfg.download.source = crate::config::SourceChoice::Official;
        let src = resolve_mirror(&reqwest::Client::new(), &mut cfg).await;
        assert_eq!(src.mirror.id, "official");
        assert_eq!(
            src.fallback.as_ref().map(|m| m.id.as_str()),
            Some("bmclapi")
        );
        let d = src.downloader(reqwest::Client::new());
        assert!(d.fallback_mirror().is_some());
    }

    #[tokio::test]
    async fn auto_with_stored_pick_falls_back_to_the_other_mirror() {
        let mut cfg = GlobalConfig::default();
        cfg.download.auto_pick = Some("official".into());
        let src = resolve_mirror(&reqwest::Client::new(), &mut cfg).await;
        assert_eq!(src.mirror.id, "official");
        assert_eq!(
            src.fallback.as_ref().map(|m| m.id.as_str()),
            Some("bmclapi")
        );
        cfg.download.auto_pick = Some("bmclapi".into());
        let src = resolve_mirror(&reqwest::Client::new(), &mut cfg).await;
        assert_eq!(src.mirror.id, "bmclapi");
        assert_eq!(
            src.fallback.as_ref().map(|m| m.id.as_str()),
            Some("official")
        );
    }

    #[tokio::test]
    async fn all_failed_speedtest_uses_official_without_persisting() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let probe = "https://probe.invalid/probe";
        let cands = ["official", "bmclapi"].map(|id| Mirror {
            id: id.into(),
            rules: vec![("https://probe.invalid".into(), server.uri())],
        });
        let mut cfg = GlobalConfig::default();
        let before = cfg.clone();
        let c = reqwest::Client::new();
        let src = resolve_mirror_with(&c, &mut cfg, &cands, probe, Duration::from_secs(2)).await;
        assert_eq!(src.mirror, Mirror::official());
        assert_eq!(src.fallback, Some(Mirror::bmclapi()));
        // Callers persist only when the config changed, so nothing is written.
        assert_eq!(cfg, before, "an all-fail speed test must not be stored");
    }
}
