//! 提交：把当前索引固化为树 + 提交对象，并前进 HEAD。
//!
//! 与 git 的对应关系：
//! - 树的构建委托 `snapshot::build_tree_from_index`（自底向上，子树去重）
//! - `allow_empty` 对齐 `git commit --allow-empty`
//! - 作者/提交者默认取配置中的 `user.name` / `user.email`

use crate::error::{Error, Result};
use crate::object::{Commit, Object, Signature};
use crate::oid::Oid;
use crate::repo::Repo;
use crate::snapshot::build_tree_from_index;

#[derive(Debug, Clone, Default)]
pub struct CommitOptions {
    pub message: String,
    /// 索引与 HEAD 树一致时也允许提交
    pub allow_empty: bool,
    /// 自定义作者署名（缺省用仓库配置）
    pub author: Option<Signature>,
}

#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub oid: Oid,
    pub commit: Commit,
    /// 父提交数（0 = 根提交）
    pub parents: usize,
}

/// 清理提交信息：去掉行尾空白、压缩连续空行、去掉首尾空行。
/// `#` 注释行的剥离由 CLI 层（编辑器模式）负责。
pub fn clean_message(raw: &str) -> String {
    let raw = raw.trim_start();
    let mut out: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            // 延迟压紧空行
            if out.last().map(|l| l.is_empty()).unwrap_or(true) {
                continue;
            }
            out.push("");
        } else {
            out.push(trimmed);
        }
    }
    while out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}

/// 执行提交。
pub fn commit(repo: &Repo, opts: &CommitOptions) -> Result<CommitOutcome> {
    let message = clean_message(&opts.message);
    if message.is_empty() {
        return Err(Error::Msg("empty commit message".into()));
    }

    let store = repo.store()?;
    let index = repo.index()?;
    let new_tree = build_tree_from_index(&store, &index)?;

    let parents: Vec<Oid> = match repo.head_commit()? {
        Some((oid, _)) => vec![oid],
        None => Vec::new(),
    };
    let old_tree = repo.head_tree()?;

    if !opts.allow_empty && old_tree == Some(new_tree) {
        return Err(Error::NothingToCommit);
    }

    let sig = match &opts.author {
        Some(s) => s.clone(),
        None => repo.repo_config()?.signature_now(),
    };

    let commit = Commit {
        tree: new_tree,
        parents: parents.clone(),
        author: sig.clone(),
        committer: sig,
        message,
    };
    let oid = store.write(&Object::Commit(commit.clone()))?;
    repo.refs().advance_head(&oid)?;

    Ok(CommitOutcome {
        oid,
        commit,
        parents: parents.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::MODE_BLOB;
    use crate::snapshot::AddOptions;
    use crate::status::status;
    use std::fs;

    fn setup() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        (dir, repo)
    }

    #[test]
    fn clean_message_rules() {
        assert_eq!(clean_message("  hi  \n\n\nthere\n\n"), "hi\n\nthere");
        assert_eq!(clean_message("\n\nonly\n"), "only");
        assert_eq!(clean_message("   \n  \n"), "");
    }

    #[test]
    fn root_commit() {
        let (dir, repo) = setup();
        fs::write(dir.path().join("a.txt"), b"aaa").unwrap();
        crate::snapshot::add(&repo, &AddOptions::default()).unwrap();
        let out = commit(
            &repo,
            &CommitOptions {
                message: "initial".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(out.parents, 0);
        assert_eq!(out.commit.summary(), "initial");
        // HEAD 前进到新提交
        let (head_oid, head_commit) = repo.head_commit().unwrap().unwrap();
        assert_eq!(head_oid, out.oid);
        assert_eq!(head_commit.tree, out.commit.tree);
        // 树内容正确
        let flat =
            crate::snapshot::flatten_tree(&repo.store().unwrap(), Some(out.commit.tree)).unwrap();
        assert_eq!(flat.len(), 1);
        assert_eq!(flat["a.txt"].1, MODE_BLOB);
        // 提交后 status 干净
        let s = status(&repo).unwrap();
        assert!(s.is_clean(), "{:?}", crate::status::porcelain(&s));
    }

    #[test]
    fn second_commit_has_parent() {
        let (dir, repo) = setup();
        fs::write(dir.path().join("a.txt"), b"aaa").unwrap();
        crate::snapshot::add(&repo, &AddOptions::default()).unwrap();
        let first = commit(
            &repo,
            &CommitOptions {
                message: "c1".into(),
                ..Default::default()
            },
        )
        .unwrap();
        fs::write(dir.path().join("a.txt"), b"aaaa").unwrap();
        crate::snapshot::add(&repo, &AddOptions::default()).unwrap();
        let second = commit(
            &repo,
            &CommitOptions {
                message: "c2".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(second.parents, 1);
        assert_eq!(second.commit.parents, vec![first.oid]);
        assert_ne!(second.commit.tree, first.commit.tree);
    }

    #[test]
    fn empty_commit_rejected_then_allowed() {
        let (dir, repo) = setup();
        fs::write(dir.path().join("a.txt"), b"aaa").unwrap();
        crate::snapshot::add(&repo, &AddOptions::default()).unwrap();
        commit(
            &repo,
            &CommitOptions {
                message: "c1".into(),
                ..Default::default()
            },
        )
        .unwrap();
        // 无变化 → NothingToCommit
        let err = commit(
            &repo,
            &CommitOptions {
                message: "c2".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::NothingToCommit));
        // allow_empty
        let out = commit(
            &repo,
            &CommitOptions {
                message: "empty".into(),
                allow_empty: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(out.commit.parents.len(), 1);
        assert_eq!(out.commit.tree, repo.head_tree().unwrap().unwrap());
    }

    #[test]
    fn empty_message_rejected() {
        let (_d, repo) = setup();
        let err = commit(
            &repo,
            &CommitOptions {
                message: "  \n  ".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty commit message"));
    }

    #[test]
    fn commit_empty_repo_with_no_files() {
        // 索引为空且无 HEAD：树为空树，允许提交（git 也允许 --allow-empty 的空根提交）
        let (_d, repo) = setup();
        let out = commit(
            &repo,
            &CommitOptions {
                message: "root".into(),
                allow_empty: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(out.parents, 0);
        let flat =
            crate::snapshot::flatten_tree(&repo.store().unwrap(), Some(out.commit.tree)).unwrap();
        assert!(flat.is_empty());
        // 空树对象确实存在
        assert!(repo.store().unwrap().contains(&out.commit.tree));
    }

    #[test]
    fn custom_author_signature() {
        let (dir, repo) = setup();
        fs::write(dir.path().join("f.txt"), b"x").unwrap();
        crate::snapshot::add(&repo, &AddOptions::default()).unwrap();
        let sig = Signature {
            name: "Alice".into(),
            email: "alice@example.com".into(),
            seconds: 1_600_000_000,
            tz_offset_minutes: 480,
        };
        let out = commit(
            &repo,
            &CommitOptions {
                message: "by alice".into(),
                author: Some(sig.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(out.commit.author, sig);
        assert_eq!(out.commit.committer, sig);
        // 1_600_000_000 = 2020-09-13 12:26:40 UTC，+0800 本地时间为 20:26:40
        assert_eq!(out.commit.author.format_time(), "2020-09-13 20:26:40 +0800");
    }

    #[test]
    fn commit_uses_nested_tree_structure() {
        let (dir, repo) = setup();
        fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        fs::write(dir.path().join("src/deep/mod.rs"), b"mod").unwrap();
        fs::write(dir.path().join("README.md"), b"readme").unwrap();
        crate::snapshot::add(&repo, &AddOptions::default()).unwrap();
        let out = commit(
            &repo,
            &CommitOptions {
                message: "nested".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let store = repo.store().unwrap();
        let tree = store
            .read(&out.commit.tree)
            .unwrap()
            .as_tree()
            .cloned()
            .unwrap();
        assert!(tree.is_dir("src"));
        assert_eq!(tree.get("README.md").unwrap().mode, MODE_BLOB);
    }
}
