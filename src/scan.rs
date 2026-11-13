//! One pass over transcript files, feeding each wanted record to the detectors.

use std::borrow::Cow;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{FrameDecoder, StreamingDecoder};
use serde_json::Value;

use crate::report::{Opts, Section};
use crate::time::parse_ts;

/// Which agent wrote a transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

/// One transcript file.
#[derive(Debug, Clone, Copy)]
pub struct Session {
    pub agent: Agent,
    pub subagent: bool,
}

impl Session {
    #[must_use]
    pub fn scope(self) -> &'static str {
        if self.subagent { "subagent" } else { "main" }
    }
}

/// A parsed line and whether its time falls in the window.
pub struct Record<'a> {
    pub value: &'a Value,
    pub in_window: bool,
}

impl Record<'_> {
    /// The record's `type` field, or "" when it has none.
    #[must_use]
    pub fn kind(&self) -> &str {
        self.value.get("type").and_then(Value::as_str).unwrap_or("")
    }
}

/// A waste detector. It sees only lines it wants, parsed once per line.
pub trait Detector {
    fn agents(&self) -> &'static [Agent];
    /// A cheap test on the raw line; only lines that pass are parsed.
    ///
    /// shortcut: the tests look for compact JSON such as `"type":"attachment"`, as
    /// both agents write it. A writer that adds spaces would make every line fail
    /// them; parsing each line first would be the upgrade, at several times the cost.
    fn wants(&self, s: &Session, line: &str) -> bool;
    fn begin(&mut self, _s: &Session) {}
    fn record(&mut self, s: &Session, r: &Record<'_>);
    fn end(&mut self, _s: &Session) {}
    fn section(&self, opts: &Opts) -> Section;
}

/// What the scan saw under one root.
#[derive(Debug, Clone)]
pub struct RootStats {
    pub agent: Agent,
    pub root: PathBuf,
    pub files: u64,
    pub unreadable: u64,
    pub bad_lines: u64,
    /// `*.jsonl.zst` transcripts in the window that do not start with the zstd magic
    /// number, so nothing in them was read.
    pub compressed: u64,
}

/// Claude's request blobs repeat message content already in the transcript.
const CLAUDE_SKIP_PREFIX: &str = "{\"type\":\"api-request-blob\"";

/// Scans every transcript under each root that a chosen detector reads and that
/// was modified at or after `since`.
pub fn scan(
    roots: &[(Agent, PathBuf)],
    since: DateTime<Utc>,
    dets: &mut [Box<dyn Detector>],
) -> Vec<RootStats> {
    let cutoff = u64::try_from(since.timestamp())
        .ok()
        .and_then(|s| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(s)))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut out = Vec::new();
    for (agent, root) in roots {
        let active: Vec<usize> = (0..dets.len())
            .filter(|&i| dets[i].agents().contains(agent))
            .collect();
        if active.is_empty() {
            continue;
        }
        let mut st = RootStats {
            agent: *agent,
            root: root.clone(),
            files: 0,
            unreadable: 0,
            bad_lines: 0,
            compressed: 0,
        };
        for path in list_files(root, cutoff, &mut st) {
            let reader = match open(&path) {
                Ok(r) => r,
                Err(Open::NotZstd) => {
                    st.compressed += 1;
                    continue;
                }
                Err(Open::Io) => {
                    st.files += 1;
                    st.unreadable += 1;
                    continue;
                }
            };
            st.files += 1;
            if scan_file(reader, &path, *agent, since, dets, &active, &mut st).is_err() {
                st.unreadable += 1;
            }
        }
        out.push(st);
    }
    out
}

fn list_files(root: &Path, cutoff: SystemTime, st: &mut RootStats) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            st.unreadable += 1;
            continue;
        };
        for entry in entries {
            let Ok((entry, ft)) = entry.and_then(|e| e.file_type().map(|ft| (e, ft))) else {
                st.unreadable += 1;
                continue;
            };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else {
                let compressed = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".jsonl.zst"));
                if !compressed && path.extension().is_none_or(|x| x != "jsonl") {
                    continue;
                }
                match entry.metadata().and_then(|m| m.modified()) {
                    Ok(m) if m >= cutoff => out.push(path),
                    Ok(_) => {}
                    Err(_) => st.unreadable += 1,
                }
            }
        }
    }
    out.sort();
    out
}

/// The longest line read, in bytes. A decoded `*.jsonl.zst` line is not bounded by
/// the file's size: a small frame can decode to gigabytes with no newline.
const MAX_LINE: u64 = 64 << 20;

/// shortcut: a line is held whole up to `MAX_LINE` bytes, far above the few MB of
/// the longest lines seen in real transcripts; a longer one is skipped unread and
/// counted as a skipped line. Parsing records as a stream would remove the cap.
struct Lines {
    reader: Box<dyn BufRead>,
    buf: Vec<u8>,
    /// Lines skipped for being longer than `MAX_LINE`.
    over_long: u64,
}

/// Why a transcript could not be opened.
enum Open {
    /// A `*.jsonl.zst` file that does not start with the zstd magic number.
    NotZstd,
    /// A file that cannot be read, or a zstd frame header that cannot be decoded.
    Io,
}

/// Opens a transcript, decoding a `*.jsonl.zst` one as it is read.
fn open(path: &Path) -> Result<Box<dyn BufRead>, Open> {
    let file = BufReader::new(File::open(path).map_err(|_| Open::Io)?);
    if path.extension().is_none_or(|x| x != "zst") {
        return Ok(Box::new(file));
    }
    let decoder = StreamingDecoder::new(file).map_err(|e| match e {
        FrameDecoderError::ReadFrameHeaderError(
            ReadFrameHeaderError::BadMagicNumber(_) | ReadFrameHeaderError::MagicNumberReadError(_),
        ) => Open::NotZstd,
        _ => Open::Io,
    })?;
    Ok(Box::new(BufReader::new(OneFrame(decoder))))
}

/// Decodes a compressed transcript and fails, rather than stopping quietly, on a bad
/// checksum or anything after the frame.
///
/// shortcut: reads one zstd frame, as Codex writes one per rollout (all 203 on one
/// machine had one, 13 November 2026); a file with more frames counts as unreadable
/// after its first. Decoding each frame in turn would be the upgrade.
struct OneFrame(StreamingDecoder<BufReader<File>, FrameDecoder>);

impl Read for OneFrame {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.0.read(buf)?;
        if n == 0 && !buf.is_empty() {
            let frame = &self.0.decoder;
            if let Some(sum) = frame.get_checksum_from_data()
                && frame.get_calculated_checksum() != Some(sum)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "zstd checksum mismatch",
                ));
            }
            if !self.0.get_mut().fill_buf()?.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "data after the zstd frame",
                ));
            }
        }
        Ok(n)
    }
}

impl Lines {
    fn new(reader: Box<dyn BufRead>) -> Self {
        Lines {
            reader,
            buf: Vec::new(),
            over_long: 0,
        }
    }

    /// The next line no longer than `MAX_LINE`, skipping and counting longer ones.
    fn next_line(&mut self) -> io::Result<Option<Cow<'_, str>>> {
        loop {
            self.buf.clear();
            let n = (&mut self.reader)
                .take(MAX_LINE)
                .read_until(b'\n', &mut self.buf)?;
            if n == 0 {
                return Ok(None);
            }
            if n as u64 == MAX_LINE && self.buf.last() != Some(&b'\n') {
                self.reader.skip_until(b'\n')?;
                self.over_long += 1;
                self.buf = Vec::new();
                continue;
            }
            return Ok(Some(String::from_utf8_lossy(&self.buf)));
        }
    }
}

fn scan_file(
    reader: Box<dyn BufRead>,
    path: &Path,
    agent: Agent,
    since: DateTime<Utc>,
    dets: &mut [Box<dyn Detector>],
    active: &[usize],
    st: &mut RootStats,
) -> io::Result<()> {
    let mut lines = Lines::new(reader);
    let Some(first) = lines.next_line()? else {
        st.bad_lines += lines.over_long;
        return Ok(());
    };
    let first = first.trim_start_matches('\u{feff}').trim_end().to_owned();
    let session = Session {
        agent,
        subagent: is_subagent(agent, path, &first),
    };
    for &i in active {
        dets[i].begin(&session);
    }
    let mut feed = Feed {
        session,
        since,
        last_time: None,
        dets,
        active,
        st,
    };
    feed.line(&first);
    let result = loop {
        match lines.next_line() {
            Ok(Some(line)) => feed.line(line.trim_end()),
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    feed.st.bad_lines += lines.over_long;
    for &i in active {
        feed.dets[i].end(&session);
    }
    result
}

// shortcut: a Claude subagent is told apart by its `subagents` folder and a Codex one
// by `parent_thread_id` in its first record, as both agents lay them out in November
// 2026. A layout change makes subagents count as main sessions; a record field naming
// the parent would be the upgrade.
fn is_subagent(agent: Agent, path: &Path, first: &str) -> bool {
    match agent {
        Agent::Claude => path.components().any(|c| c.as_os_str() == "subagents"),
        Agent::Codex => serde_json::from_str::<Value>(first).is_ok_and(|v| {
            v.pointer("/payload/parent_thread_id")
                .and_then(Value::as_str)
                .is_some_and(|p| !p.is_empty())
        }),
    }
}

struct Feed<'a> {
    session: Session,
    since: DateTime<Utc>,
    last_time: Option<DateTime<Utc>>,
    dets: &'a mut [Box<dyn Detector>],
    active: &'a [usize],
    st: &'a mut RootStats,
}

impl Feed<'_> {
    fn line(&mut self, line: &str) {
        if line.is_empty()
            || (self.session.agent == Agent::Claude && line.starts_with(CLAUDE_SKIP_PREFIX))
        {
            return;
        }
        if !line.ends_with('}') {
            self.st.bad_lines += 1;
            return;
        }
        let wanting: Vec<usize> = self
            .active
            .iter()
            .copied()
            .filter(|&i| self.dets[i].wants(&self.session, line))
            .collect();
        if wanting.is_empty() {
            if let Some(t) = raw_timestamp(line) {
                self.last_time = Some(t);
            }
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            self.st.bad_lines += 1;
            return;
        };
        if let Some(t) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts)
        {
            self.last_time = Some(t);
        }
        let rec = Record {
            value: &value,
            in_window: self.last_time.is_some_and(|t| t >= self.since),
        };
        for i in wanting {
            self.dets[i].record(&self.session, &rec);
        }
    }
}

/// A record's top-level `timestamp`, read without parsing the line: Codex writes
/// it as the first key, Claude near the end after the message.
///
/// shortcut: in a Claude line, a nested object after the top-level key that has its
/// own `timestamp` is read instead. Only records without a timestamp of their own
/// take this time; parsing every line would be exact but slower.
fn raw_timestamp(line: &str) -> Option<DateTime<Utc>> {
    const KEY: &str = "\"timestamp\":\"";
    let at = if line.strip_prefix('{').is_some_and(|r| r.starts_with(KEY)) {
        1
    } else {
        line.rfind(KEY)?
    };
    let rest = &line[at + KEY.len()..];
    parse_ts(&rest[..rest.find('"')?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_timestamps_for_both_agents() {
        let codex = r#"{"timestamp":"2026-11-08T01:00:00Z","type":"x","payload":{"timestamp":"2020-01-01T00:00:00Z"}}"#;
        assert_eq!(raw_timestamp(codex), parse_ts("2026-11-08T01:00:00Z"));
        let claude = r#"{"type":"user","message":{"content":"a \"timestamp\":\"x\""},"timestamp":"2026-11-08T02:00:00Z"}"#;
        assert_eq!(raw_timestamp(claude), parse_ts("2026-11-08T02:00:00Z"));
        assert_eq!(raw_timestamp(r#"{"type":"summary"}"#), None);
    }

    #[test]
    fn a_line_over_the_cap_is_skipped_and_counted() {
        let long = io::repeat(b'a').take(MAX_LINE + 10);
        let rest: &[u8] = b"\n{\"a\":1}\n";
        let mut lines = Lines::new(Box::new(BufReader::new(long.chain(rest))));
        let next = lines.next_line().expect("read").map(Cow::into_owned);
        assert_eq!(next.as_deref(), Some("{\"a\":1}\n"));
        assert_eq!(lines.over_long, 1);
        assert!(lines.next_line().expect("read").is_none());
    }
}
