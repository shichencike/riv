//! 文件级差异：blob vs blob、blob vs 工作区文件。
//!
//! 底层行级 diff 由 `riv-diff`（Myers O(ND)，对齐 git xdiff）提供，
//! 本模块负责取内容、识别二进制文件、包装成统一的输出结构。

use std::path::{Path, PathBuf};

use riv_diff;

use crate::error::Result;
use crate::index::Index;
use crate::oid::Oid;
use crate::repo::Repo;
use crate::snapshot::check_freshness;
use crate::store::ObjectStore;

/// 文件差异的正文。
#[derive(Debug, Clone)]
pub enum FileDiffBody {
    /// 二进制文件（任一侧含 NUL 字节）
    Binary { old_size: u64, new_size: u64 },
    /// 文本文件：git 风格 unified diff + 增删行数
    Text {
        unified: String,
        insertions: usize,
        deletions: usize,
    },
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    /// 旧侧 mode（None = 不存在该侧）
    pub old_mode: Option<u32>,
    /// 新侧 mode（None = 不存在该侧）
    pub new_mode: Option<u32>,
    pub body: FileDiffBody,
}

impl FileDiff {
    /// 增删行数汇总（二进制为 (0, 0)）。
    pub fn stat(&self) -> (usize, usize) {
        match &self.body {
            FileDiffBody::Text {
                insertions,
                deletions,
                ..
            } => (*insertions, *deletions),
            FileDiffBody::Binary { .. } => (0, 0),
        }
    }

    pub fn is_binary(&self) -> bool {
        matches!(self.body, FileDiffBody::Binary { .. })
    }
}

/// mode 比较（None 按普通文件计）：平台语义由 `mode_equivalent` 决定，
/// Windows 上 exec 位差异不可见，不会产生幻影 mode 变更。
fn modes_equivalent(a: Option<u32>, b: Option<u32>) -> bool {
    crate::object::mode_equivalent(
        a.unwrap_or(crate::object::MODE_BLOB),
        b.unwrap_or(crate::object::MODE_BLOB),
    )
}

/// 构造文件差异；内容与 mode 均一致时返回 None。
fn build(
    path: &str,
    old_mode: Option<u32>,
    old: Option<&[u8]>,
    new_mode: Option<u32>,
    new: Option<&[u8]>,
) -> Option<FileDiff> {
    if old == new && modes_equivalent(old_mode, new_mode) {
        return None;
    }
    let body = if old.map(riv_diff::is_binary).unwrap_or(false)
        || new.map(riv_diff::is_binary).unwrap_or(false)
    {
        FileDiffBody::Binary {
            old_size: old.map(|v| v.len() as u64).unwrap_or(0),
            new_size: new.map(|v| v.len() as u64).unwrap_or(0),
        }
    } else {
        let unified = riv_diff::format_file_diff(path, old_mode, old, new_mode, new, 3);
        let (insertions, deletions) = riv_diff::file_stat(path, old, new)
            .map(|s| (s.insertions, s.deletions))
            .unwrap_or((0, 0));
        FileDiffBody::Text {
            unified,
            insertions,
            deletions,
        }
    };
    Some(FileDiff {
        path: path.to_string(),
        old_mode,
        new_mode,
        body,
    })
}

/// 两个 blob 之间的差异（None 表示「不存在」），带两侧 mode。
pub fn diff_blobs(
    store: &ObjectStore,
    path: &str,
    old: Option<(Oid, u32)>,
    new: Option<(Oid, u32)>,
) -> Result<Option<FileDiff>> {
    let old_data = match old {
        Some((o, _)) => Some(store.read_blob(&o)?),
        None => None,
    };
    let new_data = match new {
        Some((o, _)) => Some(store.read_blob(&o)?),
        None => None,
    };
    Ok(build(
        path,
        old.map(|(_, m)| m),
        old_data.as_deref(),
        new.map(|(_, m)| m),
        new_data.as_deref(),
    ))
}

/// 索引中的 blob 与工作区文件之间的差异（带 mode；工作区侧取实际文件类型）。
pub fn diff_blob_workdir(
    store: &ObjectStore,
    path: &str,
    old: Option<(Oid, u32)>,
    workdir_file: &Path,
) -> Result<Option<FileDiff>> {
    let old_data = match old {
        Some((o, _)) => Some(store.read_blob(&o)?),
        None => None,
    };
    let (new_data, new_mode) = match std::fs::symlink_metadata(workdir_file) {
        Ok(m) if m.file_type().is_symlink() => {
            let target = std::fs::read_link(workdir_file)?
                .to_string_lossy()
                .into_owned()
                .into_bytes();
            (Some(target), Some(crate::object::MODE_LINK))
        }
        Ok(m) => {
            let mode = crate::snapshot::stat_info(workdir_file, &m).3;
            (Some(std::fs::read(workdir_file)?), Some(mode))
        }
        Err(_) => (None, None),
    };
    Ok(build(
        path,
        old.map(|(_, m)| m),
        old_data.as_deref(),
        new_mode,
        new_data.as_deref(),
    ))
}

/// 暂存内容（索引 oid）与工作区文件的差异；无差异返回 None。
///
/// 索引由调用方传入并复用：逐路径调用时若各自 `repo.index()`，
/// 整个索引文件会被反复解码（O(n²) 次 IO）。
pub fn diff_index_vs_workdir(repo: &Repo, index: &Index, path: &str) -> Result<Option<FileDiff>> {
    let Some(entry) = index.get(path) else {
        return Ok(None);
    };
    let abs = repo.workdir.join(path);
    // 一致性快路：缓存可信且无变化 → 无差异
    if let crate::snapshot::Freshness::Clean = check_freshness(entry, &abs, index.index_mtime_sec)?
    {
        return Ok(None);
    }
    diff_blob_workdir(&repo.store()?, path, Some((entry.oid, entry.mode)), &abs)
}

/// HEAD 树与工作区之间的差异（`git diff HEAD`）。
pub fn diff_head_vs_workdir(repo: &Repo) -> Result<Vec<FileDiff>> {
    let index = repo.index()?;
    let store = repo.store()?;
    let head_tree = match repo.head_tree()? {
        Some(t) => Some(crate::snapshot::flatten_tree(&store, Some(t))?),
        None => None,
    };
    let mut out = Vec::new();
    for entry in &index.entries {
        let abs = repo.workdir.join(&entry.path);
        let head_entry = head_tree.as_ref().and_then(|t| t.get(&entry.path)).copied();
        // 工作区 vs HEAD：若缓存可信且与索引一致，而索引与 HEAD 一致 → 无差异
        let clean_vs_index = matches!(
            check_freshness(entry, &abs, index.index_mtime_sec)?,
            crate::snapshot::Freshness::Clean
        );
        let same_as_head = head_entry
            .map(|(o, m)| o == entry.oid && modes_equivalent(Some(m), Some(entry.mode)))
            .unwrap_or(false);
        if clean_vs_index && same_as_head {
            continue;
        }
        if let Some(d) = diff_blob_workdir(&store, &entry.path, head_entry, &abs)? {
            out.push(d);
        }
    }
    // HEAD 有、工作区/索引没有的文件（已暂存删除）也出现在 diff 中
    if let Some(head_tree) = head_tree {
        for (path, (oid, mode)) in &head_tree {
            if index.get(path).is_none() {
                if let Some(d) = diff_blobs(&store, path, Some((*oid, *mode)), None)? {
                    out.push(d);
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// 批量计算多个路径的「HEAD vs 工作区」差异（对齐 `git diff -- <paths>`）。
pub fn diff_paths(repo: &Repo, paths: &[PathBuf]) -> Result<Vec<FileDiff>> {
    let all = diff_head_vs_workdir(repo)?;
    if paths.is_empty() {
        return Ok(all);
    }
    let wanted: Vec<String> = paths
        .iter()
        .map(|p| {
            let rel = repo.to_repo_relative(p)?;
            Ok(rel.to_string_lossy().replace('\\', "/"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(all
        .into_iter()
        .filter(|d| wanted.contains(&d.path))
        .collect())
}

/// 渲染差异列表为完整文本（git diff 风格）。
pub fn render(diffs: &[FileDiff]) -> String {
    let mut out = String::new();
    for d in diffs {
        match &d.body {
            FileDiffBody::Text { unified, .. } => {
                out.push_str(unified);
                if !unified.ends_with('\n') {
                    out.push('\n');
                }
            }
            FileDiffBody::Binary { old_size, new_size } => {
                out.push_str(&format!(
                    "diff --riv a/{p} b/{p}\nBinary files differ ({old_size} -> {new_size} bytes)\n",
                    p = d.path
                ));
            }
        }
    }
    out
}

/// 渲染 `--stat` 风格汇总。
pub fn render_stat(diffs: &[FileDiff]) -> String {
    let stats: Vec<riv_diff::FileStat> = diffs
        .iter()
        .filter(|d| !d.is_binary())
        .map(|d| {
            let (insertions, deletions) = d.stat();
            riv_diff::FileStat {
                path: d.path.clone(),
                insertions,
                deletions,
            }
        })
        .collect();
    riv_diff::format_stat(&stats, 52)
}

/// 索引是否为空（便捷判断）。
#[allow(dead_code)]
pub fn index_is_empty(index: &Index) -> bool {
    index.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::{commit, CommitOptions};
    use crate::object::{Object, MODE_BLOB};
    #[cfg(unix)]
    use crate::object::{MODE_BLOB_EXEC, MODE_LINK};
    use crate::snapshot::AddOptions;
    use std::fs;

    fn setup() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        (dir, repo)
    }

    #[test]
    fn diff_blobs_text() {
        let (_d, repo) = setup();
        let store = repo.store().unwrap();
        let old = store
            .write(&Object::Blob(b"line1\nline2\nline3\n".to_vec()))
            .unwrap();
        let new = store
            .write(&Object::Blob(
                b"line1\nline2 changed\nline3\nline4\n".to_vec(),
            ))
            .unwrap();
        let d = diff_blobs(
            &store,
            "f.txt",
            Some((old, MODE_BLOB)),
            Some((new, MODE_BLOB)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(d.path, "f.txt");
        let (ins, del) = d.stat();
        assert_eq!(ins, 2);
        assert_eq!(del, 1);
        if let FileDiffBody::Text { unified, .. } = &d.body {
            assert!(unified.contains("@@"));
            assert!(unified.contains("-line2"));
            assert!(unified.contains("+line2 changed"));
            assert!(unified.contains("+line4"));
        } else {
            panic!("expected text diff");
        }
        // 无差异
        assert!(diff_blobs(
            &store,
            "f.txt",
            Some((old, MODE_BLOB)),
            Some((old, MODE_BLOB))
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn diff_blobs_added_and_deleted() {
        let (_d, repo) = setup();
        let store = repo.store().unwrap();
        let old = store.write(&Object::Blob(b"only\n".to_vec())).unwrap();
        // 新增（old=None）
        let d = diff_blobs(&store, "new.txt", None, Some((old, MODE_BLOB)))
            .unwrap()
            .unwrap();
        assert_eq!(d.stat(), (1, 0));
        // 删除（new=None）
        let d = diff_blobs(&store, "old.txt", Some((old, MODE_BLOB)), None)
            .unwrap()
            .unwrap();
        assert_eq!(d.stat(), (0, 1));
    }

    #[test]
    fn diff_blobs_binary() {
        let (_d, repo) = setup();
        let store = repo.store().unwrap();
        let old = store.write(&Object::Blob(vec![0u8, 1, 2, 0, 3])).unwrap();
        let new = store
            .write(&Object::Blob(vec![0u8, 9, 9, 0, 9, 9]))
            .unwrap();
        let d = diff_blobs(
            &store,
            "bin",
            Some((old, MODE_BLOB)),
            Some((new, MODE_BLOB)),
        )
        .unwrap()
        .unwrap();
        assert!(d.is_binary());
        assert_eq!(d.stat(), (0, 0));
        if let FileDiffBody::Binary { old_size, new_size } = &d.body {
            assert_eq!(*old_size, 5);
            assert_eq!(*new_size, 6);
        }
    }

    #[test]
    #[cfg(unix)] // exec 位是 Unix 文件系统特性；Windows 上 mode_equivalent 忽略 exec 位
    fn diff_blobs_mode_only_change() {
        // 内容相同、mode 不同 → 产生 mode-only 差异（git 的 chmod 呈现）
        let (_d, repo) = setup();
        let store = repo.store().unwrap();
        let blob = store.write(&Object::Blob(b"#!/bin/sh\n".to_vec())).unwrap();
        let d = diff_blobs(
            &store,
            "run.sh",
            Some((blob, MODE_BLOB)),
            Some((blob, MODE_BLOB_EXEC)),
        )
        .unwrap()
        .unwrap();
        if let FileDiffBody::Text { unified, .. } = &d.body {
            assert!(unified.contains("old mode 100644"), "{unified}");
            assert!(unified.contains("new mode 100755"), "{unified}");
        } else {
            panic!("expected text diff");
        }
        assert_eq!(d.stat(), (0, 0));
    }

    #[test]
    fn index_vs_workdir_changes() {
        let (dir, repo) = setup();
        fs::write(dir.path().join("a.txt"), b"v1\n").unwrap();
        crate::snapshot::add(&repo, &AddOptions::default()).unwrap();
        let index = repo.index().unwrap();
        // 无变化
        assert!(diff_index_vs_workdir(&repo, &index, "a.txt")
            .unwrap()
            .is_none());
        // 工作区改动
        fs::write(dir.path().join("a.txt"), b"v2\n").unwrap();
        let d = diff_index_vs_workdir(&repo, &index, "a.txt")
            .unwrap()
            .unwrap();
        assert_eq!(d.stat(), (1, 1));
    }

    #[test]
    #[cfg(unix)] // exec 位是 Unix 文件系统特性
    fn index_vs_workdir_mode_only_change() {
        // chmod +x（内容不变）：索引 vs 工作区应产出 mode-only 差异
        let (dir, repo) = setup();
        fs::write(dir.path().join("run.sh"), b"#!/bin/sh\n").unwrap();
        crate::snapshot::add(&repo, &AddOptions::default()).unwrap();
        let index = repo.index().unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let d = diff_index_vs_workdir(&repo, &index, "run.sh")
            .unwrap()
            .unwrap();
        if let FileDiffBody::Text { unified, .. } = &d.body {
            assert!(unified.contains("old mode 100644"), "{unified}");
            assert!(unified.contains("new mode 100755"), "{unified}");
        } else {
            panic!("expected text diff");
        }
    }

    #[test]
    fn head_vs_workdir_diff_and_filter() {
        let (dir, repo) = setup();
        fs::write(dir.path().join("a.txt"), b"v1\n").unwrap();
        fs::write(dir.path().join("b.txt"), b"b1\n").unwrap();
        crate::snapshot::add(&repo, &AddOptions::default()).unwrap();
        commit(
            &repo,
            &CommitOptions {
                message: "c1".into(),
                ..Default::default()
            },
        )
        .unwrap();
        // 改 a.txt，删 b.txt
        fs::write(dir.path().join("a.txt"), b"v1\nv2\n").unwrap();
        fs::remove_file(dir.path().join("b.txt")).unwrap();
        let diffs = diff_head_vs_workdir(&repo).unwrap();
        let paths: Vec<&str> = diffs.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["a.txt", "b.txt"]);
        assert_eq!(diffs[0].stat(), (1, 0));
        assert_eq!(diffs[1].stat(), (0, 1));
        // 过滤路径
        let filtered = diff_paths(&repo, &[dir.path().join("a.txt")]).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].path, "a.txt");
    }

    #[test]
    fn render_output() {
        let (_d, repo) = setup();
        let store = repo.store().unwrap();
        let old = store.write(&Object::Blob(b"a\nb\n".to_vec())).unwrap();
        let new = store.write(&Object::Blob(b"a\nB\n".to_vec())).unwrap();
        let d = diff_blobs(
            &store,
            "f.txt",
            Some((old, MODE_BLOB)),
            Some((new, MODE_BLOB)),
        )
        .unwrap()
        .unwrap();
        let text = render(std::slice::from_ref(&d));
        assert!(text.contains("--- a/f.txt"));
        assert!(text.contains("+++ b/f.txt"));
        assert!(text.contains("@@ -1,2 +1,2 @@"));
        // stat 渲染：` f.txt |    2 ++--...`
        let stat_text = render_stat(std::slice::from_ref(&d));
        assert!(stat_text.contains("f.txt"));
        assert!(stat_text.contains('+'));
        assert!(stat_text.contains('-'));
        assert!(stat_text.contains("|"));
    }

    #[test]
    #[cfg(unix)] // 符号链接语义仅 Unix
    fn blob_workdir_symlink_change() {
        let (dir, repo) = setup();
        #[cfg(unix)]
        {
            let store = repo.store().unwrap();
            let link_target = store.write(&Object::Blob(b"a.txt".to_vec())).unwrap();
            std::os::unix::fs::symlink("a.txt", dir.path().join("link")).unwrap();
            let d = diff_blob_workdir(
                &store,
                "link",
                Some((link_target, MODE_LINK)),
                &dir.path().join("link"),
            )
            .unwrap();
            assert!(d.is_none()); // 目标一致
                                  // 改指向
            fs::remove_file(dir.path().join("link")).unwrap();
            std::os::unix::fs::symlink("other.txt", dir.path().join("link")).unwrap();
            let d = diff_blob_workdir(
                &store,
                "link",
                Some((link_target, MODE_LINK)),
                &dir.path().join("link"),
            )
            .unwrap()
            .unwrap();
            assert_eq!(d.stat(), (1, 1));
        }
    }
}
