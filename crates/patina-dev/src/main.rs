//! `patina-dev`: development CLI over patina-core.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use patina_core::config::{GlobalConfig, SourceChoice};
use patina_core::download::{Downloader, Mirror, PROBE_URL, Progress, measure};
use patina_core::instance::{InstanceStore, effective_java};
use patina_core::java::{ManagedRuntimes, detect_all};
use patina_core::launch::{GameProcess, LogLine, dev_offline_account};
use patina_core::paths::DataDirs;
use patina_core::prepare::{Launcher, http_client, resolve_mirror};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "patina-dev", version, about = "Patina development CLI")]
struct Cli {
    /// Data directory (overrides PATINA_DATA_DIR and the default location)
    #[arg(long, global = true, value_name = "PATH")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Manage instances
    Instance {
        #[command(subcommand)]
        cmd: InstanceCmd,
    },
    /// Manage Java installations
    Java {
        #[command(subcommand)]
        cmd: JavaCmd,
    },
    /// Download source
    Source {
        #[command(subcommand)]
        cmd: SourceCmd,
    },
    /// Launch an instance
    Launch {
        /// Instance ID (directory name)
        id: String,
        /// Offline player name (development only)
        #[arg(long, value_name = "PLAYER")]
        offline: String,
    },
}

#[derive(Subcommand)]
enum InstanceCmd {
    /// List instances
    List,
    /// Create an instance
    Create {
        name: String,
        /// Version id, or `latest` for the latest release
        version: String,
    },
    /// Delete an instance
    Delete { id: String },
}

#[derive(Subcommand)]
enum JavaCmd {
    /// List detected Java installations and managed runtimes
    List,
    /// Remove a managed runtime
    Remove { component: String },
}

#[derive(Subcommand)]
enum SourceCmd {
    /// Show the current download source
    Show,
    /// Set the download source
    Set { source: SourceArg },
    /// Measure the built-in mirrors and store the fastest for auto mode
    Speedtest,
}

#[derive(Clone, Copy, ValueEnum)]
enum SourceArg {
    Auto,
    Official,
    Bmclapi,
}

impl From<SourceArg> for SourceChoice {
    fn from(s: SourceArg) -> Self {
        match s {
            SourceArg::Auto => SourceChoice::Auto,
            SourceArg::Official => SourceChoice::Official,
            SourceArg::Bmclapi => SourceChoice::Bmclapi,
        }
    }
}

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
        let mirror = resolve_mirror(&client, &mut self.config).await;
        if self.config.download.auto_pick != before {
            println!("Speed test picked download source: {}", mirror.id);
            self.save()?;
        }
        Ok(Launcher {
            data: self.data.clone(),
            downloader: Downloader::new(client, mirror),
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
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
                println!(
                    "{:<20} {:<20} {:<10} last played: {}",
                    i.id, i.config.name, i.config.mc_version, played
                );
            }
        }
        InstanceCmd::Create { name, version } => {
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
            let inst = ctx.store().create(&name, &version)?;
            println!(
                "Created instance {:?} (ID: {:?}, version {version})",
                name, inst.id
            );
        }
        InstanceCmd::Delete { id } => {
            ctx.store().delete(&id)?;
            println!("Deleted instance {id:?}");
        }
    }
    Ok(())
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
            let mut best: Option<(Mirror, Duration)> = None;
            for m in [Mirror::official(), Mirror::bmclapi()] {
                let t = measure(&client, &m, PROBE_URL, Duration::from_secs(5)).await;
                match t {
                    Some(t) => println!("  {:<10} {} ms", m.id, t.as_millis()),
                    None => println!("  {:<10} failed or timed out", m.id),
                }
                if let Some(t) = t
                    && best.as_ref().is_none_or(|(_, b)| t < *b)
                {
                    best = Some((m, t));
                }
            }
            let pick = best.map_or_else(Mirror::official, |(m, _)| m);
            ctx.config.download.auto_pick = Some(pick.id.clone());
            ctx.save()?;
            println!("Auto mode will use: {}", pick.id);
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
        inst.config.mc_version,
        launcher.downloader.mirror().id,
        ctx.data.root().display()
    );
    let js = effective_java(&ctx.config.java, &inst.config);
    if js.java_path.is_none() {
        let v = launcher.ensure_version(&inst.config.mc_version).await?;
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
        "Prepared in {:.1}s; {} download(s) fell back to the official source",
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
