//! Legacy Forge (1.7.10-era) libraries with `checksums`: verification and
//! the `.pack.xz` fallback.
//!
//! A `.pack.xz` is an xz stream whose content is pack200 data followed by a
//! signature trailer: `<pack200> <signature: L bytes> <L: u32 LE> "SIGN"`.
//! The signature block is the jar's `checksums.sha1` (lines
//! `"<sha1> <entry>"`); `unpack200` restores the jar from the pack200 data
//! and the signature is added back as `checksums.sha1`. The restored jar's
//! own SHA-1 is not stable, so Forge validates it by its contents instead:
//! the SHA-1 of `checksums.sha1` is one of `checksums` and every entry it
//! lists matches (see [`verify_jar_contents`]).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::download::{DownloadError, DownloadJob, Downloader, sha1_file};
use crate::java::ManagedRuntimes;
use crate::loader::list::LoaderError;
use crate::platform;

/// Managed runtime that ships `unpack200` (removed from Java 14 on).
pub const UNPACK200_RUNTIME: &str = "jre-legacy";

/// Name of the signature entry inside a legacy library jar.
pub const SIGNATURE_ENTRY: &str = "checksums.sha1";

/// Last four bytes of a decompressed `.pack.xz`.
const TRAILER_MAGIC: &[u8; 4] = b"SIGN";

/// Largest decompressed `.pack.xz` accepted (the biggest real one, Scala's
/// compiler, is far smaller).
const MAX_UNPACKED: u64 = 256 * 1024 * 1024;

/// Spawn attempts for `unpack200` while it is busy (see
/// [`platform::is_text_file_busy`]).
const SPAWN_ATTEMPTS: u32 = 50;

fn listed(sha1: &str, checksums: &[String]) -> bool {
    checksums.iter().any(|c| c.eq_ignore_ascii_case(sha1))
}

/// Whether `path`'s SHA-1 is one of `checksums` (always true when
/// `checksums` is empty).
pub fn verify_checksums(path: &Path, checksums: &[String]) -> io::Result<bool> {
    if checksums.is_empty() {
        return Ok(true);
    }
    Ok(listed(&sha1_file(path)?, checksums))
}

/// Forge's check for a jar whose own SHA-1 is not listed: it must contain
/// `checksums.sha1` whose SHA-1 is one of `checksums`, and every
/// `"<sha1> <entry>"` line of it must name an entry with that SHA-1.
/// `Ok(false)` for anything that is not such a jar.
pub fn verify_jar_contents(path: &Path, checksums: &[String]) -> io::Result<bool> {
    let Ok(mut zip) = zip::ZipArchive::new(std::fs::File::open(path)?) else {
        return Ok(false);
    };
    let Some(signature) = read_entry(&mut zip, SIGNATURE_ENTRY)? else {
        return Ok(false);
    };
    if !listed(&hex::encode(sha1_digest(&signature)), checksums) {
        return Ok(false);
    }
    let Ok(text) = String::from_utf8(signature) else {
        return Ok(false);
    };
    for line in text.lines().map(str::trim) {
        // Forge skips lines without a space (blank ones included).
        let Some((sha1, entry)) = line.split_once(' ') else {
            continue;
        };
        match read_entry(&mut zip, entry)? {
            Some(body) if hex::encode(sha1_digest(&body)).eq_ignore_ascii_case(sha1) => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn sha1_digest(body: &[u8]) -> impl AsRef<[u8]> {
    use sha1::{Digest, Sha1};
    Sha1::digest(body)
}

/// Body of file entry `name`; `None` when there is none (or it is a
/// directory or unreadable as zip data).
fn read_entry(zip: &mut zip::ZipArchive<std::fs::File>, name: &str) -> io::Result<Option<Vec<u8>>> {
    let Ok(mut entry) = zip.by_name(name) else {
        return Ok(None);
    };
    if entry.is_dir() {
        return Ok(None);
    }
    let mut body = Vec::new();
    match entry.read_to_end(&mut body) {
        Ok(_) => Ok(Some(body)),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => Ok(None),
        Err(e) => Err(e),
    }
}

/// [`verify_checksums`] or, failing that, [`verify_jar_contents`] (Forge's
/// own rule for legacy libraries).
pub fn library_valid(path: &Path, checksums: &[String]) -> io::Result<bool> {
    Ok(verify_checksums(path, checksums)? || verify_jar_contents(path, checksums)?)
}

/// Splits a decompressed `.pack.xz` into (pack200 data, signature block).
/// `None` unless it ends with a well-formed signature trailer.
pub fn split_signed_pack(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = data.strip_suffix(TRAILER_MAGIC)?;
    let (rest, len) = rest.split_at_checked(rest.len().checked_sub(4)?)?;
    let len = usize::try_from(u32::from_le_bytes(len.try_into().ok()?)).ok()?;
    rest.split_at_checked(rest.len().checked_sub(len)?)
}

/// Decompresses an xz stream (at most [`MAX_UNPACKED`] bytes).
pub fn decompress_xz(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    lzma_rust2::XzReader::new(bytes, true)
        .take(MAX_UNPACKED + 1)
        .read_to_end(&mut out)?;
    if out.len() as u64 > MAX_UNPACKED {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pack.xz decompresses to more than the size limit",
        ));
    }
    Ok(out)
}

/// Reads the `.pack.xz` at `pack_xz`, writes its pack200 data to `pack` and
/// returns its signature block.
pub fn prepare_pack(pack_xz: &Path, pack: &Path) -> io::Result<Vec<u8>> {
    let raw = decompress_xz(&std::fs::read(pack_xz)?)?;
    let (data, signature) = split_signed_pack(&raw).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "pack.xz has no signature trailer",
        )
    })?;
    std::fs::write(pack, data)?;
    Ok(signature.to_vec())
}

/// Adds `signature` to `jar` as [`SIGNATURE_ENTRY`] unless it already has one.
pub fn add_signature(jar: &Path, signature: &[u8]) -> io::Result<()> {
    let existing = zip::ZipArchive::new(std::fs::File::open(jar)?).map_err(io::Error::other)?;
    if existing.index_for_name(SIGNATURE_ENTRY).is_some() {
        return Ok(());
    }
    drop(existing);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(jar)?;
    let mut zip = zip::ZipWriter::new_append(file).map_err(io::Error::other)?;
    zip.start_file(SIGNATURE_ENTRY, zip::write::SimpleFileOptions::default())
        .map_err(io::Error::other)?;
    zip.write_all(signature)?;
    zip.finish().map_err(io::Error::other)?.sync_all()
}

/// Runs `unpack200 <pack> <jar>`.
pub async fn run_unpack200(unpack200: &Path, pack: &Path, jar: &Path) -> io::Result<()> {
    let mut attempt = 0;
    let status = loop {
        attempt += 1;
        let spawned = tokio::process::Command::new(unpack200)
            .arg(pack)
            .arg(jar)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await;
        match spawned {
            Err(e) if platform::is_text_file_busy(&e) && attempt < SPAWN_ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            other => break other?,
        }
    };
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("unpack200 exited with {status}")))
    }
}

/// `unpack200` from the managed [`UNPACK200_RUNTIME`], installed on first
/// use. A failed install is remembered for this installation run.
pub(crate) struct Unpack200<'a> {
    dl: &'a Downloader,
    runtimes: ManagedRuntimes,
    bin: tokio::sync::OnceCell<Option<PathBuf>>,
}

impl<'a> Unpack200<'a> {
    pub(crate) fn new(dl: &'a Downloader, runtimes_root: PathBuf) -> Self {
        Self {
            dl,
            runtimes: ManagedRuntimes::new(runtimes_root),
            bin: tokio::sync::OnceCell::new(),
        }
    }

    async fn bin(&self) -> Option<PathBuf> {
        self.bin
            .get_or_init(|| async {
                if !self.runtimes.is_installed(UNPACK200_RUNTIME) {
                    self.runtimes
                        .install(self.dl, UNPACK200_RUNTIME)
                        .await
                        .ok()?;
                }
                let bin = platform::unpack200_in_home(&self.runtimes.home(UNPACK200_RUNTIME));
                bin.is_file().then_some(bin)
            })
            .await
            .clone()
    }
}

/// `dest` with `suffix` appended to its file name.
fn sibling(dest: &Path, suffix: &str) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    dest.with_file_name(name)
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

async fn valid(path: &Path, checksums: &[String]) -> bool {
    let (path, checksums) = (path.to_path_buf(), checksums.to_vec());
    tokio::task::spawn_blocking(move || library_valid(&path, &checksums).unwrap_or(false))
        .await
        .unwrap_or(false)
}

/// Makes `job.dest` a valid copy of the legacy library `name`: an existing
/// valid file is kept; otherwise the plain jar is downloaded and, if it is
/// missing or fails verification, `<url>.pack.xz` is unpacked instead.
pub(crate) async fn ensure_legacy_library(
    tools: &Unpack200<'_>,
    job: &DownloadJob,
    name: &str,
    checksums: &[String],
) -> Result<(), LoaderError> {
    let dest = &job.dest;
    if valid(dest, checksums).await {
        return Ok(());
    }
    remove_if_exists(dest)?;

    // 1. The plain jar. Its checksums are checked after the download.
    let plain = DownloadJob {
        url: job.url.clone(),
        dest: dest.clone(),
        sha1: None,
        size: None,
    };
    match tools.dl.run(vec![plain], None).await {
        Ok(()) if valid(dest, checksums).await => return Ok(()),
        Ok(()) => remove_if_exists(dest)?,
        Err(DownloadError::Failed { .. }) => {}
        Err(DownloadError::Io(e)) => return Err(e.into()),
    }

    // 2. `<url>.pack.xz`.
    let temps = [".pack.xz", ".pack", ".unpacked"].map(|s| sibling(dest, s));
    let clear = || temps.iter().try_for_each(|t| remove_if_exists(t));
    clear()?;
    let unpacked = from_pack_xz(tools, job, checksums, &temps).await;
    clear()?;
    if unpacked {
        Ok(())
    } else {
        Err(LoaderError::LegacyLibraryUnavailable {
            name: name.to_string(),
        })
    }
}

/// Step 2 of [`ensure_legacy_library`] through the temporary files `[pack.xz,
/// pack, unpacked jar]`; true once `job.dest` holds the verified jar.
async fn from_pack_xz(
    tools: &Unpack200<'_>,
    job: &DownloadJob,
    checksums: &[String],
    temps: &[PathBuf; 3],
) -> bool {
    let [xz, pack, jar] = temps.clone();
    let download = DownloadJob {
        url: format!("{}.pack.xz", job.url),
        dest: xz.clone(),
        sha1: None,
        size: None,
    };
    // Only a downloaded pack.xz is worth installing jre-legacy for.
    if let Err(e) = tools.dl.run(vec![download], None).await {
        tracing::debug!(url = %job.url, error = ?e, "pack.xz download failed");
        return false;
    }
    let Some(unpack200) = tools.bin().await else {
        tracing::debug!(url = %job.url, "no unpack200 available for pack.xz fallback");
        return false;
    };
    let pack_out = pack.clone();
    let signature = match tokio::task::spawn_blocking(move || prepare_pack(&xz, &pack_out)).await {
        Ok(Ok(signature)) => signature,
        Ok(Err(e)) => {
            tracing::debug!(url = %job.url, error = ?e, "pack.xz could not be prepared");
            return false;
        }
        Err(e) => {
            tracing::debug!(url = %job.url, error = ?e, "pack.xz preparation task failed");
            return false;
        }
    };
    if let Err(e) = run_unpack200(&unpack200, &pack, &jar).await {
        tracing::debug!(url = %job.url, error = ?e, "unpack200 failed");
        return false;
    }
    let (dest, checksums) = (job.dest.clone(), checksums.to_vec());
    let finished = tokio::task::spawn_blocking(move || -> io::Result<bool> {
        add_signature(&jar, &signature)?;
        if !library_valid(&jar, &checksums)? {
            return Ok(false);
        }
        std::fs::rename(&jar, &dest)?;
        Ok(true)
    })
    .await;
    match finished {
        Ok(Ok(true)) => true,
        Ok(Ok(false)) => {
            tracing::debug!(url = %job.url, "unpacked jar failed checksum verification");
            false
        }
        Ok(Err(e)) => {
            tracing::debug!(url = %job.url, error = ?e, "finishing unpacked jar failed");
            false
        }
        Err(e) => {
            tracing::debug!(url = %job.url, error = ?e, "finishing task failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::forge::test_support::{
        PACK_SHA1, PACK_SIG_SHA1, PACK_XZ, PACKED_ENTRIES, sha1_hex, write_zip,
    };

    fn signature() -> Vec<u8> {
        PACKED_ENTRIES
            .iter()
            .map(|(name, body)| format!("{} {name}\n", sha1_hex(body)))
            .collect::<String>()
            .into_bytes()
    }

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn checksums_accept_any_match() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("lib.jar");
        std::fs::write(&f, b"hello").unwrap();
        let other = sha1_hex(b"other");
        let hello = sha1_hex(b"hello");
        assert!(verify_checksums(&f, &[]).unwrap());
        assert!(verify_checksums(&f, &list(&[&other, &hello])).unwrap());
        assert!(verify_checksums(&f, &list(&[&hello.to_uppercase()])).unwrap());
        assert!(!verify_checksums(&f, &list(&[&other])).unwrap());
        assert!(verify_checksums(&tmp.path().join("missing"), &list(&[&hello])).is_err());
    }

    #[test]
    fn packxz_fixture_splits_into_pack_and_signature() {
        let raw = decompress_xz(PACK_XZ).unwrap();
        let (pack, sig) = split_signed_pack(&raw).unwrap();
        assert_eq!(&pack[..4], &[0xca, 0xfe, 0xd0, 0x0d], "pack200 magic");
        assert_eq!(sha1_hex(pack), PACK_SHA1);
        assert_eq!(sha1_hex(sig), PACK_SIG_SHA1);
        assert_eq!(sig, signature());

        let tmp = tempfile::tempdir().unwrap();
        let (xz, out) = (tmp.path().join("a.pack.xz"), tmp.path().join("a.pack"));
        std::fs::write(&xz, PACK_XZ).unwrap();
        assert_eq!(prepare_pack(&xz, &out).unwrap(), signature());
        assert_eq!(sha1_hex(&std::fs::read(&out).unwrap()), PACK_SHA1);
        std::fs::write(&xz, b"not xz").unwrap();
        assert!(prepare_pack(&xz, &out).is_err());
    }

    #[test]
    fn malformed_trailers_are_rejected() {
        let mut ok = b"PACKsig".to_vec();
        ok.extend(3u32.to_le_bytes());
        ok.extend(b"SIGN");
        assert_eq!(split_signed_pack(&ok), Some((&b"PACK"[..], &b"sig"[..])));
        let mut too_long = b"PACKsig".to_vec();
        too_long.extend(8u32.to_le_bytes());
        too_long.extend(b"SIGN");
        assert_eq!(split_signed_pack(&too_long), None);
        let mut unsigned = ok.clone();
        *unsigned.last_mut().unwrap() = b'X';
        assert_eq!(split_signed_pack(&unsigned), None);
        assert_eq!(split_signed_pack(b"SIGN"), None);
        assert_eq!(split_signed_pack(b""), None);
    }

    #[test]
    fn jar_contents_validate_against_signature() {
        let tmp = tempfile::tempdir().unwrap();
        let jar = tmp.path().join("lib.jar");
        let sig = signature();
        let mut entries: Vec<(&str, &[u8])> = PACKED_ENTRIES.to_vec();
        entries.push((SIGNATURE_ENTRY, &sig));
        write_zip(&jar, &entries);
        let ok = list(&["0000000000000000000000000000000000000000", PACK_SIG_SHA1]);
        assert!(verify_jar_contents(&jar, &ok).unwrap());
        assert!(library_valid(&jar, &ok).unwrap());
        assert!(!verify_jar_contents(&jar, &list(&[&sha1_hex(b"x")])).unwrap());

        // a listed entry with other content
        let mut tampered = entries.clone();
        tampered[0].1 = b"tampered";
        write_zip(&jar, &tampered);
        assert!(!verify_jar_contents(&jar, &ok).unwrap());
        // a listed entry missing
        write_zip(&jar, &[entries[1], entries[2]]);
        assert!(!verify_jar_contents(&jar, &ok).unwrap());
        // no signature entry
        write_zip(&jar, &PACKED_ENTRIES);
        assert!(!verify_jar_contents(&jar, &ok).unwrap());
        // not a zip
        std::fs::write(&jar, b"not a jar").unwrap();
        assert!(!verify_jar_contents(&jar, &ok).unwrap());
        assert!(!library_valid(&jar, &ok).unwrap());
    }

    #[test]
    fn signature_is_added_once() {
        let tmp = tempfile::tempdir().unwrap();
        let jar = tmp.path().join("lib.jar");
        write_zip(&jar, &PACKED_ENTRIES);
        add_signature(&jar, &signature()).unwrap();
        add_signature(&jar, &signature()).unwrap();
        assert!(verify_jar_contents(&jar, &list(&[PACK_SIG_SHA1])).unwrap());
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&jar).unwrap()).unwrap();
        assert_eq!(zip.len(), 3);
        let mut body = Vec::new();
        zip.by_name(SIGNATURE_ENTRY)
            .unwrap()
            .read_to_end(&mut body)
            .unwrap();
        assert_eq!(body, signature());
    }

    /// Runs the fixture through the real `unpack200` of a freshly installed
    /// managed `jre-legacy` (network).
    #[tokio::test]
    #[ignore = "downloads jre-legacy from the network"]
    async fn real_unpack200_restores_fixture() {
        use crate::download::Mirror;
        let tmp = tempfile::tempdir().unwrap();
        let dl = Downloader::new(reqwest::Client::new(), Mirror::official())
            .with_fallback(Some(Mirror::bmclapi()));
        let runtimes = ManagedRuntimes::new(tmp.path().join("runtimes"));
        runtimes.install(&dl, UNPACK200_RUNTIME).await.unwrap();
        let bin = platform::unpack200_in_home(&runtimes.home(UNPACK200_RUNTIME));
        let (xz, pack, jar) = (
            tmp.path().join("lib.jar.pack.xz"),
            tmp.path().join("lib.jar.pack"),
            tmp.path().join("lib.jar"),
        );
        std::fs::write(&xz, PACK_XZ).unwrap();
        let sig = prepare_pack(&xz, &pack).unwrap();
        run_unpack200(&bin, &pack, &jar).await.unwrap();
        add_signature(&jar, &sig).unwrap();
        assert!(library_valid(&jar, &list(&[PACK_SIG_SHA1])).unwrap());
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&jar).unwrap()).unwrap();
        for (name, body) in PACKED_ENTRIES {
            let mut got = Vec::new();
            zip.by_name(name).unwrap().read_to_end(&mut got).unwrap();
            assert_eq!(got, body, "{name}");
        }
        let missing = tmp.path().join("missing.pack");
        assert!(run_unpack200(&bin, &missing, &jar).await.is_err());
    }
}
