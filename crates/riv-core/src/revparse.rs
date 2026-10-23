//! 修订表达式解析（revparse）：把用户输入的字符串解析为提交 oid。
//!
//! 支持：
//! - `HEAD`：当前 HEAD 指向的提交
//! - 分支名：`refs/heads/<name>` 指向的提交
//! - oid：完整 64 位十六进制，或 ≥4 位的唯一前缀
//! - 导航后缀（可叠加）：
//!   - `rev^` / `rev^N`：第 N 个父提交（`^` 等价 `^1`，`^0` 为自身）
//!   - `rev~N`：沿第一父链上溯 N 步（`~` 等价 `~1`，`~0` 为自身）
//!
//! 例：`HEAD~2`、`main^2`、`a1b2c3d4^`、`HEAD~3^2`。

use crate::error::{Error, Result};
use crate::object::{Commit, Object};
use crate::oid::Oid;
use crate::repo::Repo;

/// 把修订表达式解析为提交 oid。
pub fn resolve(repo: &Repo, spec: &str) -> Result<Oid> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(Error::InvalidRevision("empty revision".into()));
    }

    // 拆分 base 与导航后缀：base 是第一个 '^' 或 '~' 之前的部分。
    let (base, navs) = split_navs(spec)?;

    let mut oid = resolve_base(repo, base)?;

    for nav in navs {
        oid = apply_nav(repo, oid, nav)?;
    }
    Ok(oid)
}

/// 拆分出 base 与后缀列表，如 `HEAD~2^` → ("HEAD", [Ancestor(2), Parent(1)])。
///
/// 后缀段只允许 `^`/`~` 加可选数字；出现任何其他字符（如 `HEAD^abc`）
/// 都是非法修订表达式，报错而不是静默吞掉。
fn split_navs(spec: &str) -> Result<(&str, Vec<Nav>)> {
    let end = spec.find(['^', '~']).unwrap_or(spec.len());
    let base = &spec[..end];
    let mut navs = Vec::new();
    let mut rest = &spec[end..];
    while !rest.is_empty() {
        let kind = match rest.as_bytes()[0] {
            b'^' => NavKind::Parent,
            b'~' => NavKind::Ancestor,
            _ => {
                return Err(Error::InvalidRevision(format!(
                    "invalid revision suffix in: {spec}"
                )))
            }
        };
        rest = &rest[1..];
        // 读取数字（缺省为 1）
        let digits_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let (digits, tail) = rest.split_at(digits_end);
        let n: u32 = if digits.is_empty() {
            1
        } else {
            digits
                .parse()
                .map_err(|_| Error::InvalidRevision(format!("invalid revision: {spec}")))?
        };
        navs.push(Nav { kind, n });
        rest = tail;
    }
    Ok((base, navs))
}

enum NavKind {
    /// `^N`：第 N 个父（1-based）
    Parent,
    /// `~N`：沿第一父上溯 N 步
    Ancestor,
}

struct Nav {
    kind: NavKind,
    n: u32,
}

fn apply_nav(repo: &Repo, oid: Oid, nav: Nav) -> Result<Oid> {
    let commit = load_commit(repo, oid)?;
    match nav.kind {
        NavKind::Parent => {
            if nav.n == 0 {
                return Ok(oid);
            }
            commit
                .parents
                .get(nav.n as usize - 1)
                .copied()
                .ok_or_else(|| {
                    Error::InvalidRevision(format!(
                        "commit {} has no parent #{}",
                        oid.short(7),
                        nav.n
                    ))
                })
        }
        NavKind::Ancestor => {
            let mut cur = oid;
            for _ in 0..nav.n {
                let c = load_commit(repo, cur)?;
                cur = c.parents.first().copied().ok_or_else(|| {
                    Error::InvalidRevision(format!("commit {} has no parent", cur.short(7)))
                })?;
            }
            Ok(cur)
        }
    }
}

fn load_commit(repo: &Repo, oid: Oid) -> Result<Commit> {
    let store = repo.store()?;
    match store.read(&oid)? {
        Object::Commit(c) => Ok(c),
        other => Err(Error::InvalidRevision(format!(
            "{} is a {}, not a commit",
            oid.short(7),
            other.otype().as_str()
        ))),
    }
}

/// 解析 base 部分：HEAD / 分支名 / oid 或唯一前缀。
fn resolve_base(repo: &Repo, base: &str) -> Result<Oid> {
    let refs = repo.refs();
    if base == "HEAD" {
        return refs
            .resolve_head()?
            .ok_or_else(|| Error::InvalidRevision("HEAD points to no commit".into()));
    }
    // 纯十六进制 → oid 或前缀
    if !base.is_empty()
        && base.len() >= 4
        && base.len() <= 64
        && base.bytes().all(|b| b.is_ascii_hexdigit())
    {
        if base.len() == 64 {
            let oid: Oid = base.parse()?;
            // 确认对象存在
            let store = repo.store()?;
            if store.contains(&oid) {
                return Ok(oid);
            }
            return Err(Error::InvalidRevision(format!(
                "unknown revision or path: {base}"
            )));
        }
        return resolve_prefix(repo, base);
    }
    // 分支名
    match refs.branch_oid(base)? {
        Some(oid) => Ok(oid),
        None => Err(Error::InvalidRevision(format!(
            "unknown revision or path: {base}"
        ))),
    }
}

/// oid 前缀解析：匹配 0 个 → unknown revision；多个 → AmbiguousOid。
pub fn resolve_prefix(repo: &Repo, prefix: &str) -> Result<Oid> {
    let store = repo.store()?;
    let mut found: Option<Oid> = None;
    for oid in store.all_oids()? {
        if oid.starts_with_hex(prefix) {
            if found.is_some() {
                return Err(Error::AmbiguousOid(prefix.to_string()));
            }
            found = Some(oid);
        }
    }
    found.ok_or_else(|| Error::InvalidRevision(format!("unknown revision or path: {prefix}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;
    use crate::object::{Signature, Tree, TreeEntry, MODE_BLOB};
    use crate::snapshot::build_tree_from_index;
    use std::fs;

    fn setup() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        (dir, repo)
    }

    /// 造一条提交链 c1 <- c2 <- c3，并让 main 指向 c3。
    fn make_chain(repo: &Repo, n: usize) -> Vec<Oid> {
        let store = repo.store().unwrap();
        let mut oids = Vec::new();
        let mut parents = Vec::new();
        for i in 0..n {
            let mut index = Index::new();
            let blob = store
                .write(&Object::Blob(format!("content {i}").into_bytes()))
                .unwrap();
            index.insert(crate::index::IndexEntry {
                path: "f.txt".into(),
                oid: blob,
                mode: MODE_BLOB,
                size: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                flags: 0,
            });
            let tree = build_tree_from_index(&store, &index).unwrap();
            let sig = Signature {
                name: "t".into(),
                email: "t@t".into(),
                seconds: 1_700_000_000 + i as i64,
                tz_offset_minutes: 0,
            };
            let commit = Commit {
                tree,
                parents: parents.clone(),
                author: sig.clone(),
                committer: sig,
                message: format!("c{i}"),
            };
            let oid = store.write(&Object::Commit(commit)).unwrap();
            parents = vec![oid];
            oids.push(oid);
        }
        repo.refs()
            .set_branch("main", oids.last().unwrap())
            .unwrap();
        oids
    }

    #[test]
    fn resolve_head_and_branch() {
        let (_d, repo) = setup();
        let chain = make_chain(&repo, 3);
        assert_eq!(resolve(&repo, "HEAD").unwrap(), chain[2]);
        assert_eq!(resolve(&repo, "main").unwrap(), chain[2]);
        assert_eq!(resolve(&repo, "refs/heads/main").ok(), None); // refs/ 前缀按分支名不匹配
    }

    #[test]
    fn resolve_full_and_prefix_oid() {
        let (_d, repo) = setup();
        let chain = make_chain(&repo, 2);
        let full = chain[1].to_hex();
        assert_eq!(resolve(&repo, &full).unwrap(), chain[1]);
        let prefix = &full[..8];
        assert_eq!(resolve(&repo, prefix).unwrap(), chain[1]);
        // 不存在的前缀
        assert!(resolve(&repo, "ffffffff").is_err());
    }

    #[test]
    fn nav_parent_and_ancestor() {
        let (_d, repo) = setup();
        let chain = make_chain(&repo, 3); // c1 <- c2 <- c3
        assert_eq!(resolve(&repo, "HEAD^").unwrap(), chain[1]);
        assert_eq!(resolve(&repo, "HEAD~1").unwrap(), chain[1]);
        assert_eq!(resolve(&repo, "HEAD~2").unwrap(), chain[0]);
        assert_eq!(resolve(&repo, "HEAD^0").unwrap(), chain[2]);
        assert!(resolve(&repo, "HEAD~3").is_err());
        assert!(resolve(&repo, "HEAD^2").is_err());
    }

    #[test]
    fn multi_nav_and_merge_parent() {
        let (_d, repo) = setup();
        let store = repo.store().unwrap();
        // 造一个合并提交：parents = [c1, c2]
        let b1 = store.write(&Object::Blob(b"x".to_vec())).unwrap();
        let tree1 = Tree::new(vec![TreeEntry {
            mode: MODE_BLOB,
            name: "f".into(),
            oid: b1,
        }]);
        let t1 = store.write(&Object::Tree(tree1.clone())).unwrap();
        let sig = Signature {
            name: "t".into(),
            email: "t@t".into(),
            seconds: 1,
            tz_offset_minutes: 0,
        };
        let c1 = store
            .write(&Object::Commit(Commit {
                tree: t1,
                parents: vec![],
                author: sig.clone(),
                committer: sig.clone(),
                message: "c1".into(),
            }))
            .unwrap();
        let c2 = store
            .write(&Object::Commit(Commit {
                tree: t1,
                parents: vec![],
                author: sig.clone(),
                committer: sig.clone(),
                message: "c2".into(),
            }))
            .unwrap();
        let merge = store
            .write(&Object::Commit(Commit {
                tree: t1,
                parents: vec![c1, c2],
                author: sig.clone(),
                committer: sig,
                message: "merge".into(),
            }))
            .unwrap();
        repo.refs().set_branch("main", &merge).unwrap();
        assert_eq!(resolve(&repo, "HEAD^1").unwrap(), c1);
        assert_eq!(resolve(&repo, "HEAD^2").unwrap(), c2);
        assert_eq!(resolve(&repo, "HEAD~1").unwrap(), c1);
    }

    #[test]
    fn invalid_specs() {
        let (_d, repo) = setup();
        assert!(resolve(&repo, "").is_err());
        assert!(resolve(&repo, "nonexistent-branch").is_err());
        assert!(resolve(&repo, "abc").is_err()); // 太短的前缀
                                                 // HEAD 无提交
        assert!(resolve(&repo, "HEAD").is_err());
        let _ = fs::remove_file(repo.riv_dir.join("refs/heads/main"));
        assert!(resolve(&repo, "main").is_err());
    }

    #[test]
    fn invalid_suffix_rejected() {
        let (_d, repo) = setup();
        make_chain(&repo, 1);
        // 后缀段只接受 ^ / ~ 与数字；其他字符必须报错而非静默解析
        assert!(resolve(&repo, "HEAD^abc").is_err());
        assert!(resolve(&repo, "HEAD~x").is_err());
        assert!(resolve(&repo, "HEAD^^!").is_err());
        // 合法后缀仍然工作
        assert_eq!(
            resolve(&repo, "HEAD^0").unwrap(),
            resolve(&repo, "HEAD").unwrap()
        );
    }
}
