//! Listing the loader versions available for a Minecraft version.

use std::cmp::Ordering;
use std::io;

use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Deserialize;

use super::LoaderKind;
use crate::download::{DownloadError, Downloader, FailReason};
use crate::instance::InstanceError;
use crate::prepare::PrepareError;

#[derive(Debug, thiserror::Error)]
pub enum LoaderError {
    #[error("{}", rust_i18n::t!("errors.loader.no_versions", kind = .kind.display_name(), mc = .mc))]
    NoVersions { kind: LoaderKind, mc: String },
    #[error("{}", rust_i18n::t!("errors.loader.version_not_found", kind = .kind.display_name(), version = .version))]
    VersionNotFound { kind: LoaderKind, version: String },
    #[error("{}", rust_i18n::t!("errors.loader.parse", error = .0))]
    Parse(String),
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error("{}", rust_i18n::t!("errors.loader.io", error = .0))]
    Io(String),
    /// A background task failed (panicked or was cancelled).
    #[error("{}", rust_i18n::t!("errors.loader.internal", error = .0))]
    Internal(String),
    #[error("{}", rust_i18n::t!("errors.loader.json", error = .0))]
    Json(String),
    #[error("{}", rust_i18n::t!("errors.loader.unsafe_metadata", name = .0))]
    UnsafeMetadata(String),
    #[error("{}", rust_i18n::t!("errors.loader.installer_invalid", reason = .0))]
    InstallerInvalid(InstallerProblem),
    /// Processor `index` (0-based, into the profile's `processors`; kept for
    /// diagnostics) exited unsuccessfully. `step` is its 1-based position
    /// among the client-side processors, as `RunProcessor` progress counts
    /// them, and is the number users see. `exit_code` is `None` when it was
    /// killed by a signal.
    /// `stderr_tail` (its last 4 KiB of stderr) is for diagnostics only: it
    /// shows in `Debug`, never in the localized message.
    #[error("{}", processor_failed_text(*.step, *.exit_code))]
    ProcessorFailed {
        index: usize,
        step: usize,
        exit_code: Option<i32>,
        stderr_tail: String,
    },
    /// A processor output does not have the SHA-1 the installer declares.
    #[error("{}", rust_i18n::t!("errors.loader.processor_output_mismatch", path = .path))]
    ProcessorOutputMismatch { path: String },
    /// A legacy Forge library with `checksums` could be obtained neither as
    /// a plain jar nor as a `.pack.xz`; `name` is its maven coordinate.
    #[error("{}", rust_i18n::t!("errors.loader.legacy_library_unavailable", name = .name))]
    LegacyLibraryUnavailable { name: String },
    /// The instance already has another kind of loader; replacing it needs
    /// the user's confirmation.
    #[error("{}", rust_i18n::t!("errors.loader.conflict", existing = .existing.display_name(), version = .existing_version))]
    Conflict {
        existing: LoaderKind,
        existing_version: String,
    },
    /// The installed loader version does not support the requested
    /// Minecraft version; `can_replace` tells whether some version of the
    /// same loader does.
    #[error("{}", incompatible_text(*.kind, .version, *.can_replace))]
    Incompatible {
        kind: LoaderKind,
        version: String,
        can_replace: bool,
    },
    #[error("{}", rust_i18n::t!("errors.loader.instance", error = .0))]
    Instance(String),
    #[error("{}", rust_i18n::t!("errors.loader.prepare", error = .0))]
    Prepare(String),
}

impl From<io::Error> for LoaderError {
    fn from(e: io::Error) -> Self {
        LoaderError::Io(e.to_string())
    }
}

impl From<tokio::task::JoinError> for LoaderError {
    fn from(e: tokio::task::JoinError) -> Self {
        LoaderError::Internal(e.to_string())
    }
}

impl From<serde_json::Error> for LoaderError {
    fn from(e: serde_json::Error) -> Self {
        LoaderError::Json(e.to_string())
    }
}

impl From<InstanceError> for LoaderError {
    fn from(e: InstanceError) -> Self {
        LoaderError::Instance(e.to_string())
    }
}

impl From<PrepareError> for LoaderError {
    fn from(e: PrepareError) -> Self {
        LoaderError::Prepare(e.to_string())
    }
}

impl From<InstallerProblem> for LoaderError {
    fn from(p: InstallerProblem) -> Self {
        LoaderError::InstallerInvalid(p)
    }
}

/// Why a loader installer cannot be used. `detail` fields carry the
/// underlying (English) library error for `Debug` only; they are never shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallerProblem {
    NotModern,
    NotLegacy,
    NeoforgeLegacy,
    NoFabricInstaller,
    /// The installer targets Minecraft `found`, but `expected` is installed.
    WrongMinecraft {
        expected: String,
        found: String,
    },
    MissingEntry {
        name: String,
    },
    UnreadableEntry {
        name: String,
        detail: String,
    },
    UnreadableArchive {
        detail: String,
    },
    MissingInstallKey {
        key: String,
    },
    DataNotObject,
    DataNotString {
        key: String,
        side: String,
    },
    ArgBadEscape {
        arg: String,
    },
    ArgUnclosed {
        arg: String,
    },
    ArgUnknownKey {
        arg: String,
        key: String,
    },
    UnreadableJar {
        path: String,
        detail: String,
    },
    NoMainClass {
        path: String,
    },
    MissingProcessorLibrary {
        coord: String,
    },
    MojmapsNoOutput,
    NoClientMappings {
        version: String,
    },
    MinecraftJarMissing {
        path: String,
    },
    NonUtf8Path {
        path: String,
    },
}

impl std::fmt::Display for InstallerProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use InstallerProblem::*;
        let text = match self {
            NotModern => rust_i18n::t!("errors.loader.installer.not_modern"),
            NotLegacy => rust_i18n::t!("errors.loader.installer.not_legacy"),
            NeoforgeLegacy => rust_i18n::t!("errors.loader.installer.neoforge_legacy"),
            NoFabricInstaller => rust_i18n::t!("errors.loader.installer.no_fabric_installer"),
            WrongMinecraft { expected, found } => rust_i18n::t!(
                "errors.loader.installer.wrong_minecraft",
                expected = expected,
                found = found
            ),
            MissingEntry { name } => {
                rust_i18n::t!("errors.loader.installer.missing_entry", name = name)
            }
            UnreadableEntry { name, .. } => {
                rust_i18n::t!("errors.loader.installer.unreadable_entry", name = name)
            }
            UnreadableArchive { .. } => rust_i18n::t!("errors.loader.installer.unreadable_archive"),
            MissingInstallKey { key } => {
                rust_i18n::t!("errors.loader.installer.missing_install_key", key = key)
            }
            DataNotObject => rust_i18n::t!("errors.loader.installer.data_not_object"),
            DataNotString { key, side } => rust_i18n::t!(
                "errors.loader.installer.data_not_string",
                key = key,
                side = side
            ),
            ArgBadEscape { arg } => {
                rust_i18n::t!("errors.loader.installer.arg_bad_escape", arg = arg)
            }
            ArgUnclosed { arg } => rust_i18n::t!("errors.loader.installer.arg_unclosed", arg = arg),
            ArgUnknownKey { arg, key } => rust_i18n::t!(
                "errors.loader.installer.arg_unknown_key",
                arg = arg,
                key = key
            ),
            UnreadableJar { path, .. } => {
                rust_i18n::t!("errors.loader.installer.unreadable_jar", path = path)
            }
            NoMainClass { path } => {
                rust_i18n::t!("errors.loader.installer.no_main_class", path = path)
            }
            MissingProcessorLibrary { coord } => rust_i18n::t!(
                "errors.loader.installer.missing_processor_library",
                coord = coord
            ),
            MojmapsNoOutput => rust_i18n::t!("errors.loader.installer.mojmaps_no_output"),
            MinecraftJarMissing { path } => {
                rust_i18n::t!("errors.loader.installer.minecraft_jar_missing", path = path)
            }
            NonUtf8Path { path } => {
                rust_i18n::t!("errors.loader.installer.non_utf8_path", path = path)
            }
            NoClientMappings { version } => rust_i18n::t!(
                "errors.loader.installer.no_client_mappings",
                version = version
            ),
        };
        f.write_str(&text)
    }
}

fn incompatible_text(kind: LoaderKind, version: &str, can_replace: bool) -> String {
    let kind = kind.display_name();
    if can_replace {
        rust_i18n::t!("errors.loader.incompatible", kind = kind, version = version)
    } else {
        rust_i18n::t!(
            "errors.loader.incompatible_no_replacement",
            kind = kind,
            version = version
        )
    }
    .to_string()
}

/// `step` is already 1-based.
fn processor_failed_text(step: usize, exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => rust_i18n::t!("errors.loader.processor_failed", step = step, code = code),
        None => rust_i18n::t!("errors.loader.processor_killed", step = step),
    }
    .to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderVersion {
    pub kind: LoaderKind,
    pub mc: String,
    /// The version shown to the user.
    pub version: String,
    /// The version string used to build maven URLs.
    pub maven_version: String,
    pub stable: bool,
    pub recommended: bool,
    pub latest: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionPick {
    Recommended,
    Latest,
    Exact(String),
}

impl VersionPick {
    pub fn pick<'a>(&self, list: &'a [LoaderVersion]) -> Option<&'a LoaderVersion> {
        match self {
            VersionPick::Recommended => list
                .iter()
                .find(|v| v.recommended)
                .or_else(|| list.iter().find(|v| v.stable))
                .or_else(|| list.first()),
            VersionPick::Latest => list.iter().find(|v| v.latest).or_else(|| list.first()),
            VersionPick::Exact(e) => list
                .iter()
                .find(|v| &v.version == e || &v.maven_version == e),
        }
    }
}

#[derive(Deserialize)]
struct FabricEntry {
    loader: FabricLoader,
}

#[derive(Deserialize)]
struct FabricLoader {
    version: String,
    #[serde(default)]
    stable: bool,
}

pub fn parse_fabric_list(json: &[u8], mc: &str) -> Result<Vec<LoaderVersion>, LoaderError> {
    let entries: Vec<FabricEntry> = serde_json::from_slice(json)?;
    let mut out: Vec<LoaderVersion> = entries
        .into_iter()
        .map(|e| LoaderVersion {
            kind: LoaderKind::Fabric,
            mc: mc.to_string(),
            maven_version: e.loader.version.clone(),
            version: e.loader.version,
            stable: e.loader.stable,
            recommended: false,
            latest: false,
        })
        .collect();
    if let Some(first) = out.first_mut() {
        first.latest = true;
    }
    if let Some(s) = out.iter_mut().find(|v| v.stable) {
        s.recommended = true;
    }
    Ok(out)
}

/// Reads every `<versions><version>` text node of a maven-metadata.xml.
fn maven_versions(xml: &str) -> Result<Vec<String>, LoaderError> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut in_versions = false;
    let mut in_version = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                "versions" => in_versions = true,
                "version" if in_versions => in_version = true,
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                "versions" => in_versions = false,
                "version" => in_version = false,
                _ => {}
            },
            Ok(Event::Text(t)) if in_version => {
                let s: &str = t.as_ref();
                out.push(s.trim().to_string());
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(LoaderError::Parse(e.to_string())),
            _ => {}
        }
    }
    if out.is_empty() {
        return Err(LoaderError::Parse("no <version> entries".into()));
    }
    Ok(out)
}

/// Numeric dotted segments of the leading part of a version, e.g.
/// `47.1.106` or `26.1.0.1-beta` -> `[26, 1, 0, 1]`.
fn numeric_key(v: &str) -> Vec<u64> {
    let head = v.split(['-', '+']).next().unwrap_or(v);
    head.split('.')
        .map(|s| s.parse::<u64>().unwrap_or(0))
        .collect()
}

/// Newest first: higher numbers first, a release before its pre-releases.
fn newest_first(a: &LoaderVersion, b: &LoaderVersion) -> Ordering {
    numeric_key(&b.version)
        .cmp(&numeric_key(&a.version))
        .then_with(|| b.stable.cmp(&a.stable))
        .then_with(|| b.version.cmp(&a.version))
}

pub fn parse_forge(
    metadata_xml: &str,
    promotions: Option<&[u8]>,
    mc: &str,
) -> Result<Vec<LoaderVersion>, LoaderError> {
    let prefix = format!("{mc}-");
    let mut out: Vec<LoaderVersion> = Vec::new();
    for maven in maven_versions(metadata_xml)? {
        if maven.contains("_pre") || maven.contains("prerelease") {
            continue;
        }
        let Some(rest) = maven.strip_prefix(&prefix) else {
            continue;
        };
        // `forgever[-branch]`
        let version = rest.split('-').next().unwrap_or(rest);
        if version.is_empty() {
            continue;
        }
        out.push(LoaderVersion {
            kind: LoaderKind::Forge,
            mc: mc.to_string(),
            version: version.to_string(),
            maven_version: maven.clone(),
            stable: true,
            recommended: false,
            latest: false,
        });
    }
    out.sort_by(newest_first);

    #[derive(Deserialize)]
    struct P {
        promos: std::collections::HashMap<String, String>,
    }
    // Promotions only mark versions: an unparseable body (an error page, a
    // changed format) counts as missing, not as a failed listing.
    let promos = promotions
        .and_then(|bytes| serde_json::from_slice::<P>(bytes).ok())
        .map(|p| p.promos);
    let promo = |kind: &str| promos.as_ref().and_then(|p| p.get(&format!("{mc}-{kind}")));
    let rec = promo("recommended");
    let lat = promo("latest");
    for v in &mut out {
        v.recommended = rec == Some(&v.version);
        v.latest = lat == Some(&v.version);
    }
    if !out.iter().any(|v| v.latest)
        && let Some(first) = out.first_mut()
    {
        first.latest = true;
    }
    Ok(out)
}

/// Maps a NeoForge version to the Minecraft version it targets.
fn neoforge_mc(version: &str) -> Option<String> {
    let segs: Vec<&str> = version.split(['-', '+']).next()?.split('.').collect();
    let major: u32 = segs.first()?.parse().ok()?;
    let minor: u32 = segs.get(1)?.parse().ok()?;
    let third: u32 = segs.get(2)?.parse().ok()?;
    if major >= 26 {
        Some(if third == 0 {
            format!("{major}.{minor}")
        } else {
            format!("{major}.{minor}.{third}")
        })
    } else if minor == 0 {
        Some(format!("1.{major}"))
    } else {
        Some(format!("1.{major}.{minor}"))
    }
}

pub fn parse_neoforge(metadata_xml: &str, mc: &str) -> Result<Vec<LoaderVersion>, LoaderError> {
    let mut out: Vec<LoaderVersion> = maven_versions(metadata_xml)?
        .into_iter()
        .filter(|v| neoforge_mc(v).as_deref() == Some(mc))
        .map(|v| LoaderVersion {
            kind: LoaderKind::Neoforge,
            mc: mc.to_string(),
            stable: !v.contains('-'),
            maven_version: v.clone(),
            version: v,
            recommended: false,
            latest: false,
        })
        .collect();
    out.sort_by(newest_first);
    mark_first(&mut out);
    Ok(out)
}

/// `net/neoforged/forge` versions look like `1.20.1-47.1.106`.
pub fn parse_neoforge_legacy(
    metadata_xml: &str,
    mc: &str,
) -> Result<Vec<LoaderVersion>, LoaderError> {
    let prefix = format!("{mc}-");
    let mut out: Vec<LoaderVersion> = maven_versions(metadata_xml)?
        .into_iter()
        .filter_map(|maven| {
            let version = maven.strip_prefix(&prefix)?.to_string();
            Some(LoaderVersion {
                kind: LoaderKind::Neoforge,
                mc: mc.to_string(),
                stable: !version.contains('-'),
                version,
                maven_version: maven,
                recommended: false,
                latest: false,
            })
        })
        .collect();
    out.sort_by(newest_first);
    mark_first(&mut out);
    Ok(out)
}

/// `latest` goes on the first entry, `recommended` on the first stable one.
fn mark_first(list: &mut [LoaderVersion]) {
    if let Some(first) = list.first_mut() {
        first.latest = true;
    }
    if let Some(s) = list.iter_mut().find(|v| v.stable) {
        s.recommended = true;
    }
}

pub async fn list_versions(
    dl: &Downloader,
    kind: LoaderKind,
    mc: &str,
) -> Result<Vec<LoaderVersion>, LoaderError> {
    let no_versions = || LoaderError::NoVersions {
        kind,
        mc: mc.to_string(),
    };
    let list = match kind {
        LoaderKind::Fabric => {
            let url = format!("https://meta.fabricmc.net/v2/versions/loader/{mc}");
            match dl.fetch_bytes(&url, None).await {
                Ok(bytes) => parse_fabric_list(&bytes, mc)?,
                // An unknown Minecraft version is a 404 (or 400) from the meta server.
                Err(DownloadError::Failed {
                    reason: FailReason::Http(400 | 404),
                    ..
                }) => return Err(no_versions()),
                Err(e) => return Err(e.into()),
            }
        }
        LoaderKind::Forge => {
            let xml = dl
                .fetch_bytes(
                    "https://maven.minecraftforge.net/net/minecraftforge/forge/maven-metadata.xml",
                    None,
                )
                .await?;
            let promos = dl
                .fetch_bytes(
                    "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json",
                    None,
                )
                .await
                .ok();
            parse_forge(&utf8(xml)?, promos.as_deref(), mc)?
        }
        LoaderKind::Neoforge => {
            if mc == "1.20.1" {
                let xml = dl
                    .fetch_bytes(
                        "https://maven.neoforged.net/releases/net/neoforged/forge/maven-metadata.xml",
                        None,
                    )
                    .await?;
                parse_neoforge_legacy(&utf8(xml)?, mc)?
            } else {
                let xml = dl
                    .fetch_bytes(
                        "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml",
                        None,
                    )
                    .await?;
                parse_neoforge(&utf8(xml)?, mc)?
            }
        }
    };
    if list.is_empty() {
        return Err(no_versions());
    }
    Ok(list)
}

fn utf8(bytes: Vec<u8>) -> Result<String, LoaderError> {
    String::from_utf8(bytes).map_err(|e| LoaderError::Parse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::Mirror;
    use crate::i18n::{Language, locale_lock, set_language};
    use crate::loader::forge::test_support::offline_client;
    use std::time::Duration;
    use wiremock::matchers::{path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn every_problem() -> Vec<InstallerProblem> {
        let s = |v: &str| v.to_string();
        vec![
            InstallerProblem::NotModern,
            InstallerProblem::NotLegacy,
            InstallerProblem::NeoforgeLegacy,
            InstallerProblem::NoFabricInstaller,
            InstallerProblem::WrongMinecraft {
                expected: s("1.21.1"),
                found: s("1.20.1"),
            },
            InstallerProblem::MissingEntry {
                name: s("install_profile.json"),
            },
            InstallerProblem::UnreadableEntry {
                name: s("version.json"),
                detail: s("invalid Zip archive"),
            },
            InstallerProblem::UnreadableArchive {
                detail: s("invalid Zip archive"),
            },
            InstallerProblem::MissingInstallKey { key: s("path") },
            InstallerProblem::DataNotObject,
            InstallerProblem::DataNotString {
                key: s("MAPPINGS"),
                side: s("client"),
            },
            InstallerProblem::ArgBadEscape { arg: s("{A") },
            InstallerProblem::ArgUnclosed { arg: s("{A") },
            InstallerProblem::ArgUnknownKey {
                arg: s("{A}"),
                key: s("A"),
            },
            InstallerProblem::UnreadableJar {
                path: s("/lib/x.jar"),
                detail: s("invalid Zip archive"),
            },
            InstallerProblem::NoMainClass {
                path: s("/lib/x.jar"),
            },
            InstallerProblem::MissingProcessorLibrary {
                coord: s("x:absent:1"),
            },
            InstallerProblem::MojmapsNoOutput,
            InstallerProblem::NoClientMappings {
                version: s("1.21.1"),
            },
            InstallerProblem::MinecraftJarMissing {
                path: s("/data/versions/1.21.1/1.21.1.jar"),
            },
            InstallerProblem::NonUtf8Path {
                path: s("/data/\u{FFFD}"),
            },
        ]
    }

    #[test]
    fn installer_problems_localized() {
        let _locale = locale_lock();
        set_language(Language::ZhCn);
        // Values that legitimately stay as written (coordinates, paths, versions, names).
        let literals = [
            "/data/versions/1.21.1/1.21.1.jar",
            "/data/\u{FFFD}",
            "1.21.1",
            "1.20.1",
            "install_profile.json",
            "version.json",
            "path",
            "MAPPINGS",
            "client",
            "{A",
            "{A}",
            "A",
            "/lib/x.jar",
            "x:absent:1",
            "Minecraft",
            "NeoForge",
            "Fabric",
            "jar",
            "Main-Class",
            "UTF",
            "versionInfo",
            "processors",
            "install",
            "data",
            "DOWNLOAD_MOJMAPS",
            "processors",
        ];
        for p in every_problem() {
            let mut text = p.to_string();
            assert!(!text.is_empty());
            for lit in literals {
                text = text.replace(lit, " ");
            }
            let english = text
                .split(|c: char| !c.is_ascii_alphabetic())
                .any(|w| !w.is_empty());
            assert!(!english, "{p:?} -> {}", p);
            // The library detail never leaks into the message.
            assert!(!p.to_string().contains("Zip"), "{p:?}");
        }
        set_language(Language::En);
        let en = LoaderError::InstallerInvalid(InstallerProblem::MissingProcessorLibrary {
            coord: "x:absent:1".into(),
        });
        assert!(en.to_string().contains("x:absent:1") && en.to_string().contains("missing"));
        set_language(Language::ZhCn);
    }

    #[tokio::test]
    async fn join_error_is_internal() {
        let join = tokio::task::spawn_blocking(|| -> u8 { panic!("boom") })
            .await
            .unwrap_err();
        assert!(matches!(LoaderError::from(join), LoaderError::Internal(_)));
    }

    #[test]
    fn wrapped_loader_errors_have_no_source() {
        use std::error::Error;
        let errs: Vec<LoaderError> = vec![
            io::Error::other("boom").into(),
            serde_json::from_str::<u8>("x").unwrap_err().into(),
            InstanceError::NotFound("x".into()).into(),
            PrepareError::Internal("boom".into()).into(),
            LoaderError::Internal("boom".into()),
            InstallerProblem::NotModern.into(),
        ];
        for e in errs {
            assert!(e.source().is_none(), "{e:?}");
        }
    }

    const FABRIC: &[u8] = include_bytes!("../../tests/fixtures/fabric_loader_list_1.21.1.json");
    const FORGE_XML: &str = include_str!("../../tests/fixtures/forge_maven_metadata.xml");
    const PROMOS: &[u8] = include_bytes!("../../tests/fixtures/forge_promotions_slim.json");
    const NEO_XML: &str = include_str!("../../tests/fixtures/neoforge_maven_metadata.xml");
    const NEO_LEGACY: &str =
        include_str!("../../tests/fixtures/neoforge_legacy_forge_maven_metadata.xml");

    #[test]
    fn fabric_list_marks_recommended() {
        let l = parse_fabric_list(FABRIC, "1.21.1").unwrap();
        assert!(l[0].latest && l.iter().filter(|v| v.latest).count() == 1);
        let first_stable = l.iter().position(|v| v.stable).unwrap();
        assert!(l[first_stable].recommended);
        assert_eq!(l.iter().filter(|v| v.recommended).count(), 1);
        let got = VersionPick::Recommended.pick(&l).unwrap();
        assert_eq!(got.version, l[first_stable].version);
        assert_eq!(got.kind, LoaderKind::Fabric);
    }

    #[test]
    fn forge_promotions_and_branch_suffix() {
        let l = parse_forge(FORGE_XML, Some(PROMOS), "1.7.10").unwrap();
        let v = l.iter().find(|v| v.version == "10.13.4.1614").unwrap();
        assert_eq!(v.maven_version, "1.7.10-10.13.4.1614-1.7.10");
        assert!(v.recommended && v.latest);
        assert!(l.iter().all(|v| !v.maven_version.contains("prerelease")));
        assert!(l.iter().all(|v| !v.maven_version.contains("_pre")));
        assert!(l.iter().any(
            |v| v.maven_version == "1.7.10-10.13.3.1401-1710ls" && v.version == "10.13.3.1401"
        ));
        assert_eq!(l[0].version, "10.13.4.1614");

        let l = parse_forge(FORGE_XML, Some(PROMOS), "1.12.2").unwrap();
        assert_eq!(
            VersionPick::Recommended.pick(&l).unwrap().version,
            "14.23.5.2859"
        );
        assert_eq!(
            VersionPick::Latest.pick(&l).unwrap().version,
            "14.23.5.2864"
        );
        // The pre-release mc string never matches a real mc request either.
        assert!(
            parse_forge(FORGE_XML, Some(PROMOS), "1.7.10_pre4")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn forge_without_promotions_uses_first_as_latest() {
        let l = parse_forge(FORGE_XML, None, "1.12.2").unwrap();
        assert!(l[0].latest && !l[0].recommended);
        assert_eq!(l.iter().filter(|v| v.latest).count(), 1);
        assert_eq!(VersionPick::Latest.pick(&l).unwrap().version, l[0].version);
        // Recommended falls back to the first stable one.
        assert_eq!(
            VersionPick::Recommended.pick(&l).unwrap().version,
            l[0].version
        );
    }

    #[test]
    fn malformed_promotions_are_ignored() {
        let none = parse_forge(FORGE_XML, None, "1.12.2").unwrap();
        for bad in [&b"<html>502</html>"[..], b"", b"{\"promos\": 3}", b"[]"] {
            let l = parse_forge(FORGE_XML, Some(bad), "1.12.2").unwrap();
            assert_eq!(l, none, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn processor_failure_shows_the_client_step() {
        let _locale = locale_lock();
        set_language(Language::En);
        let failed = LoaderError::ProcessorFailed {
            index: 4,
            step: 2,
            exit_code: Some(1),
            stderr_tail: String::new(),
        };
        let killed = LoaderError::ProcessorFailed {
            index: 4,
            step: 2,
            exit_code: None,
            stderr_tail: String::new(),
        };
        for e in [&failed, &killed] {
            let s = e.to_string();
            assert!(s.contains("step 2 ") && !s.contains('5'), "{s}");
        }
    }

    #[test]
    fn neoforge_version_mapping() {
        let l = parse_neoforge(NEO_XML, "26.3").unwrap();
        assert!(l.iter().any(|v| v.version == "26.3.0.99"));
        assert_eq!(l[0].version, "26.3.0.99");
        assert!(l[0].latest);
        let l = parse_neoforge(NEO_XML, "1.21.1").unwrap();
        assert!(l.iter().any(|v| v.version == "21.1.265"));
        let l = parse_neoforge(NEO_XML, "26.1.2").unwrap();
        assert!(l.iter().any(|v| v.version == "26.1.2.124"));
        assert!(l.iter().all(|v| !v.version.starts_with("26.3")));
        let l = parse_neoforge(NEO_XML, "1.21").unwrap();
        assert!(l.iter().any(|v| v.version == "21.0.149"));
        let beta = l.iter().find(|v| v.version == "21.0.139-beta").unwrap();
        assert!(!beta.stable);
        assert!(l.iter().find(|v| v.version == "21.0.149").unwrap().stable);
        assert!(VersionPick::Recommended.pick(&l).unwrap().stable);
        assert_eq!(neoforge_mc("26.1.2.123").as_deref(), Some("26.1.2"));
    }

    #[test]
    fn neoforge_legacy_1_20_1() {
        let l = parse_neoforge_legacy(NEO_LEGACY, "1.20.1").unwrap();
        let v = &l[0];
        assert_eq!(v.version, "47.1.106");
        assert_eq!(v.maven_version, "1.20.1-47.1.106");
        assert!(v.latest);
        // Numeric, not lexical, ordering.
        assert_eq!(l.last().unwrap().version, "47.1.7");
    }

    #[test]
    fn exact_pick_by_version_or_maven_version() {
        let l = parse_forge(FORGE_XML, Some(PROMOS), "1.7.10").unwrap();
        let a = VersionPick::Exact("10.13.4.1614".into()).pick(&l).unwrap();
        let b = VersionPick::Exact("1.7.10-10.13.4.1614-1.7.10".into())
            .pick(&l)
            .unwrap();
        assert_eq!(a, b);
        assert!(VersionPick::Exact("nope".into()).pick(&l).is_none());
        assert!(VersionPick::Latest.pick(&[]).is_none());
    }

    /// The Fabric and NeoForge hosts map to `server/fabric` and `server/neo`,
    /// with a fallback mirror at `server/fb/...`. The real hosts are
    /// unreachable, so a request never leaves the machine: the "original URL"
    /// candidate between the two mirrors fails with a connection error.
    fn dl_for(server: &MockServer) -> Downloader {
        let mirror = |id: &str, base: String| Mirror {
            id: id.into(),
            rules: vec![
                ("https://meta.fabricmc.net".into(), format!("{base}/fabric")),
                (
                    "https://maven.neoforged.net/releases".into(),
                    format!("{base}/neo"),
                ),
            ],
        };
        Downloader::new(offline_client(), mirror("m", server.uri()))
            .with_fallback(Some(mirror("fb", format!("{}/fb", server.uri()))))
            .with_retry(1, Duration::ZERO)
    }

    #[tokio::test]
    async fn unsupported_mc_version_is_reported() {
        {
            let _locale = locale_lock();
            let e = parse_fabric_list(b"[]", "1.2.3")
                .map(|l| {
                    if l.is_empty() {
                        Err(LoaderError::NoVersions {
                            kind: LoaderKind::Fabric,
                            mc: "1.2.3".into(),
                        })
                    } else {
                        Ok(l)
                    }
                })
                .unwrap()
                .unwrap_err();
            set_language(Language::ZhCn);
            let zh = e.to_string();
            assert!(zh.contains("1.2.3") && !zh.is_ascii(), "{zh}");
            set_language(Language::En);
            assert!(e.to_string().is_ascii());
        }

        let server = MockServer::start().await;
        // Fabric: empty list.
        Mock::given(path("/fabric/v2/versions/loader/1.2.3"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&server)
            .await;
        // Fabric: 404 for an unknown mc, from both mirrors (the last
        // candidate's answer is the one reported).
        Mock::given(path_regex(r"^(/fb)?/fabric/v2/versions/loader/9\.9\.9$"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        // NeoForge: a mc with no entries.
        Mock::given(path("/neo/net/neoforged/neoforge/maven-metadata.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(NEO_XML))
            .mount(&server)
            .await;
        let dl = dl_for(&server);
        for (kind, mc) in [
            (LoaderKind::Fabric, "1.2.3"),
            (LoaderKind::Fabric, "9.9.9"),
            (LoaderKind::Neoforge, "1.2.3"),
        ] {
            let r = list_versions(&dl, kind, mc).await;
            assert!(
                matches!(&r, Err(LoaderError::NoVersions { kind: k, mc: m }) if *k == kind && m == mc),
                "{kind:?} {mc}: {r:?}"
            );
        }
        let ok = list_versions(&dl, LoaderKind::Neoforge, "1.21.1")
            .await
            .unwrap();
        assert!(ok.iter().any(|v| v.version == "21.1.265"));
    }
}
