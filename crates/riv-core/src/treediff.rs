//! 树间差异：以路径为粒度比较两棵树（或树与空）。
//!
//! 输出 Added / Modified / Deleted / TypeChanged 四种变更，
//! 按路径字典序排列。实现为先分别展平两棵树再合并比较
//! （共同子树 oid 相同的情形由展平天然覆盖，正确性优先）。

use crate::error::Result;
use crate::object::{Object, MODE_TREE};
use crate::oid::Oid;
use crate::snapshot::{flatten_tree, TreeMap};
use crate::store::ObjectStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// 类型变化（如 blob ↔ tree、普通文件 ↔ 可执行 / symlink）
    TypeChanged,
}

#[derive(Debug, Clone)]
pub struct TreeChange {
    pub path: String,
    pub kind: ChangeKind,
    pub old: Option<(Oid, u32)>,
    pub new: Option<(Oid, u32)>,
}

/// 比较两棵树（None 表示「无树」，即空树）。
pub fn diff_trees(
    store: &ObjectStore,
    old: Option<Oid>,
    new: Option<Oid>,
) -> Result<Vec<TreeChange>> {
    let old_map = flatten_tree(store, old)?;
    let new_map = flatten_tree(store, new)?;
    Ok(diff_tree_maps(&old_map, &new_map))
}

/// 比较两个已展平的树映射。
pub fn diff_tree_maps(old: &TreeMap, new: &TreeMap) -> Vec<TreeChange> {
    let mut out = Vec::new();
    let mut paths: Vec<&String> = old.keys().chain(new.keys()).collect();
    paths.sort();
    paths.dedup();
    for p in paths {
        let o = old.get(p).copied();
        let n = new.get(p).copied();
        match (o, n) {
            (None, Some(nv)) => out.push(TreeChange {
                path: p.clone(),
                kind: ChangeKind::Added,
                old: None,
                new: Some(nv),
            }),
            (Some(ov), None) => out.push(TreeChange {
                path: p.clone(),
                kind: ChangeKind::Deleted,
                old: Some(ov),
                new: None,
            }),
            (Some(ov), Some(nv)) => {
                // oid 相同且 mode 在当前平台语义下等价 → 无变化。
                // Windows 上 exec 位差异不可见（对齐 core.filemode=false），
                // 避免跨平台提交产生幻影 mode 变更。
                if ov.0 == nv.0 && crate::object::mode_equivalent(ov.1, nv.1) {
                    continue;
                }
                let kind = if is_type_change(ov.1, nv.1) {
                    ChangeKind::TypeChanged
                } else {
                    ChangeKind::Modified
                };
                out.push(TreeChange {
                    path: p.clone(),
                    kind,
                    old: Some(ov),
                    new: Some(nv),
                });
            }
            (None, None) => unreachable!(),
        }
    }
    out
}

/// mode 的「类型位」是否变化（忽略可执行位差异）。
fn is_type_change(old_mode: u32, new_mode: u32) -> bool {
    fn fmt(m: u32) -> u32 {
        if m == MODE_TREE {
            MODE_TREE
        } else {
            m & 0o170000
        }
    }
    fmt(old_mode) != fmt(new_mode)
}

/// 树中某个路径指向的对象内容是否与另一树相同（用于递归剪枝的辅助）。
#[allow(dead_code)]
pub fn entry_eq(store: &ObjectStore, a: (Oid, u32), b: (Oid, u32)) -> Result<bool> {
    if a == b {
        return Ok(true);
    }
    // blob 内容比较（tree 由 oid 唯一确定，无需展开）
    if a.1 != MODE_TREE && b.1 != MODE_TREE {
        let ba = store.read_blob(&a.0)?;
        let bb = store.read_blob(&b.0)?;
        return Ok(ba == bb);
    }
    Ok(false)
}

/// 变更统计：Added / Modified / Deleted / TypeChanged 计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub type_changed: usize,
}

pub fn stat(changes: &[TreeChange]) -> DiffStat {
    let mut s = DiffStat::default();
    for c in changes {
        match c.kind {
            ChangeKind::Added => s.added += 1,
            ChangeKind::Modified => s.modified += 1,
            ChangeKind::Deleted => s.deleted += 1,
            ChangeKind::TypeChanged => s.type_changed += 1,
        }
    }
    s
}

/// 确认 oid 是树对象（防御性检查，供调用方使用）。
#[allow(dead_code)]
pub fn is_tree(store: &ObjectStore, oid: &Oid) -> Result<bool> {
    Ok(matches!(store.read(oid)?, Object::Tree(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Index, IndexEntry};
    #[cfg(unix)]
    use crate::object::MODE_BLOB_EXEC;
    use crate::object::{Tree, TreeEntry, MODE_BLOB};
    use crate::repo::Repo;
    use crate::snapshot::build_tree_from_index;

    fn setup() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        (dir, repo)
    }

    fn index_with(repo: &Repo, files: &[(&str, &[u8], u32)]) -> Oid {
        let store = repo.store().unwrap();
        let mut index = Index::new();
        for (path, content, mode) in files {
            let blob = store.write(&Object::Blob(content.to_vec())).unwrap();
            index.insert(IndexEntry {
                path: (*path).into(),
                oid: blob,
                mode: *mode,
                size: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                flags: 0,
            });
        }
        build_tree_from_index(&store, &index).unwrap()
    }

    #[test]
    fn diff_added_modified_deleted() {
        let (_d, repo) = setup();
        let old = index_with(
            &repo,
            &[
                ("a.txt", b"aaa", MODE_BLOB),
                ("b.txt", b"bbb", MODE_BLOB),
                ("del.txt", b"gone", MODE_BLOB),
                ("src/main.rs", b"fn main", MODE_BLOB),
            ],
        );
        let new = index_with(
            &repo,
            &[
                ("a.txt", b"aaa", MODE_BLOB),           // 不变
                ("b.txt", b"BBBB", MODE_BLOB),          // 修改
                ("src/main.rs", b"fn main", MODE_BLOB), // 不变
                ("new.txt", b"new", MODE_BLOB),         // 新增
                ("x/y.txt", b"deep", MODE_BLOB),        // 新增（深层）
            ],
        );
        let changes = diff_trees(&repo.store().unwrap(), Some(old), Some(new)).unwrap();
        let summary: Vec<(String, ChangeKind)> =
            changes.iter().map(|c| (c.path.clone(), c.kind)).collect();
        assert_eq!(
            summary,
            vec![
                ("b.txt".to_string(), ChangeKind::Modified),
                ("del.txt".to_string(), ChangeKind::Deleted),
                ("new.txt".to_string(), ChangeKind::Added),
                ("x/y.txt".to_string(), ChangeKind::Added),
            ]
        );
        let s = stat(&changes);
        assert_eq!(
            s,
            DiffStat {
                added: 2,
                modified: 1,
                deleted: 1,
                type_changed: 0
            }
        );
    }

    #[test]
    fn diff_type_changed() {
        let (_d, repo) = setup();
        #[cfg(unix)]
        {
            // exec 位变化（oid 相同、mode 不同）→ git 语义为 Modified
            let old = index_with(&repo, &[("run.sh", b"#!/bin/sh", MODE_BLOB)]);
            let new = index_with(&repo, &[("run.sh", b"#!/bin/sh", MODE_BLOB_EXEC)]);
            let changes = diff_trees(&repo.store().unwrap(), Some(old), Some(new)).unwrap();
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].kind, ChangeKind::Modified);
        }

        // 真正的类型变化：普通文件 → symlink
        let store = repo.store().unwrap();
        let link_blob = store.write(&Object::Blob(b"target.txt".to_vec())).unwrap();
        let old2 = index_with(&repo, &[("lnk", b"whatever", MODE_BLOB)]);
        let new2 = store
            .write(&Object::Tree(Tree::new(vec![TreeEntry {
                mode: crate::object::MODE_LINK,
                name: "lnk".into(),
                oid: link_blob,
            }])))
            .unwrap();
        let changes = diff_trees(&store, Some(old2), Some(new2)).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::TypeChanged);
        assert_eq!(stat(&changes).type_changed, 1);
    }

    #[test]
    fn diff_blob_to_tree_is_type_change() {
        let (_d, repo) = setup();
        let store = repo.store().unwrap();
        let old = index_with(&repo, &[("x", b"file", MODE_BLOB)]);
        // new: 根树的 x 变成目录，指向含 y 的子树
        let blob = store.write(&Object::Blob(b"deep".to_vec())).unwrap();
        let sub = Tree::new(vec![TreeEntry {
            mode: MODE_BLOB,
            name: "y".into(),
            oid: blob,
        }]);
        let sub_oid = store.write(&Object::Tree(sub)).unwrap();
        let new = store
            .write(&Object::Tree(Tree::new(vec![TreeEntry {
                mode: MODE_TREE,
                name: "x".into(),
                oid: sub_oid,
            }])))
            .unwrap();
        let changes = diff_trees(&store, Some(old), Some(new)).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(changes
            .iter()
            .any(|c| c.path == "x" && c.kind == ChangeKind::Deleted));
        assert!(changes
            .iter()
            .any(|c| c.path == "x/y" && c.kind == ChangeKind::Added));
    }

    #[test]
    fn diff_from_and_to_empty() {
        let (_d, repo) = setup();
        let tree = index_with(&repo, &[("a.txt", b"a", MODE_BLOB)]);
        // None → Some：全部新增
        let changes = diff_trees(&repo.store().unwrap(), None, Some(tree)).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Added);
        // Some → None：全部删除
        let changes = diff_trees(&repo.store().unwrap(), Some(tree), None).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Deleted);
        // None → None：无变更
        assert!(diff_trees(&repo.store().unwrap(), None, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn diff_identical_trees_no_changes() {
        let (_d, repo) = setup();
        let tree = index_with(
            &repo,
            &[("a.txt", b"a", MODE_BLOB), ("d/b.txt", b"b", MODE_BLOB)],
        );
        assert!(diff_trees(&repo.store().unwrap(), Some(tree), Some(tree))
            .unwrap()
            .is_empty());
    }
}
