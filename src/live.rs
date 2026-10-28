use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::anim::FLASH;
use crate::children::{ChildKey, WorkflowAgent};
use crate::transcript::ToolCall;

/// How close to the wall clock a call must have started to count as new rather than one
/// read back from a resumed, reset or jumped tail.
const RECENT_SECS: i64 = 5;
pub const STREAM_LEN: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Agent(String),
    Child(String, ChildKey),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamItem {
    pub id: String,
    pub at: i64,
    pub who: String,
    pub child: Option<String>,
    pub tool: String,
    pub summary: String,
    pub highlighted_at: Option<Instant>,
}

pub fn call_item(who: &str, child: Option<&str>, call: &ToolCall) -> Option<StreamItem> {
    Some(StreamItem {
        id: call.id.clone(),
        at: call.at?,
        who: who.to_string(),
        child: child.map(String::from),
        tool: call.name.clone(),
        summary: call.summary.clone(),
        highlighted_at: None,
    })
}

/// A workflow agent's latest call. Workflow files carry no call id, so the progress time
/// stands in for one.
pub fn workflow_item(who: &str, workflow: &str, agent: &WorkflowAgent) -> Option<StreamItem> {
    let ms = agent.last_progress_at?;
    Some(StreamItem {
        id: format!("{workflow}/{}/{ms}", agent.label),
        at: ms.div_euclid(1000),
        who: who.to_string(),
        child: Some(format!("wf {}", agent.label)),
        tool: agent.last_tool.clone()?,
        summary: agent.last_summary.clone().unwrap_or_default(),
        highlighted_at: None,
    })
}

/// Which rows started a new call since the last rebuild, and which stream items are new.
#[derive(Debug)]
pub struct Tracker {
    opened: i64,
    last_tool: HashMap<Key, String>,
    changed_at: HashMap<Key, Instant>,
    /// When each stream item was highlighted, or `None` for one first seen already old.
    first_seen: HashMap<String, Option<Instant>>,
    fade_until: Option<Instant>,
}

impl Tracker {
    /// Calls that started before `opened` (epoch seconds) never highlight.
    pub fn new(opened: i64) -> Self {
        Tracker {
            opened,
            last_tool: HashMap::new(),
            changed_at: HashMap::new(),
            first_seen: HashMap::new(),
            fade_until: None,
        }
    }

    /// Takes every row's current call and every stream item of this rebuild, and returns
    /// the stream newest first with highlights set.
    pub fn update(
        &mut self,
        current: &[(Key, Option<&ToolCall>)],
        items: Vec<StreamItem>,
        wall: i64,
        now: Instant,
    ) -> Vec<StreamItem> {
        for (key, call) in current {
            // A row that lost its call (a /clear, a reset tail) keeps the last one it had.
            let Some(call) = call else { continue };
            let previous = self.last_tool.insert(key.clone(), call.id.clone());
            let changed = previous.is_some_and(|p| p != call.id);
            if changed && call.at.is_some_and(|at| wall - at <= RECENT_SECS) {
                self.changed_at.insert(key.clone(), now);
                self.bump(now);
            }
        }
        let keys: HashSet<&Key> = current.iter().map(|(k, _)| k).collect();
        self.last_tool.retain(|k, _| keys.contains(k));
        self.changed_at.retain(|k, _| keys.contains(k));

        let mut stream = items;
        stream.sort_by_key(|i| Reverse(i.at));
        stream.truncate(STREAM_LEN);
        for item in &mut stream {
            item.highlighted_at = match self.first_seen.get(&item.id) {
                Some(seen) => *seen,
                None => {
                    let fresh = item.at > self.opened && wall - item.at <= RECENT_SECS;
                    let seen = fresh.then_some(now);
                    if fresh {
                        self.bump(now);
                    }
                    self.first_seen.insert(item.id.clone(), seen);
                    seen
                }
            };
        }
        let ids: HashSet<&str> = stream.iter().map(|i| i.id.as_str()).collect();
        self.first_seen.retain(|id, _| ids.contains(id.as_str()));
        stream
    }

    fn bump(&mut self, now: Instant) {
        let until = now + FLASH;
        self.fade_until = Some(self.fade_until.map_or(until, |t| t.max(until)));
    }

    pub fn changed_at(&self, key: &Key) -> Option<Instant> {
        self.changed_at.get(key).copied()
    }

    /// Until when a flash or highlight is on screen and frames must keep running.
    pub fn fade_until(&self) -> Option<Instant> {
        self.fade_until
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const WALL: i64 = 1_000_000;

    fn call(id: &str, at: i64) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "Read".into(),
            summary: format!("{id} file"),
            at: Some(at),
        }
    }

    fn pane(p: &str) -> Key {
        Key::Agent(p.into())
    }

    fn item(id: &str, at: i64) -> StreamItem {
        call_item("w1", None, &call(id, at)).unwrap()
    }

    #[test]
    fn first_sight_flashes_and_highlights_nothing() {
        let mut t = Tracker::new(WALL);
        let now = Instant::now();
        let c = call("t1", WALL - 2);
        let stream = t.update(
            &[(pane("p1"), Some(&c))],
            vec![item("t1", WALL - 2)],
            WALL,
            now,
        );
        assert_eq!(t.changed_at(&pane("p1")), None);
        assert_eq!(stream[0].highlighted_at, None);
        assert_eq!(t.fade_until(), None);
    }

    #[test]
    fn a_new_recent_call_flashes_and_highlights() {
        let mut t = Tracker::new(WALL);
        let start = Instant::now();
        let (c1, c2) = (call("t1", WALL - 30), call("t2", WALL + 8));
        t.update(&[(pane("p1"), Some(&c1))], vec![], WALL, start);
        let now = start + Duration::from_secs(10);
        let stream = t.update(
            &[(pane("p1"), Some(&c2))],
            vec![item("t2", WALL + 8)],
            WALL + 10,
            now,
        );
        assert_eq!(t.changed_at(&pane("p1")), Some(now));
        assert_eq!(stream[0].highlighted_at, Some(now));
        assert_eq!(t.fade_until(), Some(now + FLASH));
    }

    #[test]
    fn a_resumed_session_or_cleared_tail_does_not_flash() {
        let mut t = Tracker::new(WALL);
        let now = Instant::now();
        let (old, older) = (call("t1", WALL - 30), call("t0", WALL - 900));
        t.update(&[(pane("p1"), Some(&old))], vec![], WALL, now);
        t.update(&[(pane("p1"), Some(&older))], vec![], WALL + 1, now);
        assert_eq!(t.changed_at(&pane("p1")), None);
        t.update(&[(pane("p1"), None)], vec![], WALL + 2, now);
        assert_eq!(t.changed_at(&pane("p1")), None);
        t.update(&[(pane("p1"), Some(&older))], vec![], WALL + 3, now);
        assert_eq!(t.changed_at(&pane("p1")), None);
        assert_eq!(t.fade_until(), None);
    }

    #[test]
    fn an_old_item_is_not_highlighted_even_when_new() {
        let mut t = Tracker::new(WALL);
        let now = Instant::now();
        let stream = t.update(&[], vec![item("t1", WALL + 3)], WALL + 60, now);
        assert_eq!(stream[0].highlighted_at, None);
        assert_eq!(t.fade_until(), None);
    }

    #[test]
    fn a_highlight_happens_once_per_item() {
        let mut t = Tracker::new(WALL);
        let now = Instant::now();
        let first = t.update(&[], vec![item("t1", WALL + 1)], WALL + 1, now);
        let later = now + Duration::from_secs(1);
        let again = t.update(&[], vec![item("t1", WALL + 1)], WALL + 2, later);
        assert_eq!(first[0].highlighted_at, Some(now));
        assert_eq!(again[0].highlighted_at, Some(now));
        assert_eq!(t.fade_until(), Some(now + FLASH));
    }

    #[test]
    fn workflow_items_keep_their_id_across_rebuilds() {
        let agent = WorkflowAgent {
            label: "review:a".into(),
            state: "progress".into(),
            last_tool: Some("Read".into()),
            last_summary: Some("src/a.rs".into()),
            last_progress_at: Some((WALL + 1) * 1000 + 250),
        };
        let a = workflow_item("harness", "wf_1", &agent).unwrap();
        let b = workflow_item("harness", "wf_1", &agent.clone()).unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(a.at, WALL + 1);
        assert_eq!(a.child.as_deref(), Some("wf review:a"));
        let mut t = Tracker::new(WALL);
        let now = Instant::now();
        t.update(&[], vec![a], WALL + 1, now);
        let later = now + Duration::from_secs(5);
        let again = t.update(&[], vec![b], WALL + 6, later);
        assert_eq!(again[0].highlighted_at, Some(now));
        let quiet = WorkflowAgent {
            last_progress_at: None,
            ..agent
        };
        assert_eq!(workflow_item("harness", "wf_1", &quiet), None);
    }

    #[test]
    fn the_stream_is_newest_first_and_bounded() {
        let mut t = Tracker::new(WALL);
        let items = (0..STREAM_LEN as i64 + 5)
            .map(|i| item(&format!("t{i}"), WALL - 100 + i))
            .collect();
        let stream = t.update(&[], items, WALL, Instant::now());
        assert_eq!(stream.len(), STREAM_LEN);
        assert_eq!(stream[0].id, format!("t{}", STREAM_LEN + 4));
        assert!(stream.windows(2).all(|w| w[0].at >= w[1].at));
    }

    #[test]
    fn maps_forget_rows_and_items_that_are_gone() {
        let mut t = Tracker::new(WALL);
        let now = Instant::now();
        let (c1, c2) = (call("t1", WALL), call("t2", WALL + 1));
        let child = Key::Child("p1".into(), ChildKey::Subagent("a1".into()));
        t.update(
            &[(pane("p1"), Some(&c1)), (child.clone(), Some(&c1))],
            vec![item("t1", WALL)],
            WALL,
            now,
        );
        t.update(
            &[(pane("p1"), Some(&c2)), (child.clone(), Some(&c2))],
            vec![item("t1", WALL), item("t2", WALL + 1)],
            WALL + 1,
            now,
        );
        assert_eq!(t.changed_at.len(), 2);
        t.update(&[(pane("p2"), Some(&c1))], vec![], WALL + 2, now);
        assert_eq!(t.last_tool.keys().collect::<Vec<_>>(), vec![&pane("p2")]);
        assert!(t.changed_at.is_empty());
        assert!(t.first_seen.is_empty());
    }

    #[test]
    fn the_fade_runs_until_the_last_flash_has_shown() {
        let mut t = Tracker::new(WALL);
        let start = Instant::now();
        let (c1, c2, c3) = (call("t1", WALL), call("t2", WALL + 1), call("t3", WALL + 2));
        t.update(&[(pane("p1"), Some(&c1))], vec![], WALL, start);
        let first = start + Duration::from_millis(100);
        t.update(&[(pane("p1"), Some(&c2))], vec![], WALL + 1, first);
        let second = start + Duration::from_millis(400);
        t.update(&[(pane("p1"), Some(&c3))], vec![], WALL + 2, second);
        assert_eq!(t.fade_until(), Some(second + FLASH));
    }
}
