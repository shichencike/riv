//! 历史遍历：沿父链收集提交（默认第一父链，对齐 `git log` 的默认行为）。

use crate::error::{Error, Result};
use crate::object::{Commit, Object};
use crate::oid::Oid;
use crate::repo::Repo;

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub oid: Oid,
    pub commit: Commit,
}

/// 从 `start`（缺省 HEAD）沿第一父链遍历历史。
///
/// `limit` 为 None 时遍历到根提交。遇到缺失/损坏对象时停止
/// （浅克隆或部分仓库的容错行为）。
pub fn log(repo: &Repo, start: Option<Oid>, limit: Option<usize>) -> Result<Vec<LogEntry>> {
    let store = repo.store()?;
    let mut cur = match start {
        Some(o) => Some(o),
        None => repo.refs().resolve_head()?,
    };
    let mut out = Vec::new();
    while let Some(oid) = cur {
        if let Some(l) = limit {
            if out.len() >= l {
                break;
            }
        }
        match store.read(&oid)? {
            Object::Commit(c) => {
                cur = c.parents.first().copied();
                out.push(LogEntry { oid, commit: c });
            }
            _ => {
                return Err(Error::InvalidRevision(format!(
                    "{} is not a commit",
                    oid.short(7)
                )))
            }
        }
    }
    Ok(out)
}

/// 从 `start` 出发按 BFS 收集全部可达提交（含合并提交的所有父链），
/// 按提交时间降序排列（时间相同按 oid 降序，保证确定性）。
pub fn log_all(repo: &Repo, start: Option<Oid>) -> Result<Vec<LogEntry>> {
    let store = repo.store()?;
    let root = match start {
        Some(o) => o,
        None => repo
            .refs()
            .resolve_head()?
            .ok_or_else(|| Error::InvalidRevision("HEAD points to no commit".into()))?,
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(root);
    seen.insert(root);
    let mut out = Vec::new();
    while let Some(oid) = queue.pop_front() {
        match store.read(&oid)? {
            Object::Commit(c) => {
                for p in &c.parents {
                    if seen.insert(*p) {
                        queue.push_back(*p);
                    }
                }
                out.push(LogEntry { oid, commit: c });
            }
            _ => {
                return Err(Error::InvalidRevision(format!(
                    "{} is not a commit",
                    oid.short(7)
                )))
            }
        }
    }
    out.sort_by(|a, b| {
        b.commit
            .committer
            .seconds
            .cmp(&a.commit.committer.seconds)
            .then_with(|| b.oid.cmp(&a.oid))
    });
    Ok(out)
}

/// `oid` 是否是从 `from` 可达的祖先（用于分支合并检查）。
pub fn is_ancestor(repo: &Repo, from: Oid, target: Oid) -> Result<bool> {
    if from == target {
        return Ok(true);
    }
    let store = repo.store()?;
    let mut seen = std::collections::BTreeSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(from);
    seen.insert(from);
    while let Some(oid) = queue.pop_front() {
        let commit = match store.read(&oid)? {
            Object::Commit(c) => c,
            _ => continue,
        };
        for p in &commit.parents {
            if *p == target {
                return Ok(true);
            }
            if seen.insert(*p) {
                queue.push_back(*p);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Index, IndexEntry};
    use crate::object::{Signature, MODE_BLOB};
    use crate::repo::Repo;
    use crate::snapshot::build_tree_from_index;

    fn setup() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        (dir, repo)
    }

    fn make_commit(repo: &Repo, content: &str, parents: Vec<Oid>, secs: i64) -> Oid {
        let store = repo.store().unwrap();
        let mut index = Index::new();
        let blob = store
            .write(&Object::Blob(content.as_bytes().to_vec()))
            .unwrap();
        index.insert(IndexEntry {
            path: "f.txt".into(),
            oid: blob,
            mode: MODE_BLOB,
            size: content.len() as u64,
            mtime_sec: 0,
            mtime_nsec: 0,
            flags: 0,
        });
        let tree = build_tree_from_index(&store, &index).unwrap();
        let sig = Signature {
            name: "t".into(),
            email: "t@t".into(),
            seconds: secs,
            tz_offset_minutes: 0,
        };
        store
            .write(&Object::Commit(Commit {
                tree,
                parents,
                author: sig.clone(),
                committer: sig,
                message: format!("commit at {secs}"),
            }))
            .unwrap()
    }

    #[test]
    fn log_first_parent_chain() {
        let (_d, repo) = setup();
        let c1 = make_commit(&repo, "1", vec![], 1000);
        let c2 = make_commit(&repo, "2", vec![c1], 2000);
        let c3 = make_commit(&repo, "3", vec![c2], 3000);
        repo.refs().set_branch("main", &c3).unwrap();

        let entries = log(&repo, None, None).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].oid, c3);
        assert_eq!(entries[1].oid, c2);
        assert_eq!(entries[2].oid, c1);
        assert_eq!(entries[2].commit.parents, Vec::new());

        // limit
        let limited = log(&repo, None, Some(2)).unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[1].oid, c2);

        // 指定起点
        let from_c2 = log(&repo, Some(c2), None).unwrap();
        assert_eq!(from_c2.len(), 2);
        assert_eq!(from_c2[0].oid, c2);
    }

    #[test]
    fn log_all_includes_merge_side() {
        let (_d, repo) = setup();
        let c1 = make_commit(&repo, "1", vec![], 1000);
        let c2 = make_commit(&repo, "2", vec![c1], 2000);
        let side = make_commit(&repo, "side", vec![c1], 1500);
        let merge = make_commit(&repo, "3", vec![c2, side], 3000);
        repo.refs().set_branch("main", &merge).unwrap();

        // 第一父链：merge, c2, c1（不含 side）
        let first = log(&repo, None, None).unwrap();
        assert_eq!(first.len(), 3);
        assert!(!first.iter().any(|e| e.oid == side));

        // 全部可达：4 个
        let all = log_all(&repo, None).unwrap();
        assert_eq!(all.len(), 4);
        assert!(all.iter().any(|e| e.oid == side));
        // 时间降序
        assert_eq!(all[0].oid, merge);
        assert_eq!(all[3].oid, c1);
    }

    #[test]
    fn is_ancestor_checks() {
        let (_d, repo) = setup();
        let c1 = make_commit(&repo, "1", vec![], 1000);
        let c2 = make_commit(&repo, "2", vec![c1], 2000);
        let other = make_commit(&repo, "other", vec![], 1500);
        assert!(is_ancestor(&repo, c2, c1).unwrap());
        assert!(is_ancestor(&repo, c1, c1).unwrap());
        assert!(!is_ancestor(&repo, c1, c2).unwrap());
        assert!(!is_ancestor(&repo, c2, other).unwrap());
    }

    #[test]
    fn log_empty_repo() {
        let (_d, repo) = setup();
        assert!(log(&repo, None, None).unwrap().is_empty());
        assert!(log_all(&repo, None).is_err());
    }
}
