use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub status: String,
    pub deliverable: String,
    updated: u64,
}

/// Owner to item, from a crew `board.tsv` (`id status owner branch into updated deliverable`,
/// no header). An owner with several items shows the one in progress.
pub fn parse_board(text: &str) -> HashMap<String, Item> {
    let mut items: HashMap<String, Item> = HashMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [id, status, owner, _branch, _into, updated, deliverable, ..] = f[..] else {
            continue;
        };
        if owner.is_empty() || owner == "-" {
            continue;
        }
        let item = Item {
            id: id.to_string(),
            status: status.to_string(),
            deliverable: deliverable.to_string(),
            updated: updated.parse().unwrap_or_default(),
        };
        let wins = items.get(owner).is_none_or(|old| {
            (rank(&item.status), item.updated) > (rank(&old.status), old.updated)
        });
        if wins {
            items.insert(owner.to_string(), item);
        }
    }
    items
}

fn rank(status: &str) -> u8 {
    match status {
        "assigned" => 3,
        "blocked" => 2,
        "open" => 1,
        _ => 0,
    }
}

/// Main checkouts per cwd and crew boards per main checkout, both cached.
#[derive(Debug, Default)]
pub struct Repos {
    main_by_cwd: HashMap<String, Option<PathBuf>>,
    boards: HashMap<PathBuf, (Option<SystemTime>, HashMap<String, Item>)>,
}

impl Repos {
    /// The main checkout holding `cwd`, the way `crew-lib.sh` finds it. `None` outside git.
    pub fn main_repo(&mut self, cwd: &str) -> Option<&Path> {
        self.main_by_cwd
            .entry(cwd.to_string())
            .or_insert_with(|| find_main(cwd))
            .as_deref()
    }

    /// The crew board of a main checkout, read again when its mtime moves.
    pub fn board(&mut self, main: &Path) -> &HashMap<String, Item> {
        let path = main.join(".claude/crew/board.tsv");
        let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
        let entry = self
            .boards
            .entry(main.to_path_buf())
            .or_insert((None, HashMap::new()));
        if entry.0 != modified {
            entry.1 = fs::read_to_string(&path)
                .map(|t| parse_board(&t))
                .unwrap_or_default();
            entry.0 = modified;
        }
        &entry.1
    }
}

fn find_main(cwd: &str) -> Option<PathBuf> {
    // A plain `--git-common-dir` answers a relative `.git` in the main checkout.
    let out = Command::new("git")
        .args([
            "-C",
            cwd,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    fs::canonicalize(common.join("..")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn board_maps_owner_to_the_item_in_progress() {
        let board = "B1\tdone\tharness-w2\tw/B1\tmain\t100\tfirst\n\
                     B3\tassigned\tharness-w2\tw/B3\tmain\t90\tadd godview pin\n\
                     B4\topen\t-\t-\tmain\t95\tlater\n\
                     short\tline\n";
        let items = parse_board(board);
        let w2 = &items["harness-w2"];
        assert_eq!(w2.id, "B3");
        assert_eq!(w2.status, "assigned");
        assert_eq!(w2.deliverable, "add godview pin");
        assert!(!items.contains_key("-"));
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn worktrees_share_their_main_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("repo");
        fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "x"]);
        let wt = dir.path().join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        fs::create_dir_all(main.join("sub")).unwrap();

        let canonical = fs::canonicalize(&main).unwrap();
        let mut r = Repos::default();
        assert_eq!(
            r.main_repo(main.join("sub").to_str().unwrap()),
            Some(canonical.as_path())
        );
        assert_eq!(r.main_repo(wt.to_str().unwrap()), Some(canonical.as_path()));
        assert_eq!(r.main_repo(dir.path().to_str().unwrap()), None);
    }

    #[test]
    fn board_reloads_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let crew = dir.path().join(".claude/crew");
        fs::create_dir_all(&crew).unwrap();
        let mut r = Repos::default();
        assert!(r.board(dir.path()).is_empty());
        let path = crew.join("board.tsv");
        fs::write(&path, "A1\tassigned\tw1\tw/A1\tmain\t1\tthing\n").unwrap();
        assert_eq!(r.board(dir.path())["w1"].id, "A1");
        fs::write(&path, "A2\tassigned\tw1\tw/A2\tmain\t2\tother thing\n").unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        fs::File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(r.board(dir.path())["w1"].id, "A2");
    }
}
