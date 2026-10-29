//! Spawn the game process and stream its output as `LogLine`s.

use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

use super::args::LaunchCommand;
use super::log::{Log4jXmlParser, LogLine, Stream};

/// How long `wait` lets the readers drain after the child has exited.
const READER_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Deprecated: piped, kill-on-drop process used only by patina-dev. Replaced by
/// detached processes (`platform::spawn_detached`) and `GameSession`; to be
/// removed once patina-dev switches over.
pub struct GameProcess {
    child: tokio::process::Child,
    readers: Vec<tokio::task::JoinHandle<()>>,
}

async fn pump<R>(reader: R, stream: Stream, sink: UnboundedSender<LogLine>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut parser = Log4jXmlParser::new();
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if raw.last() == Some(&b'\n') {
            raw.pop();
            if raw.last() == Some(&b'\r') {
                raw.pop();
            }
        }
        let line = String::from_utf8_lossy(&raw);
        for l in parser.push_line(&line, stream) {
            let _ = sink.send(l);
        }
    }
    for l in parser.finish(stream) {
        let _ = sink.send(l);
    }
}

impl GameProcess {
    pub async fn spawn(
        cmd: &LaunchCommand,
        sink: UnboundedSender<LogLine>,
    ) -> io::Result<GameProcess> {
        let mut child = Command::new(&cmd.program)
            .args(&cmd.args)
            .current_dir(&cmd.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut readers = Vec::new();
        if let Some(out) = child.stdout.take() {
            readers.push(tokio::spawn(pump(out, Stream::Stdout, sink.clone())));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(tokio::spawn(pump(err, Stream::Stderr, sink)));
        }
        Ok(GameProcess { child, readers })
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Waits for exit, then for both readers to drain so no trailing lines are lost.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait().await?;
        // A detached grandchild may hold the pipes open; don't wait forever.
        let deadline = tokio::time::Instant::now() + READER_DRAIN_TIMEOUT;
        for mut h in self.readers.drain(..) {
            if tokio::time::timeout_at(deadline, &mut h).await.is_err() {
                h.abort();
            }
        }
        Ok(status)
    }

    /// Kills the process and reaps it; fine to call after it already exited.
    pub async fn kill(&mut self) -> io::Result<()> {
        match self.child.start_kill() {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {}
            Err(e) => return Err(e),
        }
        self.child.wait().await?;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::launch::log::{LogLevel, Stream};
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> LaunchCommand {
        LaunchCommand {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: std::env::temp_dir(),
        }
    }

    #[tokio::test]
    async fn spawns_streams_and_reports_exit() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut p = GameProcess::spawn(&sh("echo \"[main/INFO]: hi\"; echo err 1>&2; exit 3"), tx)
            .await
            .unwrap();
        let st = p.wait().await.unwrap();
        assert_eq!(st.code(), Some(3));
        drop(p);
        let mut lines = vec![];
        while let Some(l) = rx.recv().await {
            lines.push(l);
        }
        assert!(lines.iter().any(|l| l.level == LogLevel::Info
            && l.text.ends_with("hi")
            && l.stream == Stream::Stdout));
        assert!(
            lines
                .iter()
                .any(|l| l.stream == Stream::Stderr && l.text == "err")
        );
    }

    #[tokio::test]
    async fn non_utf8_output_is_lossy() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut p = GameProcess::spawn(&sh("printf 'a\\377b\\r\\n'"), tx)
            .await
            .unwrap();
        p.wait().await.unwrap();
        let l = rx.recv().await.unwrap();
        assert_eq!(l.text, "a\u{FFFD}b");
    }

    #[tokio::test]
    async fn wait_not_blocked_by_detached_grandchild() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut p = GameProcess::spawn(&sh("echo first; (sleep 30) & echo last; exit 0"), tx)
            .await
            .unwrap();
        let t = Instant::now();
        let st = p.wait().await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(4));
        assert_eq!(st.code(), Some(0));
        let mut texts = vec![];
        while let Ok(l) = rx.try_recv() {
            texts.push(l.text);
        }
        assert!(texts.contains(&"first".to_string()));
        assert!(texts.contains(&"last".to_string()));
    }

    #[tokio::test]
    async fn partial_event_at_eof_is_delivered() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut p = GameProcess::spawn(&sh("echo '<log4j:Event level=\"INFO\">'"), tx)
            .await
            .unwrap();
        p.wait().await.unwrap();
        let l = rx.recv().await.unwrap();
        assert!(l.text.starts_with("<log4j:Event"));
    }

    #[tokio::test]
    async fn kill_terminates() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut p = GameProcess::spawn(&sh("exec sleep 30"), tx).await.unwrap();
        let t = Instant::now();
        p.kill().await.unwrap();
        let st = p.wait().await.unwrap();
        assert!(!st.success());
        assert!(t.elapsed() < Duration::from_secs(2));
        p.kill().await.unwrap(); // idempotent
    }
}
