use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::children::{ChildKey, ChildTree, Children};
use crate::crew::Repos;
use crate::feed::{Feed, FeedMsg};
use crate::herdr::{Client, Snapshot, Status};
use crate::model::{AgentEntry, ChildEntry, ChildInfo, Model};
use crate::profile::Profiles;
use crate::transcript::{self, Tail};
use crate::ui::View;
use crate::watch::{self, Change, Watches};

const PEEK_LINES: usize = 200;
const BUSY_TICK: Duration = Duration::from_secs(1);
const IDLE_TICK: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum Msg {
    Feed(FeedMsg),
    Watch(Change),
    Key(KeyEvent),
    Resize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    Redraw,
    Quit,
}

/// One live Claude session: its main transcript and its children.
#[derive(Debug)]
struct Session {
    transcript: PathBuf,
    tail: Tail,
    children: Children,
}

pub struct App {
    client: Client,
    feed: Feed,
    projects: PathBuf,
    snapshot: Snapshot,
    status: Option<String>,
    sessions: HashMap<String, Session>,
    profiles: Profiles,
    repos: Repos,
    watches: Watches,
    model: Model,
    peek: Option<(String, String)>,
}

impl App {
    pub fn new(client: Client, feed: Feed, watches: Watches, projects: PathBuf) -> Self {
        App {
            client,
            feed,
            projects,
            snapshot: Snapshot::default(),
            status: Some("connecting to herdr".into()),
            sessions: HashMap::new(),
            profiles: Profiles::new("/proc"),
            repos: Repos::default(),
            watches,
            model: Model::default(),
            peek: None,
        }
    }

    pub fn handle(&mut self, msg: Msg) -> Effect {
        match msg {
            Msg::Feed(FeedMsg::Snapshot(snapshot)) => {
                self.status = None;
                self.on_snapshot(snapshot);
                Effect::Redraw
            }
            Msg::Feed(FeedMsg::Lost(reason)) => {
                crate::log::warn(&format!("herdr connection lost: {reason}"));
                self.status = Some("herdr connection lost, retrying".into());
                Effect::Redraw
            }
            Msg::Watch(change) => {
                if self.on_change(&change) {
                    self.rebuild();
                    Effect::Redraw
                } else {
                    Effect::None
                }
            }
            Msg::Key(key) => self.on_key(key),
            Msg::Resize => Effect::Redraw,
        }
    }

    /// Ages and the bounded liveness of children move with the clock alone.
    pub fn tick(&mut self) {
        self.rebuild();
        if self.model.peek {
            self.refresh_peek();
        }
    }

    pub fn tick_interval(&self) -> Duration {
        let busy = self
            .snapshot
            .agents
            .iter()
            .any(|a| matches!(a.status, Status::Working | Status::Blocked));
        if busy || self.model.peek {
            BUSY_TICK
        } else {
            IDLE_TICK
        }
    }

    pub fn view(&self) -> View<'_> {
        let now = SystemTime::now();
        View {
            model: &self.model,
            now: epoch(now),
            peek: self.peek.as_ref().map(|(_, text)| text.as_str()),
            status: self.status.as_deref(),
            running_children: self
                .sessions
                .values()
                .map(|s| s.children.running(now))
                .sum(),
        }
    }

    fn on_snapshot(&mut self, snapshot: Snapshot) {
        let now = SystemTime::now();
        let live: HashMap<String, &str> = snapshot
            .agents
            .iter()
            .filter(|a| a.kind == "claude")
            .filter_map(|a| Some((a.session_id.clone()?, a.cwd.as_str())))
            .collect();
        let gone: Vec<String> = self
            .sessions
            .keys()
            .filter(|sid| !live.contains_key(*sid))
            .cloned()
            .collect();
        for sid in gone {
            self.watches.untrack(&sid);
            self.sessions.remove(&sid);
        }
        for (sid, cwd) in &live {
            if self.sessions.contains_key(sid) {
                continue;
            }
            // A session that has not written its transcript yet is looked up again on the
            // next snapshot.
            let Some(path) = transcript::locate(&self.projects, sid, cwd) else {
                continue;
            };
            let mut session = Session {
                children: Children::new(watch::session_dir(&path)),
                transcript: path,
                tail: Tail::default(),
            };
            self.watches.ensure(sid, &session.transcript);
            session.children.rescan(now);
            read_tail(&mut session);
            self.sessions.insert(sid.clone(), session);
        }
        for a in &snapshot.agents {
            let client = &self.client;
            self.profiles.get(&a.pane_id, || {
                client.foreground_pgid(&a.pane_id).ok().flatten()
            });
        }
        let panes: HashSet<&str> = snapshot.agents.iter().map(|a| a.pane_id.as_str()).collect();
        self.profiles.retain(|p| panes.contains(p));
        self.snapshot = snapshot;
        self.rebuild();
    }

    /// `true` when the change touched something on screen.
    fn on_change(&mut self, change: &Change) -> bool {
        let path = &change.path;
        let now = SystemTime::now();
        if let Some(session) = self.sessions.values_mut().find(|s| &s.transcript == path) {
            read_tail(session);
            return true;
        }
        let parent = path.parent();
        let grandparent = parent.and_then(Path::parent);
        let owner = self.sessions.iter_mut().find_map(|(sid, s)| {
            let dir = watch::session_dir(&s.transcript);
            let hit = path == &dir || parent == Some(&dir) || grandparent == Some(&dir);
            hit.then_some((sid, s, dir))
        });
        if let Some((sid, session, dir)) = owner {
            let is_dir = path == &dir
                || (parent == Some(&dir)
                    && matches!(
                        path.file_name().and_then(|n| n.to_str()),
                        Some("subagents" | "workflows")
                    ));
            if is_dir {
                if self.watches.ensure(sid, &session.transcript) {
                    session.children.rescan(now);
                }
                return true;
            }
            return session.children.on_path(path, now);
        }
        // A session file nobody shows yet: a pane just started or /clear-ed into it, and
        // herdr raises no event for that.
        if change.created && path.extension().is_some_and(|e| e == "jsonl") {
            self.feed.refresh();
        }
        false
    }

    fn rebuild(&mut self) {
        let now = SystemTime::now();
        let all = self.model.all_children;
        let mut entries = Vec::with_capacity(self.snapshot.agents.len());
        for a in &self.snapshot.agents {
            let main = self.repos.main_repo(&a.cwd).map(Path::to_path_buf);
            let (group_key, group_label) = match &main {
                Some(m) => (
                    m.display().to_string(),
                    m.file_name().map_or(m.display().to_string(), |n| {
                        n.to_string_lossy().into_owned()
                    }),
                ),
                None => (a.cwd.clone(), home_relative(&a.cwd)),
            };
            let crew = match &main {
                Some(m) if a.named => self.repos.board(m).get(&a.label).cloned(),
                _ => None,
            };
            let session = a.session_id.as_ref().and_then(|s| self.sessions.get(s));
            let activity = session.map(|s| s.tail.activity.clone());
            let children = session.map_or_else(Vec::new, |s| {
                child_entries(&s.children, s.children.tree(now, all))
            });
            entries.push(AgentEntry {
                agent: a.clone(),
                profile: self.profiles.get(&a.pane_id, || None).map(String::from),
                group_key,
                group_label,
                crew,
                active_at: activity.as_ref().and_then(|x| x.last_at),
                activity,
                children,
            });
        }
        self.model.update(entries);
    }

    fn on_key(&mut self, key: KeyEvent) -> Effect {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Effect::Quit,
            KeyCode::Char('c') if ctrl => return Effect::Quit,
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Char('g') | KeyCode::Home => self.move_selection(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.move_selection(isize::MAX / 2),
            KeyCode::Char(' ') => self.model.toggle_fold(),
            KeyCode::Char('w') => {
                let on = !self.model.working_only();
                self.model.set_working_only(on);
            }
            KeyCode::Char('a') => {
                self.model.all_children = !self.model.all_children;
                self.rebuild();
            }
            KeyCode::Char('p') => {
                self.model.peek = !self.model.peek;
                if self.model.peek {
                    self.refresh_peek();
                }
            }
            KeyCode::Enter => self.focus_selected(),
            _ => return Effect::None,
        }
        Effect::Redraw
    }

    fn move_selection(&mut self, delta: isize) {
        self.model.move_selection(delta);
        if self.model.peek {
            self.refresh_peek();
        }
    }

    fn focus_selected(&mut self) {
        let Some((entry, _)) = self.model.selection() else {
            return;
        };
        if let Err(e) = self.client.focus_pane(&entry.agent.pane_id) {
            self.status = Some(format!("focus failed: {e:#}"));
        }
    }

    fn refresh_peek(&mut self) {
        let Some((entry, _)) = self.model.selection() else {
            self.peek = None;
            return;
        };
        let pane = entry.agent.pane_id.clone();
        let text = self
            .client
            .read_pane(&pane, PEEK_LINES)
            .unwrap_or_else(|e| format!("could not read the pane: {e:#}"));
        self.peek = Some((pane, text));
    }
}

fn read_tail(session: &mut Session) {
    if let Err(e) = session.tail.read(&session.transcript) {
        crate::log::warn(&format!("read {}: {e}", session.transcript.display()));
    }
}

fn child_entries(children: &Children, tree: Vec<ChildTree>) -> Vec<ChildEntry> {
    tree.into_iter()
        .filter_map(|t| {
            let (info, active_at) = match &t.key {
                ChildKey::Subagent(id) => {
                    let s = children.subagent(id)?;
                    let info = ChildInfo::Subagent {
                        agent_type: s.agent_type.clone(),
                        description: s.description.clone(),
                        activity: s.activity().cloned(),
                    };
                    (info, s.modified.map(epoch))
                }
                ChildKey::Workflow(id) => {
                    let w = children.workflow(id)?;
                    let info = ChildInfo::Workflow {
                        name: w.name.clone(),
                        status: w.status.clone(),
                        phase: w.phase.clone(),
                        agents: w.agents.clone(),
                        running: w.running_agents(),
                    };
                    (info, w.started.or(w.modified).map(epoch))
                }
            };
            Some(ChildEntry {
                key: t.key,
                liveness: t.liveness,
                info,
                active_at,
                children: child_entries(children, t.children),
            })
        })
        .collect()
}

fn epoch(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn home_relative(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}
