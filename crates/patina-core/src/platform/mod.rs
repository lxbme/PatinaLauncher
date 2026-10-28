//! Platform abstraction. All OS-specific logic lives here; other modules
//! must go through `crate::platform::*`.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meminfo_kib_to_mib() {
        let s = "MemTotal:       16141312 kB\nMemFree:  1 kB\nMemAvailable:    8699904 kB\n";
        assert_eq!(parse_meminfo(s), (Some(15763), Some(8496)));
    }
}
