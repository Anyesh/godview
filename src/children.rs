use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::transcript::{Activity, Tail};

pub const FRESH: Duration = Duration::from_secs(90);
pub const PENDING_WINDOW: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Running,
    Done,
    Stale,
}

#[derive(Debug, Default)]
pub struct Subagent {
    pub id: String,
    pub agent_type: String,
    pub description: String,
    pub parent: Option<String>,
    pub finished: bool,
    pub started: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    meta_modified: Option<SystemTime>,
    tail: Option<Tail>,
}

impl Subagent {
    pub fn activity(&self) -> Option<&Activity> {
        self.tail.as_ref().map(|t| &t.activity)
    }

    /// A `finished` that never got written (a killed session) must not read as running
    /// forever, so an unfinished child counts only while it writes or waits on a call.
    pub fn liveness(&self, now: SystemTime) -> Liveness {
        if self.finished {
            return Liveness::Done;
        }
        let Some(age) = age(self.modified, now) else {
            return Liveness::Stale;
        };
        let pending = self.activity().is_some_and(Activity::is_pending);
        if age <= FRESH || (pending && age <= PENDING_WINDOW) {
            Liveness::Running
        } else {
            Liveness::Stale
        }
    }
}

fn age(at: Option<SystemTime>, now: SystemTime) -> Option<Duration> {
    at.map(|t| now.duration_since(t).unwrap_or_default())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    pub index: usize,
    pub total: usize,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowAgent {
    pub label: String,
    pub state: String,
    pub last_tool: Option<String>,
    pub last_summary: Option<String>,
}

#[derive(Debug, Default)]
pub struct Workflow {
    pub id: String,
    pub name: String,
    pub status: String,
    pub phase: Option<Phase>,
    pub agents: Vec<WorkflowAgent>,
    pub started: Option<SystemTime>,
    pub modified: Option<SystemTime>,
}

impl Workflow {
    pub fn running_agents(&self) -> usize {
        self.agents
            .iter()
            .filter(|a| matches!(a.state.as_str(), "start" | "progress"))
            .count()
    }

    pub fn liveness(&self, now: SystemTime) -> Liveness {
        if matches!(
            self.status.as_str(),
            "completed" | "killed" | "failed" | "error" | "cancelled"
        ) {
            return Liveness::Done;
        }
        match age(self.modified, now) {
            Some(age) if age <= PENDING_WINDOW => Liveness::Running,
            _ => Liveness::Stale,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChildKey {
    Subagent(String),
    Workflow(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildTree {
    pub key: ChildKey,
    pub liveness: Liveness,
    pub children: Vec<ChildTree>,
}

/// The subagents and workflows of one session, read from `<sid>/subagents` and `<sid>/workflows`.
#[derive(Debug)]
pub struct Children {
    dir: PathBuf,
    subagents: BTreeMap<String, Subagent>,
    workflows: BTreeMap<String, Workflow>,
}

impl Children {
    pub fn new(session_dir: PathBuf) -> Self {
        Children {
            dir: session_dir,
            subagents: BTreeMap::new(),
            workflows: BTreeMap::new(),
        }
    }

    pub fn subagent(&self, id: &str) -> Option<&Subagent> {
        self.subagents.get(id)
    }

    pub fn workflow(&self, id: &str) -> Option<&Workflow> {
        self.workflows.get(id)
    }

    pub fn subagents_dir(&self) -> PathBuf {
        self.dir.join("subagents")
    }

    pub fn workflows_dir(&self) -> PathBuf {
        self.dir.join("workflows")
    }

    /// Reads every file under the session dir again. `true` when it holds any child.
    pub fn rescan(&mut self, now: SystemTime) -> bool {
        let subagent_ids: Vec<String> = file_names(&self.subagents_dir())
            .filter_map(|n| subagent_id(&n).map(String::from))
            .collect();
        self.subagents
            .retain(|id, _| subagent_ids.iter().any(|s| s == id));
        for id in &subagent_ids {
            self.load_subagent(id, now);
        }
        let workflow_ids: Vec<String> = file_names(&self.workflows_dir())
            .filter_map(|n| workflow_id(&n).map(String::from))
            .collect();
        self.workflows
            .retain(|id, _| workflow_ids.iter().any(|s| s == id));
        for id in &workflow_ids {
            self.load_workflow(id);
        }
        !self.subagents.is_empty() || !self.workflows.is_empty()
    }

    /// Takes in one changed file. `true` when it belonged to this session's children.
    pub fn on_path(&mut self, path: &Path, now: SystemTime) -> bool {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
        else {
            return false;
        };
        if parent == self.subagents_dir() {
            if let Some(id) = subagent_id(name) {
                self.load_subagent(id, now);
                return true;
            }
        } else if parent == self.workflows_dir() {
            if let Some(id) = workflow_id(name) {
                self.load_workflow(id);
                return true;
            }
        }
        false
    }

    fn load_subagent(&mut self, id: &str, now: SystemTime) {
        let base = self.subagents_dir();
        let meta_path = base.join(format!("agent-{id}.meta.json"));
        let log_path = base.join(format!("agent-{id}.jsonl"));
        let meta_md = fs::metadata(&meta_path).ok();
        let log_md = fs::metadata(&log_path).ok();
        if meta_md.is_none() && log_md.is_none() {
            self.subagents.remove(id);
            return;
        }
        let entry = self
            .subagents
            .entry(id.to_string())
            .or_insert_with(|| Subagent {
                id: id.to_string(),
                ..Subagent::default()
            });
        let meta_modified = meta_md.as_ref().and_then(|m| m.modified().ok());
        if meta_modified.is_some() && meta_modified != entry.meta_modified {
            if let Some(meta) = read_json(&meta_path) {
                entry.agent_type = meta["agentType"].as_str().unwrap_or("agent").to_string();
                entry.description = meta["description"].as_str().unwrap_or_default().to_string();
                entry.parent = meta["parentAgentId"].as_str().map(String::from);
                entry.finished = meta["finished"] == true;
                entry.meta_modified = meta_modified;
            }
        }
        let times = || meta_md.iter().chain(log_md.iter());
        entry.started = times().filter_map(|m| m.created().ok()).min();
        entry.modified = times().filter_map(|m| m.modified().ok()).max();
        let recent = age(entry.modified, now).is_some_and(|a| a <= PENDING_WINDOW);
        if entry.finished || log_md.is_none() || !recent {
            entry.tail = None;
        } else if let Err(e) = entry.tail.get_or_insert_default().read(&log_path) {
            crate::log::warn(&format!("read {}: {e}", log_path.display()));
        }
    }

    fn load_workflow(&mut self, id: &str) {
        let path = self.workflows_dir().join(format!("{id}.json"));
        let Ok(md) = fs::metadata(&path) else {
            self.workflows.remove(id);
            return;
        };
        let modified = md.modified().ok();
        let entry = self
            .workflows
            .entry(id.to_string())
            .or_insert_with(|| Workflow {
                id: id.to_string(),
                ..Workflow::default()
            });
        if modified.is_some() && entry.modified == modified {
            return;
        }
        // A file caught mid-write keeps what was read before it.
        let Some(v) = read_json(&path) else {
            return;
        };
        entry.name = v["workflowName"].as_str().unwrap_or(id).to_string();
        entry.status = v["status"].as_str().unwrap_or_default().to_string();
        let progress = v["workflowProgress"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        entry.agents = progress
            .iter()
            .filter(|p| p["type"] == "workflow_agent")
            .map(|p| WorkflowAgent {
                label: p["label"].as_str().unwrap_or_default().to_string(),
                state: p["state"].as_str().unwrap_or_default().to_string(),
                last_tool: p["lastToolName"].as_str().map(String::from),
                last_summary: p["lastToolSummary"].as_str().map(String::from),
            })
            .collect();
        entry.phase = current_phase(&v, progress);
        entry.started = md.created().ok();
        entry.modified = modified;
    }

    /// The tree in start order. Without `all`, only running nodes and their ancestors.
    pub fn tree(&self, now: SystemTime, all: bool) -> Vec<ChildTree> {
        let mut kids: HashMap<Option<&str>, Vec<&Subagent>> = HashMap::new();
        for s in self.subagents.values() {
            let parent = s
                .parent
                .as_deref()
                .filter(|p| *p != s.id && self.subagents.contains_key(*p));
            kids.entry(parent).or_default().push(s);
        }
        for list in kids.values_mut() {
            list.sort_by(|a, b| (a.started, &a.id).cmp(&(b.started, &b.id)));
        }
        let mut roots: Vec<(Option<SystemTime>, ChildTree)> = kids
            .get(&None)
            .into_iter()
            .flatten()
            .filter_map(|s| Some((s.started, self.subagent_tree(s, &kids, now, all, 0)?)))
            .collect();
        for w in self.workflows.values() {
            let liveness = w.liveness(now);
            if all || liveness == Liveness::Running {
                let key = ChildKey::Workflow(w.id.clone());
                roots.push((
                    w.started,
                    ChildTree {
                        key,
                        liveness,
                        children: Vec::new(),
                    },
                ));
            }
        }
        roots.sort_by(|a, b| a.0.cmp(&b.0));
        roots.into_iter().map(|(_, t)| t).collect()
    }

    fn subagent_tree(
        &self,
        s: &Subagent,
        kids: &HashMap<Option<&str>, Vec<&Subagent>>,
        now: SystemTime,
        all: bool,
        depth: usize,
    ) -> Option<ChildTree> {
        let children = if depth < MAX_DEPTH {
            kids.get(&Some(s.id.as_str()))
                .into_iter()
                .flatten()
                .filter_map(|c| self.subagent_tree(c, kids, now, all, depth + 1))
                .collect()
        } else {
            Vec::new()
        };
        let liveness = s.liveness(now);
        (all || liveness == Liveness::Running || !children.is_empty()).then(|| ChildTree {
            key: ChildKey::Subagent(s.id.clone()),
            liveness,
            children,
        })
    }

    pub fn running(&self, now: SystemTime) -> usize {
        let subagents = self
            .subagents
            .values()
            .filter(|s| s.liveness(now) == Liveness::Running)
            .count();
        let workflows = self
            .workflows
            .values()
            .filter(|w| w.liveness(now) == Liveness::Running)
            .count();
        subagents + workflows
    }
}

const MAX_DEPTH: usize = 8;

fn file_names(dir: &Path) -> impl Iterator<Item = String> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
}

fn subagent_id(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("agent-")?;
    rest.strip_suffix(".meta.json")
        .or_else(|| rest.strip_suffix(".jsonl"))
}

fn workflow_id(name: &str) -> Option<&str> {
    name.strip_suffix(".json").filter(|n| n.starts_with("wf_"))
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// The phase a running agent is in, else the last phase the workflow entered.
fn current_phase(v: &Value, progress: &[Value]) -> Option<Phase> {
    let titles: Vec<(usize, &str)> = progress
        .iter()
        .filter(|p| p["type"] == "workflow_phase")
        .filter_map(|p| Some((p["index"].as_u64()? as usize, p["title"].as_str()?)))
        .collect();
    let running = progress
        .iter()
        .filter(|p| {
            p["type"] == "workflow_agent"
                && matches!(p["state"].as_str(), Some("start" | "progress"))
        })
        .filter_map(|p| p["phaseIndex"].as_u64())
        .max()
        .map(|i| i as usize);
    let index = running.or_else(|| titles.iter().map(|t| t.0).max())?;
    let declared = v["phases"].as_array().map(Vec::len).unwrap_or_default();
    let title = titles
        .iter()
        .find(|t| t.0 == index)
        .map(|t| t.1)
        .or_else(|| v["phases"][index.checked_sub(1)?]["title"].as_str())
        .unwrap_or_default();
    Some(Phase {
        index,
        total: declared.max(index),
        title: title.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs::File;

    fn set_mtime(path: &Path, t: SystemTime) {
        File::options()
            .append(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        session: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let session = dir.path().join("sid");
            fs::create_dir_all(session.join("subagents")).unwrap();
            fs::create_dir_all(session.join("workflows")).unwrap();
            Fixture { _dir: dir, session }
        }

        fn subagent(&self, id: &str, meta: Value, transcript: &str, age: Duration) {
            let base = self.session.join("subagents");
            let meta_path = base.join(format!("agent-{id}.meta.json"));
            let log_path = base.join(format!("agent-{id}.jsonl"));
            fs::write(&meta_path, meta.to_string()).unwrap();
            fs::write(&log_path, transcript).unwrap();
            let t = SystemTime::now() - age;
            set_mtime(&meta_path, t);
            set_mtime(&log_path, t);
        }
    }

    fn pending_call() -> String {
        json!({"type":"assistant","timestamp":"2026-10-28T10:50:16Z","message":{"id":"m1",
            "stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Grep",
            "input":{"pattern":"stale refs"}}]}})
        .to_string()
            + "\n"
    }

    fn resolved_call() -> String {
        pending_call()
            + &json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}})
                .to_string()
            + "\n"
    }

    fn meta(desc: &str, depth: u32, parent: Option<&str>, finished: bool) -> Value {
        let mut m = json!({"agentType":"Explore","description":desc,"spawnDepth":depth});
        if let Some(p) = parent {
            m["parentAgentId"] = json!(p);
        }
        if finished {
            m["finished"] = json!(true);
        }
        m
    }

    #[test]
    fn depth_two_nests_under_its_parent() {
        let f = Fixture::new();
        f.subagent(
            "a1",
            meta("Apply audit", 1, None, false),
            &resolved_call(),
            Duration::from_secs(5),
        );
        f.subagent(
            "a2",
            meta("Find refs", 2, Some("a1"), false),
            &pending_call(),
            Duration::from_secs(1),
        );
        let mut c = Children::new(f.session.clone());
        let now = SystemTime::now();
        assert!(c.rescan(now));
        let tree = c.tree(now, false);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].key, ChildKey::Subagent("a1".into()));
        assert_eq!(tree[0].children[0].key, ChildKey::Subagent("a2".into()));
        assert_eq!(tree[0].children[0].liveness, Liveness::Running);
        let a2 = c.subagent("a2").unwrap();
        assert_eq!(a2.activity().unwrap().current().unwrap().name, "Grep");
        assert_eq!(c.running(now), 2);
    }

    #[test]
    fn absent_finished_is_running_only_while_fresh() {
        let f = Fixture::new();
        f.subagent(
            "new",
            meta("fresh", 1, None, false),
            &resolved_call(),
            Duration::from_secs(10),
        );
        f.subagent(
            "old",
            meta("quiet", 1, None, false),
            &resolved_call(),
            Duration::from_secs(300),
        );
        f.subagent(
            "wait",
            meta("long tool", 1, None, false),
            &pending_call(),
            Duration::from_secs(300),
        );
        f.subagent(
            "dead",
            meta("killed", 1, None, false),
            &pending_call(),
            Duration::from_secs(3600),
        );
        let mut c = Children::new(f.session.clone());
        let now = SystemTime::now();
        c.rescan(now);
        let live = |id: &str| c.subagent(id).unwrap().liveness(now);
        assert_eq!(live("new"), Liveness::Running);
        assert_eq!(live("old"), Liveness::Stale);
        assert_eq!(live("wait"), Liveness::Running);
        assert_eq!(live("dead"), Liveness::Stale);
    }

    #[test]
    fn finished_children_hide_until_all() {
        let f = Fixture::new();
        f.subagent(
            "a1",
            meta("done one", 1, None, true),
            &resolved_call(),
            Duration::from_secs(1),
        );
        f.subagent(
            "a2",
            meta("old one", 1, None, false),
            &resolved_call(),
            Duration::from_secs(900),
        );
        let mut c = Children::new(f.session.clone());
        let now = SystemTime::now();
        c.rescan(now);
        assert_eq!(c.subagent("a1").unwrap().liveness(now), Liveness::Done);
        assert!(c.tree(now, false).is_empty());
        let all = c.tree(now, true);
        assert_eq!(all.len(), 2);
        assert_eq!(c.running(now), 0);
    }

    #[test]
    fn a_running_child_keeps_its_finished_parent_visible() {
        let f = Fixture::new();
        f.subagent(
            "a1",
            meta("parent", 1, None, true),
            &resolved_call(),
            Duration::from_secs(1),
        );
        f.subagent(
            "a2",
            meta("child", 2, Some("a1"), false),
            &pending_call(),
            Duration::from_secs(1),
        );
        let mut c = Children::new(f.session.clone());
        let now = SystemTime::now();
        c.rescan(now);
        let tree = c.tree(now, false);
        assert_eq!(tree[0].liveness, Liveness::Done);
        assert_eq!(tree[0].children.len(), 1);
    }

    #[test]
    fn workflow_reports_its_phase_and_agents() {
        let f = Fixture::new();
        let wf = json!({"runId":"wf_1","workflowName":"review-changes","status":"running",
            "phases":[{"title":"Review"},{"title":"Verify"}],
            "workflowProgress":[
                {"type":"workflow_phase","index":1,"title":"Review"},
                {"type":"workflow_agent","label":"r1","phaseIndex":1,"state":"done"},
                {"type":"workflow_phase","index":2,"title":"Verify"},
                {"type":"workflow_agent","label":"v1","phaseIndex":2,"state":"progress","lastToolName":"Read","lastToolSummary":"src/a.rs"},
                {"type":"workflow_agent","label":"v2","phaseIndex":2,"state":"start"}]});
        fs::write(f.session.join("workflows/wf_1.json"), wf.to_string()).unwrap();
        let mut c = Children::new(f.session.clone());
        let now = SystemTime::now();
        c.rescan(now);
        let w = c.workflow("wf_1").unwrap();
        assert_eq!(w.name, "review-changes");
        assert_eq!(
            w.phase,
            Some(Phase {
                index: 2,
                total: 2,
                title: "Verify".into()
            })
        );
        assert_eq!(w.agents.len(), 3);
        assert_eq!(w.running_agents(), 2);
        assert_eq!(w.liveness(now), Liveness::Running);
        assert_eq!(c.tree(now, false)[0].key, ChildKey::Workflow("wf_1".into()));
    }

    #[test]
    fn completed_or_abandoned_workflows_are_not_running() {
        let f = Fixture::new();
        let path = f.session.join("workflows/wf_2.json");
        fs::write(
            &path,
            json!({"status":"completed","workflowName":"x"}).to_string(),
        )
        .unwrap();
        let mut c = Children::new(f.session.clone());
        let now = SystemTime::now();
        c.rescan(now);
        assert_eq!(c.workflow("wf_2").unwrap().liveness(now), Liveness::Done);
        fs::write(
            &path,
            json!({"status":"running","workflowName":"x"}).to_string(),
        )
        .unwrap();
        set_mtime(&path, now - Duration::from_secs(3600));
        assert!(c.on_path(&path, now));
        assert_eq!(c.workflow("wf_2").unwrap().liveness(now), Liveness::Stale);
    }

    #[test]
    fn on_path_picks_up_a_new_child_and_ignores_others() {
        let f = Fixture::new();
        let mut c = Children::new(f.session.clone());
        let now = SystemTime::now();
        c.rescan(now);
        f.subagent(
            "a9",
            meta("late", 1, None, false),
            &pending_call(),
            Duration::ZERO,
        );
        let log = f.session.join("subagents/agent-a9.jsonl");
        assert!(c.on_path(&log, now));
        assert_eq!(c.subagent("a9").unwrap().description, "late");
        assert!(!c.on_path(Path::new("/elsewhere/agent-a9.jsonl"), now));
        assert!(!c.on_path(&f.session.join("subagents/notes.txt"), now));
    }
}
