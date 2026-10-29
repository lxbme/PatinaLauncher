//! `patina-dev`: development CLI over patina-core.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use patina_core::accounts::AccountStore;
use patina_core::config::{GlobalConfig, SourceChoice};
use patina_core::content::index::ModIndex;
use patina_core::content::mods::{ModFormat, delete_mod, scan_mods, set_mod_enabled};
use patina_core::content::packs::{scan_resource_packs, scan_shader_packs};
use patina_core::content::saves::scan_saves;
use patina_core::download::{Mirror, PROBE_URL, SPEEDTEST_TIMEOUT, fastest, measure_all};
use patina_core::fsutil::{is_safe_name, sweep_owned_temps};
use patina_core::instance::Instance;
use patina_core::instance::{InstanceStore, effective_java};
use patina_core::instance_files::{early_window_disabled, set_early_window_disabled};
use patina_core::java::{ManagedRuntimes, detect_all};
#[cfg(feature = "dev-offline")]
use patina_core::launch::dev_offline_account;
use patina_core::launch::running::CURRENT_LOG;
use patina_core::launch::{GameSession, LogLine, SessionExit};
use patina_core::loader::LoaderKind;
use patina_core::loader::list::{LoaderError, VersionPick, list_versions};
use patina_core::loader::ops::{
    OpOutcome, install_loader, reinstall_loader, remove_loader, set_mc_version,
};
use patina_core::paths::DataDirs;
use patina_core::prepare::{Launcher, http_client, resolve_mirror};
use patina_core::progress::{Phase, TaskProgress, Unit};
use patina_dev::cli::{
    AccountCmd, Cli, Cmd, IconCmd, InstanceCmd, JavaCmd, LoaderCmd, LoaderSpec, ModsCmd, SourceCmd,
    Toggle, parse_pick,
};
use patina_dev::follow::follow_logs;
use patina_dev::{out, outln};
use tokio::sync::mpsc;

struct Ctx {
    data: DataDirs,
    config: GlobalConfig,
}

impl Ctx {
    fn load(data_dir: Option<PathBuf>) -> Result<Self> {
        let data = match data_dir {
            Some(p) => DataDirs::new(std::path::absolute(p)?),
            None => DataDirs::from_env(),
        };
        let config = GlobalConfig::load(&data.config_file())
            .with_context(|| format!("reading {}", data.config_file().display()))?;
        Ok(Self { data, config })
    }

    fn save(&self) -> Result<()> {
        self.config.save(&self.data.config_file())?;
        Ok(())
    }

    fn store(&self) -> InstanceStore {
        InstanceStore::new(self.data.instances())
    }

    /// Builds a launcher; may speed-test and persist the auto pick.
    async fn launcher(&mut self) -> Result<Launcher> {
        let client = http_client()?;
        let before = self.config.download.auto_pick.clone();
        let source = resolve_mirror(&client, &mut self.config).await;
        if self.config.download.auto_pick != before {
            outln!("Speed test picked download source: {}", source.mirror.id);
            self.save()?;
        }
        Ok(Launcher {
            data: self.data.clone(),
            downloader: source.downloader(client),
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Developer tool: core errors printed here must be English.
    patina_core::i18n::set_language(patina_core::i18n::Language::En);
    let cli = Cli::parse();
    let rust_log = std::env::var("RUST_LOG").ok();
    let (filter, warning) = log_filter(cli.verbose, rust_log.as_deref());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    if let Some(w) = warning {
        eprintln!("{w}");
    }
    let mut ctx = Ctx::load(cli.data_dir)?;
    // After argument parsing, so `--help` and usage errors stay fast. The
    // explicit `sweep` command reports its own result instead.
    if !matches!(cli.cmd, Cmd::Sweep) {
        auto_sweep(&ctx.data);
    }
    match cli.cmd {
        Cmd::Instance { cmd } => instance(&mut ctx, cmd).await,
        Cmd::Java { cmd } => java(&ctx, cmd).await,
        Cmd::Source { cmd } => source(&mut ctx, cmd).await,
        #[cfg(feature = "dev-offline")]
        Cmd::Launch {
            id,
            account,
            offline,
        } => launch(&mut ctx, &id, account.as_deref(), offline.as_deref()).await,
        #[cfg(not(feature = "dev-offline"))]
        Cmd::Launch { id, account } => launch(&mut ctx, &id, account.as_deref(), None).await,
        Cmd::Status => status(&ctx),
        Cmd::Kill { id } => kill(&ctx, &id).await,
        Cmd::Logs { id, follow } => logs(&ctx, &id, follow).await,
        Cmd::Search { query } => search(&ctx, &query).await,
        Cmd::Account { cmd } => account(&ctx, cmd),
        Cmd::Sweep => {
            let n = sweep(&ctx.data)?;
            outln!("Removed {n} stale temp file(s)");
            Ok(())
        }
    }
}

/// Log level when neither `-v` nor `RUST_LOG` says otherwise.
const DEFAULT_LOG: &str = "warn";
/// `-v`: debug output from Patina's own crates only; dependencies stay at warn.
const VERBOSE_LOG: &str = "warn,patina_core=debug,patina_dev=debug";

/// The tracing filter: a valid, non-empty `RUST_LOG` wins; otherwise the
/// `-v` / default directives. An invalid `RUST_LOG` yields a warning to print.
fn log_filter(
    verbose: bool,
    rust_log: Option<&str>,
) -> (tracing_subscriber::EnvFilter, Option<String>) {
    use tracing_subscriber::EnvFilter;
    let fallback = || EnvFilter::new(if verbose { VERBOSE_LOG } else { DEFAULT_LOG });
    match rust_log.map(str::trim).filter(|v| !v.is_empty()) {
        None => (fallback(), None),
        Some(v) => match EnvFilter::try_new(v) {
            Ok(f) => (f, None),
            Err(e) => (
                fallback(),
                Some(format!("warning: ignoring invalid RUST_LOG {v:?}: {e}")),
            ),
        },
    }
}

/// Temp files older than this are leftovers of interrupted writes.
const SWEEP_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

fn sweep(data: &DataDirs) -> std::io::Result<usize> {
    sweep_owned_temps(&data.root(), SWEEP_MIN_AGE)
}

/// Startup housekeeping: never fatal, silent unless something was removed.
fn auto_sweep(data: &DataDirs) {
    match sweep(data) {
        Ok(0) => {}
        Ok(n) => eprintln!("Removed {n} stale temp file(s)"),
        Err(e) => tracing::warn!("startup sweep failed: {e}"),
    }
}

/// Healthy instances of the store; broken ones go to stderr like `list` does.
fn list_healthy(store: &InstanceStore) -> Result<patina_core::instance::ListOutcome> {
    let out = store.list()?;
    for b in &out.broken {
        eprintln!("broken instance {}: {}", b.id, b.error);
    }
    Ok(out)
}

fn status(ctx: &Ctx) -> Result<()> {
    let out = list_healthy(&ctx.store())?;
    if out.instances.is_empty() {
        outln!("(no instances)");
    }
    for inst in &out.instances {
        // Dropping a reattached session releases its locks and keeps the record.
        match GameSession::reattach(inst) {
            Some(s) => outln!("{}  running pid={}", inst.id, s.pid()),
            None => outln!("{}  stopped", inst.id),
        }
    }
    Ok(())
}

fn print_exit(exit: &SessionExit) {
    outln!("Game exited: {exit:?}");
    if let Some(p) = &exit.saved_log {
        outln!("Crash log saved to {}", p.display());
    }
}

async fn kill(ctx: &Ctx, id: &str) -> Result<()> {
    let inst = ctx.store().load(id)?;
    let Some(session) = GameSession::reattach(&inst) else {
        bail!("instance {id:?} is not running");
    };
    let pid = session.pid();
    session.kill().await?;
    // Waiting clears the running record and reports a crash if there was one.
    let exit = session.wait().await;
    outln!("Stopped pid {pid}");
    print_exit(&exit);
    Ok(())
}

/// The newest `<YYYYMMDD-HHMMSS>-crash[-N].log` in `dir`.
fn newest_crash_log(dir: &std::path::Path) -> Option<PathBuf> {
    let mut best: Option<((String, u32), PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".log")) else {
            continue;
        };
        let Some((stamp, rest)) = stem.split_once("-crash") else {
            continue;
        };
        let n = match rest {
            "" => 0,
            r => match r.strip_prefix('-').and_then(|d| d.parse::<u32>().ok()) {
                Some(n) => n,
                None => continue,
            },
        };
        let key = (stamp.to_string(), n);
        if best.as_ref().is_none_or(|(k, _)| key > *k) {
            best = Some((key, entry.path()));
        }
    }
    best.map(|(_, p)| p)
}

async fn logs(ctx: &Ctx, id: &str, follow: bool) -> Result<()> {
    let inst = ctx.store().load(id)?;
    let Some(session) = GameSession::reattach(&inst) else {
        match newest_crash_log(&inst.dir.join("patina-logs")) {
            Some(p) => outln!("{}", p.display()),
            None => outln!("no crash logs"),
        }
        return Ok(());
    };
    if !follow {
        drop(session);
        let mut f = std::fs::File::open(inst.dir.join(CURRENT_LOG))
            .with_context(|| format!("opening the log of {id:?}"))?;
        if let Err(e) = std::io::copy(&mut f, &mut std::io::stdout().lock()) {
            patina_dev::out::exit_if_broken_pipe(&e);
            return Err(e.into());
        }
        return Ok(());
    }
    let print = |l: LogLine| outln!("[{}] {}", format!("{:?}", l.level).to_uppercase(), l.text);
    // Ctrl-C only stops watching; the game keeps running.
    let stop = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if let Some(exit) = follow_logs(session, print, stop).await {
        print_exit(&exit);
    }
    Ok(())
}

async fn search(ctx: &Ctx, query: &str) -> Result<()> {
    let store = ctx.store();
    let out = list_healthy(&store)?;
    let mut index = ModIndex::open(&ctx.data);
    index.refresh(&store).await?;
    outln!("Instances:");
    let found = InstanceStore::search_instances(&out, query);
    if found.is_empty() {
        outln!("  (none)");
    }
    for i in found {
        outln!("  {}  {}  {}", i.id, i.config.name, i.config.mc_version());
    }
    outln!("Mods:");
    let hits = index.search(query, 50);
    if hits.is_empty() {
        outln!("  (none)");
    }
    for h in hits {
        outln!(
            "  {} {} \u{2014} {}/{}",
            h.name,
            h.version.as_deref().unwrap_or("-"),
            h.instance_id,
            h.file_name
        );
    }
    outln!("Online:");
    outln!("  (online search: coming in P1)");
    Ok(())
}

fn account(ctx: &Ctx, cmd: AccountCmd) -> Result<()> {
    let mut store = AccountStore::load(&ctx.data)?;
    match cmd {
        AccountCmd::List => {
            if store.accounts().is_empty() {
                outln!("(no accounts)");
            }
            let current = store.current().map(|a| a.id.clone());
            for a in store.accounts() {
                let mark = if current.as_deref() == Some(a.id.as_str()) {
                    '*'
                } else {
                    ' '
                };
                outln!("{mark} {}  {:?}  {}", a.id, a.kind, a.username);
            }
            return Ok(());
        }
        #[cfg(feature = "dev-offline")]
        AccountCmd::AddOffline { name } => {
            let a = store.add_offline(&name)?;
            outln!("Account {} ({})", a.id, a.username);
        }
        AccountCmd::Use { id } => {
            store.set_current(&id)?;
            outln!("Current account: {id}");
        }
        AccountCmd::Remove { id } => {
            store.remove(&id)?;
            outln!("Removed account {id}");
        }
    }
    store.save()?;
    Ok(())
}

/// The early-window setting only affects NeoForge; a note for anything else.
fn early_window_warning(inst: &Instance) -> Option<String> {
    match inst.config.loader() {
        Some((LoaderKind::Neoforge, _)) => None,
        other => Some(format!(
            "warning: the early window setting only affects NeoForge; {:?} uses {}",
            inst.id,
            other.map_or("no loader".to_string(), |(k, _)| kind_name(k).to_string())
        )),
    }
}

fn mod_format_for(inst: &Instance) -> Option<ModFormat> {
    inst.config.loader().map(|(k, _)| match k {
        LoaderKind::Forge => ModFormat::Forge,
        LoaderKind::Neoforge => ModFormat::NeoForge,
        LoaderKind::Fabric => ModFormat::Fabric,
    })
}

/// `dir/file` for a mod file named on the command line; only a plain file
/// name is accepted.
fn mod_file_path(dir: &std::path::Path, file: &str) -> Result<PathBuf> {
    if !is_safe_name(file) {
        bail!("{file:?} is not a plain file name");
    }
    Ok(dir.join(file))
}

fn mods(ctx: &Ctx, id: &str, cmd: ModsCmd) -> Result<()> {
    let store = ctx.store();
    let inst = store.load(id)?;
    let dir = inst.game_dir().join("mods");
    let file_path = |file: &str| mod_file_path(&dir, file);
    match cmd {
        ModsCmd::List => {
            let entries = scan_mods(&dir, mod_format_for(&inst))?;
            if entries.is_empty() {
                outln!("(no mods)");
            }
            for e in entries {
                let state = if e.enabled { "enabled" } else { "disabled" };
                match &e.meta {
                    Some(m) => outln!(
                        "{}  [{state}]  {} {}  {:?}",
                        e.file_name,
                        m.name,
                        m.version.as_deref().unwrap_or("-"),
                        m.format
                    ),
                    None => outln!("{}  [{state}]  (no metadata)", e.file_name),
                }
            }
        }
        ModsCmd::Enable { file } => {
            let _lock = store.lock(id)?;
            let p = set_mod_enabled(&file_path(&file)?, true)?;
            outln!("{}", p.file_name().unwrap_or_default().to_string_lossy());
        }
        ModsCmd::Disable { file } => {
            let _lock = store.lock(id)?;
            let p = set_mod_enabled(&file_path(&file)?, false)?;
            outln!("{}", p.file_name().unwrap_or_default().to_string_lossy());
        }
        ModsCmd::Delete { file } => {
            let _lock = store.lock(id)?;
            delete_mod(&file_path(&file)?)?;
            outln!("Deleted {file}");
        }
    }
    Ok(())
}

fn print_packs(packs: Vec<patina_core::content::packs::PackEntry>) {
    if packs.is_empty() {
        outln!("(none)");
    }
    for p in packs {
        let kind = if p.is_dir { "dir" } else { "zip" };
        outln!(
            "{}  [{kind}]  {}",
            p.name,
            p.description.as_deref().unwrap_or("-")
        );
    }
}

fn saves(inst: &Instance) -> Result<()> {
    let worlds = scan_saves(&inst.game_dir().join("saves"))?;
    if worlds.is_empty() {
        outln!("(no worlds)");
    }
    for w in worlds {
        let played = w
            .last_played
            .map_or("never".to_string(), |ms| format!("{ms} (unix ms)"));
        outln!(
            "{}  {:?}  last played: {played}",
            w.dir_name,
            w.level_name.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

async fn instance(ctx: &mut Ctx, cmd: InstanceCmd) -> Result<()> {
    match cmd {
        InstanceCmd::List => {
            let out = ctx.store().list()?;
            for b in &out.broken {
                eprintln!("broken instance {}: {}", b.id, b.error);
            }
            let list = out.instances;
            if list.is_empty() {
                outln!("(no instances)");
            }
            for i in list {
                let played = i
                    .config
                    .last_played
                    .map_or("never".to_string(), |t| t.to_string());
                let loader = i.config.loader().map_or("vanilla".to_string(), |(k, c)| {
                    format!("{}@{}", kind_name(k), c.version)
                });
                outln!(
                    "{:<20} {:<20} {:<10} {:<28} last played: {}",
                    i.id,
                    i.config.name,
                    i.config.mc_version(),
                    loader,
                    played
                );
            }
        }
        InstanceCmd::Create {
            name,
            version,
            loader,
        } => {
            let launcher = ctx.launcher().await?;
            let manifest = launcher.manifest().await?;
            let version = if version == "latest" {
                manifest.latest.release.clone()
            } else {
                version
            };
            if manifest.find(&version).is_none() {
                bail!("version {version} is not in the version manifest");
            }
            let store = ctx.store();
            let inst = store.create(&name, &version)?;
            outln!(
                "Created instance {:?} (ID: {:?}, version {version})",
                name,
                inst.id
            );
            if let Some(LoaderSpec { kind, pick }) = loader {
                let result = run_install(&launcher, &store, &inst.id, kind, pick, false).await;
                if let Err(e) = result {
                    // Leave nothing half-made behind: the same command can be re-run.
                    return Err(undo_create(&inst.id, e, || store.delete(&inst.id)));
                }
            }
        }
        InstanceCmd::Delete { id } => {
            ctx.store().delete(&id)?;
            outln!("Deleted instance {id:?}");
        }
        InstanceCmd::Loader { cmd } => loader(ctx, cmd).await?,
        InstanceCmd::Mods { id, cmd } => mods(ctx, &id, cmd)?,
        InstanceCmd::Packs { id } => {
            let inst = ctx.store().load(&id)?;
            print_packs(scan_resource_packs(&inst.game_dir().join("resourcepacks"))?);
        }
        InstanceCmd::Shaders { id } => {
            let inst = ctx.store().load(&id)?;
            print_packs(scan_shader_packs(&inst.game_dir().join("shaderpacks"))?);
        }
        InstanceCmd::Saves { id } => saves(&ctx.store().load(&id)?)?,
        InstanceCmd::Icon { id, cmd } => {
            let store = ctx.store();
            let inst = match cmd {
                IconCmd::Builtin { name } => store.set_builtin_icon(&id, &name)?,
                IconCmd::Custom { path } => store.set_custom_icon(&id, &path)?,
                IconCmd::Clear => store.clear_icon(&id)?,
            };
            outln!("Icon of {id:?} is now {:?}", inst.config.icon);
        }
        InstanceCmd::ReinstallLoader { id } => {
            let launcher = ctx.launcher().await?;
            let start = Instant::now();
            let (tx, printer) = install_printer(start);
            let result = reinstall_loader(&launcher, &ctx.store(), &id, Some(tx)).await;
            let _ = printer.await;
            let outcome = loader_result(result)?;
            outln!(
                "Reinstalled {} for {id:?} in {:.1}s; {} download(s) fell back to another source",
                loader_label(&outcome),
                start.elapsed().as_secs_f64(),
                launcher.downloader.fallback_count()
            );
            print_notices(&outcome);
        }
        InstanceCmd::EarlyWindow { id, state } => {
            let store = ctx.store();
            let inst = store.load(&id)?;
            if let Some(w) = early_window_warning(&inst) {
                eprintln!("{w}");
            }
            let _lock = store.lock(&id)?;
            set_early_window_disabled(&inst.game_dir(), state == Toggle::Off)?;
            let on = !early_window_disabled(&inst.game_dir())?;
            outln!(
                "Early window of {id:?} is now {}",
                if on { "on" } else { "off" }
            );
        }
        InstanceCmd::SetVersion {
            id,
            mc_version,
            loader_action,
        } => {
            let launcher = ctx.launcher().await?;
            let store = ctx.store();
            let start = Instant::now();
            let (tx, printer) = install_printer(start);
            let result = set_mc_version(
                &launcher,
                &store,
                &id,
                &mc_version,
                loader_action.map(Into::into),
                Some(tx),
            )
            .await;
            let _ = printer.await;
            if let Err(e) = &result
                && let Some(hint) = loader_error_hint(e)
            {
                outln!("{hint}");
            }
            let outcome = loader_result(result)?;
            outln!(
                "Instance {id:?} is now Minecraft {} with {} ({:.1}s; {} download(s) fell back to another source)",
                outcome.instance.config.mc_version(),
                loader_label(&outcome),
                start.elapsed().as_secs_f64(),
                launcher.downloader.fallback_count()
            );
            print_notices(&outcome);
        }
    }
    Ok(())
}

async fn loader(ctx: &mut Ctx, cmd: LoaderCmd) -> Result<()> {
    match cmd {
        LoaderCmd::List { id, kind } => {
            let kind: LoaderKind = kind.into();
            let inst = ctx.store().load(&id)?;
            let mc = inst.config.mc_version().to_string();
            let launcher = ctx.launcher().await?;
            let list = loader_result(list_versions(&launcher.downloader, kind, &mc).await)?;
            outln!("{} versions for Minecraft {mc}:", kind_name(kind));
            for v in list {
                let mut tags = Vec::new();
                if v.recommended {
                    tags.push("[recommended]");
                }
                if v.latest {
                    tags.push("[latest]");
                }
                if !v.stable {
                    tags.push("[beta]");
                }
                if tags.is_empty() {
                    outln!("{}", v.version);
                } else {
                    outln!("{} {}", v.version, tags.join(" "));
                }
            }
        }
        LoaderCmd::Install {
            id,
            kind,
            version,
            replace,
        } => {
            let pick = match version {
                Some(v) => parse_pick(&v).map_err(anyhow::Error::msg)?,
                None => VersionPick::Recommended,
            };
            let launcher = ctx.launcher().await?;
            run_install(&launcher, &ctx.store(), &id, kind.into(), pick, replace).await?;
        }
        LoaderCmd::Remove { id } => {
            let outcome = loader_result(remove_loader(&ctx.store(), &id))?;
            outln!(
                "Instance {id:?} is now Minecraft {} with {}",
                outcome.instance.config.mc_version(),
                loader_label(&outcome)
            );
        }
    }
    Ok(())
}

/// Installs a loader into an existing instance, printing progress, timing,
/// the fallback count and any notices.
async fn run_install(
    launcher: &Launcher,
    store: &InstanceStore,
    id: &str,
    kind: LoaderKind,
    pick: VersionPick,
    replace: bool,
) -> Result<()> {
    outln!("Installing {} ({pick:?}) into {id:?}", kind_name(kind));
    let start = Instant::now();
    let (tx, printer) = install_printer(start);
    let result = install_loader(launcher, store, id, kind, pick, replace, Some(tx)).await;
    let _ = printer.await;
    if let Err(e) = &result
        && let Some(hint) = loader_error_hint(e)
    {
        outln!("{hint}");
    }
    let outcome = loader_result(result)?;
    outln!(
        "Installed {} into {id:?} in {:.1}s; {} download(s) fell back to another source",
        loader_label(&outcome),
        start.elapsed().as_secs_f64(),
        launcher.downloader.fallback_count()
    );
    print_notices(&outcome);
    Ok(())
}

/// The command-line flag that gets past `e`, when there is one.
fn loader_error_hint(e: &LoaderError) -> Option<&'static str> {
    match e {
        LoaderError::Conflict { .. } => Some("Pass --replace to replace the existing loader."),
        LoaderError::Incompatible {
            can_replace: true, ..
        } => Some(
            "Pass --loader-action replace to install the recommended version of the same loader, \
             or --loader-action remove to remove the loader.",
        ),
        LoaderError::Incompatible {
            can_replace: false, ..
        } => Some("Pass --loader-action remove to remove the loader."),
        _ => None,
    }
}

/// Undoes `create --loader` after the install failed with `err`: deletes
/// instance `id` through `delete`. A failed delete is reported on stderr and
/// never hides `err`, which is returned either way.
fn undo_create<E: std::fmt::Display>(
    id: &str,
    err: anyhow::Error,
    delete: impl FnOnce() -> std::result::Result<(), E>,
) -> anyhow::Error {
    match delete() {
        Ok(()) => outln!("Deleted instance {id:?} again"),
        Err(d) => eprintln!("Could not delete the half-made instance {id:?}: {d}"),
    }
    err
}

/// Turns a core loader error into an anyhow error that keeps its Debug
/// details (processor stderr tails, causes) for the developer.
fn loader_result<T>(r: Result<T, LoaderError>) -> Result<T> {
    r.map_err(|e| {
        let details = format!("{e:?}");
        anyhow::anyhow!("{e}\n\nDetails: {details}")
    })
}

fn kind_name(kind: LoaderKind) -> &'static str {
    match kind {
        LoaderKind::Forge => "forge",
        LoaderKind::Neoforge => "neoforge",
        LoaderKind::Fabric => "fabric",
    }
}

fn loader_label(outcome: &OpOutcome) -> String {
    match outcome.instance.config.loader() {
        Some((k, c)) => format!(
            "{} {} (profile {})",
            kind_name(k),
            c.version,
            c.profile.as_deref().unwrap_or("-")
        ),
        None => "no loader".to_string(),
    }
}

fn print_notices(outcome: &OpOutcome) {
    for n in &outcome.notices {
        outln!("Notice ({n:?}): {n}");
    }
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::FetchMetadata => "fetch-metadata",
        Phase::DownloadRuntime => "download-runtime",
        Phase::DownloadAssetIndex => "download-asset-index",
        Phase::DownloadGameFiles => "download-game-files",
        Phase::DownloadInstaller => "download-installer",
        Phase::DownloadLibraries => "download-libraries",
        Phase::RunProcessor => "run-processor",
        Phase::Finalize => "finalize",
        Phase::Launching => "launching",
    }
}

/// `done/total`, in MB for byte progress.
fn format_amount(p: &TaskProgress) -> String {
    match p.unit {
        Unit::Items => format!("{}/{}", p.done, p.total),
        Unit::Bytes => format!(
            "{:.1}/{:.1} MB",
            p.done as f64 / 1_000_000.0,
            p.total as f64 / 1_000_000.0
        ),
    }
}

/// A progress sink plus the task printing it as one `[phase] done/total`
/// line, refreshed with `\r` every 200 ms; it ends when the sender is dropped.
fn install_printer(
    start: Instant,
) -> (
    mpsc::UnboundedSender<TaskProgress>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    (tx, tokio::spawn(print_install_progress(rx, start)))
}

async fn print_install_progress(mut rx: mpsc::UnboundedReceiver<TaskProgress>, start: Instant) {
    let mut current: Option<TaskProgress> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let render = |p: &TaskProgress, newline: bool| {
        let line = format!(
            "[{}] {} ({:.1}s)",
            phase_name(p.phase),
            format_amount(p),
            start.elapsed().as_secs_f64()
        );
        if newline {
            outln!("\r{line:<60}");
        } else {
            out!("\r{line:<60}");
        }
    };
    loop {
        tokio::select! {
            p = rx.recv() => match p {
                Some(p) => {
                    // A finished phase keeps its own line.
                    if let Some(c) = current
                        && c.phase != p.phase
                    {
                        render(&c, true);
                    }
                    current = Some(p);
                }
                None => break,
            },
            _ = tick.tick() => {
                if let Some(c) = &current {
                    render(c, false);
                }
            }
        }
    }
    if let Some(c) = &current {
        render(c, true);
    }
}

async fn java(ctx: &Ctx, cmd: JavaCmd) -> Result<()> {
    let runtimes = ManagedRuntimes::new(ctx.data.runtimes());
    match cmd {
        JavaCmd::List => {
            outln!("Detected Java installations:");
            let found = detect_all();
            if found.is_empty() {
                outln!("  (none found)");
            }
            for j in found {
                let bits = if j.is_64bit { "64-bit" } else { "32-bit" };
                outln!(
                    "  Java {:<3} {:<14} {bits}  {}",
                    j.major,
                    j.version,
                    j.path.display()
                );
            }
            outln!("Managed runtimes ({}):", ctx.data.runtimes().display());
            let installed = runtimes.list_installed();
            if installed.is_empty() {
                outln!("  (none installed)");
            }
            for (component, version) in installed {
                outln!(
                    "  {component:<24} {version:<16} {}",
                    runtimes.java_bin(&component).display()
                );
            }
        }
        JavaCmd::Remove { component } => {
            runtimes.remove(&component).await?;
            outln!("Removed managed runtime {component}");
        }
    }
    Ok(())
}

async fn source(ctx: &mut Ctx, cmd: SourceCmd) -> Result<()> {
    match cmd {
        SourceCmd::Show => {
            let d = &ctx.config.download;
            outln!("Download source: {:?}", d.source);
            outln!(
                "Auto speed-test pick: {}",
                d.auto_pick.as_deref().unwrap_or("(none)")
            );
        }
        SourceCmd::Set { source } => {
            ctx.config.download.source = source.into();
            if ctx.config.download.source == SourceChoice::Auto {
                // Force a fresh speed test next time.
                ctx.config.download.auto_pick = None;
            }
            ctx.save()?;
            outln!("Download source set to {:?}", ctx.config.download.source);
        }
        SourceCmd::Speedtest => {
            let client = http_client()?;
            let builtin = [Mirror::official(), Mirror::bmclapi()];
            let results = measure_all(&client, &builtin, PROBE_URL, SPEEDTEST_TIMEOUT).await;
            for (m, t) in &results {
                match t {
                    Some(t) => outln!("  {:<10} {} ms", m.id, t.as_millis()),
                    None => outln!("  {:<10} failed or timed out", m.id),
                }
            }
            match fastest(&results) {
                Some(pick) => {
                    ctx.config.download.auto_pick = Some(pick.id.clone());
                    ctx.save()?;
                    outln!("Auto mode will use: {}", pick.id);
                }
                None => outln!("Every source failed; nothing stored, auto mode will test again"),
            }
        }
    }
    Ok(())
}

/// The account to play as: `--offline` (not stored; only with the
/// `dev-offline` feature, otherwise always `None`), `--account`, or the
/// current account. A Microsoft account fails with the localized
/// `MsaUnavailable` message until sign-in exists.
fn launch_account(
    ctx: &Ctx,
    account: Option<&str>,
    offline: Option<&str>,
) -> Result<patina_core::launch::LaunchAccount> {
    if let Some(name) = offline {
        #[cfg(feature = "dev-offline")]
        return Ok(dev_offline_account(name));
        #[cfg(not(feature = "dev-offline"))]
        bail!("offline account {name:?} needs the dev-offline feature (use `cargo dev`)");
    }
    let accounts = AccountStore::load(&ctx.data)?;
    let id = match account {
        Some(id) => id.to_string(),
        None => match accounts.current() {
            Some(a) => a.id.clone(),
            None if patina_core::DEV_OFFLINE => {
                bail!("no account yet; add one with `account add-offline <NAME>`")
            }
            None => bail!(
                "no account yet; Microsoft sign-in is not available yet, and offline \
                 accounts need the dev-offline feature (use `cargo dev`)"
            ),
        },
    };
    Ok(accounts.launch_account(&id)?)
}

async fn launch(
    ctx: &mut Ctx,
    id: &str,
    account: Option<&str>,
    offline: Option<&str>,
) -> Result<()> {
    let store = ctx.store();
    let inst = store.load(id)?;
    let account = launch_account(ctx, account, offline)?;
    let launcher = ctx.launcher().await?;
    outln!(
        "Instance {:?}, version {}, source {}, data dir {}",
        inst.config.name,
        inst.config.mc_version(),
        launcher.downloader.mirror().id,
        ctx.data.root().display()
    );
    let js = effective_java(&ctx.config.java, &inst.config);
    if js.java_path.is_none() {
        let v = launcher.ensure_version(inst.config.mc_version()).await?;
        let component = v.java_requirement().component;
        let runtimes = ManagedRuntimes::new(ctx.data.runtimes());
        if runtimes.is_installed(&component) {
            outln!("Using managed Java runtime {component}");
        } else {
            outln!("Managed Java runtime {component} is not installed; downloading...");
        }
    }

    let start = Instant::now();
    let (ptx, printer) = install_printer(start);
    let result = GameSession::start(&launcher, &inst, &ctx.config, &account, Some(ptx)).await;
    let _ = printer.await;
    let mut game = result?;
    outln!(
        "Started in {:.1}s; {} download(s) fell back to another source",
        start.elapsed().as_secs_f64(),
        launcher.downloader.fallback_count()
    );
    for w in game.warnings() {
        outln!("Warning: {w}");
    }
    outln!("Game process PID: {}", game.pid());
    if let Err(e) = store.touch_played(&inst.id) {
        eprintln!("Warning: failed to save last_played: {e}");
    }

    let mut logs = game.logs();
    let printer = tokio::spawn(async move {
        while let Some(l) = logs.recv().await {
            outln!("[{}] {}", format!("{:?}", l.level).to_uppercase(), l.text);
        }
    });
    let killer = game.killer();
    let wait = game.wait();
    tokio::pin!(wait);
    let exit = tokio::select! {
        e = &mut wait => e,
        // Ctrl-C stops the game (not just this tool); the session is still
        // waited on afterwards so the exit is reported and the record cleared.
        _ = tokio::signal::ctrl_c() => {
            outln!("Ctrl-C received, stopping the game process...");
            if let Err(e) = killer.kill().await {
                eprintln!("Failed to stop the game: {e}");
            }
            wait.await
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(3), printer).await;
    print_exit(&exit);
    exit_status(&exit)
}

/// `launch` fails (non-zero exit) when the game crashed.
fn exit_status(exit: &SessionExit) -> Result<()> {
    if exit.crashed {
        bail!("the game crashed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_file_guard_rejects_paths() {
        let dir = std::path::Path::new("/m");
        for bad in ["../x.jar", "a/b.jar", "", ".", "..", "/abs.jar"] {
            assert!(mod_file_path(dir, bad).is_err(), "{bad:?}");
        }
        assert_eq!(mod_file_path(dir, "ok.jar").unwrap(), dir.join("ok.jar"));
    }

    #[test]
    fn early_window_warns_unless_neoforge() {
        use patina_core::instance::{Component, ComponentUid};
        let mut inst = Instance {
            id: "i".into(),
            dir: PathBuf::from("/i"),
            config: Default::default(),
        };
        assert!(early_window_warning(&inst).is_some());
        for (uid, warn) in [(ComponentUid::Forge, true), (ComponentUid::Neoforge, false)] {
            inst.config
                .set_loader(Some(Component {
                    uid,
                    version: "1".into(),
                    profile: None,
                }))
                .unwrap();
            assert_eq!(early_window_warning(&inst).is_some(), warn, "{uid:?}");
        }
    }

    #[test]
    fn crashed_game_is_a_failure() {
        let exit = |crashed| SessionExit {
            code: Some(1),
            crashed,
            saved_log: None,
        };
        assert!(exit_status(&exit(true)).is_err());
        assert!(exit_status(&exit(false)).is_ok());
    }

    #[test]
    fn verbose_logs_only_patina_crates_at_debug() {
        let (f, w) = log_filter(true, None);
        assert!(w.is_none());
        let s = f.to_string();
        assert!(
            s.contains("patina_core=debug") && s.contains("patina_dev=debug"),
            "{s}"
        );
        assert!(!s.split(',').any(|d| d == "debug"), "{s}");
        assert_eq!(log_filter(false, Some("  ")).0.to_string(), "warn");
    }

    #[test]
    fn invalid_rust_log_warns_once_and_falls_back() {
        let (f, w) = log_filter(false, Some("patina_core=loud"));
        assert_eq!(f.to_string(), "warn");
        let w = w.expect("a warning");
        assert!(
            w.contains("RUST_LOG") && w.contains("patina_core=loud"),
            "{w}"
        );
        let (f, w) = log_filter(true, Some("reqwest=trace"));
        assert!(w.is_none());
        assert_eq!(f.to_string(), "reqwest=trace");
    }

    #[test]
    fn newest_crash_log_orders_by_stamp_then_counter() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(newest_crash_log(d.path()), None);
        for n in [
            "current.log",
            "20260101-000000-crash.log",
            "20260102-120000-crash.log",
            "20260102-120000-crash-2.log",
            "20260102-120000-crash-10.log",
            "20260102-120000-crash-x.log",
            "20260103-000000-crashy.txt",
        ] {
            std::fs::write(d.path().join(n), "").unwrap();
        }
        assert_eq!(
            newest_crash_log(d.path()).unwrap().file_name().unwrap(),
            "20260102-120000-crash-10.log"
        );
    }

    #[test]
    fn failed_cleanup_keeps_the_install_error() {
        let e = undo_create("x", anyhow::anyhow!("install failed"), || {
            Err::<(), _>("disk full")
        });
        assert_eq!(e.to_string(), "install failed");
        let mut deleted = false;
        let e = undo_create("x", anyhow::anyhow!("install failed"), || {
            deleted = true;
            Ok::<(), String>(())
        });
        assert!(deleted);
        assert_eq!(e.to_string(), "install failed");
    }

    #[test]
    fn loader_errors_hint_at_their_flags() {
        let incompatible = |can_replace| LoaderError::Incompatible {
            kind: LoaderKind::Forge,
            version: "47.1.0".into(),
            can_replace,
        };
        let both = loader_error_hint(&incompatible(true)).unwrap();
        assert!(
            both.contains("--loader-action replace") && both.contains("--loader-action remove"),
            "{both}"
        );
        let remove = loader_error_hint(&incompatible(false)).unwrap();
        assert!(
            remove.contains("--loader-action remove") && !remove.contains("replace"),
            "{remove}"
        );
        let conflict = LoaderError::Conflict {
            existing: LoaderKind::Fabric,
            existing_version: "0.16.10".into(),
        };
        assert!(loader_error_hint(&conflict).unwrap().contains("--replace"));
        assert_eq!(loader_error_hint(&LoaderError::Parse("x".into())), None);
    }
}
