//! 引用管理：HEAD、分支（`refs/heads/*`）。
//!
//! 文件格式：
//! - `.riv/HEAD`：`ref: refs/heads/main\n`（符号引用）或 64 位 oid（分离头指针）
//! - `.riv/refs/heads/<name>`：64 位 oid + `\n`

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::oid::Oid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// 指向某个分支
    Symbolic(String),
    /// 直接指向某个提交
    Detached(Oid),
}

pub struct RefStore {
    riv_dir: PathBuf,
}

impl RefStore {
    pub fn new(riv_dir: &Path) -> Self {
        RefStore {
            riv_dir: riv_dir.to_path_buf(),
        }
    }

    fn head_path(&self) -> PathBuf {
        self.riv_dir.join("HEAD")
    }

    fn branch_path(&self, name: &str) -> Result<PathBuf> {
        // 所有分支路径的收口：写路径（set_branch / set_head_symbolic）本就校验，
        // 读/删路径（branch_oid / delete_branch）也在这里统一拦截非法名，
        // 防止 `../../etc/passwd` 之类把引用读写引到 .riv 之外。
        Self::validate_branch_name(name)?;
        Ok(self.riv_dir.join("refs").join("heads").join(name))
    }

    fn validate_branch_name(name: &str) -> Result<()> {
        if name.is_empty() {
            return Err(Error::InvalidRef("empty branch name".into()));
        }
        if name == "HEAD" || name.starts_with("refs/") {
            return Err(Error::InvalidRef(format!("invalid branch name: {name}")));
        }
        for bad in ["..", " ", "~", "^", ":", "?", "*", "[", "\\", "\x7f"] {
            if name.contains(bad) {
                return Err(Error::InvalidRef(format!("invalid branch name: {name}")));
            }
        }
        if name.starts_with('/') || name.ends_with('/') || name.ends_with(".lock") {
            return Err(Error::InvalidRef(format!("invalid branch name: {name}")));
        }
        Ok(())
    }

    pub fn head(&self) -> Result<Head> {
        let text = fs::read_to_string(self.head_path())?;
        let text = text.trim();
        if let Some(rest) = text.strip_prefix("ref: ") {
            let branch = rest
                .strip_prefix("refs/heads/")
                .ok_or_else(|| Error::InvalidRef(text.to_string()))?;
            Ok(Head::Symbolic(branch.to_string()))
        } else {
            Ok(Head::Detached(text.parse()?))
        }
    }

    pub fn set_head_symbolic(&self, branch: &str) -> Result<()> {
        Self::validate_branch_name(branch)?;
        fs::write(self.head_path(), format!("ref: refs/heads/{branch}\n"))?;
        Ok(())
    }

    pub fn set_head_detached(&self, oid: &Oid) -> Result<()> {
        fs::write(self.head_path(), format!("{oid}\n"))?;
        Ok(())
    }

    /// 解析 HEAD 到具体提交（分支不存在时为 None）。
    pub fn resolve_head(&self) -> Result<Option<Oid>> {
        match self.head()? {
            Head::Symbolic(b) => self.branch_oid(&b),
            Head::Detached(o) => Ok(Some(o)),
        }
    }

    pub fn current_branch(&self) -> Result<Option<String>> {
        match self.head()? {
            Head::Symbolic(b) => Ok(Some(b)),
            Head::Detached(_) => Ok(None),
        }
    }

    pub fn branches(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let dir = self.riv_dir.join("refs").join("heads");
        if dir.is_dir() {
            collect_branches(&dir, &dir, &mut out)?;
        }
        out.sort();
        Ok(out)
    }

    pub fn branch_oid(&self, name: &str) -> Result<Option<Oid>> {
        let path = self.branch_path(name)?;
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text.trim().parse()?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_branch(&self, name: &str, oid: &Oid) -> Result<()> {
        let path = self.branch_path(name)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, format!("{oid}\n"))?;
        Ok(())
    }

    pub fn delete_branch(&self, name: &str) -> Result<()> {
        let path = self.branch_path(name)?;
        fs::remove_file(&path).map_err(|_| Error::BranchNotFound(name.to_string()))?;
        // 清理可能空掉的父目录
        let mut dir = path.parent().map(|p| p.to_path_buf());
        while let Some(d) = dir {
            if d == self.riv_dir.join("refs").join("heads")
                || !d.starts_with(self.riv_dir.join("refs"))
            {
                break;
            }
            if fs::read_dir(&d)
                .map(|mut it| it.next().is_none())
                .unwrap_or(false)
            {
                let _ = fs::remove_dir(&d);
                dir = d.parent().map(|p| p.to_path_buf());
            } else {
                break;
            }
        }
        Ok(())
    }

    /// 提交后更新 HEAD：符号引用则更新分支，分离头指针则直接更新 HEAD。
    pub fn advance_head(&self, oid: &Oid) -> Result<()> {
        match self.head()? {
            Head::Symbolic(b) => self.set_branch(&b, oid),
            Head::Detached(_) => self.set_head_detached(oid),
        }
    }
}

fn collect_branches(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_branches(root, &path, out)?;
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                let name = rel.to_string_lossy().replace('\\', "/");
                if !name.is_empty() {
                    out.push(name);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, RefStore) {
        let dir = tempfile::tempdir().unwrap();
        let riv = dir.path().join(".riv");
        fs::create_dir_all(riv.join("refs/heads")).unwrap();
        fs::write(riv.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        (dir, RefStore::new(&riv))
    }

    #[test]
    fn head_roundtrip() {
        let (_d, store) = setup();
        assert_eq!(store.head().unwrap(), Head::Symbolic("main".into()));
        assert_eq!(store.current_branch().unwrap(), Some("main".into()));
        assert!(store.resolve_head().unwrap().is_none());
    }

    #[test]
    fn branch_crud() {
        let (_d, store) = setup();
        let oid: Oid = "a".repeat(64).parse().unwrap();
        store.set_branch("feature/x", &oid).unwrap();
        assert_eq!(store.branch_oid("feature/x").unwrap(), Some(oid));
        assert_eq!(store.branches().unwrap(), vec!["feature/x".to_string()]);
        store.advance_head(&oid).unwrap();
        assert_eq!(store.resolve_head().unwrap(), Some(oid));
        store.delete_branch("feature/x").unwrap();
        assert!(store.branch_oid("feature/x").unwrap().is_none());
    }

    #[test]
    fn detached_head() {
        let (_d, store) = setup();
        let oid: Oid = "b".repeat(64).parse().unwrap();
        store.set_head_detached(&oid).unwrap();
        assert_eq!(store.head().unwrap(), Head::Detached(oid));
        assert!(store.current_branch().unwrap().is_none());
        assert_eq!(store.resolve_head().unwrap(), Some(oid));
        // 分离头指针下提交直接前进 HEAD
        let oid2: Oid = "c".repeat(64).parse().unwrap();
        store.advance_head(&oid2).unwrap();
        assert_eq!(store.resolve_head().unwrap(), Some(oid2));
    }

    #[test]
    fn invalid_names_rejected() {
        let (_d, store) = setup();
        let oid = Oid::ZERO;
        assert!(store.set_branch("", &oid).is_err());
        assert!(store.set_branch("refs/heads/x", &oid).is_err());
        assert!(store.set_branch("a..b", &oid).is_err());
        assert!(store.set_branch("a b", &oid).is_err());
    }

    #[test]
    fn invalid_names_rejected_on_read_paths_too() {
        // 读/删路径与写路径同样校验：分支名是 .riv 内路径的组成部分，
        // 非法名必须被 branch_path 统一拦截，而不是拼出越界路径。
        let (_d, store) = setup();
        assert!(store.branch_oid("../../etc/passwd").is_err());
        assert!(store.branch_oid("a..b").is_err());
        assert!(store.delete_branch("../evil").is_err());
        assert!(store.set_branch("ok.name", &Oid::ZERO).is_ok());
        assert!(store.branch_oid("ok.name").is_ok());
    }

    #[test]
    fn delete_missing_branch_errors() {
        let (_d, store) = setup();
        assert!(store.delete_branch("ghost").is_err());
    }
}
