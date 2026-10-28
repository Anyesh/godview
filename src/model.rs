use std::collections::HashSet;
use std::time::Instant;

use crate::children::{ChildKey, Liveness, PENDING_WINDOW, Phase, WorkflowAgent};
use crate::crew::Item;
use crate::herdr::{Agent, Status};
use crate::transcript::{Activity, RATE_BUCKETS};

/// Done agents shown in a group before the rest fold into one row.
pub const DONE_SHOWN: usize = 3;
/// How long a working agent that waits on no call may go without a transcript write
/// before it stops reading as active.
const ACTIVE_FRESH: i64 = 60;

/// A working agent whose transcript moves, so a stalled one holds still on screen. Mirrors
/// the subagent liveness rule in `children.rs`.
pub fn is_active(status: Status, activity: Option<&Activity>, now: i64) -> bool {
    let Some(a) = activity.filter(|_| status == Status::Working) else {
        return false;
    };
    a.last_at.is_some_and(|at| {
        let age = now - at;
        age <= ACTIVE_FRESH || (a.is_pending() && age <= PENDING_WINDOW.as_secs() as i64)
    })
}

#[derive(Debug, Clone)]
pub enum ChildInfo {
    Subagent {
        agent_type: String,
        description: String,
        activity: Option<Activity>,
    },
    Workflow {
        name: String,
        status: String,
        phase: Option<Phase>,
        agents: Vec<WorkflowAgent>,
        running: usize,
    },
}

#[derive(Debug, Clone)]
pub struct ChildEntry {
    pub key: ChildKey,
    pub liveness: Liveness,
    pub info: ChildInfo,
    pub active_at: Option<i64>,
    pub tool_changed_at: Option<Instant>,
    pub children: Vec<ChildEntry>,
}

impl ChildEntry {
    fn running(&self) -> bool {
        self.liveness == Liveness::Running || self.children.iter().any(ChildEntry::running)
    }
}

#[derive(Debug, Clone)]
pub struct AgentEntry {
    pub agent: Agent,
    pub profile: Option<String>,
    pub group_key: String,
    pub group_label: String,
    pub crew: Option<Item>,
    pub activity: Option<Activity>,
    pub active_at: Option<i64>,
    pub active: bool,
    pub rate: [u16; RATE_BUCKETS],
    pub tool_changed_at: Option<Instant>,
    pub children: Vec<ChildEntry>,
}

impl AgentEntry {
    /// Work is in flight under this agent. A blocked agent's children wait on its prompt.
    pub fn wire_running(&self) -> bool {
        self.agent.status != Status::Blocked && self.children.iter().any(ChildEntry::running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeId {
    Group(String),
    Agent(String),
    Child(String, ChildKey),
    More(String),
}

#[derive(Debug)]
pub struct Group {
    pub key: String,
    pub label: String,
    pub agents: Vec<AgentEntry>,
}

#[derive(Debug)]
pub enum Row<'a> {
    Group {
        group: &'a Group,
        collapsed: bool,
    },
    Agent {
        entry: &'a AgentEntry,
    },
    Child {
        entry: &'a AgentEntry,
        child: &'a ChildEntry,
        /// Per ancestor level below the agent, whether a later sibling continues its line.
        guides: Vec<bool>,
        last: bool,
        /// Position on the agent's wire, `None` where the first column is blank.
        ordinal: Option<usize>,
        /// How many of the agent's visible rows the wire crosses.
        period: usize,
    },
    More {
        hidden: usize,
    },
}

#[derive(Debug, Default)]
pub struct Model {
    groups: Vec<Group>,
    selected: Option<NodeId>,
    last_index: usize,
    collapsed: HashSet<NodeId>,
    expanded_more: HashSet<String>,
    working_only: bool,
    pub all_children: bool,
    pub peek: bool,
}

pub fn status_rank(s: Status) -> u8 {
    match s {
        Status::Blocked => 0,
        Status::Working => 1,
        Status::Done => 2,
        Status::Idle => 3,
        Status::Unknown => 4,
    }
}

impl Model {
    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub fn update(&mut self, entries: Vec<AgentEntry>) {
        let mut groups: Vec<Group> = Vec::new();
        for e in entries {
            match groups.iter_mut().find(|g| g.key == e.group_key) {
                Some(g) => g.agents.push(e),
                None => groups.push(Group {
                    key: e.group_key.clone(),
                    label: e.group_label.clone(),
                    agents: vec![e],
                }),
            }
        }
        // herdr's status sequence orders by last status change, so streaming output never
        // moves a row.
        for g in &mut groups {
            g.agents.sort_by(|a, b| {
                (
                    status_rank(a.agent.status),
                    b.agent.status_seq,
                    &a.agent.label,
                )
                    .cmp(&(
                        status_rank(b.agent.status),
                        a.agent.status_seq,
                        &b.agent.label,
                    ))
            });
        }
        groups.sort_by(|a, b| {
            b.agents
                .len()
                .cmp(&a.agents.len())
                .then_with(|| a.label.cmp(&b.label))
        });
        self.groups = groups;
        self.settle_selection();
    }

    pub fn working_only(&self) -> bool {
        self.working_only
    }

    pub fn set_working_only(&mut self, on: bool) {
        self.working_only = on;
        self.settle_selection();
    }

    fn settle_selection(&mut self) {
        let index = self.selected_index();
        let rows = self.rows();
        let id = index.and_then(|i| rows.get(i)).map(|(id, _)| id.clone());
        drop(rows);
        if let Some(i) = index {
            self.last_index = i;
        }
        self.selected = id;
    }

    pub fn rows(&self) -> Vec<(NodeId, Row<'_>)> {
        let mut rows = Vec::new();
        for group in &self.groups {
            let visible: Vec<&AgentEntry> = group
                .agents
                .iter()
                .filter(|e| {
                    !self.working_only
                        || matches!(e.agent.status, Status::Working | Status::Blocked)
                })
                .collect();
            if visible.is_empty() {
                continue;
            }
            let group_id = NodeId::Group(group.key.clone());
            let collapsed = self.collapsed.contains(&group_id);
            rows.push((group_id, Row::Group { group, collapsed }));
            if collapsed {
                continue;
            }
            let expanded = self.expanded_more.contains(&group.key);
            let done_total = visible
                .iter()
                .filter(|e| e.agent.status == Status::Done)
                .count();
            let more = || {
                let hidden = if expanded { 0 } else { done_total - DONE_SHOWN };
                (NodeId::More(group.key.clone()), Row::More { hidden })
            };
            let mut done_seen = 0;
            for entry in visible {
                let done = entry.agent.status == Status::Done;
                if done {
                    done_seen += 1;
                    if done_seen > DONE_SHOWN && !expanded {
                        if done_seen == DONE_SHOWN + 1 {
                            rows.push(more());
                        }
                        continue;
                    }
                }
                let agent_id = NodeId::Agent(entry.agent.pane_id.clone());
                let folded = self.collapsed.contains(&agent_id);
                rows.push((agent_id, Row::Agent { entry }));
                if !folded {
                    let start = rows.len();
                    self.push_children(&mut rows, entry, &entry.children, &mut Vec::new());
                    number_wire(&mut rows[start..]);
                }
                if done && expanded && done_seen == done_total && done_total > DONE_SHOWN {
                    rows.push(more());
                }
            }
        }
        rows
    }

    fn push_children<'a>(
        &self,
        rows: &mut Vec<(NodeId, Row<'a>)>,
        entry: &'a AgentEntry,
        list: &'a [ChildEntry],
        guides: &mut Vec<bool>,
    ) {
        for (i, child) in list.iter().enumerate() {
            let last = i + 1 == list.len();
            let id = NodeId::Child(entry.agent.pane_id.clone(), child.key.clone());
            let folded = self.collapsed.contains(&id);
            rows.push((
                id,
                Row::Child {
                    entry,
                    child,
                    guides: guides.clone(),
                    last,
                    ordinal: None,
                    period: 0,
                },
            ));
            if !folded && !child.children.is_empty() {
                guides.push(!last);
                self.push_children(rows, entry, &child.children, guides);
                guides.pop();
            }
        }
    }

    /// The selected row's index, after following a selection whose node went away: a child
    /// hands it to its agent, anything else to the row at the same position.
    pub fn selected_index(&self) -> Option<usize> {
        self.selected_index_in(&self.rows())
    }

    /// [`Model::selected_index`] within rows the caller already built.
    pub fn selected_index_in(&self, rows: &[(NodeId, Row<'_>)]) -> Option<usize> {
        if rows.is_empty() {
            return None;
        }
        let find = |id: &NodeId| rows.iter().position(|(r, _)| r == id);
        let found = match &self.selected {
            None => Some(
                rows.iter()
                    .position(|(id, _)| matches!(id, NodeId::Agent(_)))
                    .unwrap_or(0),
            ),
            Some(id) => find(id).or_else(|| match id {
                NodeId::Child(pane, _) => find(&NodeId::Agent(pane.clone())),
                _ => None,
            }),
        };
        Some(found.unwrap_or(self.last_index.min(rows.len() - 1)))
    }

    pub fn move_selection(&mut self, delta: isize) {
        let Some(index) = self.selected_index() else {
            return;
        };
        let rows = self.rows();
        let next = index.saturating_add_signed(delta).min(rows.len() - 1);
        let id = rows[next].0.clone();
        drop(rows);
        self.selected = Some(id);
        self.last_index = next;
    }

    /// Folds or unfolds the selected group, agent, child or "more done" row.
    pub fn toggle_fold(&mut self) {
        let Some(index) = self.selected_index() else {
            return;
        };
        let id = self.rows()[index].0.clone();
        match &id {
            NodeId::More(key) => {
                if !self.expanded_more.remove(key) {
                    self.expanded_more.insert(key.clone());
                }
            }
            _ => {
                if !self.collapsed.remove(&id) {
                    self.collapsed.insert(id.clone());
                }
            }
        }
        self.selected = Some(id);
        self.settle_selection();
    }

    /// The selected agent, and the selected child under it if any.
    pub fn selection(&self) -> Option<(&AgentEntry, Option<&ChildEntry>)> {
        let rows = self.rows();
        rows.get(self.selected_index_in(&rows)?)?.1.selection()
    }
}

impl<'a> Row<'a> {
    /// The agent on this row, and the child too on a child row.
    pub fn selection(&self) -> Option<(&'a AgentEntry, Option<&'a ChildEntry>)> {
        match *self {
            Row::Agent { entry } => Some((entry, None)),
            Row::Child { entry, child, .. } => Some((entry, Some(child))),
            Row::Group { .. } | Row::More { .. } => None,
        }
    }
}

/// Numbers one agent's child rows along the wire in its first column: a top-level branch,
/// or the guide of a top-level child with a later sibling.
fn number_wire(rows: &mut [(NodeId, Row<'_>)]) {
    let crosses = |guides: &[bool]| guides.first().copied().unwrap_or(true);
    let period = rows
        .iter()
        .filter(|(_, r)| matches!(r, Row::Child { guides, .. } if crosses(guides)))
        .count();
    let mut next = 0;
    for (_, row) in rows {
        if let Row::Child {
            guides,
            ordinal,
            period: p,
            ..
        } = row
        {
            *p = period;
            if crosses(guides) {
                *ordinal = Some(next);
                next += 1;
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::transcript::RATE_BUCKETS;

    pub fn agent(pane: &str, label: &str, status: Status, seq: u64) -> Agent {
        Agent {
            pane_id: pane.into(),
            tab_id: format!("{pane}:t"),
            workspace_id: "w".into(),
            label: label.into(),
            named: true,
            kind: "claude".into(),
            status,
            status_seq: seq,
            cwd: "/work/repo".into(),
            session_id: Some(format!("sid-{pane}")),
            title: format!("{label} title"),
            context: None,
            cache: None,
        }
    }

    pub fn entry(group: &str, a: Agent) -> AgentEntry {
        AgentEntry {
            agent: a,
            profile: Some("alpha".into()),
            group_key: format!("/work/{group}"),
            group_label: group.into(),
            crew: None,
            activity: None,
            active_at: None,
            active: false,
            rate: [0; RATE_BUCKETS],
            tool_changed_at: None,
            children: Vec::new(),
        }
    }

    pub fn subagent(id: &str, liveness: Liveness, children: Vec<ChildEntry>) -> ChildEntry {
        ChildEntry {
            key: ChildKey::Subagent(id.into()),
            liveness,
            info: ChildInfo::Subagent {
                agent_type: "Explore".into(),
                description: format!("{id} desc"),
                activity: None,
            },
            active_at: None,
            tool_changed_at: None,
            children,
        }
    }

    fn wire(m: &Model) -> Vec<(String, Option<usize>, usize)> {
        m.rows()
            .into_iter()
            .filter_map(|(_, r)| match r {
                Row::Child {
                    child,
                    ordinal,
                    period,
                    ..
                } => match &child.key {
                    ChildKey::Subagent(id) => Some((id.clone(), ordinal, period)),
                    ChildKey::Workflow(_) => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_wire_numbers_rows_whose_first_column_it_crosses() {
        use Liveness::{Done, Running};
        let mut m = Model::default();
        let mut e = entry("incr", agent("p1", "w1", Status::Working, 1));
        e.children = vec![
            subagent(
                "c1",
                Running,
                vec![subagent(
                    "c2",
                    Running,
                    vec![subagent("c3", Running, vec![])],
                )],
            ),
            subagent("c4", Done, vec![]),
            subagent("c5", Running, vec![subagent("c6", Running, vec![])]),
        ];
        m.update(vec![e]);
        let s = |id: &str, o: Option<usize>, p: usize| (id.to_string(), o, p);
        assert_eq!(
            wire(&m),
            vec![
                s("c1", Some(0), 5),
                s("c2", Some(1), 5),
                s("c3", Some(2), 5),
                s("c4", Some(3), 5),
                s("c5", Some(4), 5),
                s("c6", None, 5),
            ]
        );
        m.move_selection(1);
        m.toggle_fold();
        assert_eq!(
            wire(&m),
            vec![
                s("c1", Some(0), 3),
                s("c4", Some(1), 3),
                s("c5", Some(2), 3),
                s("c6", None, 3),
            ]
        );
    }

    #[test]
    fn active_means_working_and_writing_or_waiting_on_a_call() {
        const NOW: i64 = 1_000_000;
        let act = |ago: i64, pending: bool| {
            let mut a = Activity {
                last_at: Some(NOW - ago),
                ..Activity::default()
            };
            if pending {
                a.pending.insert("t1".into());
            }
            a
        };
        assert!(is_active(Status::Working, Some(&act(10, false)), NOW));
        assert!(is_active(Status::Working, Some(&act(300, true)), NOW));
        assert!(!is_active(Status::Working, Some(&act(300, false)), NOW));
        assert!(!is_active(Status::Working, Some(&act(700, true)), NOW));
        assert!(!is_active(Status::Blocked, Some(&act(1, true)), NOW));
        assert!(!is_active(Status::Working, None, NOW));
    }

    #[test]
    fn a_wire_runs_under_any_running_child_unless_the_agent_is_blocked() {
        let with = |status, liveness| {
            let mut e = entry("incr", agent("p1", "w1", status, 1));
            e.children = vec![subagent(
                "c1",
                Liveness::Done,
                vec![subagent("c2", liveness, vec![])],
            )];
            e.wire_running()
        };
        assert!(with(Status::Working, Liveness::Running));
        assert!(with(Status::Done, Liveness::Running));
        assert!(!with(Status::Blocked, Liveness::Running));
        assert!(!with(Status::Working, Liveness::Stale));
    }

    fn ids(m: &Model) -> Vec<NodeId> {
        m.rows().into_iter().map(|(id, _)| id).collect()
    }

    fn a(p: &str) -> NodeId {
        NodeId::Agent(p.into())
    }

    #[test]
    fn groups_by_repo_and_sorts_by_status_then_last_change() {
        let mut m = Model::default();
        m.update(vec![
            entry("incr", agent("p1", "w1", Status::Idle, 50)),
            entry("incr", agent("p2", "w2", Status::Done, 10)),
            entry("incr", agent("p3", "w3", Status::Working, 5)),
            entry("incr", agent("p4", "w4", Status::Done, 20)),
            entry("loom", agent("p5", "loom", Status::Working, 1)),
            entry("incr", agent("p6", "w0", Status::Done, 20)),
        ]);
        assert_eq!(
            ids(&m),
            vec![
                NodeId::Group("/work/incr".into()),
                a("p3"),
                a("p6"),
                a("p4"),
                a("p2"),
                a("p1"),
                NodeId::Group("/work/loom".into()),
                a("p5"),
            ]
        );
    }

    #[test]
    fn done_agents_past_three_fold_until_expanded() {
        let mut m = Model::default();
        let entries = (0..5)
            .map(|i| {
                entry(
                    "incr",
                    agent(&format!("p{i}"), &format!("w{i}"), Status::Done, 10 - i),
                )
            })
            .collect();
        m.update(entries);
        let rows = m.rows();
        assert_eq!(rows.len(), 1 + DONE_SHOWN + 1);
        let (more_id, more) = rows.last().unwrap();
        assert!(matches!(more, Row::More { hidden: 2, .. }));

        m.selected = Some(more_id.clone());
        m.toggle_fold();
        assert_eq!(m.rows().len(), 1 + 5 + 1);
        m.toggle_fold();
        assert_eq!(m.rows().len(), 1 + DONE_SHOWN + 1);
    }

    #[test]
    fn selection_follows_the_node_when_rows_reorder() {
        let mut m = Model::default();
        m.update(vec![
            entry("incr", agent("p1", "w1", Status::Working, 1)),
            entry("incr", agent("p2", "w2", Status::Done, 1)),
        ]);
        m.move_selection(2);
        assert_eq!(m.selection().unwrap().0.agent.pane_id, "p2");
        m.update(vec![
            entry("incr", agent("p1", "w1", Status::Done, 1)),
            entry("incr", agent("p2", "w2", Status::Working, 2)),
        ]);
        assert_eq!(m.selection().unwrap().0.agent.pane_id, "p2");
        assert_eq!(m.selected_index(), Some(1));
    }

    #[test]
    fn a_vanished_child_hands_selection_to_its_agent() {
        let mut m = Model::default();
        let mut e = entry("incr", agent("p1", "w1", Status::Working, 1));
        e.children = vec![subagent("c1", Liveness::Running, vec![])];
        m.update(vec![e.clone()]);
        m.move_selection(2);
        assert!(m.selection().unwrap().1.is_some());
        e.children.clear();
        m.update(vec![e]);
        let (agent, child) = m.selection().unwrap();
        assert_eq!(agent.agent.pane_id, "p1");
        assert!(child.is_none());
    }

    #[test]
    fn children_nest_with_guides() {
        let mut m = Model::default();
        let mut e = entry("incr", agent("p1", "w1", Status::Working, 1));
        e.children = vec![
            subagent(
                "c1",
                Liveness::Running,
                vec![subagent("c2", Liveness::Running, vec![])],
            ),
            subagent("c3", Liveness::Running, vec![]),
        ];
        m.update(vec![e]);
        let rows = m.rows();
        let shape: Vec<(Vec<bool>, bool)> = rows
            .iter()
            .filter_map(|(_, r)| match r {
                Row::Child { guides, last, .. } => Some((guides.clone(), *last)),
                _ => None,
            })
            .collect();
        assert_eq!(
            shape,
            vec![(vec![], false), (vec![true], true), (vec![], true)]
        );
    }

    #[test]
    fn working_only_hides_the_rest_and_empty_groups() {
        let mut m = Model::default();
        m.update(vec![
            entry("incr", agent("p1", "w1", Status::Done, 1)),
            entry("loom", agent("p2", "l", Status::Working, 1)),
            entry("loom", agent("p3", "x", Status::Blocked, 1)),
        ]);
        m.working_only = true;
        assert_eq!(
            ids(&m),
            vec![NodeId::Group("/work/loom".into()), a("p3"), a("p2")]
        );
    }

    #[test]
    fn folding_an_agent_or_group_hides_what_is_under_it() {
        let mut m = Model::default();
        let mut e = entry("incr", agent("p1", "w1", Status::Working, 1));
        e.children = vec![subagent("c1", Liveness::Running, vec![])];
        m.update(vec![e, entry("incr", agent("p2", "w2", Status::Idle, 1))]);
        assert_eq!(m.rows().len(), 4);
        m.toggle_fold();
        assert_eq!(
            ids(&m),
            vec![NodeId::Group("/work/incr".into()), a("p1"), a("p2")]
        );
        m.move_selection(-1);
        m.toggle_fold();
        assert_eq!(ids(&m), vec![NodeId::Group("/work/incr".into())]);
    }

    #[test]
    fn the_cursor_starts_on_the_first_agent() {
        let mut m = Model::default();
        m.update(vec![entry("incr", agent("p1", "w1", Status::Idle, 1))]);
        assert_eq!(m.selected_index(), Some(1));
        assert_eq!(m.selection().unwrap().0.agent.pane_id, "p1");
    }

    #[test]
    fn selection_stays_in_bounds() {
        let mut m = Model::default();
        m.update(vec![entry("incr", agent("p1", "w1", Status::Working, 1))]);
        m.move_selection(-5);
        assert_eq!(m.selected_index(), Some(0));
        m.move_selection(50);
        assert_eq!(m.selected_index(), Some(1));
        m.update(Vec::new());
        assert_eq!(m.selected_index(), None);
        assert!(m.selection().is_none());
    }
}
