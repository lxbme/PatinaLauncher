//! [`Core`]: patina-core's services, shared by every view.

use std::sync::{Arc, PoisonError, RwLock as StdRwLock};

use futures::FutureExt;
use futures::future::BoxFuture;
use gpui_kit::{App, Global, Task};
use patina_core::accounts::{AccountError, AccountStore};
use patina_core::config::{ConfigError, GlobalConfig};
use patina_core::download::{Mirror, resolve_choice};
use patina_core::instance::InstanceStore;
use patina_core::loader::{LoaderError, LoaderKind, LoaderVersion, list_versions};
use patina_core::meta::VersionManifest;
use patina_core::paths::DataDirs;
use patina_core::prepare::{Launcher, ResolvedSource, http_client, resolve_mirror};
use rust_i18n::t;
use tokio::runtime::Handle;
use tokio::sync::{Mutex, OwnedRwLockWriteGuard, RwLock};

use super::task::{BridgeError, run, run_blocking};

/// Why a file the core reads at startup is not in memory.
#[derive(Debug)]
pub enum LoadError<E> {
    /// Reading it never finished (it panicked, or the app is quitting). What
    /// is in memory is a placeholder, not the file.
    NotLoaded,
    /// The file could not be read or parsed; it was left alone.
    Failed(Arc<E>),
}

impl<E> Clone for LoadError<E> {
    fn clone(&self) -> Self {
        match self {
            LoadError::NotLoaded => LoadError::NotLoaded,
            LoadError::Failed(e) => LoadError::Failed(e.clone()),
        }
    }
}

impl<E: std::fmt::Display> std::fmt::Display for LoadError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::NotLoaded => f.write_str(&t!("core_bridge.not_loaded")),
            LoadError::Failed(e) => e.fmt(f),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for LoadError<E> {}

/// `config.toml` as loaded at startup. With a `load_error`, `config` holds
/// the defaults, not the file.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: GlobalConfig,
    pub load_error: Option<LoadError<ConfigError>>,
}

/// The account store as loaded at startup, or why it is not available (the
/// file is then left alone).
pub type LoadedAccounts = Result<AccountStore, LoadError<AccountError>>;

/// Why the core services could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitError {
    /// The HTTP client (its TLS setup) could not be built.
    HttpClient(String),
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InitError::HttpClient(e) => f.write_str(&t!("core_bridge.http_client", error = e)),
        }
    }
}

impl std::error::Error for InitError {}

/// patina-core's services and the app's tokio runtime, as a gpui global.
///
/// The config and the accounts are reached only through
/// [`Core::read_config`], [`Core::update_config`] and [`Core::with_accounts`],
/// which run on the runtime. Their locks are tokio locks, held from the
/// moment the `Core` exists until the files have been read, so nothing sees
/// the placeholders by mistake.
///
/// The launcher sits behind a std lock around an `Arc`, held only to clone or
/// replace the `Arc`: work in flight keeps the launcher it started with, and
/// a new download source replaces the whole launcher ([`Core::set_source`]).
#[derive(Clone)]
pub struct Core {
    pub rt: Handle,
    pub data: DataDirs,
    pub store: InstanceStore,
    /// Version lists, injectable for UI tests.
    pub catalog: Arc<dyn Catalog>,
    config: Arc<RwLock<LoadedConfig>>,
    launcher: Arc<StdRwLock<Arc<Launcher>>>,
    accounts: Arc<Mutex<LoadedAccounts>>,
    startup: Startup,
}

impl Global for Core {}

/// A client that cannot reach any host: every request goes through a
/// proxy at a closed local port and fails at once. Whatever the download
/// source or the original URLs, an offline core never touches the network.
fn unreachable_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        // Port 1 is privileged, so nothing a user runs listens there.
        .proxy(reqwest::Proxy::all("http://127.0.0.1:1")?)
        .build()
}

/// Whether the core may speed-test the download sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Startup {
    /// With `Auto` and no stored pick, speed-test the mirrors in the
    /// background and store the winner.
    SpeedTest,
    /// Never touch the network (tests): no speed test, and a client that
    /// cannot reach any host.
    Offline,
}

impl Core {
    /// Creates the core services without blocking: the config and the
    /// accounts load on the runtime's blocking pool, and the launcher starts
    /// on BMCLAPI with the official source as fallback. Once the config is
    /// read the launcher follows its download-source preference; with `Auto`
    /// and no stored pick, a speed test runs in the background, swaps the
    /// launcher and stores the pick. Network failures are only logged.
    pub fn init(rt: Handle, data: DataDirs) -> Result<Core, InitError> {
        Core::start(rt, data, None, Startup::SpeedTest)
    }

    pub(super) fn start(
        rt: Handle,
        data: DataDirs,
        catalog: Option<Arc<dyn Catalog>>,
        startup: Startup,
    ) -> Result<Core, InitError> {
        let client = {
            let _rt = rt.enter();
            let client = match startup {
                Startup::SpeedTest => http_client(),
                Startup::Offline => unreachable_client(),
            };
            client.map_err(|e| InitError::HttpClient(e.to_string()))?
        };
        let initial = ResolvedSource {
            mirror: Mirror::bmclapi(),
            fallback: Some(Mirror::official()),
        };
        let launcher = Arc::new(StdRwLock::new(Arc::new(Launcher {
            data: data.clone(),
            downloader: initial.downloader(client),
        })));
        let catalog = catalog.unwrap_or_else(|| {
            Arc::new(LauncherCatalog {
                launcher: launcher.clone(),
            })
        });
        // What stays in memory if loading never finishes.
        let config = LoadedConfig {
            config: GlobalConfig::default(),
            load_error: Some(LoadError::NotLoaded),
        };
        let core = Core {
            rt,
            store: InstanceStore::new(data.instances()),
            data,
            catalog,
            config: Arc::new(RwLock::new(config)),
            launcher,
            accounts: Arc::new(Mutex::new(Err(LoadError::NotLoaded))),
            startup,
        };
        core.load();
        Ok(core)
    }

    pub fn global(cx: &App) -> &Core {
        cx.global::<Core>()
    }

    /// The current launcher. Keep the `Arc` for the whole operation rather
    /// than calling this again halfway.
    pub fn launcher(&self) -> Arc<Launcher> {
        current(&self.launcher)
    }

    /// Replaces the launcher with one that downloads from `source`, unless it
    /// already does. Work in flight finishes on the old one.
    pub fn set_source(&self, source: ResolvedSource) {
        let mut launcher = self
            .launcher
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let dl = &launcher.downloader;
        if *dl.mirror() == source.mirror && dl.fallback_mirror() == source.fallback.as_ref() {
            return;
        }
        let client = dl.client().clone();
        *launcher = Arc::new(Launcher {
            data: self.data.clone(),
            downloader: source.downloader(client),
        });
        tracing::info!(mirror = %launcher.downloader.mirror().id, "download source changed");
    }

    /// The config in memory (the defaults if it failed to load; see
    /// [`Core::config_load_error`]).
    pub fn read_config(cx: &mut App) -> Task<Result<GlobalConfig, BridgeError>> {
        let config = Core::global(cx).config.clone();
        run(cx, async move { config.read().await.config.clone() })
    }

    /// Why `config.toml` is not what is in memory, if it is not.
    pub fn config_load_error(
        cx: &mut App,
    ) -> Task<Result<Option<LoadError<ConfigError>>, BridgeError>> {
        let config = Core::global(cx).config.clone();
        run(cx, async move { config.read().await.load_error.clone() })
    }

    /// Changes the config with `f` and saves it, under the config's write
    /// lock so saves reach the disk in order. Memory changes only once the
    /// save succeeded. A changed download source also swaps the launcher.
    ///
    /// After a failed load this overwrites the unreadable `config.toml`:
    /// back it up first. Refused while the config was never loaded.
    pub fn update_config(
        cx: &mut App,
        f: impl FnOnce(&mut GlobalConfig) + Send + 'static,
    ) -> Task<Result<Result<(), ConfigError>, BridgeError>> {
        let core = Core::global(cx).clone();
        run(cx, async move { core.update_config_now(f).await })
    }

    /// Runs `f` with the account store on the blocking pool (the store does
    /// file I/O). A caller that changes the accounts must call
    /// [`AccountsChanged::bump`](super::AccountsChanged::bump) once the task
    /// has finished, so views showing accounts read them again.
    pub fn with_accounts<R: Send + 'static>(
        cx: &mut App,
        f: impl FnOnce(&mut LoadedAccounts) -> R + Send + 'static,
    ) -> Task<Result<R, BridgeError>> {
        let accounts = Core::global(cx).accounts.clone();
        run_blocking(cx, move || f(&mut accounts.blocking_lock()))
    }

    async fn update_config_now(
        &self,
        f: impl FnOnce(&mut GlobalConfig),
    ) -> Result<(), ConfigError> {
        let guard = self.config.clone().write_owned().await;
        if matches!(guard.load_error, Some(LoadError::NotLoaded)) {
            return Err(ConfigError::Io(t!("core_bridge.not_loaded").to_string()));
        }
        let mut next = guard.config.clone();
        f(&mut next);
        if next == guard.config {
            return Ok(());
        }
        let source_changed = next.download != guard.config.download;
        let path = self.data.config_file();
        let (guard, saved) = blocking(move || {
            let mut guard = guard;
            let saved = next.save(&path);
            if saved.is_ok() {
                guard.config = next;
                guard.load_error = None;
            }
            (guard, saved)
        })
        .await;
        if saved.is_ok() && source_changed {
            // A speed test may take a while; the caller need not wait.
            let core = self.clone();
            self.rt.spawn(async move { core.apply_source(guard).await });
        }
        saved
    }

    /// Loads the config and the accounts, then applies the download source.
    fn load(&self) {
        // Taken before anyone else can lock them; released once loaded.
        let mut config = (self.config.clone().try_write_owned()).expect("a new lock is free");
        let mut accounts = (self.accounts.clone().try_lock_owned()).expect("a new lock is free");
        let core = self.clone();
        self.rt.spawn(async move {
            let data = core.data.clone();
            let loaded = tokio::task::spawn_blocking(move || {
                *accounts = AccountStore::load(&data).map_err(|e| {
                    tracing::warn!(error = %e, "failed to load the accounts");
                    LoadError::Failed(Arc::new(e))
                });
                drop(accounts);
                match GlobalConfig::load(&data.config_file()) {
                    Ok(cfg) => {
                        config.config = cfg;
                        config.load_error = None;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to load config.toml; using defaults");
                        config.load_error = Some(LoadError::Failed(Arc::new(e)));
                    }
                }
                config
            })
            .await;
            // Panicked (the placeholders stay) or cancelled at shutdown.
            let Ok(config) = loaded else {
                return;
            };
            core.apply_source(config).await;
        });
    }

    /// Points the launcher at the configured source. Holds the config's
    /// write lock until then, so whoever reads the config afterwards also
    /// finds the launcher following it; only a speed test runs unlocked.
    async fn apply_source(&self, mut config: OwnedRwLockWriteGuard<LoadedConfig>) {
        let client = self.launcher().downloader.client().clone();
        let before = config.config.download.clone();
        if resolve_choice(&before).is_some() {
            // The setting names a mirror: resolving it needs no network.
            self.set_source(resolve_mirror(&client, &mut config.config).await);
            return;
        }
        drop(config);
        if self.startup == Startup::Offline {
            return;
        }

        let mut probe = GlobalConfig {
            download: before.clone(),
            ..GlobalConfig::default()
        };
        let source = resolve_mirror(&client, &mut probe).await;
        let mut config = self.config.clone().write_owned().await;
        if config.config.download != before {
            // The setting changed during the test; the change wins.
            return;
        }
        self.set_source(source);
        // No pick means every probe failed: the next run tests again. A
        // config that is not the file is not written over it.
        if probe.download.auto_pick.is_none() || config.load_error.is_some() {
            return;
        }
        let mut next = config.config.clone();
        next.download.auto_pick = probe.download.auto_pick;
        let path = self.data.config_file();
        blocking(move || match next.save(&path) {
            Ok(()) => {
                config.config = next;
                tracing::info!("stored the speed-tested download source");
            }
            Err(e) => tracing::warn!(error = %e, "failed to save config.toml"),
        })
        .await;
    }
}

/// Runs `f` on the blocking pool from runtime work; a panic in `f` goes on
/// unwinding in the caller (and so reaches its [`BridgeError`]).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => value,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        // The runtime is shutting down; so is the caller.
        Err(_) => std::future::pending().await,
    }
}

fn current(launcher: &StdRwLock<Arc<Launcher>>) -> Arc<Launcher> {
    launcher
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Version lists for the UI. Views reach them through `Core.catalog`, so UI
/// tests can inject canned lists (`core_bridge::testing`). The futures do
/// network I/O: poll them on the runtime ([`run`](super::run)).
pub trait Catalog: Send + Sync + 'static {
    /// The Minecraft version manifest; the error is ready to show.
    fn manifest(&self) -> BoxFuture<'static, Result<VersionManifest, String>>;

    /// The versions of the `kind` loader for Minecraft `mc`.
    fn loader_versions(
        &self,
        kind: LoaderKind,
        mc: String,
    ) -> BoxFuture<'static, Result<Vec<LoaderVersion>, LoaderError>>;
}

/// The real catalog: asks the launcher that is current when a list is
/// requested.
struct LauncherCatalog {
    launcher: Arc<StdRwLock<Arc<Launcher>>>,
}

impl Catalog for LauncherCatalog {
    fn manifest(&self) -> BoxFuture<'static, Result<VersionManifest, String>> {
        let launcher = current(&self.launcher);
        async move { launcher.manifest().await.map_err(|e| e.to_string()) }.boxed()
    }

    fn loader_versions(
        &self,
        kind: LoaderKind,
        mc: String,
    ) -> BoxFuture<'static, Result<Vec<LoaderVersion>, LoaderError>> {
        let launcher = current(&self.launcher);
        async move { list_versions(&launcher.downloader, kind, &mc).await }.boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use gpui_kit::TestAppContext;
    use patina_core::config::SourceChoice;

    use super::*;
    use crate::core_bridge::testing::{self, FakeCatalog};

    fn offline_core(rt: &Handle, dir: &Path) -> Core {
        testing::core(
            rt.clone(),
            DataDirs::new(dir),
            Arc::new(FakeCatalog::new().with_manifest_error("offline")),
        )
    }

    fn sources(core: &Core) -> (String, Option<String>) {
        let launcher = core.launcher();
        let dl = &launcher.downloader;
        (
            dl.mirror().id.clone(),
            dl.fallback_mirror().map(|m| m.id.clone()),
        )
    }

    /// Waits for the startup load, then returns what it left in memory.
    fn loaded(rt: &tokio::runtime::Runtime, core: &Core) -> LoadedConfig {
        rt.block_on(async { core.config.read().await.clone() })
    }

    #[test]
    fn core_follows_the_configured_source() {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[download]\nsource = \"official\"\n",
        )
        .unwrap();
        let core = offline_core(rt.handle(), dir.path());
        // Readable once loaded, and by then the launcher follows it.
        let config = loaded(&rt, &core);
        assert!(config.load_error.is_none());
        assert_eq!(config.config.download.source, SourceChoice::Official);
        assert_eq!(sources(&core), ("official".into(), Some("bmclapi".into())));
    }

    #[test]
    fn offline_core_reaches_no_host() {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[download]\nsource = \"official\"\n",
        )
        .unwrap();
        let core = offline_core(rt.handle(), dir.path());
        // Also after the configured source replaced the launcher.
        loaded(&rt, &core);
        assert_eq!(sources(&core).0, "official");
        let client = core.launcher().downloader.client().clone();
        let started = std::time::Instant::now();
        let result = rt.block_on(async move {
            client
                .get("https://piston-meta.mojang.com/mc/game/version_manifest_v2.json")
                .send()
                .await
        });
        let error = result.expect_err("an offline core cannot download");
        assert!(error.is_connect(), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn auto_starts_on_bmclapi_with_official_fallback() {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let core = offline_core(rt.handle(), dir.path());
        let config = loaded(&rt, &core);
        assert!(config.load_error.is_none());
        assert_eq!(config.config, GlobalConfig::default());
        assert_eq!(sources(&core), ("bmclapi".into(), Some("official".into())));
        assert!(!dir.path().join("config.toml").exists(), "nothing saved");
    }

    #[test]
    fn corrupt_config_is_reported_and_left_alone() {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, "download = [").unwrap();
        let core = offline_core(rt.handle(), dir.path());
        let config = loaded(&rt, &core);
        assert!(matches!(config.load_error, Some(LoadError::Failed(_))));
        assert_eq!(config.config, GlobalConfig::default());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "download = [");
    }

    #[test]
    fn accounts_load_and_a_bad_file_is_left_alone() {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let core = offline_core(rt.handle(), dir.path());
        let count = rt.block_on(async {
            let accounts = core.accounts.lock().await;
            accounts.as_ref().map(|s| s.accounts().len()).ok()
        });
        assert_eq!(count, Some(0));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("accounts.toml");
        std::fs::write(&file, "not = [toml").unwrap();
        let core = offline_core(rt.handle(), dir.path());
        let failed = rt.block_on(async {
            let accounts = core.accounts.lock().await;
            matches!(&*accounts, Err(LoadError::Failed(e)) if matches!(**e, AccountError::Parse(_)))
        });
        assert!(failed);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "not = [toml");
    }

    #[test]
    fn interrupted_load_is_marked_not_loaded() {
        let rt = crate::runtime::start();
        let handle = rt.handle().clone();
        rt.shutdown_timeout(std::time::Duration::from_secs(2));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[download]\nsource = \"official\"\n",
        )
        .unwrap();
        // The load never runs on a runtime that is shut down.
        let core = offline_core(&handle, dir.path());
        let config = core.config.try_read().expect("released").clone();
        assert!(matches!(config.load_error, Some(LoadError::NotLoaded)));
        let accounts = core.accounts.try_lock().expect("released");
        assert!(matches!(&*accounts, Err(LoadError::NotLoaded)));
        drop(accounts);
        // The user may see it: localized, not developer text.
        {
            use crate::i18n::{Language, set_language};
            let _locale = crate::i18n::locale_lock();
            let not_loaded = || LoadError::<ConfigError>::NotLoaded.to_string();
            set_language(Language::En);
            assert_eq!(
                not_loaded(),
                "Not loaded: reading the file was interrupted."
            );
            set_language(Language::ZhCn);
            assert_eq!(not_loaded(), "未加载：读取文件时被中断。");
        }

        // A config that never loaded is not saved over the file.
        let other = crate::runtime::start();
        let saved = other.block_on(core.update_config_now(|c| c.java.min_mem_mib = 1024));
        assert!(saved.is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            "[download]\nsource = \"official\"\n"
        );
    }

    #[test]
    fn set_source_replaces_the_launcher_only_on_change() {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let core = offline_core(rt.handle(), dir.path());
        loaded(&rt, &core);
        let before = core.launcher();
        core.set_source(ResolvedSource {
            mirror: Mirror::bmclapi(),
            fallback: Some(Mirror::official()),
        });
        assert!(
            Arc::ptr_eq(&before, &core.launcher()),
            "same source, same launcher"
        );
        core.set_source(ResolvedSource {
            mirror: Mirror::official(),
            fallback: Some(Mirror::bmclapi()),
        });
        assert_eq!(sources(&core), ("official".into(), Some("bmclapi".into())));
        // Work holding the old launcher keeps it.
        assert_eq!(before.downloader.mirror().id, "bmclapi");
    }

    #[test]
    fn catalog_is_injectable() {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let core = offline_core(rt.handle(), dir.path());
        let manifest = rt.block_on(core.catalog.manifest());
        assert_eq!(manifest, Err("offline".to_string()));
        let loaders = rt.block_on(
            core.catalog
                .loader_versions(LoaderKind::Fabric, "1.21".into()),
        );
        assert!(matches!(loaders, Err(LoaderError::NoVersions { .. })));
    }

    #[gpui_kit::test]
    async fn accessors_read_update_and_save(cx: &mut TestAppContext) {
        let rt = crate::runtime::start();
        let dir = tempfile::tempdir().unwrap();
        let core = offline_core(rt.handle(), dir.path());
        cx.update(|cx| cx.set_global(core.clone()));
        cx.executor().allow_parking();

        let config = cx.update(Core::read_config).await.unwrap();
        assert_eq!(config, GlobalConfig::default());
        assert!(cx.update(Core::config_load_error).await.unwrap().is_none());

        let saved =
            cx.update(|cx| Core::update_config(cx, |c| c.download.source = SourceChoice::Official));
        saved.await.unwrap().unwrap();
        let on_disk = GlobalConfig::load(&dir.path().join("config.toml")).unwrap();
        assert_eq!(on_disk.download.source, SourceChoice::Official);
        let config = cx.update(Core::read_config).await.unwrap();
        assert_eq!(config.download.source, SourceChoice::Official);
        // The source swap follows the save, under the config lock.
        cx.update(Core::read_config).await.unwrap();
        assert_eq!(sources(&core), ("official".into(), Some("bmclapi".into())));

        let count = cx
            .update(|cx| Core::with_accounts(cx, |a| a.as_ref().map(|s| s.accounts().len()).ok()));
        assert_eq!(count.await.unwrap(), Some(0));
    }
}
