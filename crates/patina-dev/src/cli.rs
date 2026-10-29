//! Command-line definitions for `patina-dev` (parsed with clap).

use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, Subcommand, ValueEnum};
use patina_core::config::SourceChoice;
use patina_core::loader::LoaderKind;
use patina_core::loader::VersionPick;
use patina_core::loader::ops::LoaderAction;

#[derive(Parser, Debug)]
#[command(name = "patina-dev", version, about = "Patina development CLI")]
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
    /// Launch an instance
    Launch {
        /// Instance ID (directory name)
        id: String,
        /// Offline player name (development only)
        #[arg(long, value_name = "PLAYER")]
        offline: String,
    },
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
