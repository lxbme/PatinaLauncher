//! `patina-dev`: development CLI over patina-core.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;
use patina_core::config::{GlobalConfig, SourceChoice};
use patina_core::download::{Mirror, PROBE_URL, Progress, SPEEDTEST_TIMEOUT, fastest, measure_all};
use patina_core::instance::{InstanceStore, effective_java};
use patina_core::java::{ManagedRuntimes, detect_all};
use patina_core::launch::{GameProcess, LogLine, dev_offline_account};
use patina_core::loader::list::{LoaderError, VersionPick, list_versions};
use patina_core::loader::ops::{OpOutcome, install_loader, remove_loader, set_mc_version};
use patina_core::loader::{InstallPhase, InstallProgress, LoaderKind};
use patina_core::paths::DataDirs;
use patina_core::prepare::{Launcher, http_client, resolve_mirror};
use patina_dev::cli::{
    Cli, Cmd, InstanceCmd, JavaCmd, LoaderCmd, LoaderSpec, SourceCmd, parse_pick,
};
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
            println!("Speed test picked download source: {}", source.mirror.id);
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
    let default_level = if cli.verbose { "debug" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_level)),
        )
        .with_writer(std::io::stderr)
        .init();
    let mut ctx = Ctx::load(cli.data_dir)?;
    match cli.cmd {
        Cmd::Instance { cmd } => instance(&mut ctx, cmd).await,
        Cmd::Java { cmd } => java(&ctx, cmd),
        Cmd::Source { cmd } => source(&mut ctx, cmd).await,
        Cmd::Launch { id, offline } => launch(&mut ctx, &id, &offline).await,
    }
}

async fn instance(ctx: &mut Ctx, cmd: InstanceCmd) -> Result<()> {
    match cmd {
        InstanceCmd::List => {
            let list = ctx.store().list()?;
            if list.is_empty() {
                println!("(no instances)");
            }
            for i in list {
                let played = i
                    .config
                    .last_played
                    .map_or("never".to_string(), |t| t.to_string());
                let loader = i.config.loader().map_or("vanilla".to_string(), |(k, c)| {
                    format!("{}@{}", kind_name(k), c.version)
                });
                println!(
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
            println!(
                "Created instance {:?} (ID: {:?}, version {version})",
                name, inst.id
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
            println!("Deleted instance {id:?}");
        }
        InstanceCmd::Loader { cmd } => loader(ctx, cmd).await?,
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
                println!("{hint}");
            }
            let outcome = loader_result(result)?;
            println!(
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
            println!("{} versions for Minecraft {mc}:", kind_name(kind));
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
                    println!("{}", v.version);
                } else {
                    println!("{} {}", v.version, tags.join(" "));
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
            println!(
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
    println!("Installing {} ({pick:?}) into {id:?}", kind_name(kind));
    let start = Instant::now();
    let (tx, printer) = install_printer(start);
    let result = install_loader(launcher, store, id, kind, pick, replace, Some(tx)).await;
    let _ = printer.await;
    if let Err(e) = &result
        && let Some(hint) = loader_error_hint(e)
    {
        println!("{hint}");
    }
    let outcome = loader_result(result)?;
    println!(
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
        Ok(()) => println!("Deleted instance {id:?} again"),
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
        println!("Notice ({n:?}): {n}");
    }
}

fn phase_name(p: InstallPhase) -> &'static str {
    match p {
        InstallPhase::FetchMetadata => "fetch-metadata",
        InstallPhase::DownloadInstaller => "download-installer",
        InstallPhase::DownloadLibraries => "download-libraries",
        InstallPhase::RunProcessor => "run-processor",
        InstallPhase::Finalize => "finalize",
    }
}

/// A progress sink plus the task printing it as one `[phase] done/total`
/// line, refreshed with `\r` every 200 ms; it ends when the sender is dropped.
fn install_printer(
    start: Instant,
) -> (
    mpsc::UnboundedSender<InstallProgress>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    (tx, tokio::spawn(print_install_progress(rx, start)))
}

async fn print_install_progress(mut rx: mpsc::UnboundedReceiver<InstallProgress>, start: Instant) {
    use std::io::Write;
    let mut current: Option<InstallProgress> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let render = |p: &InstallProgress, newline: bool| {
        let line = format!(
            "[{}] {}/{} ({:.1}s)",
            phase_name(p.phase),
            p.done,
            p.total,
            start.elapsed().as_secs_f64()
        );
        if newline {
            println!("\r{line:<60}");
        } else {
            print!("\r{line:<60}");
            let _ = std::io::stdout().flush();
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

fn java(ctx: &Ctx, cmd: JavaCmd) -> Result<()> {
    let runtimes = ManagedRuntimes::new(ctx.data.runtimes());
    match cmd {
        JavaCmd::List => {
            println!("Detected Java installations:");
            let found = detect_all();
            if found.is_empty() {
                println!("  (none found)");
            }
            for j in found {
                let bits = if j.is_64bit { "64-bit" } else { "32-bit" };
                println!(
                    "  Java {:<3} {:<14} {bits}  {}",
                    j.major,
                    j.version,
                    j.path.display()
                );
            }
            println!("Managed runtimes ({}):", ctx.data.runtimes().display());
            let installed = runtimes.list_installed();
            if installed.is_empty() {
                println!("  (none installed)");
            }
            for (component, version) in installed {
                println!(
                    "  {component:<24} {version:<16} {}",
                    runtimes.java_bin(&component).display()
                );
            }
        }
        JavaCmd::Remove { component } => {
            runtimes.remove(&component)?;
            println!("Removed managed runtime {component}");
        }
    }
    Ok(())
}

async fn source(ctx: &mut Ctx, cmd: SourceCmd) -> Result<()> {
    match cmd {
        SourceCmd::Show => {
            let d = &ctx.config.download;
            println!("Download source: {:?}", d.source);
            println!(
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
            println!("Download source set to {:?}", ctx.config.download.source);
        }
        SourceCmd::Speedtest => {
            let client = http_client()?;
            let builtin = [Mirror::official(), Mirror::bmclapi()];
            let results = measure_all(&client, &builtin, PROBE_URL, SPEEDTEST_TIMEOUT).await;
            for (m, t) in &results {
                match t {
                    Some(t) => println!("  {:<10} {} ms", m.id, t.as_millis()),
                    None => println!("  {:<10} failed or timed out", m.id),
                }
            }
            match fastest(&results) {
                Some(pick) => {
                    ctx.config.download.auto_pick = Some(pick.id.clone());
                    ctx.save()?;
                    println!("Auto mode will use: {}", pick.id);
                }
                None => println!("Every source failed; nothing stored, auto mode will test again"),
            }
        }
    }
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Prints download progress on one line, at most every 200 ms.
async fn print_progress(mut rx: mpsc::UnboundedReceiver<Progress>, start: Instant) {
    use std::io::Write;
    let mut last_print: Option<Instant> = None;
    let mut last = None;
    let mut first = true;
    while let Some(p) = rx.recv().await {
        if first {
            println!(
                "Downloading game files (setup took {:.1}s)",
                start.elapsed().as_secs_f64()
            );
            first = false;
        }
        last = Some(p);
        if last_print.is_none_or(|t| t.elapsed() >= Duration::from_millis(200)) {
            print!("\rDownloading {}/{} ", p.done, p.total);
            let _ = std::io::stdout().flush();
            last_print = Some(Instant::now());
        }
    }
    if let Some(p) = last {
        println!(
            "\rDownloaded {}/{} (total {:.1}s)",
            p.done,
            p.total,
            start.elapsed().as_secs_f64()
        );
    }
}

async fn print_logs(mut rx: mpsc::UnboundedReceiver<LogLine>) {
    while let Some(l) = rx.recv().await {
        println!("[{}] {}", format!("{:?}", l.level).to_uppercase(), l.text);
    }
}

async fn launch(ctx: &mut Ctx, id: &str, player: &str) -> Result<()> {
    let store = ctx.store();
    let mut inst = store.load(id)?;
    let launcher = ctx.launcher().await?;
    println!(
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
            println!("Using managed Java runtime {component}");
        } else {
            println!("Managed Java runtime {component} is not installed; downloading...");
        }
    }

    let start = Instant::now();
    let (ptx, prx) = mpsc::unbounded_channel();
    let printer = tokio::spawn(print_progress(prx, start));
    let account = dev_offline_account(player);
    let result = launcher
        .prepare(&inst, &ctx.config, &account, Some(ptx))
        .await;
    let _ = printer.await;
    let prepared = result?;
    println!(
        "Prepared in {:.1}s; {} download(s) fell back to another source",
        start.elapsed().as_secs_f64(),
        launcher.downloader.fallback_count()
    );
    for w in &prepared.warnings {
        println!("Warning: {w}");
    }
    let cmd = &prepared.command;
    println!(
        "Launching {} ({} args), working dir {}",
        cmd.program.display(),
        cmd.args.len(),
        cmd.cwd.display()
    );

    let (ltx, lrx) = mpsc::unbounded_channel();
    let mut game = GameProcess::spawn(cmd, ltx)
        .await
        .context("failed to start the game process")?;
    println!("Game process PID: {}", game.id().unwrap_or(0));
    inst.config.last_played = Some(now_secs());
    if let Err(e) = store.save(&inst) {
        eprintln!("Warning: failed to save last_played: {e}");
    }
    let logs = tokio::spawn(print_logs(lrx));

    let status = tokio::select! {
        s = game.wait() => s?,
        _ = tokio::signal::ctrl_c() => {
            println!("Ctrl-C received, stopping the game process...");
            game.kill().await?;
            game.wait().await?
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(3), logs).await;
    println!("Game exited: {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
