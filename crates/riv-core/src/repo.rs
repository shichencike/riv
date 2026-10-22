//! 仓库：`.riv` 目录的定位、初始化与高层访问入口。

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::config::{Config, RepoConfig};
use crate::error::{Error, Result};
use crate::index::Index;
use crate::object::{Commit, Object};
use crate::oid::Oid;
use crate::refs::RefStore;
use crate::store::ObjectStore;

pub struct Repo {
    pub workdir: PathBuf,
    pub riv_dir: PathBuf,
}

impl Repo {
    /// 在 `path` 初始化新仓库。
    pub fn init(path: &Path, initial_branch: Option<&str>) -> Result<Repo> {
        let workdir = fs::canonicalize(path)
            .map_err(|_| Error::Msg(format!("cannot access directory: {}", path.display())))?;
        let riv_dir = workdir.join(".riv");
        if riv_dir.exists() {
            return Err(Error::RepoExists(riv_dir));
        }
        fs::create_dir_all(riv_dir.join("objects").join("tmp"))?;
        fs::create_dir_all(riv_dir.join("refs").join("heads"))?;
        let branch = initial_branch.unwrap_or("main");
        fs::write(riv_dir.join("HEAD"), format!("ref: refs/heads/{branch}\n"))?;
        fs::write(riv_dir.join("config"), RepoConfig::default_config_text())?;
        Ok(Repo { workdir, riv_dir })
    }

    /// 从 `start` 开始向上寻找 `.riv` 目录。
    pub fn discover(start: Option<&Path>) -> Result<Repo> {
        let start = match start {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir()?,
        };
        let start = fs::canonicalize(&start).unwrap_or(start);
        let mut dir = start.as_path();
        loop {
            let candidate = dir.join(".riv");
            if candidate.is_dir() {
                return Ok(Repo {
                    workdir: dir.to_path_buf(),
                    riv_dir: candidate,
                });
            }
            match dir.parent() {
                Some(p) => dir = p,
                None => return Err(Error::NotARepo(start.to_path_buf())),
            }
        }
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn store(&self) -> Result<ObjectStore> {
        Ok(ObjectStore::new(
            &self.riv_dir,
            self.repo_config()?.compression,
        ))
    }

    pub fn config(&self) -> Result<Config> {
        Config::load(&self.riv_dir.join("config"))
    }

    pub fn repo_config(&self) -> Result<RepoConfig> {
        Ok(RepoConfig::from_config(&self.config()?))
    }

    pub fn refs(&self) -> RefStore {
        RefStore::new(&self.riv_dir)
    }

    pub fn index(&self) -> Result<Index> {
        Index::load(&self.riv_dir.join("index"))
    }

    pub fn save_index(&self, index: &Index) -> Result<()> {
        index.save(&self.riv_dir.join("index"))
    }

    /// HEAD 指向的提交（不存在时为 None）。
    pub fn head_commit(&self) -> Result<Option<(Oid, Commit)>> {
        let store = self.store()?;
        match self.refs().resolve_head()? {
            Some(oid) => {
                let obj = store.read(&oid)?;
                match obj {
                    Object::Commit(c) => Ok(Some((oid, c))),
                    other => Err(Error::CorruptObject(format!(
                        "HEAD points to a {}, not a commit",
                        other.otype().as_str()
                    ))),
                }
            }
            None => Ok(None),
        }
    }

    /// HEAD 提交的 tree oid。
    pub fn head_tree(&self) -> Result<Option<Oid>> {
        Ok(self.head_commit()?.map(|(_, c)| c.tree))
    }

    /// 把用户给出的路径规范化为「相对仓库根、正斜杠」的路径。
    /// 相对路径基于当前工作目录解析。
    pub fn to_repo_relative(&self, path: &Path) -> Result<PathBuf> {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let abs_lex = normalize_path(&abs);
        // 剥离仓库根前缀。`workdir` 来自 canonicalize，Windows 上是
        // `\\?\C:\...` verbatim 形式，而 current_dir()/用户输入是普通
        // `C:\...` 形式——Path::strip_prefix 按组件匹配，两侧 Prefix 组件
        // 不同（VerbatimDisk vs Disk）必然失败，所有相对路径命令都会误报
        // OutsideRepo。因此把两侧都归一成普通形式，逐一组合后再试。
        let abs_plain = plain_path(&abs_lex);
        let workdir_plain = plain_path(&self.workdir);
        let candidates: Vec<(&Path, &Path)> = vec![
            (&self.workdir, &abs_lex),
            (&workdir_plain, &abs_plain),
            (&self.workdir, &abs_plain),
            (&workdir_plain, &abs_lex),
        ];
        let rel = candidates
            .iter()
            .find_map(|(w, a)| strip_workdir_prefix(w, a))
            .map(|r| r.to_path_buf())
            // 兜底：cwd 位于符号链接中（如 macOS /tmp）时，canonicalize 之后可比
            .or_else(|| {
                fs::canonicalize(&abs).ok().and_then(|c| {
                    strip_workdir_prefix(&self.workdir, &c)
                        .or_else(|| strip_workdir_prefix(&workdir_plain, &c))
                        .map(|r| r.to_path_buf())
                })
            })
            .ok_or_else(|| Error::OutsideRepo(path.display().to_string()))?;
        let s = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if s.is_empty() {
            return Err(Error::OutsideRepo(path.display().to_string()));
        }
        Ok(PathBuf::from(s))
    }
}

/// 组件级剥离仓库根前缀，返回仓库内相对路径。
fn strip_workdir_prefix<'a>(workdir: &Path, abs: &'a Path) -> Option<&'a Path> {
    abs.strip_prefix(workdir).ok()
}

/// 把路径归一成「普通形式」：Windows 上剥掉 `\\?\` verbatim（扩展长度）前缀。
///
/// 必须经**组件重建**实现——`Path::strip_prefix` 按组件匹配，无法剥掉这个
/// 4 字符的字符串前缀（`Prefix::VerbatimDisk` 与 `Prefix::Disk` 是不同的
/// 组件）。非 Windows 平台为恒等变换。
fn plain_path(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::Component;
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                Component::Prefix(prefix) => {
                    // VerbatimDisk('C') 的 OsStr 是 `\\?\C:`，剥壳得 `C:`
                    let s = prefix.as_os_str().to_string_lossy();
                    let stripped = s.strip_prefix(r"\\?\").unwrap_or(&s);
                    out.push(stripped);
                }
                other => out.push(other.as_os_str()),
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        p.to_path_buf()
    }
}

/// 词法规范化：去掉 `.` 与多余的 `..`（不触碰文件系统，对不存在的路径也有效）。
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(Component::Normal(_)) = out.last() {
                    out.pop();
                } else if !matches!(out.last(), Some(Component::RootDir)) {
                    // 保留开头的 ..（随后会被 strip_prefix 判为仓库外）
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    let mut result = PathBuf::new();
    for c in out {
        result.push(c.as_os_str());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_structure() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        assert!(repo.riv_dir.join("HEAD").is_file());
        assert!(repo.riv_dir.join("config").is_file());
        assert!(repo.riv_dir.join("objects").is_dir());
        assert!(repo.riv_dir.join("refs/heads").is_dir());
        assert_eq!(
            fs::read_to_string(repo.riv_dir.join("HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
        // 重复 init 报错
        assert!(Repo::init(dir.path(), None).is_err());
    }

    #[test]
    fn init_custom_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), Some("trunk")).unwrap();
        assert_eq!(
            fs::read_to_string(repo.riv_dir.join("HEAD")).unwrap(),
            "ref: refs/heads/trunk\n"
        );
    }

    #[test]
    fn discover_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        let sub = dir.path().join("a/b/c");
        fs::create_dir_all(&sub).unwrap();
        let found = Repo::discover(Some(&sub)).unwrap();
        assert_eq!(found.workdir, repo.workdir);
    }

    #[test]
    fn discover_from_nonexistent_fails() {
        let dir = tempfile::tempdir().unwrap();
        // 空目录（无 .riv）
        assert!(Repo::discover(Some(dir.path())).is_err());
    }

    #[test]
    fn to_repo_relative() {
        // 本测试会修改进程级当前目录：加锁防止与并行测试相互干扰，并在结束后恢复。
        static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CWD_LOCK.lock().unwrap();
        let original_cwd = std::env::current_dir().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        let sub = dir.path().join("src");
        fs::create_dir_all(&sub).unwrap();
        std::env::set_current_dir(&sub).unwrap();
        let rel = repo.to_repo_relative(Path::new("main.rs")).unwrap();
        assert_eq!(rel, PathBuf::from("src/main.rs"));
        let rel2 = repo.to_repo_relative(Path::new("./x/../y.txt")).unwrap();
        assert_eq!(rel2, PathBuf::from("src/y.txt"));
        let rel3 = repo
            .to_repo_relative(&dir.path().join("README.md"))
            .unwrap();
        assert_eq!(rel3, PathBuf::from("README.md"));
        // 仓库外的路径
        assert!(repo.to_repo_relative(Path::new("/etc/passwd")).is_err());

        std::env::set_current_dir(original_cwd).unwrap();
    }

    #[test]
    fn to_repo_relative_nonexistent_path() {
        // 跨平台回归：新文件 / 已删除文件不在磁盘上，也必须能规范化。
        // Windows 上 canonicalize 返回 `\\?\C:\...` 扩展路径，而 current_dir
        // 返回 `C:\...` 普通形式——修复前 strip_prefix 必然失败（OutsideRepo）。
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();

        // 文件不存在，但父目录存在
        let rel = repo
            .to_repo_relative(&dir.path().join("sub/new.txt"))
            .unwrap();
        assert_eq!(rel, PathBuf::from("sub/new.txt"));

        // 绝对路径形式同样可行
        let rel2 = repo.to_repo_relative(&dir.path().join("gone.txt")).unwrap();
        assert_eq!(rel2, PathBuf::from("gone.txt"));

        // 仓库外仍然报错
        let outside = if cfg!(windows) {
            PathBuf::from("C:\\Windows\\notepad.exe")
        } else {
            PathBuf::from("/etc/passwd")
        };
        assert!(repo.to_repo_relative(&outside).is_err());
    }

    #[test]
    fn normalize_path_lexical() {
        assert_eq!(
            normalize_path(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(normalize_path(Path::new("/a/./b")), PathBuf::from("/a/b"));
        assert_eq!(normalize_path(Path::new("a/b/..")), PathBuf::from("a"));
    }

    #[test]
    fn head_commit_none_on_fresh_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        assert!(repo.head_commit().unwrap().is_none());
        assert!(repo.head_tree().unwrap().is_none());
        assert!(repo.index().unwrap().is_empty());
    }

    #[test]
    fn unused_oid_import() {
        // 保证 Oid 在本模块被引用（供将来 API 使用）
        let _ = Oid::ZERO;
    }
}
