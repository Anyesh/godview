use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};

const ANSWER: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    Blocked,
    Working,
    Done,
    Idle,
    Unknown,
}

impl Status {
    fn parse(s: &str) -> Self {
        match s {
            "blocked" => Status::Blocked,
            "working" => Status::Working,
            "done" => Status::Done,
            "idle" => Status::Idle,
            _ => Status::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextUse {
    pub percent: u8,
    pub tokens: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    /// The herdr agent name, else the tab label.
    pub label: String,
    pub named: bool,
    pub kind: String,
    pub status: Status,
    /// herdr's `state_change_seq`: grows each time the status changes.
    pub status_seq: u64,
    pub cwd: String,
    pub session_id: Option<String>,
    pub title: String,
    pub context: Option<ContextUse>,
    pub cache: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub agents: Vec<Agent>,
}

impl Snapshot {
    pub fn agent_panes(&self) -> Vec<String> {
        let mut panes: Vec<String> = self.agents.iter().map(|a| a.pane_id.clone()).collect();
        panes.sort();
        panes
    }
}

#[derive(Debug)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "herdr {}: {}", self.code, self.message)
    }
}

impl std::error::Error for HerdrError {}

#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Client {
            socket: socket.into(),
        }
    }

    pub fn from_env() -> Option<Self> {
        std::env::var_os("HERDR_SOCKET_PATH").map(Client::new)
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn connect(&self) -> Result<UnixStream> {
        let stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect {}", self.socket.display()))?;
        stream.set_read_timeout(Some(ANSWER))?;
        stream.set_write_timeout(Some(ANSWER))?;
        Ok(stream)
    }

    /// One request per connection: herdr answers one line and closes.
    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut stream = self.connect()?;
        let body = json!({"id": "godview", "method": method, "params": params});
        writeln!(stream, "{body}")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        if line.is_empty() {
            bail!("{method}: herdr closed the connection without an answer");
        }
        take_result(serde_json::from_str(&line)?)
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        let result = self.request("session.snapshot", json!({}))?;
        Ok(parse_snapshot(&result["snapshot"]))
    }

    /// The pid leading the pane's foreground process group.
    pub fn foreground_pgid(&self, pane_id: &str) -> Result<Option<u32>> {
        let result = self.request("pane.process_info", json!({"pane_id": pane_id}))?;
        Ok(result["process_info"]["foreground_process_group_id"]
            .as_u64()
            .map(|p| p as u32))
    }

    /// The executables in the pane's foreground process group, by `argv[0]` basename,
    /// since a process name can be rewritten.
    pub fn foreground_programs(&self, pane_id: &str) -> Result<Vec<String>> {
        let result = self.request("pane.process_info", json!({"pane_id": pane_id}))?;
        Ok(result["process_info"]["foreground_processes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["argv"][0].as_str())
            .filter_map(|a| Path::new(a).file_name()?.to_str().map(String::from))
            .collect())
    }

    pub fn read_pane(&self, pane_id: &str, lines: usize) -> Result<String> {
        let result = self.request(
            "pane.read",
            json!({"pane_id": pane_id, "source": "recent", "lines": lines}),
        )?;
        Ok(result["read"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    pub fn focus_pane(&self, pane_id: &str) -> Result<()> {
        self.request("pane.focus", json!({"pane_id": pane_id}))
            .map(drop)
    }

    pub fn pane_ids(&self) -> Result<Vec<String>> {
        let result = self.request("pane.list", json!({}))?;
        Ok(result["panes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["pane_id"].as_str().map(String::from))
            .collect())
    }

    /// Opens a subscription and waits for herdr to start it. The returned reader yields its
    /// event lines.
    pub fn subscribe(
        &self,
        subscriptions: &[Value],
    ) -> Result<(UnixStream, BufReader<UnixStream>)> {
        let mut stream = self.connect()?;
        let body = json!({
            "id": "godview:subscribe",
            "method": "events.subscribe",
            "params": {"subscriptions": subscriptions},
        });
        writeln!(stream, "{body}")?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut ack = String::new();
        reader.read_line(&mut ack)?;
        if ack.is_empty() {
            bail!("subscribe: herdr closed the connection");
        }
        take_result(serde_json::from_str(&ack)?)?;
        reader.get_ref().set_read_timeout(None)?;
        Ok((stream, reader))
    }
}

fn take_result(mut answer: Value) -> Result<Value> {
    if let Some(code) = answer["error"]["code"].as_str() {
        return Err(HerdrError {
            code: code.to_string(),
            message: answer["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        }
        .into());
    }
    match answer.get_mut("result") {
        Some(result) => Ok(result.take()),
        None => Err(anyhow!("herdr answered without a result")),
    }
}

pub fn error_code(e: &anyhow::Error) -> Option<&str> {
    e.downcast_ref::<HerdrError>().map(|h| h.code.as_str())
}

pub fn parse_snapshot(snapshot: &Value) -> Snapshot {
    let tab_labels: HashMap<&str, &str> = snapshot["tabs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| Some((t["tab_id"].as_str()?, t["label"].as_str()?)))
        .collect();
    let agents = snapshot["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| parse_agent(a, &tab_labels))
        .collect();
    Snapshot { agents }
}

fn parse_agent(a: &Value, tab_labels: &HashMap<&str, &str>) -> Option<Agent> {
    let str_of = |k: &str| a[k].as_str().unwrap_or_default().to_string();
    let pane_id = a["pane_id"].as_str()?.to_string();
    let tab_id = str_of("tab_id");
    let name = a["name"].as_str().filter(|n| !n.is_empty());
    let label = name
        .or_else(|| tab_labels.get(tab_id.as_str()).copied())
        .unwrap_or(&pane_id)
        .to_string();
    let cwd = a["foreground_cwd"]
        .as_str()
        .or_else(|| a["cwd"].as_str())
        .unwrap_or_default()
        .to_string();
    Some(Agent {
        tab_id,
        workspace_id: str_of("workspace_id"),
        label,
        named: name.is_some(),
        kind: str_of("agent"),
        status: Status::parse(a["agent_status"].as_str().unwrap_or_default()),
        status_seq: a["state_change_seq"].as_u64().unwrap_or_default(),
        cwd,
        session_id: a["agent_session"]["value"].as_str().map(String::from),
        title: str_of("terminal_title_stripped"),
        context: a["tokens"]["context"].as_str().and_then(parse_context),
        cache: a["tokens"]["cache_high"].as_str().map(String::from),
        pane_id,
    })
}

/// `"⛁ 33% (328k)"` into 33 and `328k`.
pub fn parse_context(s: &str) -> Option<ContextUse> {
    let pct_end = s.find('%')?;
    let start = s[..pct_end]
        .rfind(|c: char| !c.is_ascii_digit())
        .map_or(0, |i| i + s[i..].chars().next().map_or(1, char::len_utf8));
    let percent = s[start..pct_end].parse().ok()?;
    let tokens = s[pct_end..]
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(t, _)| t.trim().to_string())
        .unwrap_or_default();
    Some(ContextUse { percent, tokens })
}

#[cfg(test)]
pub mod fake {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;

    use serde_json::{Value, json};

    /// A herdr stand-in: answers `session.snapshot` with the current snapshot, keeps
    /// subscriptions open, and records every request.
    pub struct FakeHerdr {
        _dir: tempfile::TempDir,
        pub path: PathBuf,
        pub snapshot: Arc<Mutex<Value>>,
        pub subscriptions: Arc<Mutex<Vec<(Value, UnixStream)>>>,
        pub requests: mpsc::Receiver<Value>,
    }

    impl FakeHerdr {
        pub fn start(snapshot: Value) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("h.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let snapshot = Arc::new(Mutex::new(snapshot));
            let subscriptions = Arc::new(Mutex::new(Vec::new()));
            let (tx, rx) = mpsc::channel();
            let accept = listener;
            let (snap, subs) = (snapshot.clone(), subscriptions.clone());
            thread::spawn(move || {
                for stream in accept.incoming() {
                    let Ok(stream) = stream else { return };
                    let (snap, subs, tx) = (snap.clone(), subs.clone(), tx.clone());
                    thread::spawn(move || serve(stream, &snap, &subs, &tx));
                }
            });
            FakeHerdr {
                _dir: dir,
                path,
                snapshot,
                subscriptions,
                requests: rx,
            }
        }

        pub fn set_snapshot(&self, v: Value) {
            *self.snapshot.lock().unwrap() = v;
        }

        /// Writes one line to every open subscription.
        pub fn push(&self, line: Value) {
            for (_, s) in self.subscriptions.lock().unwrap().iter_mut() {
                let _ = writeln!(s, "{line}");
            }
        }

        /// Closes every open subscription, as a herdr restart would.
        pub fn drop_subscriptions(&self) {
            for (_, s) in self.subscriptions.lock().unwrap().drain(..) {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    fn serve(
        stream: UnixStream,
        snap: &Mutex<Value>,
        subs: &Mutex<Vec<(Value, UnixStream)>>,
        tx: &mpsc::Sender<Value>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let req: Value = serde_json::from_str(&line).unwrap();
        let _ = tx.send(req.clone());
        let mut out = stream;
        match req["method"].as_str() {
            Some("session.snapshot") => {
                let snapshot = snap.lock().unwrap().clone();
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"id": req["id"], "result": {"snapshot": snapshot}})
                );
            }
            Some("events.subscribe") => {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"id": req["id"], "result": {"type": "subscription_started"}})
                );
                subs.lock()
                    .unwrap()
                    .push((req["params"]["subscriptions"].clone(), out));
            }
            _ => {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"id": req["id"], "result": {"type": "ok"}})
                );
            }
        }
    }

    pub fn agent(pane: &str, name: Option<&str>, status: &str, sid: &str) -> Value {
        let mut a = json!({
            "pane_id": pane, "tab_id": format!("{pane}:t"), "workspace_id": "w1",
            "agent": "claude", "agent_status": status, "state_change_seq": 7,
            "cwd": "/repo", "foreground_cwd": "/repo",
            "agent_session": {"value": sid},
            "terminal_title_stripped": "Doing things",
            "tokens": {"context": "⛁ 33% (328k)", "cache_high": "cache hit 99.7%"},
        });
        if let Some(n) = name {
            a["name"] = json!(n);
        }
        a
    }

    pub fn snapshot(agents: Vec<Value>) -> Value {
        let tabs: Vec<Value> = agents
            .iter()
            .map(|a| json!({"tab_id": a["tab_id"], "label": "tablabel"}))
            .collect();
        json!({"agents": agents, "tabs": tabs})
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    #[test]
    fn context_parses_percent_and_tokens() {
        assert_eq!(
            parse_context("⛁ 33% (328k)"),
            Some(ContextUse {
                percent: 33,
                tokens: "328k".into()
            })
        );
        assert_eq!(parse_context("5%").unwrap().percent, 5);
        assert_eq!(parse_context("no context"), None);
    }

    #[test]
    fn snapshot_reads_agents_and_falls_back_to_the_tab_label() {
        let fake = FakeHerdr::start(snapshot(vec![
            agent("w1:p1", Some("worker-1"), "working", "s1"),
            agent("w1:p2", None, "done", "s2"),
        ]));
        let snap = Client::new(&fake.path).snapshot().unwrap();
        let a = &snap.agents[0];
        assert_eq!(a.label, "worker-1");
        assert!(a.named);
        assert_eq!(a.status, Status::Working);
        assert_eq!(a.session_id.as_deref(), Some("s1"));
        assert_eq!(a.context.as_ref().unwrap().percent, 33);
        assert_eq!(a.cache.as_deref(), Some("cache hit 99.7%"));
        assert_eq!(a.title, "Doing things");
        assert_eq!(snap.agents[1].label, "tablabel");
        assert!(!snap.agents[1].named);
        assert_eq!(snap.agent_panes(), vec!["w1:p1", "w1:p2"]);
    }

    #[test]
    fn null_name_reads_as_unnamed() {
        let mut a = agent("w1:p3", None, "idle", "s3");
        a["name"] = Value::Null;
        let snap = parse_snapshot(&snapshot(vec![a]));
        assert_eq!(snap.agents[0].label, "tablabel");
        assert_eq!(snap.agents[0].status, Status::Idle);
    }

    #[test]
    fn error_envelope_carries_its_code() {
        let answer = json!({"id":"x","error":{"code":"pane_not_found","message":"gone"}});
        let e = take_result(answer).unwrap_err();
        assert_eq!(error_code(&e), Some("pane_not_found"));
    }
}
