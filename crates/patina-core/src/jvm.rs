//! JVM memory model, GC presets and extra-argument validation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcPreset {
    #[default]
    None,
    G1Aikar,
    GenerationalZgc,
    Custom,
}

const AIKAR_ARGS: [&str; 18] = [
    "-XX:+UseG1GC",
    "-XX:+ParallelRefProcEnabled",
    "-XX:MaxGCPauseMillis=200",
    "-XX:+UnlockExperimentalVMOptions",
    "-XX:+DisableExplicitGC",
    "-XX:+AlwaysPreTouch",
    "-XX:G1NewSizePercent=30",
    "-XX:G1MaxNewSizePercent=40",
    "-XX:G1HeapRegionSize=8M",
    "-XX:G1ReservePercent=20",
    "-XX:G1HeapWastePercent=5",
    "-XX:G1MixedGCCountTarget=4",
    "-XX:InitiatingHeapOccupancyPercent=15",
    "-XX:G1MixedGCLiveThresholdPercent=90",
    "-XX:G1RSetUpdatingPauseTimePercent=5",
    "-XX:SurvivorRatio=32",
    "-XX:+PerfDisableSharedMem",
    "-XX:MaxTenuringThreshold=1",
];

impl GcPreset {
    pub fn min_java_major(self) -> u32 {
        match self {
            GcPreset::None | GcPreset::Custom => 0,
            GcPreset::G1Aikar => 8,
            GcPreset::GenerationalZgc => 21,
        }
    }

    pub fn is_compatible(self, java_major: u32) -> bool {
        java_major >= self.min_java_major()
    }

    pub fn args(self, java_major: u32) -> Vec<&'static str> {
        match self {
            GcPreset::None | GcPreset::Custom => Vec::new(),
            GcPreset::G1Aikar => AIKAR_ARGS.to_vec(),
            GcPreset::GenerationalZgc => {
                if java_major >= 23 {
                    vec!["-XX:+UseZGC"]
                } else {
                    vec!["-XX:+UseZGC", "-XX:+ZGenerational"]
                }
            }
        }
    }
}

/// Two thirds of physical memory, capped at 4096 MiB.
pub fn default_max_mem_mib(total_mib: u64) -> u32 {
    (total_mib * 2 / 3).min(4096) as u32
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JvmArgsError {
    #[error("{}", rust_i18n::t!("errors.jvm.memory_flag", flag = .0))]
    MemoryFlag(String),
    #[error("{}", rust_i18n::t!("errors.jvm.version_flag"))]
    VersionFlag,
}

const MEMORY_PREFIXES: [&str; 6] = [
    "-Xmx",
    "-Xms",
    "-XX:MaxHeapSize",
    "-XX:InitialHeapSize",
    "-XX:PermSize",
    "-XX:MaxPermSize",
];

pub fn validate_extra_jvm_args(args: &str) -> Result<(), JvmArgsError> {
    for tok in split_args(args) {
        if MEMORY_PREFIXES.iter().any(|p| tok.starts_with(p)) {
            return Err(JvmArgsError::MemoryFlag(tok));
        }
        if tok.starts_with("-version:") {
            return Err(JvmArgsError::VersionFlag);
        }
    }
    Ok(())
}

/// Splits on whitespace, honouring single and double quotes (no variable
/// expansion). An unterminated quote swallows the rest as one token.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has_tok = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                has_tok = true;
            }
            None if c.is_whitespace() => {
                if has_tok {
                    out.push(std::mem::take(&mut cur));
                    has_tok = false;
                }
            }
            None => {
                cur.push(c);
                has_tok = true;
            }
        }
    }
    if has_tok {
        out.push(cur);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryCheck {
    Ok,
    Low {
        requested_mib: u32,
        available_mib: u64,
    },
}

/// Warns when the requested memory exceeds 70% of the available memory.
pub fn check_available_memory(min_mib: u32, max_mib: u32, available_mib: u64) -> MemoryCheck {
    let requested = min_mib.max(max_mib);
    if (requested as f64) * 0.7 > available_mib as f64 {
        MemoryCheck::Low {
            requested_mib: requested,
            available_mib,
        }
    } else {
        MemoryCheck::Ok
    }
}

/// 32-bit JVMs cannot address more than about 2 GiB of heap.
pub fn clamp_for_arch(max_mib: u32, is_64bit: bool) -> u32 {
    if is_64bit { max_mib } else { max_mib.min(2048) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_memory() {
        assert_eq!(default_max_mem_mib(4096), 2730);
        assert_eq!(default_max_mem_mib(6143), 4095);
        assert_eq!(default_max_mem_mib(6144), 4096);
        assert_eq!(default_max_mem_mib(32768), 4096);
    }
    #[test]
    fn rejects_memory_and_version_flags() {
        assert!(matches!(
            validate_extra_jvm_args("-Xmx4G"),
            Err(JvmArgsError::MemoryFlag(_))
        ));
        assert!(matches!(
            validate_extra_jvm_args("-Dx=1 -Xms1G"),
            Err(JvmArgsError::MemoryFlag(_))
        ));
        assert!(matches!(
            validate_extra_jvm_args("-XX:MaxHeapSize=1g"),
            Err(JvmArgsError::MemoryFlag(_))
        ));
        assert!(matches!(
            validate_extra_jvm_args("-XX:InitialHeapSize=1g"),
            Err(JvmArgsError::MemoryFlag(_))
        ));
        assert!(matches!(
            validate_extra_jvm_args("-XX:PermSize=128m"),
            Err(JvmArgsError::MemoryFlag(_))
        ));
        assert!(matches!(
            validate_extra_jvm_args("-version:1.8"),
            Err(JvmArgsError::VersionFlag)
        ));
        assert!(validate_extra_jvm_args("-Dfoo=bar -XX:+UseG1GC").is_ok());
        assert!(validate_extra_jvm_args("").is_ok());
    }
    #[test]
    fn presets() {
        assert_eq!(GcPreset::default(), GcPreset::None);
        assert!(GcPreset::None.args(25).is_empty());
        assert!(GcPreset::Custom.args(25).is_empty());
        assert!(!GcPreset::GenerationalZgc.is_compatible(17));
        assert_eq!(
            GcPreset::GenerationalZgc.args(21),
            vec!["-XX:+UseZGC", "-XX:+ZGenerational"]
        );
        assert_eq!(GcPreset::GenerationalZgc.args(23), vec!["-XX:+UseZGC"]);
        let g1 = GcPreset::G1Aikar.args(8);
        assert_eq!(g1.len(), 18);
        assert_eq!(g1[0], "-XX:+UseG1GC");
        assert!(GcPreset::G1Aikar.is_compatible(8));
    }
    #[test]
    fn memory_check() {
        assert_eq!(
            check_available_memory(512, 4096, 2000),
            MemoryCheck::Low {
                requested_mib: 4096,
                available_mib: 2000
            }
        );
        assert_eq!(check_available_memory(512, 4096, 2868), MemoryCheck::Ok); // 4096*0.7 = 2867.2
        assert_eq!(
            check_available_memory(6000, 4096, 4000),
            MemoryCheck::Low {
                requested_mib: 6000,
                available_mib: 4000
            }
        );
    }
    #[test]
    fn arch_clamp() {
        assert_eq!(clamp_for_arch(4096, false), 2048);
        assert_eq!(clamp_for_arch(4096, true), 4096);
    }
    #[test]
    fn split() {
        assert_eq!(
            split_args(r#"-Da="x y" -Db  '-Dc=z w'"#),
            vec!["-Da=x y", "-Db", "-Dc=z w"]
        );
    }
    #[test]
    fn split_edge_cases() {
        assert!(split_args("").is_empty());
        assert!(split_args("   ").is_empty());
        assert_eq!(split_args(r#"-Da="x y"#), vec!["-Da=x y"]);
        assert_eq!(split_args(r#""" a"#), vec!["", "a"]);
    }
    #[test]
    fn gc_preset_serde_snake_case() {
        #[derive(Deserialize, Serialize)]
        struct W {
            g: GcPreset,
        }
        let w: W = toml::from_str("g = \"none\"").unwrap();
        assert_eq!(w.g, GcPreset::None);
        let w: W = toml::from_str("g = \"generational_zgc\"").unwrap();
        assert_eq!(w.g, GcPreset::GenerationalZgc);
        assert!(
            toml::to_string(&W {
                g: GcPreset::G1Aikar
            })
            .unwrap()
            .contains("g1_aikar")
        );
    }
}
