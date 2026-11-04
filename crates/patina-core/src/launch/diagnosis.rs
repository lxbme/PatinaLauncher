//! Conservative explanations of explicit terminal errors, not root-cause attribution.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosisKind {
    JavaVersion,
    JvmArgument,
    HeapExhausted,
    HeapReservation,
    NativeMemory,
    PermissionDenied,
    DiskFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EvidenceContext {
    TerminalError,
    FailedRunLog,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnosis {
    kind: DiagnosisKind,
    evidence: String,
    required_java: Option<u16>,
    supported_java: Option<u16>,
    argument: Option<String>,
}

impl Diagnosis {
    pub fn kind(&self) -> DiagnosisKind {
        self.kind
    }
    pub fn evidence(&self) -> &str {
        &self.evidence
    }
    pub fn required_java(&self) -> Option<u16> {
        self.required_java
    }
    pub fn supported_java(&self) -> Option<u16> {
        self.supported_java
    }
    pub fn argument(&self) -> Option<&str> {
        self.argument.as_deref()
    }
}

use super::log::{Log4jXmlParser, LogLevel, Stream};
use regex::Regex;
use std::sync::LazyLock;

const INPUT_LIMIT: usize = 256 * 1024;
const EVIDENCE_LIMIT: usize = 4096;
static CLASS_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"class file version ([0-9]+)\.([0-9]+)(?:\)|,|$)").unwrap());
static SUPPORTED_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"class file versions up to ([0-9]+)\.([0-9]+)(?:\)|,|$)").unwrap()
});
static THROWABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[a-zA-Z_$][a-zA-Z0-9_$]*\.)+[a-zA-Z0-9_$]*(?:Exception|Error)(?::|$)").unwrap()
});
static LEVEL_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[[^\]]*\]\s*\[[^\]]*/(?:TRACE|DEBUG|INFO|WARN|ERROR|FATAL)\]:?\s*").unwrap()
});

fn safe_line(line: &str) -> bool {
    if line.len() > EVIDENCE_LIMIT
        || line.chars().any(|c| {
            c.is_control() || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}' | '\u{fffd}')
        })
    {
        return false;
    }
    let lower = line.to_ascii_lowercase();
    ![
        "token",
        "authorization",
        "bearer ",
        "password",
        "secret",
        "sessionid",
        "--",
        "clientid",
        "client_id",
        "xuid",
    ]
    .iter()
    .any(|s| lower.contains(s))
}

fn java_major(captures: regex::Captures<'_>) -> Option<u16> {
    let major = captures[1].parse::<u16>().ok()?;
    let minor = captures[2].parse::<u16>().ok()?;
    // Only published, non-preview class formats known to this release.
    (minor == 0 && (49..=70).contains(&major)).then(|| major - 44)
}

fn matched(line: &str, technical: &str, terminal: bool) -> Option<Diagnosis> {
    if !safe_line(line) {
        return None;
    }
    let mut result = Diagnosis {
        kind: DiagnosisKind::JavaVersion,
        evidence: line.to_string(),
        required_java: None,
        supported_java: None,
        argument: None,
    };
    if technical.starts_with("java.lang.UnsupportedClassVersionError:") {
        let version = CLASS_VERSION.captures(technical)?;
        result.required_java = java_major(version);
        result.supported_java = SUPPORTED_VERSION.captures(technical).and_then(java_major);
    } else if let Some(option) = technical
        .strip_prefix("Unrecognized VM option '")
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| technical.strip_prefix("Unrecognized option: "))
        .or_else(|| technical.strip_prefix("Invalid maximum heap size: "))
        .or_else(|| technical.strip_prefix("Invalid initial heap size: "))
    {
        if option.is_empty()
            || !option
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_:+=.,".contains(c))
        {
            return None;
        }
        result.kind = DiagnosisKind::JvmArgument;
        result.argument = Some(option.to_string());
    } else if technical == "java.lang.OutOfMemoryError: Java heap space" {
        result.kind = DiagnosisKind::HeapExhausted;
    } else if technical.starts_with("Could not reserve enough space for ")
        && (technical.ends_with(" object heap") || technical.ends_with("KB object heap"))
    {
        result.kind = DiagnosisKind::HeapReservation;
    } else if technical.starts_with("# Native memory allocation (")
        && technical.contains(") failed to ")
        && technical.contains(" bytes")
    {
        result.kind = DiagnosisKind::NativeMemory;
    } else if technical.starts_with("java.nio.file.AccessDeniedException:")
        || ((technical.starts_with("java.io.FileNotFoundException:")
            || technical.starts_with("java.nio.file.FileSystemException:"))
            && technical.ends_with(" (Permission denied)"))
        || (terminal
            && matches!(
                technical,
                "Permission denied"
                    | "Permission denied (os error 13)"
                    | "Access is denied. (os error 5)"
            ))
    {
        result.kind = DiagnosisKind::PermissionDenied;
    } else if ((technical.starts_with("java.io.IOException:")
        || technical.starts_with("java.nio.file.FileSystemException:"))
        && technical.ends_with("No space left on device"))
        || (terminal
            && matches!(
                technical,
                "No space left on device"
                    | "No space left on device (os error 28)"
                    | "There is not enough space on the disk. (os error 112)"
            ))
    {
        result.kind = DiagnosisKind::DiskFull;
    } else {
        return None;
    }
    Some(result)
}

/// Explains only explicit terminal grammars in a bounded excerpt. Log severity
/// alone is not evidence, and recovered/ordinary events break a failure chain.
/// This parser is not a general log credential scrubber.
pub fn analyze(text: &str, context: EvidenceContext) -> Option<Diagnosis> {
    let text = if text.len() > INPUT_LIMIT {
        let mut start = text.len() - INPUT_LIMIT;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        let tail = &text[start..];
        tail.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        text
    };
    let mut parser = Log4jXmlParser::new();
    let mut roots: Vec<Diagnosis> = Vec::new();
    let terminal = context == EvidenceContext::TerminalError;
    let mut active = terminal;
    let mut ordinary_event = false;
    for raw in text.lines() {
        for event in parser.push_line(raw, Stream::Stdout) {
            if matches!(
                event.level,
                LogLevel::Trace | LogLevel::Debug | LogLevel::Info | LogLevel::Warn
            ) {
                roots.clear();
                active = false;
                ordinary_event = true;
                continue;
            }
            if event.level != LogLevel::Unknown {
                ordinary_event = true;
                active = false;
            }
            let event_text = LEVEL_PREFIX.replace(&event.text, "");
            for line in event_text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with("at ") || line.starts_with("... ") {
                    continue;
                }
                if matches!(
                    line,
                    "Encountered an unexpected exception" | "Encountered an unexpected exception:"
                ) || line == "Unreported exception thrown!"
                    || line == "Error occurred during initialization of VM"
                {
                    roots.clear();
                    active = true;
                    ordinary_event = false;
                    continue;
                }
                let explicit_thread = line.starts_with("Exception in thread ");
                let causal = line.starts_with("Caused by: ");
                let technical = line.strip_prefix("Caused by: ").unwrap_or(line);
                let technical = if let Some(rest) = technical.strip_prefix("Exception in thread \"")
                {
                    rest.split_once("\" ").map_or(technical, |(_, rest)| rest)
                } else {
                    technical
                };
                let throwable = THROWABLE.is_match(technical);
                let vm = line.starts_with("Unrecognized ")
                    || line.starts_with("Invalid maximum heap size: ")
                    || line.starts_with("Invalid initial heap size: ")
                    || line.starts_with("Could not reserve enough space for ")
                    || line.starts_with("# Native memory allocation (");
                if explicit_thread || vm {
                    ordinary_event = false;
                    active = true;
                }
                if (throwable || vm || terminal)
                    && !ordinary_event
                    && (event.level == LogLevel::Unknown || active)
                {
                    if let Some(found) = matched(line, technical, terminal) {
                        if causal {
                            roots.pop();
                        }
                        roots.push(found);
                        active = true;
                        continue;
                    }
                    if throwable {
                        if !causal {
                            roots.clear();
                        }
                        active = true;
                        continue;
                    }
                }
                if line.starts_with("#")
                    || matches!(
                        line,
                        "Error: Could not create the Java Virtual Machine."
                            | "Error: A fatal exception has occurred. Program will exit."
                    )
                {
                    continue;
                }
                roots.clear();
                active = false;
            }
        }
    }
    let first = roots.first()?;
    roots.iter().all(|r| r == first).then(|| first.clone())
}

/// Reads only the supplied saved regular file; never searches sibling/current logs.
pub fn analyze_saved_log(path: &std::path::Path) -> std::io::Result<Option<Diagnosis>> {
    use std::io::{Read, Seek, SeekFrom};
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::other(
            "diagnosis input is not a regular file",
        ));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    #[cfg(unix)]
    let mut file = {
        use rustix::fs::{Mode, OFlags, open, openat};
        // Bind the read to the log directory descriptor, so replacing that
        // directory or its final file with a symlink cannot redirect the read.
        let directory = open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let name = path
            .file_name()
            .ok_or_else(|| std::io::Error::other("missing log name"))?;
        std::fs::File::from(openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?)
    };
    #[cfg(not(unix))]
    let mut file = {
        if std::fs::symlink_metadata(parent)?.file_type().is_symlink() {
            return Err(std::io::Error::other("linked log directory"));
        }
        std::fs::File::open(path)?
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::other(
            "diagnosis input is not a regular file",
        ));
    }
    let start = metadata.len().saturating_sub(INPUT_LIMIT as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(INPUT_LIMIT as u64).read_to_end(&mut bytes)?;
    let bytes = if start > 0 {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(&[][..], |ix| &bytes[ix + 1..])
    } else {
        &bytes
    };
    Ok(analyze(
        &String::from_utf8_lossy(bytes),
        EvidenceContext::FailedRunLog,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnosis_saved_tail_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("saved.log");
        let error = "java.io.IOException: No space left on device";
        std::fs::write(
            &path,
            format!("{error}\n{}\n", "x".repeat(INPUT_LIMIT + 10)),
        )
        .unwrap();
        assert!(analyze_saved_log(&path).unwrap().is_none());
        std::fs::write(
            &path,
            format!("{}\n{error}\n", "x".repeat(INPUT_LIMIT + 10)),
        )
        .unwrap();
        assert_eq!(analyze_saved_log(&path).unwrap().unwrap().evidence(), error);
        std::fs::write(&path, [b"\xff\xfe\n".as_slice(), error.as_bytes()].concat()).unwrap();
        assert_eq!(
            analyze_saved_log(&path).unwrap().unwrap().kind(),
            DiagnosisKind::DiskFull
        );
        std::fs::write(&path, format!("{error}{}", "x".repeat(INPUT_LIMIT))).unwrap();
        assert!(analyze_saved_log(&path).unwrap().is_none());
    }

    #[test]
    fn diagnosis_saved_missing_nonregular_and_symlink_do_not_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("current.log"),
            "java.io.IOException: No space left on device",
        )
        .unwrap();
        assert!(analyze_saved_log(&dir.path().join("missing.log")).is_err());
        assert!(analyze_saved_log(dir.path()).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("current.log"), dir.path().join("link.log"))
                .unwrap();
            assert!(analyze_saved_log(&dir.path().join("link.log")).is_err());
            std::os::unix::fs::symlink(dir.path(), dir.path().join("linked-directory")).unwrap();
            assert!(analyze_saved_log(&dir.path().join("linked-directory/current.log")).is_err());
            rustix::fs::mknodat(
                rustix::fs::CWD,
                dir.path().join("fifo"),
                rustix::fs::FileType::Fifo,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
                0,
            )
            .unwrap();
            assert!(analyze_saved_log(&dir.path().join("fifo")).is_err());
        }
    }

    const JAVA: &str = "Exception in thread \"main\" java.lang.UnsupportedClassVersionError: example/Main has been compiled by a more recent version of the Java Runtime (class file version 65.0), this version of the Java Runtime only recognizes class file versions up to 61.0";

    #[test]
    fn diagnosis_explicit_terminal_formats() {
        for (text, kind) in [
            (JAVA, DiagnosisKind::JavaVersion),
            (
                "Unrecognized VM option 'UseConcMarkSweepGC'",
                DiagnosisKind::JvmArgument,
            ),
            (
                "Unrecognized option: -notARealOption",
                DiagnosisKind::JvmArgument,
            ),
            (
                "Invalid maximum heap size: -Xmxnope",
                DiagnosisKind::JvmArgument,
            ),
            (
                "Exception in thread \"Render thread\" java.lang.OutOfMemoryError: Java heap space",
                DiagnosisKind::HeapExhausted,
            ),
            (
                "Error occurred during initialization of VM\nCould not reserve enough space for 2097152KB object heap",
                DiagnosisKind::HeapReservation,
            ),
            (
                "# There is insufficient memory for the Java Runtime Environment to continue.\n# Native memory allocation (mmap) failed to map 123456 bytes. Error detail: committing reserved memory.",
                DiagnosisKind::NativeMemory,
            ),
            (
                "Exception in thread \"main\" java.nio.file.AccessDeniedException: /fixture/game",
                DiagnosisKind::PermissionDenied,
            ),
            (
                "java.io.FileNotFoundException: /fixture/game (Permission denied)",
                DiagnosisKind::PermissionDenied,
            ),
            (
                "java.io.IOException: No space left on device",
                DiagnosisKind::DiskFull,
            ),
            (
                "Permission denied (os error 13)",
                DiagnosisKind::PermissionDenied,
            ),
            (
                "No space left on device (os error 28)",
                DiagnosisKind::DiskFull,
            ),
        ] {
            let result =
                analyze(text, EvidenceContext::TerminalError).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(result.kind(), kind, "{text}");
            assert!(!result.evidence().is_empty());
            assert!(text.contains(result.evidence()));
        }
        let result = analyze(JAVA, EvidenceContext::FailedRunLog).unwrap();
        assert_eq!(result.required_java(), Some(21));
        assert_eq!(result.supported_java(), Some(17));
        assert_eq!(result.argument(), None);
        assert_eq!(result.evidence(), JAVA);
        assert_eq!(
            analyze(
                "Unrecognized VM option 'UseConcMarkSweepGC'",
                EvidenceContext::FailedRunLog
            )
            .unwrap()
            .argument(),
            Some("UseConcMarkSweepGC")
        );
    }

    #[test]
    fn diagnosis_versions_never_invent_numbers() {
        for (version, expected) in [
            ("65.0", Some(21)),
            ("100.0", None),
            ("65.65535", None),
            ("999999999.0", None),
        ] {
            let text = format!(
                "java.lang.UnsupportedClassVersionError: example/Main (class file version {version})"
            );
            let result = analyze(&text, EvidenceContext::TerminalError).unwrap();
            assert_eq!(result.required_java(), expected);
            assert_eq!(result.supported_java(), None);
        }
        assert!(
            analyze(
                "java.lang.UnsupportedClassVersionError: truncated class file version 6",
                EvidenceContext::TerminalError
            )
            .is_none()
        );
    }

    #[test]
    fn diagnosis_ordinary_event_continuations_are_not_terminal() {
        for level in ["INFO", "WARN", "ERROR"] {
            let text = format!(
                "[12:00:00] [main/{level}]: Optional file test failed\njava.io.IOException: No space left on device\n\tat example.Optional.test(Optional.java:1)"
            );
            assert!(
                analyze(&text, EvidenceContext::FailedRunLog).is_none(),
                "{level}"
            );
        }
        let genuine = format!("[12:00:00] [main/INFO]: Starting\n{JAVA}");
        assert_eq!(
            analyze(&genuine, EvidenceContext::FailedRunLog)
                .unwrap()
                .kind(),
            DiagnosisKind::JavaVersion
        );
    }

    #[test]
    fn diagnosis_structured_events_and_quoted_markers() {
        for level in ["INFO", "WARN", "ERROR"] {
            let quoted = format!(
                "<log4j:Event level=\"{level}\"><log4j:Message><![CDATA[Documentation: Encountered an unexpected exception]]></log4j:Message><log4j:Throwable><![CDATA[java.lang.OutOfMemoryError: Java heap space]]></log4j:Throwable></log4j:Event>"
            );
            assert!(
                analyze(&quoted, EvidenceContext::FailedRunLog).is_none(),
                "{level}"
            );
        }
        for text in [
            "[12:00:00] [Render thread/ERROR]: Encountered an unexpected exception\njava.lang.OutOfMemoryError: Java heap space\n\tat example.Main.run(Main.java:1)",
            "<log4j:Event level=\"ERROR\"><log4j:Message><![CDATA[Encountered an unexpected exception]]></log4j:Message><log4j:Throwable><![CDATA[java.lang.OutOfMemoryError: Java heap space\n\tat example.Main.run(Main.java:1)]]></log4j:Throwable></log4j:Event>",
        ] {
            assert_eq!(
                analyze(text, EvidenceContext::FailedRunLog).unwrap().kind(),
                DiagnosisKind::HeapExhausted
            );
        }
    }

    #[test]
    fn diagnosis_context_and_out_of_scope_abstain() {
        for text in [
            "Could not create the Java Virtual Machine.",
            "Process exited with code 137 (SIGKILL)",
            "java.lang.NoSuchMethodError: example.mod.Broken",
            "org.spongepowered.asm.mixin.throwables.MixinApplyError: broken",
            "[12:00:00] [main/ERROR]: Optional test says Java heap space",
            "[12:00:00] [Render thread/INFO]: [CHAT] java.lang.OutOfMemoryError: Java heap space",
            "[12:00:00] [main/WARN]: java.io.IOException: No space left on device",
            "[12:00:00] [main/INFO]: Unrecognized VM option 'test'",
            "Quoted: java.io.IOException: No space left on device",
            "java.lang.OutOfMemoryError: Java heap space\n[12:00:01] [main/INFO]: recovered\njava.lang.NoSuchMethodError: other",
            "java.io.IOException: Disk quota exceeded",
            "java.io.IOException: Corrupt file",
            "java.lang.OutOfMemoryError: Metaspace",
            "Permission denied (os error 13)",
        ] {
            assert_eq!(analyze(text, EvidenceContext::FailedRunLog), None, "{text}");
        }
    }

    #[test]
    fn diagnosis_specific_chain_duplicates_and_conflicting_roots() {
        let chain = "Exception in thread \"main\" java.lang.RuntimeException: Failed to start\nCaused by: java.io.IOException: wrapper\nCaused by: java.nio.file.AccessDeniedException: /fixture\n\tat example.Main.run(Main.java:1)";
        assert_eq!(
            analyze(chain, EvidenceContext::FailedRunLog)
                .unwrap()
                .kind(),
            DiagnosisKind::PermissionDenied
        );
        let duplicate = "java.io.IOException: No space left on device\njava.io.IOException: No space left on device";
        assert_eq!(
            analyze(duplicate, EvidenceContext::FailedRunLog)
                .unwrap()
                .kind(),
            DiagnosisKind::DiskFull
        );
        let conflict = "Exception in thread \"main\" java.lang.OutOfMemoryError: Java heap space\nException in thread \"worker\" java.nio.file.AccessDeniedException: /fixture";
        assert_eq!(analyze(conflict, EvidenceContext::FailedRunLog), None);
    }

    #[test]
    fn diagnosis_evidence_does_not_publish_adjacent_secrets_or_controls() {
        let text = format!("Launching --accessToken SECRET --username PRIVATE\n{JAVA}");
        assert_eq!(
            analyze(&text, EvidenceContext::FailedRunLog)
                .unwrap()
                .evidence(),
            JAVA
        );
        for suffix in [
            " --accessToken SECRET",
            " Authorization: Bearer SECRET",
            " password=SECRET",
            "\u{1b}[2J",
            "\u{202e}hidden",
            "\u{200f}hidden",
            "\u{2028}hidden",
        ] {
            assert!(analyze(&format!("{JAVA}{suffix}"), EvidenceContext::FailedRunLog).is_none());
        }
        assert!(
            analyze(
                &format!("java.nio.file.AccessDeniedException: /{}", "x".repeat(5000)),
                EvidenceContext::TerminalError
            )
            .is_none()
        );
    }
}
