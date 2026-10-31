use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct JavaInstall {
    pub path: PathBuf,
    pub major: u32,
    pub version: String,
    pub is_64bit: bool,
}

/// Extracts the major version: "1.8.0_412" -> 8, "17.0.12" -> 17, "25-ea" -> 25.
pub fn parse_major(version: &str) -> Option<u32> {
    fn leading_number(s: &str) -> Option<u32> {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        s[..end].parse().ok()
    }
    let version = version.trim();
    match version.strip_prefix("1.") {
        Some(rest) => leading_number(rest),
        None => leading_number(version),
    }
}

/// Returns `(JAVA_VERSION, OS_ARCH)` from a JDK `release` file, quotes stripped.
pub fn parse_release_file(s: &str) -> Option<(String, Option<String>)> {
    let mut version = None;
    let mut arch = None;
    for line in s.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "JAVA_VERSION" => version = Some(value),
            "OS_ARCH" => arch = Some(value),
            _ => {}
        }
    }
    Some((version.filter(|v| !v.is_empty())?, arch))
}

/// Parses `java -XshowSettings:properties -version` output into
/// `(java.version, is_64bit)`. A missing data model counts as 64-bit.
pub fn parse_properties_output(stderr: &str) -> Option<(String, bool)> {
    let mut version = None;
    let mut is_64 = true;
    for line in stderr.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "java.version" => version = Some(value.trim().to_string()),
            "sun.arch.data.model" => is_64 = value.trim() != "32",
            _ => {}
        }
    }
    Some((version.filter(|v| !v.is_empty())?, is_64))
}

fn arch_is_64bit(arch: Option<&str>) -> bool {
    !matches!(arch, Some("i386" | "i486" | "i586" | "i686" | "x86"))
}

fn run_properties(java_bin: &Path) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // A binary written moments ago can still be held open for writing by a
    // child forked concurrently elsewhere in the process, until that child
    // execs; exec then fails with ETXTBSY. That clears within milliseconds.
    let mut attempt = 0;
    let mut child = loop {
        match Command::new(java_bin)
            .args(["-XshowSettings:properties", "-version"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => break c,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 50 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    };
    let mut stderr = child.stderr.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return reader.join().ok(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                // Timed out (or wait failed): kill; the reader thread ends
                // once the pipe closes and is intentionally not joined.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Probes `java_bin` (`<home>/bin/java`): reads `<home>/release` if present,
/// otherwise runs `java -XshowSettings:properties -version` (5 s timeout).
pub fn probe(java_bin: &Path) -> Option<JavaInstall> {
    let path = java_bin.canonicalize().ok()?;
    if !path.is_file() {
        return None;
    }
    let from_release = path
        .parent()
        .and_then(Path::parent)
        .and_then(|home| std::fs::read_to_string(home.join("release")).ok())
        .and_then(|s| parse_release_file(&s))
        .map(|(version, arch)| (version, arch_is_64bit(arch.as_deref())));
    let (version, is_64bit) = match from_release {
        Some(v) => v,
        None => parse_properties_output(&run_properties(&path)?)?,
    };
    let major = parse_major(&version)?;
    Some(JavaInstall {
        path,
        major,
        version,
        is_64bit,
    })
}

/// Probes every platform candidate; dedups by canonical path, sorts by major
/// descending then path ascending.
pub fn detect_all() -> Vec<JavaInstall> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<JavaInstall> = crate::platform::java_candidates()
        .iter()
        .filter_map(|c| probe(c))
        .filter(|j| seen.insert(j.path.clone()))
        .collect();
    out.sort_by(|a, b| b.major.cmp(&a.major).then_with(|| a.path.cmp(&b.path)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn majors() {
        assert_eq!(parse_major("1.8.0_412"), Some(8));
        assert_eq!(parse_major("17.0.12"), Some(17));
        assert_eq!(parse_major("21"), Some(21));
        assert_eq!(parse_major("25-ea"), Some(25));
        assert_eq!(parse_major("garbage"), None);
    }

    #[test]
    fn release_file() {
        assert_eq!(
            parse_release_file("IMPLEMENTOR=\"X\"\nJAVA_VERSION=\"21.0.5\"\nOS_ARCH=\"x86_64\"\n"),
            Some(("21.0.5".into(), Some("x86_64".into())))
        );
        assert_eq!(
            parse_release_file("JAVA_VERSION=\"17\"\n"),
            Some(("17".into(), None))
        );
        assert_eq!(parse_release_file("FOO=\"1\"\n"), None);
    }

    #[test]
    fn properties_output() {
        let s = "Property settings:\n    java.version = 17.0.12\n    sun.arch.data.model = 64\n";
        assert_eq!(parse_properties_output(s), Some(("17.0.12".into(), true)));
        let s = "    java.version = 1.8.0_1\n    sun.arch.data.model = 32\n";
        assert_eq!(parse_properties_output(s), Some(("1.8.0_1".into(), false)));
        let s = "    java.version = 21\n";
        assert_eq!(parse_properties_output(s), Some(("21".into(), true)));
        assert_eq!(parse_properties_output("nothing"), None);
    }

    #[cfg(unix)]
    fn fake_jdk(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("jdk/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let java = bin.join("java");
        std::fs::write(&java, script).unwrap();
        std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
        java
    }

    #[cfg(unix)]
    #[test]
    fn probe_fake_jdk_via_properties() {
        let tmp = tempfile::tempdir().unwrap();
        let java = fake_jdk(
            tmp.path(),
            "#!/bin/sh\necho '    java.version = 1.8.0_202' >&2\necho '    sun.arch.data.model = 64' >&2\n",
        );
        let got = probe(&java).unwrap();
        assert_eq!(got.major, 8);
        assert_eq!(got.version, "1.8.0_202");
        assert!(got.is_64bit);
        assert_eq!(got.path, java.canonicalize().unwrap());
    }

    /// A just-written executable can still be open for writing in a child
    /// forked by a concurrent spawn (ETXTBSY on exec); the probe must wait
    /// that out instead of reporting the Java as unusable.
    #[cfg(unix)]
    #[test]
    fn probe_retries_while_the_binary_is_busy() {
        let tmp = tempfile::tempdir().unwrap();
        let java = fake_jdk(
            tmp.path(),
            "#!/bin/sh\necho '    java.version = 21' >&2\necho '    sun.arch.data.model = 64' >&2\n",
        );
        // An open write handle makes exec fail with ETXTBSY.
        let writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&java)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(writer);
        });
        let got = probe(&java).expect("probe must outlast a transient ETXTBSY");
        release.join().unwrap();
        assert_eq!(got.major, 21);
    }

    #[cfg(unix)]
    #[test]
    fn probe_prefers_release_file() {
        let tmp = tempfile::tempdir().unwrap();
        // script would fail if executed
        let java = fake_jdk(tmp.path(), "#!/bin/sh\nexit 1\n");
        std::fs::write(
            tmp.path().join("jdk/release"),
            "JAVA_VERSION=\"21.0.5\"\nOS_ARCH=\"i386\"\n",
        )
        .unwrap();
        let got = probe(&java).unwrap();
        assert_eq!(
            (got.major, got.version.as_str(), got.is_64bit),
            (21, "21.0.5", false)
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        // `exec`: the probe kills only the process it started, so a forked
        // `sleep` would outlive the test (and hold the stderr pipe open).
        let java = fake_jdk(tmp.path(), "#!/bin/sh\nexec sleep 30\n");
        let t = std::time::Instant::now();
        assert_eq!(probe(&java), None);
        assert!(t.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn probe_missing_is_none() {
        assert_eq!(probe(Path::new("/nonexistent/bin/java")), None);
    }

    #[test]
    #[ignore]
    fn detects_this_machine() {
        let all = detect_all();
        for j in &all {
            println!("{j:?}");
        }
        assert!(all.iter().any(|j| j.major >= 17));
    }
}
