use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT: &str = "default";

/// Profile names per pane, read from the environ of the pane's foreground process group
/// leader and cached until that process is gone.
#[derive(Debug)]
pub struct Profiles {
    proc_root: PathBuf,
    by_pane: HashMap<String, (u32, String)>,
}

impl Profiles {
    pub fn new(proc_root: impl Into<PathBuf>) -> Self {
        Profiles {
            proc_root: proc_root.into(),
            by_pane: HashMap::new(),
        }
    }

    /// `pgid` asks herdr for the pane's foreground process group, and runs only on a miss.
    pub fn get(&mut self, pane_id: &str, pgid: impl FnOnce() -> Option<u32>) -> Option<&str> {
        let alive = |pid: u32| self.proc_root.join(pid.to_string()).exists();
        if !self
            .by_pane
            .get(pane_id)
            .is_some_and(|(pid, _)| alive(*pid))
        {
            self.by_pane.remove(pane_id);
            let pid = pgid()?;
            let profile = read_profile(&self.proc_root, pid)?;
            self.by_pane.insert(pane_id.to_string(), (pid, profile));
        }
        self.by_pane.get(pane_id).map(|(_, p)| p.as_str())
    }

    pub fn retain(&mut self, live: impl Fn(&str) -> bool) {
        self.by_pane.retain(|pane, _| live(pane));
    }
}

/// `CLAUDE_PROFILE`, else the `<name>` of a `CLAUDE_CONFIG_DIR` ending in `.claude-<name>`,
/// which is all a crew worker's launch sets.
fn read_profile(proc_root: &Path, pid: u32) -> Option<String> {
    let environ = fs::read(proc_root.join(pid.to_string()).join("environ")).ok()?;
    let var = |name: &[u8]| {
        environ
            .split(|&b| b == 0)
            .find_map(|kv| kv.strip_prefix(name))
            .filter(|v| !v.is_empty())
            .map(|v| String::from_utf8_lossy(v).into_owned())
    };
    let from_dir = || {
        let dir = var(b"CLAUDE_CONFIG_DIR=")?;
        let base = Path::new(&dir).file_name()?.to_str()?.to_string();
        base.strip_prefix(".claude-").map(String::from)
    };
    Some(
        var(b"CLAUDE_PROFILE=")
            .or_else(from_dir)
            .unwrap_or_else(|| DEFAULT.into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn proc_with(dir: &Path, pid: u32, environ: &[&str]) {
        let p = dir.join(pid.to_string());
        fs::create_dir_all(&p).unwrap();
        let mut bytes = Vec::new();
        for kv in environ {
            bytes.extend_from_slice(kv.as_bytes());
            bytes.push(0);
        }
        fs::write(p.join("environ"), bytes).unwrap();
    }

    #[test]
    fn reads_claude_profile_from_the_group_leader() {
        let dir = tempfile::tempdir().unwrap();
        proc_with(dir.path(), 42, &["HOME=/h", "CLAUDE_PROFILE=alpha", "X=1"]);
        let mut p = Profiles::new(dir.path());
        assert_eq!(p.get("w:p1", || Some(42)), Some("alpha"));
    }

    #[test]
    fn falls_back_to_the_config_dir_suffix() {
        let dir = tempfile::tempdir().unwrap();
        proc_with(dir.path(), 5, &["CLAUDE_CONFIG_DIR=/h/.claude-delta"]);
        proc_with(dir.path(), 6, &["CLAUDE_CONFIG_DIR=/h/.claude"]);
        proc_with(
            dir.path(),
            8,
            &[
                "CLAUDE_CONFIG_DIR=/h/.claude-delta/",
                "CLAUDE_PROFILE=alpha",
            ],
        );
        let mut p = Profiles::new(dir.path());
        assert_eq!(p.get("a", || Some(5)), Some("delta"));
        assert_eq!(p.get("b", || Some(6)), Some(DEFAULT));
        assert_eq!(p.get("c", || Some(8)), Some("alpha"));
    }

    #[test]
    fn a_plain_launch_is_the_default_profile() {
        let dir = tempfile::tempdir().unwrap();
        proc_with(dir.path(), 7, &["HOME=/h"]);
        let mut p = Profiles::new(dir.path());
        assert_eq!(p.get("w:p1", || Some(7)), Some(DEFAULT));
    }

    #[test]
    fn caches_until_the_process_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        proc_with(dir.path(), 42, &["CLAUDE_PROFILE=alpha"]);
        let mut p = Profiles::new(dir.path());
        let calls = Cell::new(0);
        let lookup = |pid| {
            calls.set(calls.get() + 1);
            Some(pid)
        };
        p.get("w:p1", || lookup(42));
        p.get("w:p1", || lookup(42));
        assert_eq!(calls.get(), 1);

        fs::remove_dir_all(dir.path().join("42")).unwrap();
        proc_with(dir.path(), 43, &["CLAUDE_PROFILE=beta"]);
        assert_eq!(p.get("w:p1", || lookup(43)), Some("beta"));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn an_unreadable_process_is_unknown_and_retried() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Profiles::new(dir.path());
        assert_eq!(p.get("w:p1", || Some(99)), None);
        assert_eq!(p.get("w:p1", || None), None);
        proc_with(dir.path(), 99, &["CLAUDE_PROFILE=gamma"]);
        assert_eq!(p.get("w:p1", || Some(99)), Some("gamma"));
    }
}
