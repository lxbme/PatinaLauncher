//! Parsing of the loader-related `patina-dev` commands.

use clap::Parser;
use patina_core::loader::ops::LoaderAction;
use patina_core::loader::{LoaderKind, VersionPick};
use patina_dev::cli::{
    AccountCmd, Cli, Cmd, IconCmd, InstanceCmd, KindArg, LoaderActionArg, LoaderCmd, LoaderSpec,
    ModsCmd, SourceArg, SourceCmd, Toggle,
};

fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(std::iter::once("patina-dev").chain(args.iter().copied()))
}

fn instance(args: &[&str]) -> InstanceCmd {
    match parse(args).unwrap().cmd {
        Cmd::Instance { cmd } => cmd,
        other => panic!("not an instance command: {other:?}"),
    }
}

fn loader(args: &[&str]) -> LoaderCmd {
    match instance(args) {
        InstanceCmd::Loader { cmd } => cmd,
        other => panic!("not a loader command: {other:?}"),
    }
}

#[test]
fn create_without_and_with_loader() {
    match instance(&["instance", "create", "van", "1.21.1"]) {
        InstanceCmd::Create {
            name,
            version,
            loader,
        } => {
            assert_eq!((name.as_str(), version.as_str()), ("van", "1.21.1"));
            assert_eq!(loader, None);
        }
        other => panic!("{other:?}"),
    }
    let cases = [
        (
            "fabric@recommended",
            LoaderKind::Fabric,
            VersionPick::Recommended,
        ),
        ("neoforge@latest", LoaderKind::Neoforge, VersionPick::Latest),
        (
            "forge@47.2.0",
            LoaderKind::Forge,
            VersionPick::Exact("47.2.0".into()),
        ),
        ("forge", LoaderKind::Forge, VersionPick::Recommended),
    ];
    for (spec, kind, pick) in cases {
        match instance(&["instance", "create", "x", "latest", "--loader", spec]) {
            InstanceCmd::Create { loader, .. } => {
                assert_eq!(loader, Some(LoaderSpec { kind, pick }), "{spec}")
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn create_rejects_bad_loader_spec() {
    for spec in ["quilt", "quilt@latest", "forge@", "@latest", ""] {
        assert!(
            parse(&["instance", "create", "x", "1.20.1", "--loader", spec]).is_err(),
            "{spec:?} should be rejected"
        );
    }
}

#[test]
fn loader_list() {
    match loader(&["instance", "loader", "list", "fab-new", "fabric"]) {
        LoaderCmd::List { id, kind } => {
            assert_eq!(id, "fab-new");
            assert_eq!(kind, KindArg::Fabric);
            assert_eq!(LoaderKind::from(kind), LoaderKind::Fabric);
        }
        other => panic!("{other:?}"),
    }
    assert!(parse(&["instance", "loader", "list", "fab-new"]).is_err());
    assert!(parse(&["instance", "loader", "list", "fab-new", "quilt"]).is_err());
}

#[test]
fn loader_install() {
    match loader(&["instance", "loader", "install", "a", "neoforge"]) {
        LoaderCmd::Install {
            id,
            kind,
            version,
            replace,
        } => {
            assert_eq!(id, "a");
            assert_eq!(kind, KindArg::Neoforge);
            assert_eq!(version, None);
            assert!(!replace);
        }
        other => panic!("{other:?}"),
    }
    match loader(&[
        "instance",
        "loader",
        "install",
        "a",
        "forge",
        "47.2.0",
        "--replace",
    ]) {
        LoaderCmd::Install {
            kind,
            version,
            replace,
            ..
        } => {
            assert_eq!(kind, KindArg::Forge);
            assert_eq!(version.as_deref(), Some("47.2.0"));
            assert!(replace);
        }
        other => panic!("{other:?}"),
    }
    match loader(&["instance", "loader", "install", "a", "fabric", "latest"]) {
        LoaderCmd::Install { version, .. } => assert_eq!(version.as_deref(), Some("latest")),
        other => panic!("{other:?}"),
    }
    assert!(parse(&["instance", "loader", "install", "a"]).is_err());
    assert!(parse(&["instance", "loader", "install", "a", "liteloader"]).is_err());
    assert!(parse(&["instance", "loader", "install", "a", "forge", "1", "2"]).is_err());
    assert!(parse(&["instance", "loader", "install", "a", "forge", "--force"]).is_err());
}

#[test]
fn version_picks() {
    use patina_dev::cli::parse_pick;
    assert_eq!(parse_pick("recommended"), Ok(VersionPick::Recommended));
    assert_eq!(parse_pick("latest"), Ok(VersionPick::Latest));
    assert_eq!(
        parse_pick("0.16.9"),
        Ok(VersionPick::Exact("0.16.9".into()))
    );
    assert!(parse_pick("").is_err());
}

#[test]
fn loader_remove() {
    match loader(&["instance", "loader", "remove", "neo-1211"]) {
        LoaderCmd::Remove { id } => assert_eq!(id, "neo-1211"),
        other => panic!("{other:?}"),
    }
    assert!(parse(&["instance", "loader", "remove"]).is_err());
}

#[test]
fn set_version() {
    match instance(&["instance", "set-version", "forge-1122", "1.20.1"]) {
        InstanceCmd::SetVersion {
            id,
            mc_version,
            loader_action,
        } => {
            assert_eq!((id.as_str(), mc_version.as_str()), ("forge-1122", "1.20.1"));
            assert_eq!(loader_action, None);
        }
        other => panic!("{other:?}"),
    }
    for (arg, want) in [
        ("replace", LoaderAction::Replace),
        ("remove", LoaderAction::Remove),
    ] {
        match instance(&[
            "instance",
            "set-version",
            "a",
            "1.20.1",
            "--loader-action",
            arg,
        ]) {
            InstanceCmd::SetVersion { loader_action, .. } => {
                let got: LoaderActionArg = loader_action.unwrap();
                assert_eq!(LoaderAction::from(got), want);
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(
        parse(&[
            "instance",
            "set-version",
            "a",
            "1.20.1",
            "--loader-action",
            "keep"
        ])
        .is_err()
    );
    assert!(parse(&["instance", "set-version", "a"]).is_err());
}

#[test]
fn source_set() {
    for (arg, want) in [
        ("auto", SourceArg::Auto),
        ("official", SourceArg::Official),
        ("bmclapi", SourceArg::Bmclapi),
    ] {
        match parse(&["source", "set", arg]).unwrap().cmd {
            Cmd::Source {
                cmd: SourceCmd::Set { source },
            } => assert_eq!(source, want),
            other => panic!("{other:?}"),
        }
    }
    assert!(parse(&["source", "set", "mirror"]).is_err());
}

#[test]
fn launch_unchanged() {
    match parse(&["launch", "fab-new", "--offline", "Dev"])
        .unwrap()
        .cmd
    {
        Cmd::Launch { id, offline, .. } => {
            assert_eq!((id.as_str(), offline.as_deref()), ("fab-new", Some("Dev")))
        }
        other => panic!("{other:?}"),
    }
    // Without --offline the current account is used.
    assert!(parse(&["launch", "fab-new"]).is_ok());
}

fn cmd(args: &[&str]) -> Cmd {
    parse(args).unwrap().cmd
}

#[test]
fn status_kill_logs() {
    assert!(matches!(cmd(&["status"]), Cmd::Status));
    assert!(parse(&["status", "extra"]).is_err());
    match cmd(&["kill", "forge-1201"]) {
        Cmd::Kill { id } => assert_eq!(id, "forge-1201"),
        other => panic!("{other:?}"),
    }
    assert!(parse(&["kill"]).is_err());
    match cmd(&["logs", "a"]) {
        Cmd::Logs { id, follow } => assert_eq!((id.as_str(), follow), ("a", false)),
        other => panic!("{other:?}"),
    }
    match cmd(&["logs", "a", "--follow"]) {
        Cmd::Logs { follow, .. } => assert!(follow),
        other => panic!("{other:?}"),
    }
    assert!(parse(&["logs"]).is_err());
    assert!(parse(&["logs", "a", "--tail"]).is_err());
}

#[test]
fn search_and_sweep() {
    match cmd(&["search", "sodium"]) {
        Cmd::Search { query } => assert_eq!(query, "sodium"),
        other => panic!("{other:?}"),
    }
    assert!(parse(&["search"]).is_err());
    assert!(matches!(cmd(&["sweep"]), Cmd::Sweep));
    assert!(parse(&["sweep", "now"]).is_err());
}

#[test]
fn instance_mods() {
    let mods = |args: &[&str]| match instance(args) {
        InstanceCmd::Mods { id, cmd } => (id, cmd),
        other => panic!("{other:?}"),
    };
    let (id, c) = mods(&["instance", "mods", "fab", "list"]);
    assert_eq!(id, "fab");
    assert!(matches!(c, ModsCmd::List));
    assert!(matches!(
        mods(&["instance", "mods", "fab", "enable", "a.jar"]).1,
        ModsCmd::Enable { file } if file == "a.jar"
    ));
    assert!(matches!(
        mods(&["instance", "mods", "fab", "disable", "a.jar"]).1,
        ModsCmd::Disable { file } if file == "a.jar"
    ));
    assert!(matches!(
        mods(&["instance", "mods", "fab", "delete", "a.jar"]).1,
        ModsCmd::Delete { file } if file == "a.jar"
    ));
    assert!(parse(&["instance", "mods", "fab"]).is_err());
    assert!(parse(&["instance", "mods", "fab", "enable"]).is_err());
    assert!(parse(&["instance", "mods", "fab", "remove", "a.jar"]).is_err());
}

#[test]
fn instance_packs_shaders_saves() {
    assert!(matches!(
        instance(&["instance", "packs", "a"]),
        InstanceCmd::Packs { id } if id == "a"
    ));
    assert!(matches!(
        instance(&["instance", "shaders", "a"]),
        InstanceCmd::Shaders { id } if id == "a"
    ));
    assert!(matches!(
        instance(&["instance", "saves", "a"]),
        InstanceCmd::Saves { id } if id == "a"
    ));
    for sub in ["packs", "shaders", "saves"] {
        assert!(parse(&["instance", sub]).is_err(), "{sub}");
    }
}

#[test]
fn instance_icon() {
    let icon = |args: &[&str]| match instance(args) {
        InstanceCmd::Icon { id, cmd } => (id, cmd),
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        icon(&["instance", "icon", "a", "builtin", "grass"]).1,
        IconCmd::Builtin { name } if name == "grass"
    ));
    assert!(matches!(
        icon(&["instance", "icon", "a", "custom", "/tmp/x.png"]).1,
        IconCmd::Custom { path } if path.to_str() == Some("/tmp/x.png")
    ));
    assert!(matches!(
        icon(&["instance", "icon", "a", "clear"]).1,
        IconCmd::Clear
    ));
    assert!(parse(&["instance", "icon", "a"]).is_err());
    assert!(parse(&["instance", "icon", "a", "builtin"]).is_err());
    assert!(parse(&["instance", "icon", "a", "custom"]).is_err());
    assert!(parse(&["instance", "icon", "a", "clear", "x"]).is_err());
    assert!(parse(&["instance", "icon", "a", "round", "x"]).is_err());
}

#[test]
fn instance_reinstall_loader_and_early_window() {
    assert!(matches!(
        instance(&["instance", "reinstall-loader", "a"]),
        InstanceCmd::ReinstallLoader { id } if id == "a"
    ));
    assert!(parse(&["instance", "reinstall-loader"]).is_err());
    match instance(&["instance", "early-window", "a", "on"]) {
        InstanceCmd::EarlyWindow { id, state } => {
            assert_eq!(id, "a");
            assert_eq!(state, Toggle::On);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        instance(&["instance", "early-window", "a", "off"]),
        InstanceCmd::EarlyWindow {
            state: Toggle::Off,
            ..
        }
    ));
    assert!(parse(&["instance", "early-window", "a"]).is_err());
    assert!(parse(&["instance", "early-window", "a", "maybe"]).is_err());
    assert!(parse(&["instance", "early-window", "a", "ON"]).is_err());
}

#[test]
fn account_commands() {
    let account = |args: &[&str]| match cmd(args) {
        Cmd::Account { cmd } => cmd,
        other => panic!("{other:?}"),
    };
    assert!(matches!(account(&["account", "list"]), AccountCmd::List));
    assert!(matches!(
        account(&["account", "add-offline", "Dev"]),
        AccountCmd::AddOffline { name } if name == "Dev"
    ));
    assert!(matches!(
        account(&["account", "use", "id1"]),
        AccountCmd::Use { id } if id == "id1"
    ));
    assert!(matches!(
        account(&["account", "remove", "id1"]),
        AccountCmd::Remove { id } if id == "id1"
    ));
    for bad in [
        &["account"][..],
        &["account", "add-offline"],
        &["account", "use"],
        &["account", "remove"],
        &["account", "login"],
    ] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn launch_account_and_offline_conflict() {
    match cmd(&["launch", "a"]) {
        Cmd::Launch {
            id,
            account,
            offline,
        } => {
            assert_eq!(id, "a");
            assert_eq!((account, offline), (None, None));
        }
        other => panic!("{other:?}"),
    }
    match cmd(&["launch", "a", "--account", "acc1"]) {
        Cmd::Launch {
            account, offline, ..
        } => assert_eq!((account.as_deref(), offline), (Some("acc1"), None)),
        other => panic!("{other:?}"),
    }
    match cmd(&["launch", "a", "--offline", "Dev"]) {
        Cmd::Launch {
            account, offline, ..
        } => assert_eq!((account, offline.as_deref()), (None, Some("Dev"))),
        other => panic!("{other:?}"),
    }
    let err = parse(&["launch", "a", "--account", "x", "--offline", "Dev"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    assert!(parse(&["launch"]).is_err());
    assert!(parse(&["launch", "a", "--account"]).is_err());
}

#[test]
fn unknown_subcommands_are_rejected() {
    for bad in [&["frobnicate"][..], &["instance", "frobnicate"]] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}
