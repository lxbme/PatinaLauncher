//! Shared tracing capture for unit tests.

use std::sync::{Arc, Mutex, OnceLock};

/// An in-memory `MakeWriter` target.
#[derive(Clone, Default)]
pub(crate) struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;
    fn make_writer(&'a self) -> LogBuffer {
        self.clone()
    }
}

impl LogBuffer {
    /// Captured lines containing `needle`. Use a string unique to the test,
    /// such as a random path segment: a mock server URI is not unique, since
    /// wiremock reuses pooled servers across tests.
    pub(crate) fn lines_containing(&self, needle: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains(needle))
            .map(str::to_string)
            .collect()
    }
}

/// Installs (once) a process-wide INFO-level capturing subscriber and returns
/// its buffer. A thread-local subscriber races with the callsite interest
/// cache that concurrently running tests populate. If another global
/// subscriber already exists the install error is ignored; callers filter by
/// a unique needle.
pub(crate) fn test_log_buffer() -> LogBuffer {
    static LOG: OnceLock<LogBuffer> = OnceLock::new();
    LOG.get_or_init(|| {
        let buf = LogBuffer::default();
        let _ = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .try_init();
        buf
    })
    .clone()
}
