//! 高层分支操作：创建 / 列表 / 删除 / 重命名。
//!
//! 底层文件读写委托 `refs::RefStore`，本模块附加语义：
//! - 删除前检查目标是否已合并到当前分支（`-d`），未合并需 `-D` 强删
//! - 重命名时同步更新 HEAD（若指向被改名的分支）

use crate::error::{Error, Result};
use crate::log::{is_ancestor, log};
use crate::oid::Oid;
use crate::repo::Repo;

#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub is_current: bool,
    pub oid: Oid,
    /// 提交摘要（首行）
    pub summary: String,
}

/// 所有分支（按名称排序），标注当前分支。
pub fn list(repo: &Repo) -> Result<Vec<BranchInfo>> {
    let refs = repo.refs();
    let current = refs.current_branch()?;
    let store = repo.store()?;
    let mut out = Vec::new();
    for name in refs.branches()? {
        let Some(oid) = refs.branch_oid(&name)? else {
            continue;
        };
        let summary = match store.read(&oid)? {
            crate::object::Object::Commit(c) => c.summary().to_string(),
            _ => String::new(),
        };
        out.push(BranchInfo {
            is_current: current.as_deref() == Some(name.as_str()),
            name,
            oid,
            summary,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// 创建分支。`start` 为缺省的起始点修订表达式（`None` = HEAD）。
pub fn create(repo: &Repo, name: &str, start: Option<&str>) -> Result<Oid> {
    let oid = match start {
        Some(spec) => crate::revparse::resolve(repo, spec)?,
        None => repo
            .refs()
            .resolve_head()?
            .ok_or_else(|| Error::InvalidRevision("HEAD points to no commit".into()))?,
    };
    if repo.refs().branch_oid(name)?.is_some() {
        return Err(Error::BranchExists(name.to_string()));
    }
    repo.refs().set_branch(name, &oid)?;
    Ok(oid)
}

/// 删除分支。`force=false` 时要求目标已合并到当前 HEAD。
pub fn delete(repo: &Repo, name: &str, force: bool) -> Result<()> {
    let refs = repo.refs();
    let current = refs.current_branch()?;
    if current.as_deref() == Some(name) {
        return Err(Error::DeleteCurrentBranch(name.to_string()));
    }
    let target = refs
        .branch_oid(name)?
        .ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
    if !force {
        // 未合并检查：从当前 HEAD 可达 target 才允许删除
        let head = refs
            .resolve_head()?
            .ok_or_else(|| Error::InvalidRevision("HEAD points to no commit".into()))?;
        if !is_ancestor(repo, head, target)? {
            return Err(Error::Msg(format!(
                "branch '{name}' is not fully merged; use -D to force delete"
            )));
        }
    }
    refs.delete_branch(name)
}

/// 重命名分支；若重命名的是当前分支，同步更新 HEAD。
pub fn rename(repo: &Repo, old: &str, new: &str) -> Result<()> {
    let refs = repo.refs();
    let oid = refs
        .branch_oid(old)?
        .ok_or_else(|| Error::BranchNotFound(old.to_string()))?;
    if refs.branch_oid(new)?.is_some() {
        return Err(Error::BranchExists(new.to_string()));
    }
    refs.set_branch(new, &oid)?;
    refs.delete_branch(old)?;
    if refs.current_branch()?.as_deref() == Some(old) {
        // 删除后 HEAD 变成悬空符号引用，重新指向新名字
        refs.set_head_symbolic(new)?;
    }
    Ok(())
}

/// 当前分支最新提交的摘要（用于 prompt 等场景）。
#[allow(dead_code)]
pub fn current_summary(repo: &Repo) -> Result<Option<String>> {
    let entries = log(repo, None, Some(1))?;
    Ok(entries.first().map(|e| e.commit.summary().to_string()))
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

    fn make_commit(repo: &Repo, msg: &str, parents: Vec<Oid>) -> Oid {
        let store = repo.store().unwrap();
        let mut index = Index::new();
        let blob = store
            .write(&crate::object::Object::Blob(msg.as_bytes().to_vec()))
            .unwrap();
        index.insert(IndexEntry {
            path: "f.txt".into(),
            oid: blob,
            mode: MODE_BLOB,
            size: msg.len() as u64,
            mtime_sec: 0,
            mtime_nsec: 0,
            flags: 0,
        });
        let tree = build_tree_from_index(&store, &index).unwrap();
        let sig = Signature {
            name: "t".into(),
            email: "t@t".into(),
            seconds: 1_700_000_000,
            tz_offset_minutes: 0,
        };
        store
            .write(&crate::object::Object::Commit(crate::object::Commit {
                tree,
                parents,
                author: sig.clone(),
                committer: sig,
                message: msg.into(),
            }))
            .unwrap()
    }

    #[test]
    fn create_and_list() {
        let (_d, repo) = setup();
        let c1 = make_commit(&repo, "c1", vec![]);
        repo.refs().set_branch("main", &c1).unwrap();
        // 从 HEAD 创建
        create(&repo, "feature", None).unwrap();
        // 从指定起点创建
        create(&repo, "from-c1", Some("HEAD")).unwrap();
        let branches = list(&repo).unwrap();
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, vec!["feature", "from-c1", "main"]);
        assert!(branches.iter().any(|b| b.name == "main" && b.is_current));
        assert!(branches.iter().all(|b| b.oid == c1 && b.summary == "c1"));
        // 重名报错
        assert!(matches!(
            create(&repo, "feature", None).unwrap_err(),
            Error::BranchExists(_)
        ));
    }

    #[test]
    fn create_from_start_point() {
        let (_d, repo) = setup();
        let c1 = make_commit(&repo, "c1", vec![]);
        let c2 = make_commit(&repo, "c2", vec![c1]);
        repo.refs().set_branch("main", &c2).unwrap();
        // 从 c1 创建（HEAD~1）
        let oid = create(&repo, "old", Some("HEAD~1")).unwrap();
        assert_eq!(oid, c1);
        assert_eq!(repo.refs().branch_oid("old").unwrap(), Some(c1));
    }

    #[test]
    fn delete_merged_and_unmerged() {
        let (_d, repo) = setup();
        let c1 = make_commit(&repo, "c1", vec![]);
        let c2 = make_commit(&repo, "c2", vec![c1]);
        repo.refs().set_branch("main", &c2).unwrap();
        // merged：分支指向 c1（HEAD 的祖先）→ 可删
        repo.refs().set_branch("merged", &c1).unwrap();
        delete(&repo, "merged", false).unwrap();
        assert!(repo.refs().branch_oid("merged").unwrap().is_none());
        // unmerged：独立分支 → 拒绝
        let other = make_commit(&repo, "other", vec![]);
        repo.refs().set_branch("wip", &other).unwrap();
        assert!(delete(&repo, "wip", false).is_err());
        // 强删可以
        delete(&repo, "wip", true).unwrap();
        assert!(repo.refs().branch_oid("wip").unwrap().is_none());
    }

    #[test]
    fn delete_current_branch_rejected() {
        let (_d, repo) = setup();
        let c1 = make_commit(&repo, "c1", vec![]);
        repo.refs().set_branch("main", &c1).unwrap();
        assert!(matches!(
            delete(&repo, "main", true).unwrap_err(),
            Error::DeleteCurrentBranch(_)
        ));
        // 不存在的分支
        assert!(matches!(
            delete(&repo, "ghost", false).unwrap_err(),
            Error::BranchNotFound(_)
        ));
    }

    #[test]
    fn rename_branch() {
        let (_d, repo) = setup();
        let c1 = make_commit(&repo, "c1", vec![]);
        repo.refs().set_branch("main", &c1).unwrap();
        // 重命名当前分支：HEAD 跟随
        rename(&repo, "main", "trunk").unwrap();
        assert!(repo.refs().branch_oid("main").unwrap().is_none());
        assert_eq!(repo.refs().branch_oid("trunk").unwrap(), Some(c1));
        assert_eq!(repo.refs().current_branch().unwrap(), Some("trunk".into()));
        // 重命名非当前分支
        repo.refs().set_branch("other", &c1).unwrap();
        rename(&repo, "other", "renamed").unwrap();
        assert_eq!(repo.refs().current_branch().unwrap(), Some("trunk".into()));
        assert_eq!(repo.refs().branch_oid("renamed").unwrap(), Some(c1));
        // 目标已存在
        assert!(matches!(
            rename(&repo, "trunk", "renamed").unwrap_err(),
            Error::BranchExists(_)
        ));
    }

    #[test]
    fn create_on_empty_repo_fails() {
        let (_d, repo) = setup();
        assert!(create(&repo, "x", None).is_err());
    }
}
