use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::children::{ChildKey, ChildTree, Children, Liveness};
use crate::crew::Repos;
use crate::feed::{Feed, FeedMsg};
use crate::herdr::{Client, Snapshot, Status};
use crate::live::{self, Key, StreamItem, Tracker};
use crate::model::{self, AgentEntry, ChildEntry, ChildInfo, Model, Row};
use crate::profile::Profiles;
use crate::transcript::{self, Activity, RATE_BUCKETS, Tail, ToolCall};
use crate::ui::View;
use crate::watch::{self, Change, Target, Watches};

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
    tracker: Tracker,
    stream: Vec<StreamItem>,
    running_children: usize,
    in_motion: bool,
    motion: bool,
    frame: u64,
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
            tracker: Tracker::new(epoch(SystemTime::now())),
            stream: Vec::new(),
            running_children: 0,
            in_motion: false,
            motion: true,
            frame: 0,
        }
    }

    /// Frames run only for motion on screen. A flash or highlight needs no frames, only the
    /// draw at [`App::fade_until`] that clears it.
    pub fn animating(&self) -> bool {
        self.motion && self.in_motion
    }

    pub fn fade_until(&self) -> Option<Instant> {
        self.tracker.fade_until()
    }

    pub fn advance_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    pub fn handle(&mut self, msg: Msg) -> Effect {
        let effect = self.dispatch(msg);
        if effect == Effect::Redraw && self.peek_is_stale() {
            self.refresh_peek();
        }
        effect
    }

    fn dispatch(&mut self, msg: Msg) -> Effect {
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
        let working = self
            .snapshot
            .agents
            .iter()
            .any(|a| a.status == Status::Working);
        tick_for(working, self.model.peek, self.running_children)
    }

    /// Runs on every frame, so it only borrows what rebuilds computed.
    pub fn view(&self) -> View<'_> {
        View {
            model: &self.model,
            now: epoch(SystemTime::now()),
            peek: self
                .peek
                .as_ref()
                .map(|(pane, text)| (pane.as_str(), text.as_str())),
            status: self.status.as_deref(),
            running_children: self.running_children,
            frame: self.frame,
            instant: Instant::now(),
            motion: self.motion,
            stream: &self.stream,
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
            self.profiles.get(&a.pane_id, a.session_id.as_deref(), || {
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
        match self.watches.classify(path) {
            Target::Transcript(sid) => self.sessions.get_mut(&sid).is_some_and(|s| {
                read_tail(s);
                true
            }),
            Target::SessionDir(sid) => {
                let Some(session) = self.sessions.get_mut(&sid) else {
                    return false;
                };
                if self.watches.ensure(&sid, &session.transcript) {
                    session.children.rescan(now);
                }
                true
            }
            Target::Child(sid) => self
                .sessions
                .get_mut(&sid)
                .is_some_and(|s| s.children.on_path(path, now)),
            Target::Other => {
                // A session file nobody shows yet: a pane just started or /clear-ed into it,
                // and herdr raises no event for that.
                if change.created && path.extension().is_some_and(|e| e == "jsonl") {
                    self.feed.refresh();
                }
                false
            }
        }
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
                profile: self
                    .profiles
                    .get(&a.pane_id, a.session_id.as_deref(), || None)
                    .map(String::from),
                group_key,
                group_label,
                crew,
                active_at: activity.as_ref().and_then(|x| x.last_at),
                active: model::is_active(a.status, activity.as_ref(), epoch(now)),
                rate: activity
                    .as_ref()
                    .map_or([0; RATE_BUCKETS], |x| x.rate(epoch(now))),
                tool_changed_at: None,
                wired: false,
                activity,
                children,
            });
        }
        let mut current = Vec::new();
        let mut items = Vec::new();
        for e in &entries {
            collect_calls(e, &mut current, &mut items);
        }
        self.stream = self
            .tracker
            .update(&current, items, epoch(now), Instant::now());
        for e in &mut entries {
            let pane = e.agent.pane_id.clone();
            e.tool_changed_at = self.tracker.changed_at(&Key::Agent(pane.clone()));
            mark_changes(&self.tracker, &pane, &mut e.children);
        }
        self.running_children = self
            .sessions
            .values()
            .map(|s| s.children.running(now))
            .sum();
        self.model.update(entries);
        self.in_motion = in_motion(&self.model);
    }

    fn on_key(&mut self, key: KeyEvent) -> Effect {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Effect::Quit,
            KeyCode::Char('c') if ctrl => return Effect::Quit,
            KeyCode::Char('j') | KeyCode::Down => self.model.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.model.move_selection(-1),
            KeyCode::PageDown => self.model.move_selection(10),
            KeyCode::PageUp => self.model.move_selection(-10),
            KeyCode::Char('g') | KeyCode::Home => self.model.move_selection(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.model.move_selection(isize::MAX / 2),
            KeyCode::Char(' ') => self.model.toggle_fold(),
            KeyCode::Char('w') => {
                let on = !self.model.working_only();
                self.model.set_working_only(on);
            }
            KeyCode::Char('m') => self.motion = !self.motion,
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
        // Folds and filters change what is on screen without a rebuild.
        self.in_motion = in_motion(&self.model);
        Effect::Redraw
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

    fn selected_pane(&self) -> Option<&str> {
        self.model
            .selection()
            .map(|(entry, _)| entry.agent.pane_id.as_str())
    }

    /// The selection moved to another pane without a key: its pane closed, a filter or fold
    /// hid it, or rows reordered.
    fn peek_is_stale(&self) -> bool {
        self.model.peek && self.peek.as_ref().map(|(pane, _)| pane.as_str()) != self.selected_pane()
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
                tool_changed_at: None,
                children: child_entries(children, t.children),
            })
        })
        .collect()
}

fn collect_calls<'a>(
    e: &'a AgentEntry,
    current: &mut Vec<(Key, Option<&'a ToolCall>)>,
    items: &mut Vec<StreamItem>,
) {
    let who = &e.agent.label;
    let activity = e.activity.as_ref();
    current.push((
        Key::Agent(e.agent.pane_id.clone()),
        activity.and_then(Activity::current),
    ));
    // Newest first, so calls within one second keep their order in the stable sort.
    let recent = activity.into_iter().flat_map(|a| a.recent.iter().rev());
    items.extend(recent.filter_map(|c| live::call_item(who, None, c)));
    collect_child_calls(e, &e.children, current, items);
}

fn collect_child_calls<'a>(
    e: &AgentEntry,
    children: &'a [ChildEntry],
    current: &mut Vec<(Key, Option<&'a ToolCall>)>,
    items: &mut Vec<StreamItem>,
) {
    let who = &e.agent.label;
    for c in children {
        let running = c.liveness == Liveness::Running;
        match (&c.info, &c.key) {
            (
                ChildInfo::Subagent {
                    agent_type,
                    activity,
                    ..
                },
                _,
            ) => {
                let key = Key::Child(e.agent.pane_id.clone(), c.key.clone());
                current.push((key, activity.as_ref().and_then(Activity::current)));
                if running {
                    let recent = activity.iter().flat_map(|a| a.recent.iter().rev());
                    items.extend(recent.filter_map(|t| live::call_item(who, Some(agent_type), t)));
                }
            }
            (ChildInfo::Workflow { agents, .. }, ChildKey::Workflow(id)) if running => {
                let in_flight = agents
                    .iter()
                    .filter(|a| matches!(a.state.as_str(), "start" | "progress"));
                items.extend(in_flight.filter_map(|a| live::workflow_item(who, id, a)));
            }
            _ => {}
        }
        collect_child_calls(e, &c.children, current, items);
    }
}

fn mark_changes(tracker: &Tracker, pane: &str, children: &mut [ChildEntry]) {
    for c in children {
        c.tool_changed_at = tracker.changed_at(&Key::Child(pane.to_string(), c.key.clone()));
        mark_changes(tracker, pane, &mut c.children);
    }
}

/// A visible row moves: an active agent's spinner, or a wire with work in flight. Agents
/// folded, filtered or under "more done" draw nothing that moves.
fn in_motion(model: &Model) -> bool {
    model.rows().iter().any(|(_, row)| match row {
        Row::Agent { entry } => entry.active,
        Row::Child { entry, ordinal, .. } => ordinal.is_some() && entry.wired,
        Row::Group { .. } | Row::More { .. } => false,
    })
}

/// Seconds-resolution ages show for working agents and peeked output, and a running child
/// goes stale with the clock alone; a blocked agent can sit on a prompt for hours and needs
/// no 1 Hz redraw.
fn tick_for(working: bool, peek: bool, running_children: usize) -> Duration {
    if working || peek || running_children > 0 {
        BUSY_TICK
    } else {
        IDLE_TICK
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::children::Liveness;
    use crate::model::tests::{agent, entry, subagent};

    fn with(status: Status, active: bool, child: Option<Liveness>) -> AgentEntry {
        let mut e = entry("incr", agent("p1", "w1", status, 1));
        e.active = active;
        e.children = child
            .map(|l| subagent("c1", l, vec![]))
            .into_iter()
            .collect();
        e
    }

    fn motion_of(entries: Vec<AgentEntry>, change: impl FnOnce(&mut Model)) -> bool {
        let mut m = Model::default();
        m.update(entries);
        change(&mut m);
        in_motion(&m)
    }

    #[test]
    fn blocked_or_stale_agents_do_not_animate() {
        let alone = |e| motion_of(vec![e], |_| {});
        assert!(!alone(with(
            Status::Blocked,
            false,
            Some(Liveness::Running)
        )));
        assert!(!alone(with(Status::Working, false, None)));
        assert!(!alone(with(Status::Working, false, Some(Liveness::Stale))));
        assert!(alone(with(Status::Working, true, None)));
        assert!(alone(with(Status::Idle, false, Some(Liveness::Running))));
    }

    #[test]
    fn motion_off_screen_does_not_animate() {
        let mut done: Vec<AgentEntry> = (0..4)
            .map(|i| {
                entry(
                    "incr",
                    agent(&format!("p{i}"), &format!("w{i}"), Status::Done, 10 - i),
                )
            })
            .collect();
        done[3].children = vec![subagent("c1", Liveness::Running, vec![])];
        assert!(!motion_of(done, |_| {}));
        let wire = with(Status::Idle, false, Some(Liveness::Running));
        assert!(!motion_of(vec![wire.clone()], Model::toggle_fold));
        assert!(!motion_of(vec![wire], |m| m.set_working_only(true)));
    }

    #[test]
    fn running_children_keep_the_busy_tick() {
        assert_eq!(tick_for(false, false, 0), IDLE_TICK);
        assert_eq!(tick_for(false, false, 1), BUSY_TICK);
        assert_eq!(tick_for(true, false, 0), BUSY_TICK);
        assert_eq!(tick_for(false, true, 0), BUSY_TICK);
    }
}
