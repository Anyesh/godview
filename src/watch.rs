use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::children::child_dirs;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub created: bool,
}

/// What a changed path belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Transcript(String),
    /// `<sid>/` or one of its child dirs appeared.
    SessionDir(String),
    /// A file in one of the session's child dirs.
    Child(String),
    Other,
}

/// The non-recursive inotify watches for each live session: its project dir (where the main
/// transcript lives), `<sid>/`, and the child dirs once they exist.
pub struct Watches {
    watcher: RecommendedWatcher,
    owners: HashMap<PathBuf, HashSet<String>>,
    by_sid: HashMap<String, Vec<PathBuf>>,
    transcripts: HashMap<String, PathBuf>,
}

impl std::fmt::Debug for Watches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watches")
            .field("owners", &self.owners)
            .finish()
    }
}

impl Watches {
    /// Changes reach `sink`, on notify's thread. Reads are not reported, so godview's own
    /// reads of the files it watches cause no events.
    pub fn new(sink: impl Fn(Change) + Send + 'static) -> Result<Self> {
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            let created = match event.kind {
                EventKind::Create(_) => true,
                EventKind::Modify(_) | EventKind::Remove(_) => false,
                _ => return,
            };
            for path in event.paths {
                sink(Change { path, created });
            }
        })?;
        Ok(Watches {
            watcher,
            owners: HashMap::new(),
            by_sid: HashMap::new(),
            transcripts: HashMap::new(),
        })
    }

    pub fn classify(&self, path: &Path) -> Target {
        let parent = path.parent();
        for (sid, transcript) in &self.transcripts {
            if path == transcript {
                return Target::Transcript(sid.clone());
            }
            let session = session_dir(transcript);
            let child_dirs = child_dirs(&session);
            if path == session || child_dirs.iter().any(|d| d == path) {
                return Target::SessionDir(sid.clone());
            }
            if parent.is_some_and(|p| child_dirs.iter().any(|d| d == p)) {
                return Target::Child(sid.clone());
            }
        }
        Target::Other
    }

    /// Watches whichever of the session's dirs exist and are not watched yet. `true` when it
    /// added a watch under the session dir, so the caller must rescan it for files written
    /// before the watch existed.
    pub fn ensure(&mut self, sid: &str, transcript: &Path) -> bool {
        let Some(project) = transcript.parent() else {
            return false;
        };
        self.transcripts
            .insert(sid.to_string(), transcript.to_path_buf());
        let session = session_dir(transcript);
        let dirs = std::iter::once((project.to_path_buf(), false))
            .chain(child_dirs(&session).into_iter().map(|d| (d, true)))
            .chain(std::iter::once((session, true)));
        let mut added_under_session = false;
        for (dir, under_session) in dirs {
            let owned = self.by_sid.get(sid).is_some_and(|v| v.contains(&dir));
            if owned || !dir.is_dir() {
                continue;
            }
            if !self.owners.contains_key(&dir)
                && let Err(e) = self.watcher.watch(&dir, RecursiveMode::NonRecursive)
            {
                crate::log::warn(&format!("watch {}: {e}", dir.display()));
                continue;
            }
            self.owners
                .entry(dir.clone())
                .or_default()
                .insert(sid.to_string());
            self.by_sid.entry(sid.to_string()).or_default().push(dir);
            added_under_session |= under_session;
        }
        added_under_session
    }

    pub fn untrack(&mut self, sid: &str) {
        self.transcripts.remove(sid);
        for dir in self.by_sid.remove(sid).unwrap_or_default() {
            let Some(owners) = self.owners.get_mut(&dir) else {
                continue;
            };
            owners.remove(sid);
            if owners.is_empty() {
                self.owners.remove(&dir);
                // inotify drops the watch of a deleted dir itself, so this can fail harmlessly.
                if let Err(e) = self.watcher.unwatch(&dir) {
                    crate::log::warn(&format!("unwatch {}: {e}", dir.display()));
                }
            }
        }
    }

    #[cfg(test)]
    pub fn watched(&self) -> impl Iterator<Item = &Path> {
        self.owners.keys().map(PathBuf::as_path)
    }
}

/// `<project>/<sid>` for the transcript `<project>/<sid>.jsonl`.
pub fn session_dir(transcript: &Path) -> PathBuf {
    transcript.with_extension("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use std::time::Duration;

    fn setup() -> (tempfile::TempDir, PathBuf, Watches, mpsc::Receiver<Change>) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("-repo");
        fs::create_dir_all(&project).unwrap();
        let (tx, rx) = mpsc::channel();
        let w = Watches::new(move |c| {
            let _ = tx.send(c);
        })
        .unwrap();
        (dir, project, w, rx)
    }

    fn saw(rx: &mpsc::Receiver<Change>, path: &Path) -> bool {
        while let Ok(c) = rx.recv_timeout(Duration::from_secs(2)) {
            if c.path == path {
                return true;
            }
        }
        false
    }

    fn sorted(w: &Watches) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = w.watched().map(Path::to_path_buf).collect();
        v.sort();
        v
    }

    #[test]
    fn session_dirs_are_watched_once_they_appear() {
        let (_d, project, mut w, rx) = setup();
        let transcript = project.join("s1.jsonl");
        fs::write(&transcript, "").unwrap();
        assert!(!w.ensure("s1", &transcript));
        assert_eq!(sorted(&w), vec![project.clone()]);

        fs::write(&transcript, "x\n").unwrap();
        assert!(saw(&rx, &transcript));

        let sub = project.join("s1/subagents");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("agent-a.jsonl"), "early\n").unwrap();
        assert!(w.ensure("s1", &transcript), "new dirs need a rescan");
        assert_eq!(
            sorted(&w),
            vec![project.clone(), project.join("s1"), sub.clone()]
        );
        assert!(!w.ensure("s1", &transcript));

        let late = sub.join("agent-b.jsonl");
        fs::write(&late, "late\n").unwrap();
        assert!(saw(&rx, &late));
    }

    #[test]
    fn untrack_keeps_a_project_dir_another_session_shares() {
        let (_d, project, mut w, rx) = setup();
        let (t1, t2) = (project.join("s1.jsonl"), project.join("s2.jsonl"));
        fs::create_dir_all(project.join("s1/workflows")).unwrap();
        w.ensure("s1", &t1);
        w.ensure("s2", &t2);
        w.untrack("s1");
        assert_eq!(sorted(&w), vec![project.clone()]);

        let gone = project.join("s1/workflows/wf_1.json");
        fs::write(&gone, "{}").unwrap();
        fs::write(&t2, "x\n").unwrap();
        let mut seen = Vec::new();
        while let Ok(c) = rx.recv_timeout(Duration::from_millis(500)) {
            seen.push(c.path);
        }
        assert!(seen.contains(&t2));
        assert!(!seen.contains(&gone));

        w.untrack("s2");
        assert_eq!(sorted(&w), Vec::<PathBuf>::new());
    }

    #[test]
    fn classifies_changes_by_the_session_that_owns_them() {
        let (_d, project, mut w, _rx) = setup();
        let t = project.join("s1.jsonl");
        w.ensure("s1", &t);
        let s1 = || "s1".to_string();
        assert_eq!(w.classify(&t), Target::Transcript(s1()));
        assert_eq!(w.classify(&project.join("s1")), Target::SessionDir(s1()));
        assert_eq!(
            w.classify(&project.join("s1/subagents")),
            Target::SessionDir(s1())
        );
        assert_eq!(
            w.classify(&project.join("s1/workflows/wf_1.json")),
            Target::Child(s1())
        );
        assert_eq!(
            w.classify(&project.join("s1/tool-results/x")),
            Target::Other
        );
        assert_eq!(w.classify(&project.join("s9.jsonl")), Target::Other);
        w.untrack("s1");
        assert_eq!(w.classify(&t), Target::Other);
    }

    #[test]
    fn creation_is_flagged() {
        let (_d, project, mut w, rx) = setup();
        w.ensure("s1", &project.join("s1.jsonl"));
        let fresh = project.join("s9.jsonl");
        fs::write(&fresh, "").unwrap();
        let c = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            c,
            Change {
                path: fresh,
                created: true
            }
        );
    }
}
