//! Game output parsing: log4j XMLLayout events and plain log lines.

use std::sync::LazyLock;

use regex::Regex;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    pub level: LogLevel,
    pub text: String,
    pub stream: Stream,
}

const MAX_EVENT_BYTES: usize = 1024 * 1024;
const MAX_EVENT_LINES: usize = 10_000;
const EVENT_START: &str = "<log4j:Event";
const EVENT_END: &str = "</log4j:Event>";

static PLAIN_LEVEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[[^\]]*/(TRACE|DEBUG|INFO|WARN|ERROR|FATAL)\]").expect("valid regex")
});

fn parse_level(s: &str) -> LogLevel {
    match s {
        "TRACE" => LogLevel::Trace,
        "DEBUG" => LogLevel::Debug,
        "INFO" => LogLevel::Info,
        "WARN" => LogLevel::Warn,
        "ERROR" => LogLevel::Error,
        "FATAL" => LogLevel::Fatal,
        _ => LogLevel::Unknown,
    }
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Content of `<tag>...</tag>`; CDATA is literal, anything else is entity-decoded.
fn element_text(buf: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = buf.find(&open)? + open.len();
    let end = start + buf[start..].find(&close)?;
    let inner = buf[start..end].trim_start();
    if let Some(rest) = inner.strip_prefix("<![CDATA[") {
        let body = rest.rfind("]]>").map_or(rest, |i| &rest[..i]);
        Some(body.to_string())
    } else {
        Some(decode_entities(inner.trim_end()))
    }
}

fn parse_event(buf: &str, stream: Stream) -> LogLine {
    let header = buf.split('>').next().unwrap_or("");
    let level = header
        .split_once("level=\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map_or(LogLevel::Unknown, |(v, _)| parse_level(&decode_entities(v)));
    let mut text = element_text(buf, "log4j:Message").unwrap_or_default();
    if let Some(t) = element_text(buf, "log4j:Throwable") {
        text.push('\n');
        text.push_str(&t);
    }
    LogLine {
        level,
        text,
        stream,
    }
}

fn plain_line(line: &str, stream: Stream) -> LogLine {
    let mut level = PLAIN_LEVEL
        .captures(line)
        .map_or(LogLevel::Unknown, |c| parse_level(&c[1]));
    if level == LogLevel::Unknown && stream == Stream::Stderr {
        level = LogLevel::Warn;
    }
    LogLine {
        level,
        text: line.to_string(),
        stream,
    }
}

fn flush_plain(buf: &str, stream: Stream) -> Vec<LogLine> {
    let level = if stream == Stream::Stderr {
        LogLevel::Warn
    } else {
        LogLevel::Unknown
    };
    buf.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| LogLine {
            level,
            text: l.to_string(),
            stream,
        })
        .collect()
}

/// Incremental parser; one instance per output stream.
#[derive(Default)]
pub struct Log4jXmlParser {
    buf: Option<String>,
    lines: usize,
}

impl Log4jXmlParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_line(&mut self, line: &str, stream: Stream) -> Vec<LogLine> {
        if self.buf.is_none() {
            if line.trim_start().starts_with(EVENT_START) {
                self.buf = Some(String::new());
                self.lines = 0;
            } else if line.trim().is_empty() {
                return Vec::new();
            } else {
                return vec![plain_line(line, stream)];
            }
        }
        let buf = self.buf.as_mut().expect("buffering");
        if self.lines > 0 {
            buf.push('\n');
        }
        buf.push_str(line);
        self.lines += 1;

        if line.contains(EVENT_END) {
            let done = self.buf.take().expect("buffering");
            self.lines = 0;
            return vec![parse_event(&done, stream)];
        }
        if buf.len() > MAX_EVENT_BYTES || self.lines > MAX_EVENT_LINES {
            let done = self.buf.take().expect("buffering");
            self.lines = 0;
            return flush_plain(&done, stream);
        }
        Vec::new()
    }

    /// Call at end of stream: flushes a buffered, unterminated event as plain lines.
    pub fn finish(&mut self, stream: Stream) -> Vec<LogLine> {
        self.lines = 0;
        self.buf
            .take()
            .map_or_else(Vec::new, |b| flush_plain(&b, stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiline_xml_event() {
        let mut p = Log4jXmlParser::new();
        let mut out = vec![];
        for l in [
            r#"<log4j:Event logger="net.minecraft.client.Minecraft" timestamp="1" level="INFO" thread="Render thread">"#,
            r#"  <log4j:Message><![CDATA[Setting user: Dev]]></log4j:Message>"#,
            r#"</log4j:Event>"#,
        ] {
            out.extend(p.push_line(l, Stream::Stdout));
        }
        assert_eq!(
            out,
            vec![LogLine {
                level: LogLevel::Info,
                text: "Setting user: Dev".into(),
                stream: Stream::Stdout
            }]
        );
    }

    #[test]
    fn parses_single_line_event_with_throwable() {
        let mut p = Log4jXmlParser::new();
        let l = "<log4j:Event level=\"ERROR\" thread=\"t\"><log4j:Message><![CDATA[boom]]></log4j:Message><log4j:Throwable><![CDATA[java.lang.X\n\tat y]]></log4j:Throwable></log4j:Event>";
        let out = p.push_line(l, Stream::Stdout);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].level, LogLevel::Error);
        assert_eq!(out[0].text, "boom\njava.lang.X\n\tat y");
    }

    #[test]
    fn multiline_cdata_throwable_across_lines() {
        let mut p = Log4jXmlParser::new();
        let mut out = vec![];
        for l in [
            "<log4j:Event logger=\"a\" level=\"WARN\">",
            "<log4j:Message><![CDATA[bad]]></log4j:Message>",
            "<log4j:Throwable><![CDATA[java.lang.X",
            "\tat y]]></log4j:Throwable>",
            "</log4j:Event>",
            "",
        ] {
            out.extend(p.push_line(l, Stream::Stdout));
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].level, LogLevel::Warn);
        assert_eq!(out[0].text, "bad\njava.lang.X\n\tat y");
    }

    #[test]
    fn plain_lines() {
        let mut p = Log4jXmlParser::new();
        assert_eq!(
            p.push_line("[12:00:00] [main/WARN]: x", Stream::Stdout)[0].level,
            LogLevel::Warn
        );
        assert_eq!(
            p.push_line("hello", Stream::Stdout)[0].level,
            LogLevel::Unknown
        );
        assert_eq!(p.push_line("oops", Stream::Stderr)[0].level, LogLevel::Warn);
    }

    #[test]
    fn blank_lines_dropped_and_unknown_level() {
        let mut p = Log4jXmlParser::new();
        assert!(p.push_line("", Stream::Stdout).is_empty());
        assert!(p.push_line("   ", Stream::Stdout).is_empty());
        let out = p.push_line(
            "<log4j:Event level=\"WEIRD\"><log4j:Message><![CDATA[x]]></log4j:Message></log4j:Event>",
            Stream::Stdout,
        );
        assert_eq!(out[0].level, LogLevel::Unknown);
    }

    #[test]
    fn decode_entities_helper() {
        assert_eq!(
            decode_entities("&lt;a&gt; &amp; &quot;x&quot; &apos;"),
            "<a> & \"x\" '"
        );
        assert_eq!(decode_entities("&amp;lt;"), "&lt;");
    }

    #[test]
    fn cdata_is_literal_and_odd_level_unknown() {
        let mut p = Log4jXmlParser::new();
        let out = p.push_line(
            "<log4j:Event logger=\"a&amp;b\" level=\"&quot;INFO&quot;\"><log4j:Message><![CDATA[a &lt; b]]></log4j:Message></log4j:Event>",
            Stream::Stdout,
        );
        assert_eq!(out[0].text, "a &lt; b");
        assert_eq!(out[0].level, LogLevel::Unknown);
    }

    #[test]
    fn finish_flushes_partial_event() {
        let mut p = Log4jXmlParser::new();
        assert!(
            p.push_line("<log4j:Event level=\"INFO\">", Stream::Stderr)
                .is_empty()
        );
        assert!(p.push_line("  <log4j:Message>", Stream::Stderr).is_empty());
        let out = p.finish(Stream::Stderr);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "<log4j:Event level=\"INFO\">");
        assert!(out.iter().all(|l| l.level == LogLevel::Warn));
        assert!(p.finish(Stream::Stderr).is_empty());
    }

    #[test]
    fn safety_valve_flushes_unterminated_event() {
        let mut p = Log4jXmlParser::new();
        let mut out = p.push_line("<log4j:Event level=\"INFO\">", Stream::Stdout);
        for _ in 0..10_100 {
            out.extend(p.push_line("junk", Stream::Stdout));
        }
        assert!(!out.is_empty());
        assert!(out.iter().all(|l| l.level == LogLevel::Unknown));
        // parser keeps working afterwards
        let o = p.push_line("[a/INFO] ok", Stream::Stdout);
        assert_eq!(o[0].level, LogLevel::Info);
    }
}
