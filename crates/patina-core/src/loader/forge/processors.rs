//! Running the processors of modern Forge / NeoForge installers (client side).

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Deserialize;
use serde_json::Value;
use zip::ZipArchive;
use zip::result::ZipError;

use crate::download::mirror::canonical_url;
use crate::download::{DownloadJob, Downloader, sha1_file};
use crate::fsutil::{is_safe_rel, safe_join};
use crate::loader::list::LoaderError;
use crate::loader::{InstallPhase, ProgressSink, report};
use crate::meta::{VersionJson, maven_path};
use crate::platform::CLASSPATH_SEPARATOR;

/// The side Patina installs.
pub const CLIENT: &str = "client";

/// Everything processor arguments can refer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessorEnv {
    /// Resolved `data` entries (see [`resolve_data`]): keys without braces,
    /// values final.
    pub data: HashMap<String, String>,
    pub minecraft_jar: PathBuf,
    pub minecraft_version: String,
    /// The data root (holds `libraries/` and `versions/`).
    pub root: PathBuf,
    pub installer: PathBuf,
    pub library_dir: PathBuf,
}

impl ProcessorEnv {
    /// Built-in variables first, then `data`.
    fn lookup(&self, key: &str) -> Result<Option<String>, LoaderError> {
        let path = match key {
            "MINECRAFT_JAR" => &self.minecraft_jar,
            "ROOT" => &self.root,
            "INSTALLER" => &self.installer,
            "LIBRARY_DIR" => &self.library_dir,
            "MINECRAFT_VERSION" => return Ok(Some(self.minecraft_version.clone())),
            "SIDE" => return Ok(Some(CLIENT.to_string())),
            _ => return Ok(self.data.get(key).cloned()),
        };
        path_str(path).map(Some)
    }
}

/// One entry of `install_profile.json`'s `processors`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Processor {
    pub jar: String,
    #[serde(default)]
    pub classpath: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub outputs: HashMap<String, String>,
    #[serde(default)]
    pub sides: Vec<String>,
}

impl Processor {
    /// No `sides` means every side.
    pub fn runs_on_client(&self) -> bool {
        self.sides.is_empty() || self.sides.iter().any(|s| s == CLIENT)
    }

    /// `--task DOWNLOAD_MOJMAPS`: the launcher downloads the mappings itself
    /// (through its mirrors) instead of letting the processor fetch them.
    fn is_download_mojmaps(&self) -> bool {
        self.args
            .windows(2)
            .any(|w| w[0] == "--task" && w[1] == "DOWNLOAD_MOJMAPS")
    }
}

fn invalid(msg: impl std::fmt::Display) -> LoaderError {
    LoaderError::InstallerInvalid(msg.to_string())
}

/// Processor arguments are strings, so paths must be valid UTF-8.
fn path_str(p: &Path) -> Result<String, LoaderError> {
    p.to_str().map(str::to_string).ok_or_else(|| {
        LoaderError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("path is not valid UTF-8: {}", p.display()),
        ))
    })
}

/// `libraries/<maven path of coord>`, contained in `libraries`.
pub(crate) fn library_path(coord: &str, libraries: &Path) -> Result<PathBuf, LoaderError> {
    maven_path(coord)
        .and_then(|rel| safe_join(libraries, &rel))
        .ok_or_else(|| LoaderError::UnsafeMetadata(coord.to_string()))
}

/// `expanded` relative to the library dir (`/`-separated), when it lies
/// inside it: lexically, after `strip_prefix`, the rest must be a safe
/// relative path (no `..`, no empty or `.` segments).
fn library_rel(expanded: &str, env: &ProcessorEnv) -> Option<String> {
    let rel = Path::new(expanded).strip_prefix(&env.library_dir).ok()?;
    let rel = rel.to_str()?;
    is_safe_rel(rel).then(|| rel.to_string())
}

/// An expanded path the launcher itself writes or deletes (processor
/// outputs, the mappings download): it must lie inside the library dir,
/// else `UnsafeMetadata`.
fn confined_path(expanded: &str, env: &ProcessorEnv) -> Result<PathBuf, LoaderError> {
    library_rel(expanded, env)
        .and_then(|rel| safe_join(&env.library_dir, &rel))
        .ok_or_else(|| LoaderError::UnsafeMetadata(expanded.to_string()))
}

/// `[coord]` → `coord`.
fn bracketed(s: &str) -> Option<&str> {
    s.strip_prefix('[')?.strip_suffix(']')
}

/// Resolves the `side` value of every `data` entry (entries without one are
/// left out): `[coord]` → library path, `'literal'` → `literal`, `/path` →
/// that installer entry, extracted into `extracted_dir`; anything else is
/// kept verbatim.
pub fn resolve_data(
    raw: &Value,
    side: &str,
    libraries: &Path,
    installer: &Path,
    extracted_dir: &Path,
) -> Result<HashMap<String, String>, LoaderError> {
    let Some(entries) = raw.as_object() else {
        return if raw.is_null() {
            Ok(HashMap::new())
        } else {
            Err(invalid("install_profile.json data is not an object"))
        };
    };
    let mut out = HashMap::new();
    let mut extract = Vec::new();
    for (key, entry) in entries {
        let Some(value) = entry.get(side) else {
            continue;
        };
        let value = value
            .as_str()
            .ok_or_else(|| invalid(format!("data.{key}.{side} is not a string")))?;
        let resolved = if let Some(coord) = bracketed(value) {
            path_str(&library_path(coord, libraries)?)?
        } else if let Some(lit) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            lit.to_string()
        } else if let Some(name) = value.strip_prefix('/') {
            let dest = safe_join(extracted_dir, name)
                .ok_or_else(|| LoaderError::UnsafeMetadata(value.to_string()))?;
            let resolved = path_str(&dest)?;
            extract.push((name.to_string(), dest));
            resolved
        } else {
            value.to_string()
        };
        out.insert(key.clone(), resolved);
    }
    extract_entries(installer, &extract, true)?;
    Ok(out)
}

/// Copies jar entries to their destinations (through `.part` files) and
/// returns the destinations written. A missing entry is an error when
/// `required`, else skipped.
pub(crate) fn extract_entries(
    jar: &Path,
    entries: &[(String, PathBuf)],
    required: bool,
) -> Result<Vec<PathBuf>, LoaderError> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut zip = ZipArchive::new(File::open(jar)?).map_err(invalid)?;
    let mut written = Vec::new();
    for (name, dest) in entries {
        let mut entry = match zip.by_name(name) {
            Ok(e) => e,
            Err(ZipError::FileNotFound) if !required => continue,
            Err(e) => return Err(invalid(format!("{name}: {e}"))),
        };
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut part = dest.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        let copied = File::create(&part).and_then(|mut f| {
            std::io::copy(&mut entry, &mut f)?;
            f.sync_all()
        });
        if let Err(e) = copied {
            let _ = std::fs::remove_file(&part);
            return Err(e.into());
        }
        std::fs::rename(&part, dest)?;
        written.push(dest.clone());
    }
    Ok(written)
}

/// Expands one processor argument (or `outputs` key / value): `[coord]` →
/// library path; otherwise `{KEY}` tokens are replaced (built-ins
/// `MINECRAFT_JAR`, `MINECRAFT_VERSION`, `SIDE`, `ROOT`, `INSTALLER`,
/// `LIBRARY_DIR`, then `data`), `'…'` is literal and `\` escapes the next
/// character, as the official installer does. An unknown key is
/// `InstallerInvalid`.
pub fn expand_arg(arg: &str, env: &ProcessorEnv) -> Result<String, LoaderError> {
    if let Some(coord) = bracketed(arg) {
        return path_str(&library_path(coord, &env.library_dir)?);
    }
    let bad = |why: &str| invalid(format!("processor argument {arg:?}: {why}"));
    let mut out = String::new();
    let mut chars = arg.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next().ok_or_else(|| bad("bad escape"))?),
            '{' | '\'' => {
                let close = if c == '{' { '}' } else { '\'' };
                let mut token = String::new();
                loop {
                    match chars.next() {
                        None => return Err(bad("unclosed token")),
                        Some('\\') => token.push(chars.next().ok_or_else(|| bad("bad escape"))?),
                        Some(d) if d == close => break,
                        Some(d) => token.push(d),
                    }
                }
                if c == '\'' {
                    out.push_str(&token);
                } else {
                    let value = env
                        .lookup(&token)?
                        .ok_or_else(|| bad(&format!("unknown key {token}")))?;
                    out.push_str(&value);
                }
            }
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// `Main-Class` of a jar manifest's main section.
fn parse_main_class(manifest: &str) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in manifest.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        // A line starting with a space continues the previous one.
        if let Some(rest) = line.strip_prefix(' ')
            && let Some(last) = lines.last_mut()
        {
            last.push_str(rest);
            continue;
        }
        lines.push(line.to_string());
    }
    lines
        .iter()
        .take_while(|l| !l.is_empty())
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("Main-Class")
                .then(|| v.trim().to_string())
        })
        .filter(|m| !m.is_empty())
}

fn main_class(jar: &Path) -> Result<String, LoaderError> {
    let mut zip = ZipArchive::new(File::open(jar)?)
        .map_err(|e| invalid(format!("{}: {e}", jar.display())))?;
    let mut text = String::new();
    zip.by_name("META-INF/MANIFEST.MF")
        .map_err(|e| invalid(format!("{}: {e}", jar.display())))?
        .read_to_string(&mut text)?;
    parse_main_class(&text)
        .ok_or_else(|| invalid(format!("{}: no Main-Class in the manifest", jar.display())))
}

/// The first output whose file is missing or has another SHA-1.
async fn first_mismatch(outputs: &[(PathBuf, String)]) -> Option<PathBuf> {
    let first = outputs.first().map(|(path, _)| path.clone());
    let outputs = outputs.to_vec();
    tokio::task::spawn_blocking(move || {
        outputs.into_iter().find_map(|(path, sha1)| {
            let ok = sha1_file(&path).is_ok_and(|a| a.eq_ignore_ascii_case(&sha1));
            (!ok).then_some(path)
        })
    })
    .await
    // A panicked check proves nothing: count it as a mismatch.
    .unwrap_or(first)
}

/// Runs the client-side processors in order. A processor whose declared
/// `outputs` all exist with the right SHA-1 is skipped; otherwise its
/// existing outputs are deleted and it runs again (an interrupted install
/// leaves stale files). `DOWNLOAD_MOJMAPS` is done by the launcher.
pub async fn run_processors(
    java: &Path,
    procs: &[Processor],
    env: &ProcessorEnv,
    dl: &Downloader,
    vanilla: &VersionJson,
    progress: &ProgressSink,
) -> Result<(), LoaderError> {
    let client: Vec<(usize, &Processor)> = procs
        .iter()
        .enumerate()
        .filter(|(_, p)| p.runs_on_client())
        .collect();
    // Every processor's jar and classpath must be in place before the first
    // one runs, so a missing library cannot leave a half-processed install.
    for (_, p) in client.iter().filter(|(_, p)| !p.is_download_mojmaps()) {
        for coord in std::iter::once(&p.jar).chain(&p.classpath) {
            if !library_path(coord, &env.library_dir)?.is_file() {
                return Err(invalid(format!("processor library {coord} is missing")));
            }
        }
    }
    let total = client.len();
    report(progress, InstallPhase::RunProcessor, 0, total);
    for (done, (index, p)) in client.into_iter().enumerate() {
        if p.is_download_mojmaps() {
            download_mojmaps(p, env, dl, vanilla).await?;
        } else {
            run_one(java, index, done + 1, p, env).await?;
        }
        report(progress, InstallPhase::RunProcessor, done + 1, total);
    }
    Ok(())
}

async fn download_mojmaps(
    p: &Processor,
    env: &ProcessorEnv,
    dl: &Downloader,
    vanilla: &VersionJson,
) -> Result<(), LoaderError> {
    let output = p
        .args
        .windows(2)
        .find(|w| w[0] == "--output")
        .map(|w| &w[1])
        .ok_or_else(|| invalid("DOWNLOAD_MOJMAPS without --output"))?;
    let dest = confined_path(&expand_arg(output, env)?, env)?;
    let mappings = vanilla.downloads.client_mappings.as_ref().ok_or_else(|| {
        invalid(format!(
            "Minecraft {} has no client mappings to download",
            vanilla.id
        ))
    })?;
    let job = DownloadJob {
        url: canonical_url(&mappings.url),
        dest,
        sha1: Some(mappings.sha1.clone()),
        size: Some(mappings.size),
    };
    dl.run(vec![job], None).await?;
    Ok(())
}

/// Runs processor `index` (into all processors), the `step`th (1-based)
/// client-side one.
async fn run_one(
    java: &Path,
    index: usize,
    step: usize,
    p: &Processor,
    env: &ProcessorEnv,
) -> Result<(), LoaderError> {
    let outputs = p
        .outputs
        .iter()
        .map(|(k, v)| {
            Ok((
                confined_path(&expand_arg(k, env)?, env)?,
                expand_arg(v, env)?,
            ))
        })
        .collect::<Result<Vec<_>, LoaderError>>()?;
    if !outputs.is_empty() {
        if first_mismatch(&outputs).await.is_none() {
            return Ok(());
        }
        for (path, _) in &outputs {
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
    }

    let jar = library_path(&p.jar, &env.library_dir)?;
    let mut classpath = vec![path_str(&jar)?];
    for coord in &p.classpath {
        classpath.push(path_str(&library_path(coord, &env.library_dir)?)?);
    }
    let args = p
        .args
        .iter()
        .map(|a| expand_arg(a, env))
        .collect::<Result<Vec<_>, _>>()?;
    let main = tokio::task::spawn_blocking(move || main_class(&jar))
        .await
        .map_err(|e| LoaderError::Io(std::io::Error::other(e)))??;

    // Output is captured so it never reaches the user's terminal. Only a
    // stderr tail is kept, for diagnostics: patina-core has no developer log
    // sink yet (plan 2 Task 8 ruling; `tracing` comes with plan 4).
    let out = tokio::process::Command::new(java)
        .arg("-cp")
        .arg(classpath.join(CLASSPATH_SEPARATOR))
        .arg(&main)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await?;
    if !out.status.success() {
        return Err(LoaderError::ProcessorFailed {
            index,
            step,
            exit_code: out.status.code(),
            stderr_tail: stderr_tail(&out.stderr),
        });
    }
    if let Some(path) = first_unaccepted(&outputs).await {
        return Err(LoaderError::ProcessorOutputMismatch {
            path: path.display().to_string(),
        });
    }
    Ok(())
}

/// After a successful run: the first output that is missing, or has another
/// SHA-1 and is not an intact zip archive.
///
/// The declared SHA-1s of jar outputs (Forge's jarsplitter and
/// binarypatcher results) are of archives deflated by classic zlib. Java's
/// `java.util.zip` uses the system zlib on Linux, and where that is zlib-ng
/// (Fedora 40+) the same entries deflate to other bytes, so the SHA-1 can
/// never match there. Such an output is accepted when it is a non-empty
/// zip whose every entry reads back with a matching CRC-32: that still
/// catches truncated or damaged files. Any other output must match exactly.
async fn first_unaccepted(outputs: &[(PathBuf, String)]) -> Option<PathBuf> {
    let first = outputs.first().map(|(path, _)| path.clone());
    let outputs = outputs.to_vec();
    tokio::task::spawn_blocking(move || {
        outputs.into_iter().find_map(|(path, sha1)| {
            let ok = sha1_file(&path).is_ok_and(|a| a.eq_ignore_ascii_case(&sha1))
                || is_intact_zip(&path);
            (!ok).then_some(path)
        })
    })
    .await
    .unwrap_or(first)
}

/// A zip archive with at least one entry, every entry readable to the end
/// (the zip reader checks each entry's CRC-32 there).
fn is_intact_zip(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(mut zip) = zip::ZipArchive::new(std::io::BufReader::new(file)) else {
        return false;
    };
    if zip.is_empty() {
        return false;
    }
    (0..zip.len()).all(|i| {
        zip.by_index(i)
            .is_ok_and(|mut entry| std::io::copy(&mut entry, &mut std::io::sink()).is_ok())
    })
}

/// Most bytes of a failed processor's stderr kept for diagnostics.
const STDERR_TAIL_BYTES: usize = 4096;

/// The last [`STDERR_TAIL_BYTES`] of `stderr`, lossily decoded.
fn stderr_tail(stderr: &[u8]) -> String {
    let start = stderr.len().saturating_sub(STDERR_TAIL_BYTES);
    String::from_utf8_lossy(&stderr[start..]).into_owned()
}

/// `/`-separated paths, relative to the library dir, of the files the
/// client-side processors name in their arguments or `outputs` keys (after
/// expansion) that lie inside the library dir. Inputs are included too; the
/// caller removes the libraries it downloaded.
pub(crate) fn referenced_library_files(
    procs: &[Processor],
    env: &ProcessorEnv,
) -> Result<BTreeSet<String>, LoaderError> {
    let mut out = BTreeSet::new();
    for p in procs.iter().filter(|p| p.runs_on_client()) {
        for arg in p.args.iter().chain(p.outputs.keys()) {
            if let Some(rel) = library_rel(&expand_arg(arg, env)?, env) {
                out.insert(rel);
            }
        }
    }
    Ok(out)
}

#[cfg(all(test, unix))]
pub(crate) mod test_java {
    //! A fake `java` for processor tests: a shell script that logs its argv
    //! (one line per run, joined with `|`), writes `generated` to the path
    //! after `--out` and exits with the code after `--fail`.

    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use wiremock::MockServer;

    use crate::download::{Downloader, Mirror};
    use crate::loader::forge::test_support::{OFFICIAL_HOSTS, write_zip};

    pub const OUTPUT: &[u8] = b"generated";
    pub const PISTON_DATA: &str = "https://piston-data.mojang.com";

    pub struct FakeJava {
        pub java: PathBuf,
        pub log: PathBuf,
    }

    impl FakeJava {
        pub fn new(dir: &Path) -> FakeJava {
            use std::os::unix::fs::PermissionsExt;
            let log = dir.join("java.log");
            let java = dir.join("java");
            let script = format!(
                r#"#!/bin/sh
[ "$1" = "--probe" ] && exit 0
(IFS='|'; printf '%s\n' "$*") >> '{log}'
while [ $# -gt 0 ]; do
  case "$1" in
    --out) mkdir -p "$(dirname "$2")"; printf 'generated' > "$2" ;;
    --copy-out) mkdir -p "$(dirname "$3")"; cp "$2" "$3"; shift ;;
    --fail) echo "processor error: boom" >&2; exit "$2" ;;
  esac
  shift
done
exit 0
"#,
                log = log.display()
            );
            std::fs::write(&java, script).unwrap();
            std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
            // A freshly written executable can briefly fail with ETXTBSY while
            // another test thread forks; run it once until it execs cleanly.
            for _ in 0..50 {
                match std::process::Command::new(&java).arg("--probe").status() {
                    Ok(_) => break,
                    Err(e) if e.raw_os_error() == Some(26) => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    Err(e) => panic!("fake java: {e}"),
                }
            }
            FakeJava { java, log }
        }

        /// One entry per run, each the argv split on `|`.
        pub fn runs(&self) -> Vec<Vec<String>> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(|l| l.split('|').map(str::to_string).collect())
                .collect()
        }
    }

    /// Writes a processor jar whose manifest names `main`.
    pub fn processor_jar(path: &Path, main: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let manifest = format!("Manifest-Version: 1.0\r\nMain-Class: {main}\r\n\r\n");
        write_zip(path, &[("META-INF/MANIFEST.MF", manifest.as_bytes())]);
    }

    /// The official loader hosts plus piston-data map to `server`; the real
    /// hosts resolve to a closed local port.
    pub fn downloader(server: &MockServer) -> Downloader {
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut b = reqwest::Client::builder().no_proxy();
        for host in [
            "maven.minecraftforge.net",
            "files.minecraftforge.net",
            "maven.neoforged.net",
            "libraries.minecraft.net",
            "piston-data.mojang.com",
        ] {
            b = b.resolve(host, dead);
        }
        let rules = OFFICIAL_HOSTS
            .iter()
            .copied()
            .chain([PISTON_DATA])
            .map(|h| (h.to_string(), server.uri()))
            .collect();
        Downloader::new(
            b.build().unwrap(),
            Mirror {
                id: "fake".into(),
                rules,
            },
        )
        .with_retry(1, Duration::ZERO)
    }

    /// The modern vanilla fixture with client mappings at
    /// `piston-data.mojang.com/v1/objects/<sha1>/client.txt`.
    pub fn vanilla(mappings: Option<&[u8]>) -> crate::meta::VersionJson {
        use crate::loader::forge::test_support::sha1_hex;
        let mut v: crate::meta::VersionJson =
            serde_json::from_str(include_str!("../../../tests/fixtures/version_modern.json"))
                .unwrap();
        v.downloads.client_mappings = mappings.map(|body| {
            let sha1 = sha1_hex(body);
            crate::meta::FileRef {
                url: format!("{PISTON_DATA}/v1/objects/{sha1}/client.txt"),
                sha1,
                size: body.len() as u64,
                path: None,
            }
        });
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::forge::test_support::{sha1_hex, write_zip};
    use serde_json::json;

    fn env_in(root: &Path, data: HashMap<String, String>) -> ProcessorEnv {
        ProcessorEnv {
            data,
            minecraft_jar: root.join("versions/1.20.1/1.20.1.jar"),
            minecraft_version: "1.20.1".into(),
            root: root.to_path_buf(),
            installer: root.join("installer.jar"),
            library_dir: root.join("libraries"),
        }
    }

    fn s(p: &Path) -> String {
        p.to_str().unwrap().to_string()
    }

    #[test]
    fn resolve_data_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let libs = tmp.path().join("libraries");
        let extracted = tmp.path().join("extract");
        let installer = tmp.path().join("installer.jar");
        write_zip(&installer, &[("data/client.lzma", b"client patches")]);
        let raw = json!({
            "MAPPINGS": {"client": "[de.oceanlabs.mcp:mcp_config:1.20.1-20230612.114412:mappings@txt]", "server": "[x:y:1]"},
            "MC_SLIM_SHA": {"client": "'de86b035d2da0f78940796bb95c39a932ed84834'", "server": "'00'"},
            "BINPATCH": {"client": "/data/client.lzma", "server": "/data/server.lzma"},
            "OTHER": {"client": "{BINPATCH}"},
            "SERVER_ONLY": {"server": "'s'"}
        });
        let d = resolve_data(&raw, CLIENT, &libs, &installer, &extracted).unwrap();
        assert_eq!(
            d["MAPPINGS"],
            s(&libs.join(
                "de/oceanlabs/mcp/mcp_config/1.20.1-20230612.114412/mcp_config-1.20.1-20230612.114412-mappings.txt"
            ))
        );
        assert_eq!(d["MC_SLIM_SHA"], "de86b035d2da0f78940796bb95c39a932ed84834");
        let binpatch = extracted.join("data/client.lzma");
        assert_eq!(d["BINPATCH"], s(&binpatch));
        assert_eq!(std::fs::read(&binpatch).unwrap(), b"client patches");
        // other forms are kept verbatim; `{…}` is expanded in arguments only
        assert_eq!(d["OTHER"], "{BINPATCH}");
        assert!(!d.contains_key("SERVER_ONLY"));
        assert!(!extracted.join("data/server.lzma").exists());

        let bad = |raw: Value| resolve_data(&raw, CLIENT, &libs, &installer, &extracted);
        assert!(matches!(
            bad(json!({"A": {"client": "[a:..:1]"}})),
            Err(LoaderError::UnsafeMetadata(_))
        ));
        assert!(matches!(
            bad(json!({"A": {"client": "/../escape"}})),
            Err(LoaderError::UnsafeMetadata(_))
        ));
        assert!(matches!(
            bad(json!({"A": {"client": "/data/missing.lzma"}})),
            Err(LoaderError::InstallerInvalid(_))
        ));
        assert!(matches!(
            bad(json!({"A": {"client": 7}})),
            Err(LoaderError::InstallerInvalid(_))
        ));
    }

    #[test]
    fn expand_arg_builtins_and_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let data = HashMap::from([
            ("MC_SLIM_SHA".to_string(), "de86b035".to_string()),
            ("MAPPINGS".to_string(), "/l/m.txt".to_string()),
        ]);
        let env = env_in(root, data);
        let x = |a: &str| expand_arg(a, &env);
        assert_eq!(x("{SIDE}").unwrap(), "client");
        assert_eq!(x("{MINECRAFT_JAR}").unwrap(), s(&env.minecraft_jar));
        assert_eq!(x("{MINECRAFT_VERSION}").unwrap(), "1.20.1");
        assert_eq!(x("{ROOT}").unwrap(), s(root));
        assert_eq!(x("{INSTALLER}").unwrap(), s(&env.installer));
        assert_eq!(x("{LIBRARY_DIR}").unwrap(), s(&env.library_dir));
        assert_eq!(x("{ROOT}/run.sh").unwrap(), format!("{}/run.sh", s(root)));
        assert_eq!(x("{MC_SLIM_SHA}").unwrap(), "de86b035");
        assert_eq!(x("{MAPPINGS}").unwrap(), "/l/m.txt");
        assert_eq!(
            x("[de.oceanlabs.mcp:mcp_config:1.20.1-20230612.114412@zip]").unwrap(),
            s(&env.library_dir.join(
                "de/oceanlabs/mcp/mcp_config/1.20.1-20230612.114412/mcp_config-1.20.1-20230612.114412.zip"
            ))
        );
        assert_eq!(x("'abc'").unwrap(), "abc");
        assert_eq!(x("--task").unwrap(), "--task");
        assert_eq!(x("MCP_DATA").unwrap(), "MCP_DATA");
        for bad in ["{NOPE}", "{SIDE", "x\\"] {
            assert!(
                matches!(x(bad), Err(LoaderError::InstallerInvalid(_))),
                "{bad:?}"
            );
        }
        assert!(matches!(x("[a:..:1]"), Err(LoaderError::UnsafeMetadata(_))));
    }

    #[test]
    fn manifest_main_class() {
        assert_eq!(
            parse_main_class("Manifest-Version: 1.0\r\nMain-Class: a.B\r\n\r\n").as_deref(),
            Some("a.B")
        );
        // continuation lines are unfolded
        assert_eq!(
            parse_main_class("Manifest-Version: 1.0\nMain-Class: net.minecraftforge.\n binarypatcher.ConsoleTool\n").as_deref(),
            Some("net.minecraftforge.binarypatcher.ConsoleTool")
        );
        // only the main section counts
        assert_eq!(
            parse_main_class("Manifest-Version: 1.0\n\nName: x/\nMain-Class: a.B\n"),
            None
        );
        assert_eq!(parse_main_class("Manifest-Version: 1.0\n"), None);
    }

    #[cfg(unix)]
    mod run {
        use super::*;
        use crate::loader::forge::processors::test_java::{
            FakeJava, OUTPUT, downloader, processor_jar, vanilla,
        };
        use crate::loader::{InstallPhase, InstallProgress};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn procs(v: Value) -> Vec<Processor> {
            serde_json::from_value(v).unwrap()
        }

        struct Setup {
            _tmp: tempfile::TempDir,
            java: FakeJava,
            env: ProcessorEnv,
            server: MockServer,
        }

        async fn setup(data: &[(&str, &str)]) -> Setup {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("data");
            let java = FakeJava::new(tmp.path());
            let data = data
                .iter()
                .map(|(k, v)| (k.to_string(), v.replace("$ROOT", root.to_str().unwrap())))
                .collect();
            let env = env_in(&root, data);
            std::fs::create_dir_all(env.minecraft_jar.parent().unwrap()).unwrap();
            std::fs::write(&env.minecraft_jar, b"vanilla").unwrap();
            for (coord, main) in [
                ("x:first:1", "a.First"),
                ("x:server:1", "b.Server"),
                ("x:second:1", "c.Second"),
            ] {
                let rel = crate::meta::maven_path(coord).unwrap();
                processor_jar(&env.library_dir.join(rel), main);
            }
            Setup {
                _tmp: tmp,
                java,
                env,
                server: MockServer::start().await,
            }
        }

        impl Setup {
            async fn run(&self, p: &[Processor]) -> Result<(), LoaderError> {
                self.run_with(p, &vanilla(None), &None).await
            }

            async fn run_with(
                &self,
                p: &[Processor],
                vanilla: &VersionJson,
                progress: &ProgressSink,
            ) -> Result<(), LoaderError> {
                run_processors(
                    &self.java.java,
                    p,
                    &self.env,
                    &downloader(&self.server),
                    vanilla,
                    progress,
                )
                .await
            }

            fn lib(&self, coord: &str) -> String {
                s(&self
                    .env
                    .library_dir
                    .join(crate::meta::maven_path(coord).unwrap()))
            }
        }

        #[tokio::test]
        async fn runs_only_client_processors_in_order() {
            let st = setup(&[("OUT_A", "$ROOT/libraries/x/a/1/a-1.jar")]).await;
            let p = procs(json!([
                {"jar": "x:first:1", "classpath": ["x:dep:1", "x:dep2:2@zip"],
                 "args": ["--in", "{MINECRAFT_JAR}", "--out", "{OUT_A}"]},
                {"sides": ["server"], "jar": "x:server:1", "args": ["--out", "{ROOT}/server.jar"]},
                {"sides": ["client"], "jar": "x:second:1",
                 "args": ["--side", "{SIDE}", "--out", "[x:gen:1:client]"]}
            ]));
            for dep in ["x:dep:1", "x:dep2:2@zip"] {
                let dep = PathBuf::from(st.lib(dep));
                std::fs::create_dir_all(dep.parent().unwrap()).unwrap();
                std::fs::write(dep, b"dep").unwrap();
            }
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            st.run_with(&p, &vanilla(None), &Some(tx)).await.unwrap();

            let runs = st.java.runs();
            assert_eq!(runs.len(), 2, "{runs:?}");
            let cp_a = [
                st.lib("x:first:1"),
                st.lib("x:dep:1"),
                st.lib("x:dep2:2@zip"),
            ]
            .join(crate::platform::CLASSPATH_SEPARATOR);
            assert_eq!(
                runs[0],
                vec![
                    "-cp".to_string(),
                    cp_a,
                    "a.First".into(),
                    "--in".into(),
                    s(&st.env.minecraft_jar),
                    "--out".into(),
                    st.env.data["OUT_A"].clone(),
                ]
            );
            assert_eq!(
                runs[1],
                vec![
                    "-cp".to_string(),
                    st.lib("x:second:1"),
                    "c.Second".into(),
                    "--side".into(),
                    "client".into(),
                    "--out".into(),
                    st.lib("x:gen:1:client"),
                ]
            );
            assert_eq!(std::fs::read(st.lib("x:gen:1:client")).unwrap(), OUTPUT);
            assert!(!st.env.root.join("server.jar").exists());

            let mut last: Option<InstallProgress> = None;
            while let Ok(p) = rx.try_recv() {
                assert_eq!(p.phase, InstallPhase::RunProcessor);
                last = Some(p);
            }
            let last = last.unwrap();
            assert_eq!((last.done, last.total), (2, 2));
        }

        fn split_proc() -> Vec<Processor> {
            procs(json!([
                {"jar": "x:first:1", "args": ["--out", "{OUT}"],
                 "outputs": {"{OUT}": "{OUT_SHA}"}}
            ]))
        }

        #[tokio::test]
        async fn skips_processor_with_valid_outputs() {
            let sha = sha1_hex(OUTPUT);
            let st = setup(&[("OUT", "$ROOT/libraries/x/o/1/o-1.jar"), ("OUT_SHA", &sha)]).await;
            let out = PathBuf::from(&st.env.data["OUT"]);
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            std::fs::write(&out, OUTPUT).unwrap();
            st.run(&split_proc()).await.unwrap();
            assert!(st.java.runs().is_empty());

            // a quoted literal sha1 in `outputs` works the same
            let p = procs(json!([
                {"jar": "x:first:1", "args": ["--out", "{OUT}"],
                 "outputs": {"{OUT}": format!("'{sha}'")}}
            ]));
            st.run(&p).await.unwrap();
            assert!(st.java.runs().is_empty());
        }

        #[tokio::test]
        async fn stale_output_reruns_processor() {
            let sha = sha1_hex(OUTPUT);
            let st = setup(&[("OUT", "$ROOT/libraries/x/o/1/o-1.jar"), ("OUT_SHA", &sha)]).await;
            let out = PathBuf::from(&st.env.data["OUT"]);
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            std::fs::write(&out, b"half-written by an interrupted install").unwrap();
            st.run(&split_proc()).await.unwrap();
            assert_eq!(st.java.runs().len(), 1);
            assert_eq!(crate::download::sha1_file(&out).unwrap(), sha);
        }

        #[tokio::test]
        async fn wrong_output_after_run_is_reported() {
            let st = setup(&[
                ("OUT", "$ROOT/libraries/x/o/1/o-1.jar"),
                ("OUT_SHA", &sha1_hex(b"what the installer expects")),
            ])
            .await;
            let r = st.run(&split_proc()).await;
            match r {
                Err(LoaderError::ProcessorOutputMismatch { path }) => {
                    assert_eq!(path, st.env.data["OUT"])
                }
                other => panic!("expected an output mismatch, got {other:?}"),
            }
            assert_eq!(st.java.runs().len(), 1);
        }

        /// A stored (uncompressed) zip, so a byte of `body` can be flipped.
        fn stored_zip(path: &Path, body: &[u8]) {
            use std::io::Write;
            let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("a/B.class", opts).unwrap();
            w.write_all(body).unwrap();
            w.finish().unwrap();
        }

        /// The declared SHA-1s were made with classic zlib; a Java on a
        /// system zlib-ng (Fedora 40+) deflates the same entries to other
        /// bytes. An intact archive is accepted; a damaged one is not.
        #[tokio::test]
        async fn intact_zip_output_with_other_deflate_bytes_is_accepted() {
            let st = setup(&[
                ("OUT", "$ROOT/libraries/x/o/1/o-1.jar"),
                ("OUT_SHA", &sha1_hex(b"bytes from classic zlib")),
            ])
            .await;
            let p = |src: &Path| {
                procs(json!([
                    {"jar": "x:first:1", "args": ["--copy-out", s(src), "{OUT}"],
                     "outputs": {"{OUT}": "{OUT_SHA}"}}
                ]))
            };
            let good = st.env.root.join("good.jar");
            write_zip(&good, &[("a/B.class", b"class bytes"), ("c.txt", b"text")]);
            st.run(&p(&good)).await.unwrap();
            let out = PathBuf::from(&st.env.data["OUT"]);
            assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&good).unwrap());

            // Entry data that fails its CRC-32.
            let bad = st.env.root.join("bad.jar");
            stored_zip(&bad, b"class bytes");
            let mut bytes = std::fs::read(&bad).unwrap();
            let at = bytes.windows(11).position(|w| w == b"class bytes").unwrap();
            bytes[at] ^= 0xff;
            std::fs::write(&bad, bytes).unwrap();
            // A truncated archive, and an empty one.
            let cut = st.env.root.join("cut.jar");
            let whole = std::fs::read(&good).unwrap();
            std::fs::write(&cut, &whole[..whole.len() / 2]).unwrap();
            let empty = st.env.root.join("empty.jar");
            write_zip(&empty, &[]);
            for src in [&bad, &cut, &empty] {
                match st.run(&p(src)).await {
                    Err(LoaderError::ProcessorOutputMismatch { path }) => {
                        assert_eq!(path, st.env.data["OUT"])
                    }
                    other => panic!("{}: expected a mismatch, got {other:?}", src.display()),
                }
            }
        }

        #[tokio::test]
        async fn download_mojmaps_is_handled_by_launcher() {
            let st = setup(&[(
                "MOJMAPS",
                "$ROOT/libraries/net/minecraft/client/1.20.1/client-1.20.1-mappings.txt",
            )])
            .await;
            let body = b"net.minecraft.Foo -> a:".to_vec();
            let v = vanilla(Some(&body));
            let url = &v.downloads.client_mappings.as_ref().unwrap().url;
            let p_url = url
                .strip_prefix(super::super::test_java::PISTON_DATA)
                .unwrap();
            Mock::given(method("GET"))
                .and(path(p_url))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
                .expect(1)
                .mount(&st.server)
                .await;
            // the processor jar does not even exist: it is never launched
            let p = procs(json!([
                {"jar": "x:tools:1", "args": ["--task", "DOWNLOAD_MOJMAPS", "--version", "1.20.1",
                  "--side", "{SIDE}", "--output", "{MOJMAPS}"]}
            ]));
            st.run_with(&p, &v, &None).await.unwrap();
            assert!(st.java.runs().is_empty());
            assert_eq!(std::fs::read(&st.env.data["MOJMAPS"]).unwrap(), body);

            let r = st.run_with(&p, &vanilla(None), &None).await;
            assert!(matches!(r, Err(LoaderError::InstallerInvalid(_))), "{r:?}");
        }

        #[tokio::test]
        async fn processor_failure_reports_index() {
            let st = setup(&[]).await;
            let p = procs(json!([
                {"sides": ["server"], "jar": "x:server:1", "args": ["--fail", "9"]},
                {"sides": ["server"], "jar": "x:server:1", "args": ["--fail", "9"]},
                {"jar": "x:first:1", "args": ["--fail", "3"]},
                {"jar": "x:second:1", "args": []}
            ]));
            let r = st.run(&p).await;
            // `index` is into all processors, `step` counts client ones.
            assert!(
                matches!(
                    r,
                    Err(LoaderError::ProcessorFailed {
                        index: 2,
                        step: 1,
                        exit_code: Some(3),
                        ..
                    })
                ),
                "{r:?}"
            );
            // later processors do not run
            assert_eq!(st.java.runs().len(), 1);
            // the stderr tail is kept for diagnostics, not shown to users
            let err = r.unwrap_err();
            let LoaderError::ProcessorFailed { stderr_tail, .. } = &err else {
                unreachable!()
            };
            assert_eq!(stderr_tail, "processor error: boom\n");
            assert!(!err.to_string().contains("boom"), "{err}");
            assert!(format!("{err:?}").contains("boom"));
        }

        #[tokio::test]
        async fn missing_processor_libraries_are_reported_before_any_run() {
            let st = setup(&[]).await;
            for p in [
                json!([{"jar": "x:first:1", "args": []}, {"jar": "x:absent:1", "args": []}]),
                json!([{"jar": "x:first:1", "args": []},
                       {"jar": "x:second:1", "classpath": ["x:gone:1"], "args": []}]),
            ] {
                match st.run(&procs(p)).await {
                    Err(LoaderError::InstallerInvalid(why)) => {
                        assert!(
                            why.contains("x:absent:1") || why.contains("x:gone:1"),
                            "{why}"
                        )
                    }
                    other => panic!("expected InstallerInvalid, got {other:?}"),
                }
            }
            assert!(st.java.runs().is_empty());
        }

        #[tokio::test]
        async fn outputs_outside_the_library_dir_are_rejected() {
            let st = setup(&[]).await;
            let outside = st.env.root.parent().unwrap().join("outside.txt");
            std::fs::write(&outside, b"keep me").unwrap();
            let sha = sha1_hex(OUTPUT);
            for key in [
                "{ROOT}/../outside.txt".to_string(),
                "{LIBRARY_DIR}/../../outside.txt".to_string(),
                format!("'{}'", s(&outside)),
            ] {
                let p = procs(json!([
                    {"jar": "x:first:1", "args": ["--out", "{ROOT}/../outside.txt"],
                     "outputs": {key.clone(): &sha}}
                ]));
                let r = st.run(&p).await;
                assert!(
                    matches!(r, Err(LoaderError::UnsafeMetadata(_))),
                    "{key}: {r:?}"
                );
            }
            assert_eq!(std::fs::read(&outside).unwrap(), b"keep me");
            assert!(st.java.runs().is_empty());

            // the mappings download is confined the same way
            let p = procs(json!([
                {"jar": "x:tools:1", "args": ["--task", "DOWNLOAD_MOJMAPS",
                  "--output", "{ROOT}/../outside.txt"]}
            ]));
            let r = st.run_with(&p, &vanilla(Some(b"maps")), &None).await;
            assert!(matches!(r, Err(LoaderError::UnsafeMetadata(_))), "{r:?}");
            assert_eq!(std::fs::read(&outside).unwrap(), b"keep me");
        }
    }
}
