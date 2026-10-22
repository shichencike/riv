//! 工作区状态：HEAD 树 / 索引 / 工作区三方比较（对齐 git status 的两列模型）。
//!
//! - `staged`：HEAD 树 vs 索引（已 `add` 但未 `commit` 的变更）
//! - `unstaged`：索引 vs 工作区（已跟踪文件的本地修改）
//! - `untracked`：工作区有、索引无的文件

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::Result;
use crate::index::Index;
use crate::oid::Oid;
use crate::repo::Repo;
use crate::snapshot::{check_freshness, flatten_tree, walk_workdir, TreeMap};
use crate::treediff::{diff_tree_maps, ChangeKind};

/// 暂存区状态（HEAD 树 vs 索引）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagedStatus {
    /// 索引有、HEAD 树无
    New,
    Modified,
    Deleted,
    /// 类型变化（regular ↔ symlink、blob ↔ tree）
    TypeChanged,
}

/// 工作区状态（索引 vs 工作区）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnstagedStatus {
    Modified,
    Deleted,
    TypeChanged,
}

#[derive(Debug, Clone)]
pub struct Status {
    /// 当前分支名；分离头指针时为 `(detached)`
    pub branch: String,
    /// HEAD 指向的提交（新仓库为 None）
    pub head: Option<Oid>,
    /// (路径, 状态)，按路径排序
    pub staged: Vec<(String, StagedStatus)>,
    /// (路径, 状态)，按路径排序
    pub unstaged: Vec<(String, UnstagedStatus)>,
    /// 未跟踪文件，按路径排序
    pub untracked: Vec<String>,
}

impl Status {
    pub fn is_clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty() && self.untracked.is_empty()
    }

    pub fn has_staged(&self) -> bool {
        !self.staged.is_empty()
    }
}

/// 计算仓库当前状态。
pub fn status(repo: &Repo) -> Result<Status> {
    let refs = repo.refs();
    let branch = refs
        .current_branch()?
        .unwrap_or_else(|| "(detached)".to_string());
    let head = refs.resolve_head()?;

    // HEAD 树（新仓库为空树）
    let head_map: TreeMap = match repo.head_tree()? {
        Some(t) => flatten_tree(&repo.store()?, Some(t))?,
        None => TreeMap::new(),
    };

    let index: Index = repo.index()?;
    let index_map: TreeMap = index
        .entries
        .iter()
        .map(|e| (e.path.clone(), (e.oid, e.mode)))
        .collect();

    // ---- staged：HEAD 树 vs 索引 ----
    let mut staged = Vec::new();
    for change in diff_tree_maps(&head_map, &index_map) {
        let s = match change.kind {
            ChangeKind::Added => StagedStatus::New,
            ChangeKind::Modified => StagedStatus::Modified,
            ChangeKind::Deleted => StagedStatus::Deleted,
            ChangeKind::TypeChanged => StagedStatus::TypeChanged,
        };
        staged.push((change.path, s));
    }

    // ---- unstaged：索引 vs 工作区 ----
    let mut unstaged = Vec::new();
    for entry in &index.entries {
        let abs = repo.workdir.join(&entry.path);
        match check_freshness(entry, &abs, index.index_mtime_sec)? {
            crate::snapshot::Freshness::Clean => {}
            crate::snapshot::Freshness::Missing => {
                unstaged.push((entry.path.clone(), UnstagedStatus::Deleted))
            }
            crate::snapshot::Freshness::Dirty { mode, .. } => {
                let kind = if type_bits(mode) != type_bits(entry.mode) {
                    UnstagedStatus::TypeChanged
                } else {
                    UnstagedStatus::Modified
                };
                unstaged.push((entry.path.clone(), kind));
            }
        }
    }

    // ---- untracked：工作区有、索引无 ----
    let index_paths: std::collections::BTreeSet<&str> =
        index.entries.iter().map(|e| e.path.as_str()).collect();
    let mut untracked: Vec<String> = walk_workdir(repo)?
        .into_iter()
        .filter(|p| !index_paths.contains(p.as_str()))
        .collect();
    untracked.sort();

    Ok(Status {
        branch,
        head,
        staged,
        unstaged,
        untracked,
    })
}

/// mode 的类型位（忽略可执行位差异）。
fn type_bits(mode: u32) -> u32 {
    mode & 0o170000
}

/// 把状态整理成 `git status --porcelain` 风格的两列行。
pub fn porcelain(status: &Status) -> Vec<String> {
    let mut lines = Vec::new();
    let mut all_paths: BTreeMap<&str, (Option<StagedStatus>, Option<UnstagedStatus>)> =
        BTreeMap::new();
    for (p, s) in &status.staged {
        all_paths.entry(p).or_default().0 = Some(*s);
    }
    for (p, u) in &status.unstaged {
        all_paths.entry(p).or_default().1 = Some(*u);
    }
    for (p, (x, y)) in &all_paths {
        let xc = x.map(staged_code).unwrap_or(' ');
        let yc = y.map(unstaged_code).unwrap_or(' ');
        lines.push(format!("{xc}{yc} {p}"));
    }
    for p in &status.untracked {
        lines.push(format!("?? {p}"));
    }
    lines
}

fn staged_code(s: StagedStatus) -> char {
    match s {
        StagedStatus::New => 'A',
        StagedStatus::Modified => 'M',
        StagedStatus::Deleted => 'D',
        StagedStatus::TypeChanged => 'T',
    }
}

fn unstaged_code(u: UnstagedStatus) -> char {
    match u {
        UnstagedStatus::Modified => 'M',
        UnstagedStatus::Deleted => 'D',
        UnstagedStatus::TypeChanged => 'T',
    }
}

/// 便捷判断：路径是否被跟踪。
#[allow(dead_code)]
pub fn is_tracked(repo: &Repo, path: &Path) -> Result<bool> {
    let index = repo.index()?;
    let rel = repo.to_repo_relative(path)?;
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    Ok(index.entries.iter().any(|e| e.path == rel_str))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexEntry;
    use crate::object::{Object, MODE_BLOB};
    use crate::snapshot::{add, build_tree_from_index, AddOptions};
    use std::fs;

    fn setup() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        (dir, repo)
    }

    /// 直接把文件加入索引并建树、提交（绕过 add，精确控制内容）。
    fn commit_files(repo: &Repo, files: &[(&str, &[u8])]) -> Oid {
        let store = repo.store().unwrap();
        let mut index = Index::new();
        for (path, content) in files {
            let blob = store.write(&Object::Blob(content.to_vec())).unwrap();
            index.insert(IndexEntry {
                path: (*path).into(),
                oid: blob,
                mode: MODE_BLOB,
                size: content.len() as u64,
                mtime_sec: 0,
                mtime_nsec: 0,
                flags: 0,
            });
        }
        repo.save_index(&index).unwrap();
        let tree = build_tree_from_index(&store, &index).unwrap();
        let sig = crate::object::Signature {
            name: "t".into(),
            email: "t@t".into(),
            seconds: 1_700_000_000,
            tz_offset_minutes: 0,
        };
        let commit = crate::object::Commit {
            tree,
            parents: vec![],
            author: sig.clone(),
            committer: sig,
            message: "init".into(),
        };
        let oid = store.write(&Object::Commit(commit)).unwrap();
        repo.refs().advance_head(&oid).unwrap();
        oid
    }

    #[test]
    fn clean_after_commit() {
        let (_d, repo) = setup();
        let dir = repo.workdir.clone();
        commit_files(&repo, &[("a.txt", b"aaa")]);
        fs::write(dir.join("a.txt"), b"aaa").unwrap();
        let s = status(&repo).unwrap();
        assert_eq!(s.branch, "main");
        assert!(s.head.is_some());
        assert!(s.is_clean(), "{:?}", porcelain(&s));
    }

    #[test]
    fn staged_new_and_modified() {
        let (dir, repo) = setup();
        commit_files(&repo, &[("a.txt", b"aaa")]);
        fs::write(dir.path().join("a.txt"), b"aaa").unwrap();
        // 暂存新文件 + 修改
        fs::write(dir.path().join("b.txt"), b"new").unwrap();
        fs::write(dir.path().join("a.txt"), b"changed").unwrap();
        add(&repo, &AddOptions::default()).unwrap();
        let s = status(&repo).unwrap();
        assert_eq!(s.staged.len(), 2);
        assert!(s
            .staged
            .iter()
            .any(|(p, k)| p == "a.txt" && *k == StagedStatus::Modified));
        assert!(s
            .staged
            .iter()
            .any(|(p, k)| p == "b.txt" && *k == StagedStatus::New));
        assert!(s.unstaged.is_empty());
        assert!(s.untracked.is_empty());
        assert!(s.has_staged());
    }

    #[test]
    fn unstaged_modify_and_delete() {
        let (dir, repo) = setup();
        commit_files(&repo, &[("a.txt", b"aaa"), ("b.txt", b"bbb")]);
        fs::write(dir.path().join("a.txt"), b"aaa").unwrap();
        fs::write(dir.path().join("b.txt"), b"bbb").unwrap();
        // 工作区改动（不 add）
        fs::write(dir.path().join("a.txt"), b"modified").unwrap();
        fs::remove_file(dir.path().join("b.txt")).unwrap();
        let s = status(&repo).unwrap();
        assert!(s.staged.is_empty());
        assert_eq!(s.unstaged.len(), 2);
        assert!(s
            .unstaged
            .iter()
            .any(|(p, k)| p == "a.txt" && *k == UnstagedStatus::Modified));
        assert!(s
            .unstaged
            .iter()
            .any(|(p, k)| p == "b.txt" && *k == UnstagedStatus::Deleted));
    }

    #[test]
    fn staged_deletion() {
        let (dir, repo) = setup();
        commit_files(&repo, &[("a.txt", b"aaa")]);
        fs::write(dir.path().join("a.txt"), b"aaa").unwrap();
        fs::remove_file(dir.path().join("a.txt")).unwrap();
        add(&repo, &AddOptions::default()).unwrap();
        let s = status(&repo).unwrap();
        assert_eq!(s.staged, vec![("a.txt".to_string(), StagedStatus::Deleted)]);
        assert!(s.unstaged.is_empty());
    }

    #[test]
    fn untracked_and_porcelain() {
        let (dir, repo) = setup();
        commit_files(&repo, &[("a.txt", b"aaa")]);
        fs::write(dir.path().join("a.txt"), b"aaa").unwrap();
        fs::write(dir.path().join("new.txt"), b"x").unwrap();
        fs::write(dir.path().join("a.txt"), b"y").unwrap();
        let s = status(&repo).unwrap();
        assert_eq!(s.untracked, vec!["new.txt".to_string()]);
        let lines = porcelain(&s);
        assert!(lines.contains(&"?? new.txt".to_string()));
        assert!(lines.contains(&" M a.txt".to_string()));
    }

    #[test]
    fn type_change_exec_bit() {
        let (dir, repo) = setup();
        commit_files(&repo, &[("run.sh", b"#!/bin/sh")]);
        let f = dir.path().join("run.sh");
        fs::write(&f, b"#!/bin/sh").unwrap();
        // 改成可执行（内容不变，mode 变）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
            let s = status(&repo).unwrap();
            assert_eq!(
                s.unstaged,
                vec![("run.sh".to_string(), UnstagedStatus::Modified)]
            );
        }
    }

    #[test]
    fn fresh_repo_status() {
        let (_d, repo) = setup();
        let s = status(&repo).unwrap();
        assert_eq!(s.branch, "main");
        assert!(s.head.is_none());
        assert!(s.is_clean());
    }
}
