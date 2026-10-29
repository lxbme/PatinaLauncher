//! Command-line definitions for `patina-dev` (parsed with clap).

use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, Subcommand, ValueEnum};
use patina_core::config::SourceChoice;
use patina_core::loader::LoaderKind;
use patina_core::loader::VersionPick;
use patina_core::loader::ops::LoaderAction;

/// Patina development CLI.
///
/// Offline accounts (`account add-offline`, `launch --offline`) exist only
/// with the `dev-offline` feature; `cargo dev <ARGS>` (a cargo alias in
/// `.cargo/config.toml`) runs this tool with it enabled.
#[derive(Parser, Debug)]
#[command(
    name = "patina-dev",
    version,
    about = "Patina development CLI",
    long_about = "Patina development CLI.\n\nOffline accounts (`account add-offline`, `launch --offline`) \
                  need the `dev-offline` feature; run `cargo dev <ARGS>` (a cargo alias for \
                  `cargo run -p patina-dev --features dev-offline --`) to get them."
)]
pub struct Cli {
    /// Data directory (overrides PATINA_DATA_DIR and the default location)
    #[arg(long, global = true, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,
    /// Log at debug level (RUST_LOG overrides)
    #[arg(short, long, global = true)]
    pub verbose: bool,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
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
    /// Preferred download source
    Source {
        #[command(subcommand)]
        cmd: SourceCmd,
    },
    /// Launch an instance (as the current account unless told otherwise)
    Launch {
        /// Instance ID (directory name)
        id: String,
        /// Launch as this stored account instead of the current one
        #[cfg_attr(
            feature = "dev-offline",
            arg(long, value_name = "ACCOUNT_ID", conflicts_with = "offline")
        )]
        #[cfg_attr(not(feature = "dev-offline"), arg(long, value_name = "ACCOUNT_ID"))]
        account: Option<String>,
        /// Offline player name, without storing an account (development only)
        #[cfg(feature = "dev-offline")]
        #[arg(long, value_name = "PLAYER")]
        offline: Option<String>,
    },
    /// Show whether each instance's game is running
    Status,
    /// Stop a running game
    Kill { id: String },
    /// Print a running game's log, or the newest crash log path when it is not running
    Logs {
        id: String,
        /// Keep printing new lines until the game exits (Ctrl-C only stops watching)
        #[arg(long)]
        follow: bool,
    },
    /// Search instances and installed mods (the mod index is refreshed first)
    Search { query: String },
    /// Manage accounts
    Account {
        #[command(subcommand)]
        cmd: AccountCmd,
    },
    /// Remove stale temp files left by interrupted downloads and writes
    Sweep,
}

#[derive(Subcommand, Debug)]
pub enum AccountCmd {
    /// List accounts; the current one is marked
    List,
    /// Add an offline account (development only)
    #[cfg(feature = "dev-offline")]
    AddOffline { name: String },
    /// Make an account the current one
    Use { id: String },
    /// Remove an account
    Remove { id: String },
}

#[derive(Subcommand, Debug)]
pub enum ModsCmd {
    /// List mods with their name, version and format
    List,
    /// Enable a mod (`x.jar.disabled` becomes `x.jar`)
    Enable { file: String },
    /// Disable a mod (`x.jar` becomes `x.jar.disabled`)
    Disable { file: String },
    /// Delete a mod file
    Delete { file: String },
}

#[derive(Subcommand, Debug)]
pub enum IconCmd {
    /// Use a built-in icon
    Builtin { name: String },
    /// Use an image file (png, jpeg or webp)
    Custom { path: PathBuf },
    /// Remove the icon
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Toggle {
    On,
    Off,
}

#[derive(Subcommand, Debug)]
pub enum InstanceCmd {
    /// List instances
    List,
    /// Create an instance
    Create {
        name: String,
        /// Version id, or `latest` for the latest release
        version: String,
        /// Also install a mod loader: `<forge|neoforge|fabric>[@<version|recommended|latest>]`
        /// (the loader version defaults to `recommended`)
        #[arg(long, value_name = "KIND[@VERSION]")]
        loader: Option<LoaderSpec>,
    },
    /// Delete an instance
    Delete { id: String },
    /// Manage an instance's mod loader
    Loader {
        #[command(subcommand)]
        cmd: LoaderCmd,
    },
    /// Manage an instance's mods
    Mods {
        id: String,
        #[command(subcommand)]
        cmd: ModsCmd,
    },
    /// List an instance's resource packs
    Packs { id: String },
    /// List an instance's shader packs
    Shaders { id: String },
    /// List an instance's worlds
    Saves { id: String },
    /// Set or clear an instance's icon
    Icon {
        id: String,
        #[command(subcommand)]
        cmd: IconCmd,
    },
    /// Reinstall the instance's current loader, repairing damaged files
    ReinstallLoader { id: String },
    /// Turn NeoForge's early loading window on or off
    EarlyWindow { id: String, state: Toggle },
    /// Change an instance's Minecraft version
    SetVersion {
        id: String,
        mc_version: String,
        /// What to do with a loader that does not support the new version
        #[arg(long, value_name = "ACTION")]
        loader_action: Option<LoaderActionArg>,
    },
}

#[derive(Subcommand, Debug)]
pub enum LoaderCmd {
    /// List the loader versions available for the instance's Minecraft version
    List { id: String, kind: KindArg },
    /// Install (or change) the instance's loader
    Install {
        id: String,
        kind: KindArg,
        /// A loader version, `recommended` (default) or `latest`
        version: Option<String>,
        /// Replace a loader of another kind
        #[arg(long)]
        replace: bool,
    },
    /// Remove the instance's loader
    Remove { id: String },
}

#[derive(Subcommand, Debug)]
pub enum JavaCmd {
    /// List detected Java installations and managed runtimes
    List,
    /// Remove a managed runtime
    Remove { component: String },
}

#[derive(Subcommand, Debug)]
pub enum SourceCmd {
    /// Show the preferred download source
    Show,
    /// Set the preferred download source (the other one is still the fallback)
    Set { source: SourceArg },
    /// Measure the built-in mirrors and store the fastest for auto mode
    Speedtest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SourceArg {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    Forge,
    Neoforge,
    Fabric,
}

impl From<KindArg> for LoaderKind {
    fn from(k: KindArg) -> Self {
        match k {
            KindArg::Forge => LoaderKind::Forge,
            KindArg::Neoforge => LoaderKind::Neoforge,
            KindArg::Fabric => LoaderKind::Fabric,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LoaderActionArg {
    /// Install the recommended version of the same loader
    Replace,
    /// Remove the loader
    Remove,
}

impl From<LoaderActionArg> for LoaderAction {
    fn from(a: LoaderActionArg) -> Self {
        match a {
            LoaderActionArg::Replace => LoaderAction::Replace,
            LoaderActionArg::Remove => LoaderAction::Remove,
        }
    }
}

/// `recommended`, `latest`, or an exact loader version.
pub fn parse_pick(s: &str) -> Result<VersionPick, String> {
    match s {
        "" => Err("empty loader version".into()),
        "recommended" => Ok(VersionPick::Recommended),
        "latest" => Ok(VersionPick::Latest),
        v => Ok(VersionPick::Exact(v.to_string())),
    }
}

/// `--loader <kind>[@<version|recommended|latest>]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoaderSpec {
    pub kind: LoaderKind,
    pub pick: VersionPick,
}

impl FromStr for LoaderSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (kind, pick) = match s.split_once('@') {
            Some((k, v)) => (k, parse_pick(v)?),
            None => (s, VersionPick::Recommended),
        };
        let kind = KindArg::from_str(kind, false)
            .map_err(|_| format!("unknown loader {kind:?} (expected forge, neoforge or fabric)"))?;
        Ok(LoaderSpec {
            kind: kind.into(),
            pick,
        })
    }
}
