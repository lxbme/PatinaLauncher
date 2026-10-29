//! Parsing of the loader-related `patina-dev` commands.

use clap::Parser;
use patina_core::loader::ops::LoaderAction;
use patina_core::loader::{LoaderKind, VersionPick};
use patina_dev::cli::{
    Cli, Cmd, InstanceCmd, KindArg, LoaderActionArg, LoaderCmd, LoaderSpec, SourceArg, SourceCmd,
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
        Cmd::Launch { id, offline } => {
            assert_eq!((id.as_str(), offline.as_str()), ("fab-new", "Dev"))
        }
        other => panic!("{other:?}"),
    }
    assert!(parse(&["launch", "fab-new"]).is_err());
}
