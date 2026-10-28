use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

pub const TAIL_CAP: u64 = 256 * 1024;
const SUMMARY_CHARS: usize = 80;
const TEXT_CHARS: usize = 400;
const RECENT: usize = 10;
const PENDING_CAP: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub at: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Activity {
    pub recent: VecDeque<ToolCall>,
    pub pending: HashSet<String>,
    pub last_text: Option<String>,
    pub waiting_on_user: bool,
    pub last_at: Option<i64>,
}

impl Activity {
    pub fn current(&self) -> Option<&ToolCall> {
        self.recent.back()
    }

    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct Tail {
    offset: u64,
    partial: Vec<u8>,
    skipping_long_line: bool,
    pending_message: Option<String>,
    pub activity: Activity,
}

impl Tail {
    /// Reads what was appended since the last call, at most the last [`TAIL_CAP`] bytes.
    /// `Ok(true)` when the activity changed.
    pub fn read(&mut self, path: &Path) -> io::Result<bool> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        if len < self.offset {
            *self = Tail::default();
        }
        if len == self.offset {
            return Ok(false);
        }
        let mut start = self.offset;
        let jumped = len - start > TAIL_CAP;
        if jumped {
            self.partial.clear();
            self.skipping_long_line = false;
            // One byte early, so a seek that lands on a line start keeps that line.
            start = len - TAIL_CAP - 1;
        }
        file.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::with_capacity((len - start) as usize);
        file.take(len - start).read_to_end(&mut buf)?;
        self.offset = start + buf.len() as u64;
        let chunk = if jumped {
            match buf.iter().position(|&b| b == b'\n') {
                Some(i) => &buf[i + 1..],
                None => &[][..],
            }
        } else {
            &buf[..]
        };
        Ok(self.ingest(chunk))
    }

    fn ingest(&mut self, chunk: &[u8]) -> bool {
        let mut changed = false;
        let mut rest = chunk;
        while let Some(i) = rest.iter().position(|&b| b == b'\n') {
            let (line, tail) = rest.split_at(i);
            rest = &tail[1..];
            if std::mem::take(&mut self.skipping_long_line) {
                continue;
            }
            if self.partial.is_empty() {
                changed |= self.apply(line);
            } else {
                let mut whole = std::mem::take(&mut self.partial);
                whole.extend_from_slice(line);
                changed |= self.apply(&whole);
            }
        }
        if !self.skipping_long_line {
            self.partial.extend_from_slice(rest);
            if self.partial.len() as u64 > TAIL_CAP {
                self.partial = Vec::new();
                self.skipping_long_line = true;
            }
        }
        changed
    }

    fn apply(&mut self, line: &[u8]) -> bool {
        if line.starts_with(br#"{"type":"api-request"#) {
            return false;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return false;
        };
        let at = value["timestamp"].as_str().and_then(parse_timestamp);
        match value["type"].as_str() {
            Some("assistant") => self.assistant(&value["message"], at),
            Some("user") => self.user(&value["message"]),
            _ => return false,
        }
        if at.is_some() {
            self.activity.last_at = at;
        }
        true
    }

    fn assistant(&mut self, message: &Value, at: Option<i64>) {
        // The model only runs again once every result of its last message is in, so a new
        // message settles the calls whose results were too long to read.
        let id = message["id"].as_str();
        if id != self.pending_message.as_deref() {
            self.activity.pending.clear();
            self.pending_message = id.map(String::from);
        }
        for block in message["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("tool_use") => {
                    let id = block["id"].as_str().unwrap_or_default().to_string();
                    if self.activity.pending.len() < PENDING_CAP {
                        self.activity.pending.insert(id.clone());
                    }
                    self.activity.recent.push_back(ToolCall {
                        id,
                        name: block["name"].as_str().unwrap_or("?").to_string(),
                        summary: summarize_input(&block["input"]),
                        at,
                    });
                    if self.activity.recent.len() > RECENT {
                        self.activity.recent.pop_front();
                    }
                }
                Some("text") => {
                    let text = one_line(block["text"].as_str().unwrap_or_default(), TEXT_CHARS);
                    if !text.is_empty() {
                        self.activity.last_text = Some(text);
                    }
                }
                _ => {}
            }
        }
        self.activity.waiting_on_user = message["stop_reason"] == "end_turn";
    }

    fn user(&mut self, message: &Value) {
        match &message["content"] {
            Value::String(_) => self.activity.waiting_on_user = false,
            Value::Array(blocks) => {
                for block in blocks {
                    if block["type"] == "tool_result" {
                        if let Some(id) = block["tool_use_id"].as_str() {
                            self.activity.pending.remove(id);
                        }
                    } else {
                        self.activity.waiting_on_user = false;
                    }
                }
            }
            _ => {}
        }
    }
}

fn summarize_input(input: &Value) -> String {
    const KEYS: &[&str] = &[
        "command",
        "description",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "skill",
        "prompt",
    ];
    let text = KEYS
        .iter()
        .find_map(|k| input[*k].as_str())
        .or_else(|| input.as_object()?.values().find_map(Value::as_str))
        .unwrap_or_default();
    one_line(text, SUMMARY_CHARS)
}

pub fn one_line(text: &str, max: usize) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.len() > max * 4 {
            break;
        }
    }
    if out.chars().count() > max {
        out = out.chars().take(max - 1).collect();
        out.push('…');
    }
    out
}

/// Seconds since the epoch for an RFC 3339 UTC time like `2026-10-28T10:50:15.350Z`.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    Some(days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn tool_use(id: &str, name: &str, input: Value, ts: &str) -> String {
        json!({"type":"assistant","timestamp":ts,"message":{"stop_reason":"tool_use",
            "content":[{"type":"tool_use","id":id,"name":name,"input":input}]}})
        .to_string()
    }

    fn tool_result(id: &str, ts: &str) -> String {
        json!({"type":"user","timestamp":ts,"message":{"role":"user",
            "content":[{"type":"tool_result","tool_use_id":id,"content":"ok"}]}})
        .to_string()
    }

    fn text(t: &str, stop: &str, ts: &str) -> String {
        json!({"type":"assistant","timestamp":ts,"message":{"stop_reason":stop,
            "content":[{"type":"text","text":t}]}})
        .to_string()
    }

    fn lines(ls: &[String]) -> Vec<u8> {
        let mut out = String::new();
        for l in ls {
            out.push_str(l);
            out.push('\n');
        }
        out.into_bytes()
    }

    #[test]
    fn pending_tool_is_current_and_unresolved() {
        let mut t = Tail::default();
        t.ingest(&lines(&[tool_use(
            "t1",
            "Bash",
            json!({"command":"cargo nextest run -p sched","description":"run tests"}),
            "2026-10-28T10:50:16.027Z",
        )]));
        let cur = t.activity.current().unwrap();
        assert_eq!(cur.name, "Bash");
        assert_eq!(cur.summary, "cargo nextest run -p sched");
        assert!(t.activity.is_pending());
        assert_eq!(t.activity.last_at, Some(1_793_184_616));
    }

    #[test]
    fn resolved_tool_is_not_pending() {
        let mut t = Tail::default();
        t.ingest(&lines(&[
            tool_use(
                "t1",
                "Read",
                json!({"file_path":"/repo/src/lib.rs"}),
                "2026-10-28T10:50:16Z",
            ),
            tool_result("t1", "2026-10-28T10:50:17Z"),
        ]));
        assert!(!t.activity.is_pending());
        assert_eq!(t.activity.current().unwrap().summary, "/repo/src/lib.rs");
    }

    #[test]
    fn end_turn_waits_on_user_until_the_next_prompt() {
        let mut t = Tail::default();
        t.ingest(&lines(&[text(
            "**Done.** All\n  merged.",
            "end_turn",
            "2026-10-28T10:50:22Z",
        )]));
        assert!(t.activity.waiting_on_user);
        assert_eq!(
            t.activity.last_text.as_deref(),
            Some("**Done.** All merged.")
        );
        let prompt = json!({"type":"user","timestamp":"2026-10-28T10:51:00Z",
            "message":{"role":"user","content":"next"}})
        .to_string();
        t.ingest(&lines(&[prompt]));
        assert!(!t.activity.waiting_on_user);
    }

    #[test]
    fn partial_line_waits_for_its_end() {
        let mut t = Tail::default();
        let line = tool_use(
            "t9",
            "Grep",
            json!({"pattern":"known"}),
            "2026-10-28T10:50:16Z",
        );
        let (a, b) = line.as_bytes().split_at(20);
        assert!(!t.ingest(a));
        assert!(t.activity.current().is_none());
        let mut rest = b.to_vec();
        rest.push(b'\n');
        assert!(t.ingest(&rest));
        assert_eq!(t.activity.current().unwrap().name, "Grep");
    }

    #[test]
    fn api_request_lines_are_skipped() {
        let mut t = Tail::default();
        let req = r#"{"type":"api-request","message":{"content":[{"type":"tool_use","id":"x","name":"Bad","input":{}}]}}"#;
        t.ingest(&lines(&[req.to_string()]));
        assert!(t.activity.current().is_none());
    }

    #[test]
    fn summary_is_one_line_and_capped() {
        let mut t = Tail::default();
        let long = format!("echo {}\n  done", "x".repeat(200));
        t.ingest(&lines(&[tool_use(
            "t1",
            "Bash",
            json!({"command": long}),
            "2026-10-28T10:50:16Z",
        )]));
        let s = &t.activity.current().unwrap().summary;
        assert_eq!(s.chars().count(), SUMMARY_CHARS);
        assert!(!s.contains('\n'));
        assert!(s.ends_with('…'));
    }

    #[test]
    fn recent_keeps_the_last_ten() {
        let mut t = Tail::default();
        let ls: Vec<String> = (0..15)
            .map(|i| {
                tool_use(
                    &format!("t{i}"),
                    "Read",
                    json!({"file_path": format!("f{i}")}),
                    "2026-10-28T10:50:16Z",
                )
            })
            .collect();
        t.ingest(&lines(&ls));
        assert_eq!(t.activity.recent.len(), RECENT);
        assert_eq!(t.activity.recent.front().unwrap().summary, "f5");
    }

    #[test]
    fn read_resumes_from_offset_and_drops_first_partial_after_seek() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let filler = format!("{{\"type\":\"system\",\"pad\":\"{}\"}}\n", "p".repeat(1000));
        let mut f = File::create(&path).unwrap();
        for _ in 0..(TAIL_CAP as usize / filler.len() + 10) {
            f.write_all(filler.as_bytes()).unwrap();
        }
        f.write_all(&lines(&[tool_use(
            "a",
            "Edit",
            json!({"file_path":"x.rs"}),
            "2026-10-28T10:50:16Z",
        )]))
        .unwrap();
        f.flush().unwrap();

        let mut t = Tail::default();
        assert!(t.read(&path).unwrap());
        assert_eq!(t.activity.current().unwrap().name, "Edit");
        assert!(t.partial.is_empty());
        assert!(!t.read(&path).unwrap());

        f.write_all(&lines(&[tool_result("a", "2026-10-28T10:50:18Z")]))
            .unwrap();
        f.flush().unwrap();
        assert!(t.read(&path).unwrap());
        assert!(!t.activity.is_pending());
        assert_eq!(t.offset, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn truncated_file_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(
            &path,
            lines(&[tool_use("a", "Edit", json!({}), "2026-10-28T10:50:16Z")]),
        )
        .unwrap();
        let mut t = Tail::default();
        t.read(&path).unwrap();
        std::fs::write(
            &path,
            lines(&[tool_use("b", "R", json!({}), "2026-10-28T10:50:16Z")]),
        )
        .unwrap();
        t.read(&path).unwrap();
        assert_eq!(t.activity.current().unwrap().name, "R");
        assert_eq!(t.activity.recent.len(), 1);
    }

    #[test]
    fn timestamps_parse_to_epoch_seconds() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp("2000-03-01T00:00:01.5Z"), Some(951_868_801));
        assert_eq!(parse_timestamp("nope"), None);
    }
}
