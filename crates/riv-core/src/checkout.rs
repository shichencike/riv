//! 检出：切换分支 / 分离头指针，或从树恢复指定路径。
//!
//! 安全模型（对齐 git）：
//! - 整体切换前逐路径检查：会丢「已暂存的修改」→ `LocalChangesOverwritten`；
//!   会覆盖「未跟踪文件」→ `UntrackedOverwritten`
//! - `--force` 跳过检查直接覆盖
//! - 切换后 HEAD 指向新分支（符号引用）或提交（分离头指针）
//!
//! 路径恢复模式（`checkout [<tree-ish>] -- <paths>`）不做安全检查：
//! 用户显式指定的路径即意图覆盖。

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::index::{Index, IndexEntry};
#[cfg(unix)]
use crate::object::{Object, MODE_BLOB_EXEC, MODE_LINK, MODE_TREE};
#[cfg(not(unix))]
use crate::object::{Object, MODE_LINK, MODE_TREE};
use crate::oid::Oid;
use crate::repo::Repo;
use crate::snapshot::{check_freshness, flatten_tree, stat_info, Freshness, TreeMap};
use crate::store::ObjectStore;

#[derive(Debug, Clone, Default)]
pub struct CheckoutOptions {
    /// 只恢复这些路径（为空 = 整体切换）
    pub paths: Vec<PathBuf>,
    /// 跳过安全检查
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct CheckoutReport {
    /// 切换到的分支名（None = 分离头指针，或仅恢复路径）
    pub switched_to: Option<String>,
    /// 恢复/更新的文件（按路径排序）
    pub updated: Vec<String>,
    /// 删除的文件（按路径排序）
    pub removed: Vec<String>,
    /// 是否进入分离头指针状态
    pub detached: bool,
}

impl CheckoutReport {
    /// 是否没有发生任何工作区变化（文件未增删改）。
    pub fn is_noop(&self) -> bool {
        self.updated.is_empty() && self.removed.is_empty()
    }
}

/// 检出入口。
pub fn checkout(
    repo: &Repo,
    target: Option<&str>,
    opts: &CheckoutOptions,
) -> Result<CheckoutReport> {
    if !opts.paths.is_empty() {
        return checkout_paths(repo, target, opts);
    }
    let Some(target) = target else {
        return Err(Error::Msg("checkout: no target specified".into()));
    };

    // 分支优先（与 git 一致：同名分支胜过 oid 前缀）
    let branch_name = repo.refs().branch_oid(target)?.map(|_| target.to_string());
    let oid = match &branch_name {
        Some(b) => repo.refs().branch_oid(b)?.unwrap(),
        None => crate::revparse::resolve(repo, target)?,
    };

    // 已在目标分支上且无需变更 → 直接返回
    let current = repo.refs().current_branch()?;
    if branch_name.is_some() && current.as_deref() == branch_name.as_deref() {
        return Ok(CheckoutReport {
            switched_to: branch_name,
            updated: Vec::new(),
            removed: Vec::new(),
            detached: false,
        });
    }

    let target_tree_oid = commit_tree(repo, oid)?;
    let target_tree = flatten_tree(&repo.store()?, Some(target_tree_oid))?;

    // 安全检查
    check_safe(repo, &target_tree, opts.force)?;

    // 应用
    let mut index = repo.index()?;
    let (updated, removed) = materialize(repo, &target_tree, &mut index)?;
    repo.save_index(&index)?;

    // 更新 HEAD
    let detached = branch_name.is_none();
    match &branch_name {
        Some(b) => repo.refs().set_head_symbolic(b)?,
        None => repo.refs().set_head_detached(&oid)?,
    }

    Ok(CheckoutReport {
        switched_to: branch_name,
        updated,
        removed,
        detached,
    })
}

/// 提交的 tree oid。
fn commit_tree(repo: &Repo, oid: Oid) -> Result<Oid> {
    match repo.store()?.read(&oid)? {
        Object::Commit(c) => Ok(c.tree),
        other => Err(Error::InvalidRevision(format!(
            "{} is a {}, not a commit",
            oid.short(7),
            other.otype().as_str()
        ))),
    }
}

/// 整体切换前的安全检查：返回会被破坏的本地修改 / 未跟踪文件。
fn check_safe(repo: &Repo, target_tree: &TreeMap, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    let index = repo.index()?;
    let index_mtime = index.index_mtime_sec;
    let mut local_changes: Vec<String> = Vec::new();
    let mut untracked: Vec<String> = Vec::new();

    // 1. 目标树中的路径
    for (path, (oid, mode)) in target_tree {
        let abs = repo.workdir.join(path);
        match index.get(path) {
            Some(entry) => {
                if entry.oid != *oid || !crate::object::mode_equivalent(entry.mode, *mode) {
                    // 目标与索引不同：工作区有本地修改则不允许覆盖
                    if let Freshness::Dirty { .. } = check_freshness(entry, &abs, index_mtime)? {
                        local_changes.push(path.clone());
                    }
                }
                // 目标与索引一致：即使工作区有修改也允许切换（git 会保留携带过去）
            }
            None => {
                // 未跟踪文件挡路
                if abs.symlink_metadata().is_ok() {
                    untracked.push(path.clone());
                }
            }
        }
    }

    // 2. 当前索引有、目标树没有的路径：将被删除；有本地修改则不允许
    for entry in &index.entries {
        if !target_tree.contains_key(&entry.path) {
            let abs = repo.workdir.join(&entry.path);
            if let Freshness::Dirty { .. } = check_freshness(entry, &abs, index_mtime)? {
                local_changes.push(entry.path.clone());
            }
        }
    }

    if !local_changes.is_empty() {
        local_changes.sort();
        local_changes.dedup();
        return Err(Error::LocalChangesOverwritten(local_changes));
    }
    if !untracked.is_empty() {
        untracked.sort();
        untracked.dedup();
        return Err(Error::UntrackedOverwritten(untracked));
    }
    Ok(())
}

/// 把工作区与索引物化为目标树。返回 (更新的文件, 删除的文件)。
///
/// 两个关键性质：
/// 1. **只写变化的文件**：索引条目已与目标一致、且工作区文件经 stat 缓存
///    校验确认为干净时跳过写入。判断必须同时看索引与工作区实际状态——
///    仅凭索引一致就跳过，会在工作区文件被改动/删除时留下与目标树不符
///    的内容（status 显示 modified/deleted），那是错误的「假干净」。
/// 2. **先写后删**：所有写入成功后才执行删除。中途失败（磁盘满、权限）
///    最多留下多余文件（索引未动 → 显示为未跟踪），绝不会出现「文件已删
///    但写入失败」的半切换状态。
fn materialize(
    repo: &Repo,
    target_tree: &TreeMap,
    index: &mut Index,
) -> Result<(Vec<String>, Vec<String>)> {
    let store = repo.store()?;

    // 1. 先完整走一遍写入阶段（可能因读取失败而中止，此时未动任何文件）
    let mut updated = Vec::new();
    for (path, (oid, mode)) in target_tree {
        let abs = repo.workdir.join(path);
        let already_current = match index.get(path) {
            Some(e) if e.oid == *oid && crate::object::mode_equivalent(e.mode, *mode) => {
                // 索引与目标一致：再看工作区实际状态是否干净
                matches!(
                    check_freshness(e, &abs, index.index_mtime_sec)?,
                    Freshness::Clean
                )
            }
            _ => false,
        };
        if already_current {
            continue;
        }
        write_tree_entry(&store, &abs, oid, *mode)?;
        // 刷新索引条目（oid/mode 来自树，stat 取刚写出的文件）
        let meta = fs::symlink_metadata(&abs)?;
        let (size, sec, nsec, _) = stat_info(&abs, &meta);
        index.insert(IndexEntry {
            path: path.clone(),
            oid: *oid,
            mode: *mode,
            size,
            mtime_sec: sec,
            mtime_nsec: nsec,
            flags: 0,
        });
        updated.push(path.clone());
    }
    updated.sort();
    updated.dedup();

    // 2. 写入全部成功，才删除索引中不在目标树的文件
    let mut removed = Vec::new();
    let mut removed_parents: Vec<PathBuf> = Vec::new();
    let old_paths: Vec<String> = index.paths().map(|s| s.to_string()).collect();
    for p in old_paths {
        if !target_tree.contains_key(&p) {
            let abs = repo.workdir.join(&p);
            if abs.symlink_metadata().is_ok() {
                fs::remove_file(&abs)?;
                removed.push(p.clone());
                if let Some(parent) = abs.parent() {
                    removed_parents.push(parent.to_path_buf());
                }
            }
            index.remove(&p);
        }
    }
    removed.sort();

    // 3. 只清理因删除而可能空掉的目录（被删文件的祖先链），
    //    不动工作区里其他空目录（用户自建的 build/ 等不应被误删）。
    prune_dirs(&repo.workdir, removed_parents)?;
    Ok((updated, removed))
}

/// 路径恢复模式：`checkout [<tree-ish>] -- <paths>`。
///
/// 恢复源语义（对齐 git）：
/// - 无 `<tree-ish>`：从**索引**恢复（丢弃工作区未暂存修改）
/// - 有 `<tree-ish>`：从该树恢复（同时写入索引）
fn checkout_paths(
    repo: &Repo,
    target: Option<&str>,
    opts: &CheckoutOptions,
) -> Result<CheckoutReport> {
    let store = repo.store()?;
    let mut index = repo.index()?;

    // 恢复源：树（有 target）或索引（无 target）
    let tree: Option<TreeMap> = match target {
        Some(t) => {
            let oid = crate::revparse::resolve(repo, t)?;
            Some(flatten_tree(&store, Some(commit_tree(repo, oid)?))?)
        }
        None => None,
    };
    let from_index = tree.is_none();

    // 展开 paths：支持目录前缀
    let mut wanted: Vec<String> = Vec::new();
    for p in &opts.paths {
        let rel = repo.to_repo_relative(p)?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let known = if from_index {
            index.get(&rel_str).is_some()
        } else {
            tree.as_ref().unwrap().contains_key(&rel_str)
        };
        if known {
            wanted.push(rel_str);
        } else {
            // 目录前缀展开
            let prefix = format!("{rel_str}/");
            let mut matched = false;
            let keys: Vec<String> = if from_index {
                index.paths().map(|s| s.to_string()).collect()
            } else {
                tree.as_ref().unwrap().keys().cloned().collect()
            };
            for k in keys {
                if k.starts_with(&prefix) {
                    wanted.push(k);
                    matched = true;
                }
            }
            if !matched {
                return Err(Error::PathspecNoMatch(rel_str));
            }
        }
    }
    wanted.sort();
    wanted.dedup();

    let mut updated = Vec::new();
    let mut removed = Vec::new();
    for path in &wanted {
        let source: Option<(Oid, u32)> = if from_index {
            index.get(path).map(|e| (e.oid, e.mode))
        } else {
            tree.as_ref().unwrap().get(path).copied()
        };
        let abs = repo.workdir.join(path);
        match source {
            Some((oid, mode)) => {
                write_tree_entry(&store, &abs, &oid, mode)?;
                let meta = fs::symlink_metadata(&abs)?;
                let (size, sec, nsec, _) = stat_info(&abs, &meta);
                index.insert(IndexEntry {
                    path: path.clone(),
                    oid,
                    mode,
                    size,
                    mtime_sec: sec,
                    mtime_nsec: nsec,
                    flags: 0,
                });
                updated.push(path.clone());
            }
            None => {
                // 树中没有该路径：从工作区与索引中移除
                if abs.symlink_metadata().is_ok() {
                    fs::remove_file(&abs)?;
                }
                index.remove(path);
                removed.push(path.clone());
            }
        }
    }
    repo.save_index(&index)?;
    Ok(CheckoutReport {
        switched_to: None,
        updated,
        removed,
        detached: false,
    })
}

/// 把树条目写入工作区（原子写；symlink / 可执行位按 mode 恢复）。
fn write_tree_entry(store: &ObjectStore, abs: &Path, oid: &Oid, mode: u32) -> Result<()> {
    if let Some(parent) = abs.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    // 先移除已存在的文件/符号链接，保证类型切换（如 file → symlink）正确
    if abs.symlink_metadata().is_ok() {
        fs::remove_file(abs)?;
    }
    if mode == MODE_LINK {
        let target_bytes = store.read_blob(oid)?;
        let target_str = String::from_utf8(target_bytes)
            .map_err(|_| Error::CorruptObject(format!("{}: bad symlink target", oid.short(7))))?;
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target_str, abs)?;
        }
        #[cfg(not(unix))]
        {
            fs::write(abs, target_str)?;
        }
        return Ok(());
    }
    let data = store.read_blob(oid)?;
    atomic_write(abs, &data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = if mode == MODE_BLOB_EXEC { 0o755 } else { 0o644 };
        fs::set_permissions(abs, fs::Permissions::from_mode(perm))?;
    }
    let _ = MODE_TREE; // tree 条目不会走到这里
    Ok(())
}

/// 原子写：同目录 tmp + rename。
fn atomic_write(abs: &Path, data: &[u8]) -> Result<()> {
    let tmp = abs.with_file_name(format!(
        ".{}.rivtmp",
        abs.file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "blob".into())
    ));
    fs::write(&tmp, data)?;
    fs::rename(&tmp, abs)?;
    Ok(())
}

/// 清理一批「可能因文件删除而空掉的目录」：从每个目录逐级向上，
/// 空则删、非空即停；永不触碰仓库根与 `.riv`。
/// 相比全工作区扫描，只动与本次删除相关的目录，不会误删用户自建的空目录。
fn prune_dirs(root: &Path, candidates: Vec<PathBuf>) -> Result<()> {
    // 去重 + 按路径长度降序（先处理更深的目录，父目录是否为空由子目录决定）
    let mut dirs: Vec<PathBuf> = candidates.into_iter().collect();
    dirs.sort_by(|a, b| b.cmp(a));
    dirs.dedup();
    for dir in dirs {
        let mut cur = dir.as_path();
        // 逐级上溯：目录为空才删除并继续检查其父目录
        while cur != root && cur.starts_with(root) {
            let is_empty = fs::read_dir(cur)
                .map(|mut it| it.next().is_none())
                .unwrap_or(false);
            if !is_empty {
                break;
            }
            if fs::remove_dir(cur).is_err() {
                break;
            }
            match cur.parent() {
                Some(p) => cur = p,
                None => break,
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::{commit, CommitOptions};
    use crate::index::IndexEntry;
    use crate::object::{Commit, Signature, MODE_BLOB};
    use crate::snapshot::{add, build_tree_from_index, AddOptions};
    use crate::status::status;

    fn setup() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        (dir, repo)
    }

    /// 在 repo 的工作区写入文件并提交，返回提交 oid。
    fn commit_workdir(repo: &Repo, files: &[(&str, &[u8])], msg: &str) -> Oid {
        let dir = repo.workdir.clone();
        for (path, content) in files {
            let abs = dir.join(path);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&abs, content).unwrap();
        }
        // 删除不在 files 中的已跟踪文件
        let index = repo.index().unwrap();
        let tracked: Vec<String> = index.paths().map(|s| s.to_string()).collect();
        for p in tracked {
            if !files.iter().any(|(fp, _)| *fp == p) {
                let _ = fs::remove_file(dir.join(&p));
            }
        }
        add(repo, &AddOptions::default()).unwrap();
        commit(
            repo,
            &CommitOptions {
                message: msg.into(),
                ..Default::default()
            },
        )
        .unwrap()
        .oid
    }

    fn write_commit(repo: &Repo, files: &[(&str, &[u8])], parents: Vec<Oid>, msg: &str) -> Oid {
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
        let tree = build_tree_from_index(&store, &index).unwrap();
        let sig = Signature {
            name: "t".into(),
            email: "t@t".into(),
            seconds: 1_700_000_000,
            tz_offset_minutes: 0,
        };
        store
            .write(&Object::Commit(Commit {
                tree,
                parents,
                author: sig.clone(),
                committer: sig,
                message: msg.into(),
            }))
            .unwrap()
    }

    #[test]
    fn switch_branch_updates_workdir_and_head() {
        let (dir, repo) = setup();
        let c1 = commit_workdir(&repo, &[("a.txt", b"v1"), ("gone.txt", b"bye")], "c1");
        // 第二条提交：删 gone.txt、改 a.txt、加 b.txt
        let c2 = commit_workdir(&repo, &[("a.txt", b"v2"), ("b.txt", b"new")], "c2");
        // 从 c1 建分支
        repo.refs().set_branch("old", &c1).unwrap();
        // 回到 c1 的内容
        checkout(&repo, Some("old"), &CheckoutOptions::default()).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "v1");
        assert!(dir.path().join("gone.txt").exists());
        assert!(!dir.path().join("b.txt").exists());
        assert_eq!(repo.refs().current_branch().unwrap(), Some("old".into()));
        // 状态干净
        assert!(status(&repo).unwrap().is_clean());
        // 切回 main
        let report = checkout(&repo, Some("main"), &CheckoutOptions::default()).unwrap();
        assert_eq!(report.switched_to, Some("main".to_string()));
        assert!(!report.detached);
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "v2");
        assert!(dir.path().join("b.txt").exists());
        assert!(!dir.path().join("gone.txt").exists());
        assert_eq!(repo.refs().resolve_head().unwrap(), Some(c2));
        assert!(status(&repo).unwrap().is_clean());
    }

    #[test]
    fn local_changes_block_switch() {
        let (dir, repo) = setup();
        let c1 = commit_workdir(&repo, &[("a.txt", b"v1")], "c1");
        let c2 = commit_workdir(&repo, &[("a.txt", b"v2")], "c2");
        repo.refs().set_branch("old", &c1).unwrap();
        // 制造本地修改
        fs::write(dir.path().join("a.txt"), b"dirty").unwrap();
        let err = checkout(&repo, Some("old"), &CheckoutOptions::default()).unwrap_err();
        match err {
            Error::LocalChangesOverwritten(paths) => assert_eq!(paths, vec!["a.txt".to_string()]),
            other => panic!("unexpected error: {other:?}"),
        }
        // 工作区未被触碰
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "dirty"
        );
        assert_eq!(repo.refs().resolve_head().unwrap(), Some(c2));
        // force 强制切换
        let report = checkout(
            &repo,
            Some("old"),
            &CheckoutOptions {
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.switched_to, Some("old".to_string()));
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "v1");
        assert!(status(&repo).unwrap().is_clean());
    }

    #[test]
    fn untracked_file_blocks_switch() {
        let (dir, repo) = setup();
        let c1 = commit_workdir(&repo, &[("a.txt", b"v1")], "c1");
        // 新分支有 b.txt
        let c2 = write_commit(
            &repo,
            &[("a.txt", b"v1"), ("b.txt", b"new")],
            vec![c1],
            "c2",
        );
        repo.refs().set_branch("main", &c2).unwrap();
        repo.refs().set_branch("old", &c1).unwrap();
        // 工作区有一个未跟踪的 b.txt
        fs::write(dir.path().join("b.txt"), b"untracked").unwrap();
        // 切到 old：b.txt 不在 old 的树中 → 允许切换，未跟踪文件保留（git 语义）
        let report = checkout(&repo, Some("old"), &CheckoutOptions::default()).unwrap();
        assert_eq!(report.switched_to, Some("old".to_string()));
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "untracked"
        );
        // 从 old 切到 main：main 有 b.txt 且工作区中是未跟踪文件 → 被挡
        let err = checkout(&repo, Some("main"), &CheckoutOptions::default()).unwrap_err();
        match err {
            Error::UntrackedOverwritten(paths) => assert_eq!(paths, vec!["b.txt".to_string()]),
            other => panic!("unexpected error: {other:?}"),
        }
        // 未跟踪文件保留，HEAD 未动
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "untracked"
        );
        assert_eq!(repo.refs().current_branch().unwrap(), Some("old".into()));
    }

    #[test]
    fn detached_checkout() {
        let (dir, repo) = setup();
        let c1 = commit_workdir(&repo, &[("a.txt", b"v1")], "c1");
        let c2 = commit_workdir(&repo, &[("a.txt", b"v2")], "c2");
        // 分离头指针检出 c1
        let report = checkout(&repo, Some(&c1.to_hex()), &CheckoutOptions::default()).unwrap();
        assert!(report.detached);
        assert_eq!(report.switched_to, None);
        assert_eq!(repo.refs().current_branch().unwrap(), None);
        assert_eq!(repo.refs().resolve_head().unwrap(), Some(c1));
        // main 分支未被移动
        assert_eq!(repo.refs().branch_oid("main").unwrap(), Some(c2));
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "v1");
        assert!(status(&repo).unwrap().is_clean());
    }

    #[test]
    fn checkout_paths_restores_file() {
        let (dir, repo) = setup();
        commit_workdir(&repo, &[("a.txt", b"v1"), ("b.txt", b"v1b")], "c1");
        // 破坏工作区
        fs::write(dir.path().join("a.txt"), b"dirty").unwrap();
        fs::remove_file(dir.path().join("b.txt")).unwrap();
        // 只恢复 a.txt
        let report = checkout(
            &repo,
            None,
            &CheckoutOptions {
                paths: vec![dir.path().join("a.txt")],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.updated, vec!["a.txt".to_string()]);
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "v1");
        // b.txt 仍缺失（未请求恢复），但索引还在 → 状态为 deleted
        let s = status(&repo).unwrap();
        assert!(s
            .unstaged
            .iter()
            .any(|(p, k)| p == "b.txt" && *k == crate::status::UnstagedStatus::Deleted));
    }

    #[test]
    fn checkout_paths_from_tree_ish() {
        let (dir, repo) = setup();
        let c1 = commit_workdir(&repo, &[("a.txt", b"v1")], "c1");
        let c2 = commit_workdir(&repo, &[("a.txt", b"v2")], "c2");
        // 从 c1 恢复（HEAD 仍在 main/c2）
        let report = checkout(
            &repo,
            Some(&c1.to_hex()),
            &CheckoutOptions {
                paths: vec![dir.path().join("a.txt")],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.updated, vec!["a.txt".to_string()]);
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "v1");
        // HEAD 未动
        assert_eq!(repo.refs().resolve_head().unwrap(), Some(c2));
        // 索引被改成 c1 的内容 → staged modified
        let s = status(&repo).unwrap();
        assert!(s
            .staged
            .iter()
            .any(|(p, k)| p == "a.txt" && *k == crate::status::StagedStatus::Modified));
    }

    #[test]
    fn checkout_paths_directory_prefix() {
        let (dir, repo) = setup();
        commit_workdir(
            &repo,
            &[
                ("src/a.rs", b"a"),
                ("src/deep/b.rs", b"b"),
                ("top.txt", b"t"),
            ],
            "c1",
        );
        fs::write(dir.path().join("src/a.rs"), b"dirty").unwrap();
        fs::remove_file(dir.path().join("src/deep/b.rs")).unwrap();
        let report = checkout(
            &repo,
            None,
            &CheckoutOptions {
                paths: vec![dir.path().join("src")],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            report.updated,
            vec!["src/a.rs".to_string(), "src/deep/b.rs".to_string()]
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/a.rs")).unwrap(),
            "a"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/deep/b.rs")).unwrap(),
            "b"
        );
        assert!(status(&repo).unwrap().is_clean());
    }

    #[test]
    fn checkout_paths_no_match_errors() {
        let (_d, repo) = setup();
        commit_workdir(&repo, &[("a.txt", b"v1")], "c1");
        let err = checkout(
            &repo,
            None,
            &CheckoutOptions {
                paths: vec![repo.workdir.join("nope.txt")],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::PathspecNoMatch(_)));
    }

    #[test]
    fn already_on_branch_is_noop() {
        let (dir, repo) = setup();
        commit_workdir(&repo, &[("a.txt", b"v1")], "c1");
        let report = checkout(&repo, Some("main"), &CheckoutOptions::default()).unwrap();
        assert_eq!(report.switched_to, Some("main".to_string()));
        assert!(report.updated.is_empty());
        assert!(report.is_noop());
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "v1");
    }

    #[test]
    #[cfg(unix)] // exec 位是 Unix 文件系统特性
    fn exec_bit_restored() {
        let (dir, repo) = setup();
        let c1 = commit_workdir(&repo, &[("run.sh", b"#!/bin/sh\n")], "c1");
        // 设为可执行并提交
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path().join("run.sh"), fs::Permissions::from_mode(0o755))
                .unwrap();
            add(&repo, &AddOptions::default()).unwrap();
            let c2 = commit(
                &repo,
                &CommitOptions {
                    message: "exec".into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .oid;
            // 建一个没有 exec 位的分支
            let c_plain = write_commit(&repo, &[("run.sh", b"#!/bin/sh\n")], vec![c1], "plain");
            repo.refs().set_branch("plain", &c_plain).unwrap();
            repo.refs().set_branch("main", &c2).unwrap();
            // 切到 plain：exec 位被清掉
            checkout(&repo, Some("plain"), &CheckoutOptions::default()).unwrap();
            let mode = fs::metadata(dir.path().join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0);
            // 切回 main：exec 位恢复
            checkout(&repo, Some("main"), &CheckoutOptions::default()).unwrap();
            let mode = fs::metadata(dir.path().join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0);
            assert!(status(&repo).unwrap().is_clean());
        }
    }

    #[test]
    #[cfg(unix)] // 符号链接创建需要 Unix 权限模型
    fn symlink_restored() {
        let (dir, repo) = setup();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("a.txt", dir.path().join("link")).unwrap();
            fs::write(dir.path().join("a.txt"), b"target").unwrap();
            add(&repo, &AddOptions::default()).unwrap();
            commit(
                &repo,
                &CommitOptions {
                    message: "link".into(),
                    ..Default::default()
                },
            )
            .unwrap();
            // 删除 link，从 HEAD 恢复
            fs::remove_file(dir.path().join("link")).unwrap();
            let report = checkout(
                &repo,
                None,
                &CheckoutOptions {
                    paths: vec![dir.path().join("link")],
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(report.updated, vec!["link".to_string()]);
            let meta = fs::symlink_metadata(dir.path().join("link")).unwrap();
            assert!(meta.file_type().is_symlink());
            assert_eq!(
                fs::read_link(dir.path().join("link")).unwrap(),
                PathBuf::from("a.txt")
            );
            assert!(status(&repo).unwrap().is_clean());
        }
    }

    #[test]
    fn unchanged_files_are_not_rewritten() {
        // 切分支时，两分支间内容一致的文件不应被重写：
        // - 报告里不出现在 updated
        // - 文件 mtime 不变（没有被碰过的证据）
        let (dir, repo) = setup();
        let c1 = commit_workdir(&repo, &[("a.txt", b"v1"), ("shared.txt", b"same")], "c1");
        let _c2 = commit_workdir(&repo, &[("a.txt", b"v2"), ("shared.txt", b"same")], "c2");
        repo.refs().set_branch("old", &c1).unwrap();
        let shared = dir.path().join("shared.txt");
        let mtime_before = fs::metadata(&shared).unwrap().modified().unwrap();
        let report = checkout(&repo, Some("old"), &CheckoutOptions::default()).unwrap();
        // 只有真正变化的 a.txt 被更新；shared.txt 被跳过
        assert_eq!(report.updated, vec!["a.txt".to_string()]);
        let mtime_after = fs::metadata(&shared).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "一致文件不应被重写");
        assert!(status(&repo).unwrap().is_clean());
        // 切回 main 同样跳过 shared.txt
        let report = checkout(&repo, Some("main"), &CheckoutOptions::default()).unwrap();
        assert_eq!(report.updated, vec!["a.txt".to_string()]);
        assert_eq!(
            fs::metadata(&shared).unwrap().modified().unwrap(),
            mtime_before
        );
    }

    #[test]
    fn dirty_unchanged_file_is_restored_not_skipped() {
        // 正确性陷阱的对应用例：索引与目标一致，但工作区文件被改脏。
        // 此时「只比索引」会错误地跳过写入，留下与目标树不符的内容；
        // 实现比的是工作区实际状态 → 脏文件必须被目标内容覆盖。
        let (dir, repo) = setup();
        let c1 = commit_workdir(&repo, &[("a.txt", b"v1"), ("keep.txt", b"k")], "c1");
        let _c2 = commit_workdir(&repo, &[("a.txt", b"v2"), ("keep.txt", b"k")], "c2");
        repo.refs().set_branch("old", &c1).unwrap();
        // keep.txt 在两分支间一致，但工作区里被改脏
        fs::write(dir.path().join("keep.txt"), b"dirty").unwrap();
        let report = checkout(&repo, Some("old"), &CheckoutOptions::default()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
            "k",
            "脏文件必须被恢复为目标内容"
        );
        assert!(report.updated.contains(&"keep.txt".to_string()));
        assert!(status(&repo).unwrap().is_clean());
    }

    #[test]
    fn prune_only_removes_dirs_emptied_by_deletion() {
        // prune 只清理因本次删除而空掉的目录；用户自建的空目录不受影响
        let (dir, repo) = setup();
        let c1 = commit_workdir(&repo, &[("a.txt", b"v1")], "c1");
        let _c2 = commit_workdir(&repo, &[("a.txt", b"v1"), ("gone/deep/f.txt", b"x")], "c2");
        repo.refs().set_branch("old", &c1).unwrap();
        // 用户自建的空目录（未被跟踪）
        fs::create_dir_all(dir.path().join("user_empty")).unwrap();
        fs::create_dir_all(dir.path().join("user_empty/nested")).unwrap();
        // 切到 old：gone/deep/f.txt 被删 → gone/deep 与 gone 变空应被清理
        checkout(&repo, Some("old"), &CheckoutOptions::default()).unwrap();
        assert!(!dir.path().join("gone").exists(), "空目录链应被清理");
        assert!(
            dir.path().join("user_empty/nested").is_dir(),
            "与本次删除无关的空目录不应被误删"
        );
        assert!(status(&repo).unwrap().is_clean());
    }

    #[test]
    fn checkout_missing_branch_errors() {
        let (_d, repo) = setup();
        let err = checkout(&repo, Some("ghost"), &CheckoutOptions::default()).unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn checkout_no_target_errors() {
        let (_d, repo) = setup();
        assert!(checkout(&repo, None, &CheckoutOptions::default()).is_err());
    }
}
