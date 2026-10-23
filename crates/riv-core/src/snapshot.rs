//! 工作区遍历、文件哈希与树构建。
//!
//! - `walk_workdir`：尊重 `.rivignore`/`.gitignore`/`.`ignore` 与隐藏文件规则的并行安全遍历
//! - `hash_file`：blake3 内容哈希（大文件走 mmap，symlink 存链接目标）
//! - `build_tree_from_index`：把索引物化为树对象（自底向上）
//! - `flatten_tree`：把树对象还原为 路径 → (oid, mode) 的平坦映射

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use crate::error::{Error, Result};
use crate::index::{Index, IndexEntry};
#[cfg(unix)]
use crate::object::{Object, Tree, TreeEntry, MODE_BLOB, MODE_BLOB_EXEC, MODE_LINK, MODE_TREE};
#[cfg(not(unix))]
use crate::object::{Object, Tree, TreeEntry, MODE_BLOB, MODE_LINK, MODE_TREE};
use crate::oid::Oid;
use crate::repo::Repo;
use crate::store::ObjectStore;

/// 平坦化的树：仓库相对路径 → (oid, mode)
pub type TreeMap = BTreeMap<String, (Oid, u32)>;

// ---------------------------------------------------------------------------
// 遍历
// ---------------------------------------------------------------------------

/// 遍历工作区，返回所有「未被忽略」的文件（含 symlink），仓库相对路径、正斜杠。
pub fn walk_workdir(repo: &Repo) -> Result<Vec<String>> {
    let mut builder = ignore::WalkBuilder::new(&repo.workdir);
    builder
        .standard_filters(true) // ignore / gitignore / parents / git-global / git-exclude
        .hidden(false) // 不跳过点文件：.rivignore 等点文件与 git 的 .gitignore 一样是可跟踪内容
        .add_custom_ignore_filename(".rivignore")
        .filter_entry(|e| e.file_name() != ".riv");
    let mut out = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            Err(err) => return Err(Error::msg(format!("walk error: {err}"))),
        };
        let ft = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if ft.is_file() || ft.is_symlink() {
            if let Ok(rel) = entry.path().strip_prefix(&repo.workdir) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// 目录下是否存在任意文件（用于判断「显式路径被忽略」）。
fn has_any_file(dir: &Path) -> Result<bool> {
    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        .standard_filters(false)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".riv");
    for result in builder.build() {
        let entry = result.map_err(|err| Error::msg(format!("walk error: {err}")))?;
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// 哈希与 stat
// ---------------------------------------------------------------------------

/// 文件的 stat 摘要：(size, mtime_sec, mtime_nsec, mode)
pub fn stat_info(_path: &Path, meta: &fs::Metadata) -> (u64, i64, i32, u32) {
    let size = meta.len();
    let (sec, nsec) = match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as i64, d.subsec_nanos() as i32),
            Err(_) => (0, 0),
        },
        Err(_) => (0, 0),
    };
    let mode = file_mode(meta);
    (size, sec, nsec, mode)
}

#[cfg(unix)]
fn file_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    let ft = meta.file_type();
    if ft.is_symlink() {
        MODE_LINK
    } else if meta.permissions().mode() & 0o111 != 0 {
        MODE_BLOB_EXEC
    } else {
        MODE_BLOB
    }
}

#[cfg(not(unix))]
fn file_mode(_meta: &fs::Metadata) -> u32 {
    MODE_BLOB
}

/// 哈希单个文件，返回 **blob 对象 id**（与对象库寻址一致）。
/// symlink 的内容是其目标路径字符串。
pub fn hash_file(path: &Path) -> Result<(Oid, u64, i64, i32, u32)> {
    let meta = fs::symlink_metadata(path)?;
    let (size, sec, nsec, mode) = stat_info(path, &meta);
    let oid = if mode == MODE_LINK {
        let target = fs::read_link(path)?;
        Object::Blob(target.to_string_lossy().into_owned().into_bytes()).oid()
    } else {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&Object::blob_header(size));
        if size >= (1 << 20) {
            hasher
                .update_mmap(path)
                .map_err(|e| Error::msg(format!("cannot read {}: {e}", path.display())))?;
        } else {
            let data = fs::read(path)?;
            hasher.update(&data);
        }
        Oid::from_bytes(*hasher.finalize().as_bytes())
    };
    Ok((oid, size, sec, nsec, mode))
}

/// 哈希文件并把 blob 对象写入存储（`add` 的热路径）。
/// 大文件走流式路径，内存有界。stat 只做一次（大小直接传给存储层）。
pub fn hash_and_store(path: &Path, store: &ObjectStore) -> Result<(Oid, u64, i64, i32, u32)> {
    let meta = fs::symlink_metadata(path)?;
    let (size, sec, nsec, mode) = stat_info(path, &meta);
    let oid = if mode == MODE_LINK {
        let target = fs::read_link(path)?;
        store.write(&Object::Blob(
            target.to_string_lossy().into_owned().into_bytes(),
        ))?
    } else if size >= (1 << 20) {
        store.write_blob_file_sized(path, size)?.0
    } else {
        let data = fs::read(path)?;
        store.write(&Object::Blob(data))?
    };
    Ok((oid, size, sec, nsec, mode))
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    /// 显式路径（仓库相对或相对当前目录）；为空表示整个工作区
    pub paths: Vec<PathBuf>,
    /// -A：包含删除
    pub all: bool,
    /// -u：只更新已跟踪文件
    pub update_only: bool,
    /// -f：添加强制忽略的文件
    pub force: bool,
    /// -n：只预览
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AddReport {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub deleted: Vec<String>,
    pub unchanged: usize,
}

impl AddReport {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.deleted.is_empty()
    }
}

/// 执行 `add`：把工作区变更写入索引。
pub fn add(repo: &Repo, opts: &AddOptions) -> Result<AddReport> {
    let store = repo.store()?;
    let mut index = repo.index()?;
    let old_paths: BTreeSet<String> = index.paths().map(|s| s.to_string()).collect();

    // 1. 全量遍历（尊重忽略规则）
    let walked: BTreeSet<String> = walk_workdir(repo)?.into_iter().collect();

    // 2. 计算目标路径集合与被忽略的显式路径
    let mut targets: BTreeSet<String> = BTreeSet::new();
    let mut ignored: Vec<String> = Vec::new();
    // 显式路径中是否包含仓库根（`.` / 仓库目录本身）
    let mut root_requested = false;
    if opts.paths.is_empty() || opts.all {
        if opts.update_only {
            for p in &old_paths {
                if walked.contains(p) {
                    targets.insert(p.clone());
                }
            }
        } else {
            targets = walked.clone();
        }
    } else {
        for p in &opts.paths {
            // 仓库根本身（`.` / 仓库目录）→ 等价于整个工作区。
            // 必须在 to_repo_relative 之前判断：后者对空 rel 直接报 OutsideRepo。
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                std::env::current_dir()?.join(p)
            };
            let is_root = abs == repo.workdir
                || fs::canonicalize(&abs)
                    .map(|c| c == repo.workdir)
                    .unwrap_or(false);
            if is_root {
                root_requested = true;
                continue;
            }
            let rel = repo.to_repo_relative(p)?;
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if rel_str.is_empty() {
                // 仓库根本身：等价于整个工作区
                root_requested = true;
                continue;
            }
            let abs = repo.workdir.join(&rel);
            let is_dir = abs.is_dir()
                && !fs::symlink_metadata(&abs)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false);
            if is_dir {
                let prefix = format!("{rel_str}/");
                let mut found = false;
                for w in &walked {
                    if *w == rel_str || w.starts_with(&prefix) {
                        targets.insert(w.clone());
                        found = true;
                    }
                }
                if !found && has_any_file(&abs)? && !opts.force {
                    ignored.push(rel_str);
                }
            } else if abs.symlink_metadata().is_ok() {
                if walked.contains(&rel_str) || old_paths.contains(&rel_str) || opts.force {
                    targets.insert(rel_str);
                } else {
                    ignored.push(rel_str);
                }
            } else if old_paths.contains(&rel_str) {
                // 已跟踪但已删除 → 暂存删除
                targets.insert(rel_str);
            }
        }
        if root_requested {
            // 仓库根：目标提升为整个工作区（覆盖上面收集到的具体路径）
            targets.clear();
            if opts.update_only {
                for p in &old_paths {
                    if walked.contains(p) {
                        targets.insert(p.clone());
                    }
                }
            } else {
                targets = walked.clone();
            }
        }
    }
    if !ignored.is_empty() {
        return Err(Error::PathIgnored(ignored));
    }

    // 3. 删除检测
    let deleted: Vec<String> = if opts.paths.is_empty() || opts.all || root_requested {
        old_paths
            .iter()
            .filter(|p| !walked.contains(*p))
            .cloned()
            .collect()
    } else {
        targets
            .iter()
            .filter(|p| !walked.contains(*p) && old_paths.contains(*p))
            .cloned()
            .collect()
    };

    // 4. 分类：可复用 stat 缓存 / 需要哈希
    let mut to_hash: Vec<(String, PathBuf)> = Vec::new();
    let mut report = AddReport::default();
    for path in &targets {
        if deleted.contains(path) {
            continue;
        }
        let abs = repo.workdir.join(path);
        match fs::symlink_metadata(&abs) {
            Ok(meta) if meta.file_type().is_file() || meta.file_type().is_symlink() => {
                let (size, sec, nsec, mode) = stat_info(&abs, &meta);
                // racy 检查（对齐 git）：文件 mtime 秒 >= 索引文件 mtime 秒时，
                // stat 缓存可能与索引写盘同秒而不可信，强制重新哈希。
                let racy = match index.index_mtime_sec {
                    Some(im) => sec >= im,
                    None => false,
                };
                let reusable = !racy
                    && index.get(path).is_some_and(|old| {
                        old.size == size
                            && old.mtime_sec == sec
                            && old.mtime_nsec == nsec
                            && crate::object::mode_equivalent(old.mode, mode)
                    });
                if reusable {
                    report.unchanged += 1;
                } else {
                    to_hash.push((path.clone(), abs));
                }
            }
            _ => {}
        }
    }

    // 5. 并行哈希。
    // dry-run 只算对象 id（不写存储、不碰索引）；正常路径同时把 blob 写入对象库。
    let hashed: Vec<(String, Oid, u64, i64, i32, u32)> = to_hash
        .par_iter()
        .map(|(p, abs)| {
            if opts.dry_run {
                hash_file(abs)
            } else {
                hash_and_store(abs, &store)
            }
            .map(|(oid, size, sec, nsec, mode)| (p.clone(), oid, size, sec, nsec, mode))
        })
        .collect::<Result<Vec<_>>>()?;

    // 6. 分类（dry-run 也要：预览输出依赖 added/updated/deleted）
    for path in &deleted {
        if !opts.dry_run {
            index.remove(path);
        }
        report.deleted.push(path.clone());
    }
    for (path, oid, size, sec, nsec, mode) in &hashed {
        let old = index.get(path).cloned();
        if !opts.dry_run {
            index.insert(crate::index::IndexEntry {
                path: path.clone(),
                oid: *oid,
                mode: *mode,
                size: *size,
                mtime_sec: *sec,
                mtime_nsec: *nsec,
                flags: 0,
            });
        }
        match old {
            Some(o) if o.oid == *oid && crate::object::mode_equivalent(o.mode, *mode) => {
                report.unchanged += 1
            }
            Some(_) => report.updated.push(path.clone()),
            None => report.added.push(path.clone()),
        }
    }
    if !opts.dry_run {
        repo.save_index(&index)?;
    }
    report.added.sort();
    report.updated.sort();
    Ok(report)
}

// ---------------------------------------------------------------------------
// 索引条目与工作区的一致性检查（status / checkout 共用）
// ---------------------------------------------------------------------------

/// 索引条目与工作区文件的比较结果。
pub(crate) enum Freshness {
    /// 一致：stat 缓存可信，或内容哈希与索引相同
    Clean,
    /// 不一致：工作区当前 mode（内容已变化）
    Dirty { mode: u32 },
    /// 文件不存在，或类型已变（如变成了目录）
    Missing,
}

/// 检查索引条目与工作区文件是否一致；必要时计算新哈希。
///
/// racy 语义对齐 git：文件 mtime 秒 >= 索引文件 mtime 秒时 stat 缓存不可信，
/// 强制重新哈希（防止粗粒度 mtime 文件系统上的漏检）。
pub(crate) fn check_freshness(
    entry: &IndexEntry,
    abs: &Path,
    index_mtime_sec: Option<i64>,
) -> Result<Freshness> {
    let meta = match fs::symlink_metadata(abs) {
        Ok(m) => m,
        Err(_) => return Ok(Freshness::Missing),
    };
    if !(meta.file_type().is_file() || meta.file_type().is_symlink()) {
        return Ok(Freshness::Missing);
    }
    let (size, sec, nsec, mode) = stat_info(abs, &meta);
    let racy = match index_mtime_sec {
        Some(im) => sec >= im,
        None => false,
    };
    if !racy
        && entry.size == size
        && entry.mtime_sec == sec
        && entry.mtime_nsec == nsec
        && crate::object::mode_equivalent(entry.mode, mode)
    {
        return Ok(Freshness::Clean);
    }
    let (oid, _, _, _, hashed_mode) = hash_file(abs)?;
    if oid == entry.oid && crate::object::mode_equivalent(hashed_mode, entry.mode) {
        // 内容一致、仅 stat 变化：视为 clean（add 时会顺带刷新缓存）
        Ok(Freshness::Clean)
    } else {
        Ok(Freshness::Dirty { mode: hashed_mode })
    }
}

// ---------------------------------------------------------------------------
// 树构建 / 展平
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TreeNode {
    files: Vec<(String, Oid, u32)>,
    dirs: BTreeMap<String, TreeNode>,
}

/// 把索引物化为树对象，返回根树 oid。空索引产生空树。
pub fn build_tree_from_index(store: &ObjectStore, index: &Index) -> Result<Oid> {
    let mut root = TreeNode::default();
    for e in &index.entries {
        let comps: Vec<&str> = e.path.split('/').filter(|s| !s.is_empty()).collect();
        if comps.is_empty() {
            continue;
        }
        let mut node = &mut root;
        for c in &comps[..comps.len() - 1] {
            node = node.dirs.entry((*c).to_string()).or_default();
        }
        node.files
            .push((comps[comps.len() - 1].to_string(), e.oid, e.mode));
    }
    write_tree_node(store, &root)
}

fn write_tree_node(store: &ObjectStore, node: &TreeNode) -> Result<Oid> {
    let mut entries = Vec::new();
    for (name, oid, mode) in &node.files {
        entries.push(TreeEntry {
            mode: *mode,
            name: name.clone(),
            oid: *oid,
        });
    }
    for (name, sub) in &node.dirs {
        let sub_oid = write_tree_node(store, sub)?;
        entries.push(TreeEntry {
            mode: MODE_TREE,
            name: name.clone(),
            oid: sub_oid,
        });
    }
    store.write(&Object::Tree(Tree::new(entries)))
}

/// 把树对象展平为 路径 → (oid, mode)。
pub fn flatten_tree(store: &ObjectStore, root: Option<Oid>) -> Result<TreeMap> {
    let mut out = TreeMap::new();
    if let Some(oid) = root {
        flatten_recursive(store, &oid, "", &mut out)?;
    }
    Ok(out)
}

fn flatten_recursive(
    store: &ObjectStore,
    oid: &Oid,
    prefix: &str,
    out: &mut TreeMap,
) -> Result<()> {
    let tree = store
        .read(oid)?
        .as_tree()
        .cloned()
        .ok_or_else(|| Error::CorruptObject(format!("{} is not a tree", oid.short(7))))?;
    for e in tree.entries {
        let path = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{prefix}/{}", e.name)
        };
        if e.mode == MODE_TREE {
            flatten_recursive(store, &e.oid, &path, out)?;
        } else {
            out.insert(path, (e.oid, e.mode));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexEntry;

    fn setup_repo() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path(), None).unwrap();
        (dir, repo)
    }

    #[test]
    fn walk_respects_rivignore() {
        let (dir, repo) = setup_repo();
        fs::write(dir.path().join("keep.txt"), "a").unwrap();
        fs::write(dir.path().join("skip.log"), "b").unwrap();
        fs::write(dir.path().join(".rivignore"), "*.log\n").unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/deep.txt"), "c").unwrap();
        let files = walk_workdir(&repo).unwrap();
        assert!(files.contains(&"keep.txt".to_string()));
        assert!(files.contains(&"sub/deep.txt".to_string()));
        assert!(!files.contains(&"skip.log".to_string()));
        // .rivignore 自身会被遍历到（未被忽略），符合 git 对 .gitignore 的处理
        assert!(files.contains(&".rivignore".to_string()));
    }

    #[test]
    fn hash_file_regular_and_symlink() {
        let (dir, _repo) = setup_repo();
        let file = dir.path().join("f.txt");
        fs::write(&file, b"hello riv").unwrap();
        let (oid, size, _, _, mode) = hash_file(&file).unwrap();
        assert_eq!(size, 9);
        assert_eq!(mode, MODE_BLOB);
        // blob 对象 id（与对象库寻址一致）
        assert_eq!(oid, Object::Blob(b"hello riv".to_vec()).oid());

        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink("f.txt", &link).unwrap();
            let (loid, lsize, _, _, lmode) = hash_file(&link).unwrap();
            assert_eq!(lmode, MODE_LINK);
            assert_eq!(loid, Object::Blob(b"f.txt".to_vec()).oid());
            assert_eq!(lsize, 5);
        }
    }

    #[test]
    fn build_and_flatten_tree() {
        let (_dir, repo) = setup_repo();
        let store = repo.store().unwrap();
        let mut index = Index::new();
        for p in ["a.txt", "src/main.rs", "src/lib.rs", "src/deep/mod.rs"] {
            let oid = store.write(&Object::Blob(p.as_bytes().to_vec())).unwrap();
            index.insert(IndexEntry {
                path: p.into(),
                oid,
                mode: MODE_BLOB,
                size: 1,
                mtime_sec: 0,
                mtime_nsec: 0,
                flags: 0,
            });
        }
        let root = build_tree_from_index(&store, &index).unwrap();
        let flat = flatten_tree(&store, Some(root)).unwrap();
        assert_eq!(flat.len(), 4);
        assert_eq!(flat["src/deep/mod.rs"].1, MODE_BLOB);

        // 空索引 → 空树 → 展平为空
        let empty_root = build_tree_from_index(&store, &Index::new()).unwrap();
        assert!(flatten_tree(&store, Some(empty_root)).unwrap().is_empty());
        assert!(flatten_tree(&store, None).unwrap().is_empty());
    }

    #[test]
    fn add_basic_flow() {
        let (dir, repo) = setup_repo();
        fs::write(dir.path().join("a.txt"), "aaa").unwrap();
        fs::create_dir_all(dir.path().join("d")).unwrap();
        fs::write(dir.path().join("d/b.txt"), "bbb").unwrap();
        let report = add(&repo, &AddOptions::default()).unwrap();
        assert_eq!(
            report.added,
            vec!["a.txt".to_string(), "d/b.txt".to_string()]
        );
        let index = repo.index().unwrap();
        assert_eq!(index.entries.len(), 2);

        // 第二次 add：无变化
        let report2 = add(&repo, &AddOptions::default()).unwrap();
        assert!(report2.is_empty());
        assert_eq!(report2.unchanged, 2);

        // 修改文件后 add
        fs::write(dir.path().join("a.txt"), "aaaa").unwrap();
        let report3 = add(&repo, &AddOptions::default()).unwrap();
        assert_eq!(report3.updated, vec!["a.txt".to_string()]);

        // 删除文件后 add（无路径 → 整个工作区 → 暂存删除）
        fs::remove_file(dir.path().join("d/b.txt")).unwrap();
        let report4 = add(&repo, &AddOptions::default()).unwrap();
        assert_eq!(report4.deleted, vec!["d/b.txt".to_string()]);
        let index = repo.index().unwrap();
        assert_eq!(index.entries.len(), 1);
    }

    #[test]
    fn add_explicit_paths_and_ignored() {
        let (dir, repo) = setup_repo();
        fs::write(dir.path().join("ok.txt"), "1").unwrap();
        fs::write(dir.path().join("no.txt"), "2").unwrap();
        fs::write(dir.path().join(".rivignore"), "no.txt\n").unwrap();
        // 显式添加被忽略文件 → 报错（用绝对路径，避免依赖进程 cwd）
        let err = add(
            &repo,
            &AddOptions {
                paths: vec![dir.path().join("no.txt")],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("ignored"));
        // -f 强制添加
        let report = add(
            &repo,
            &AddOptions {
                paths: vec![dir.path().join("no.txt")],
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.added, vec!["no.txt".to_string()]);
        // 只添加 ok.txt
        let report = add(
            &repo,
            &AddOptions {
                paths: vec![dir.path().join("ok.txt")],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.added, vec!["ok.txt".to_string()]);
    }

    #[test]
    fn add_update_only_skips_untracked() {
        let (dir, repo) = setup_repo();
        fs::write(dir.path().join("tracked.txt"), "1").unwrap();
        add(&repo, &AddOptions::default()).unwrap();
        fs::write(dir.path().join("tracked.txt"), "2").unwrap();
        fs::write(dir.path().join("untracked.txt"), "3").unwrap();
        let report = add(
            &repo,
            &AddOptions {
                update_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.updated, vec!["tracked.txt".to_string()]);
        assert!(report.added.is_empty());
        let index = repo.index().unwrap();
        assert_eq!(index.entries.len(), 1);
    }

    #[test]
    fn add_dry_run_reports_without_side_effects() {
        // dry-run 必须照常分类（added/updated/deleted），但不写索引、
        // 不把 blob 写进对象库——预览是纯粹的。
        let (dir, repo) = setup_repo();
        fs::write(dir.path().join("tracked.txt"), "1").unwrap();
        fs::write(dir.path().join("gone.txt"), "4").unwrap();
        add(&repo, &AddOptions::default()).unwrap();
        let index_before = repo.index().unwrap();
        let oids_before = repo.store().unwrap().all_oids().unwrap();

        // 修改已跟踪 + 新增未跟踪 + 删除已跟踪
        fs::write(dir.path().join("tracked.txt"), "2").unwrap();
        fs::write(dir.path().join("new.txt"), "3").unwrap();
        fs::remove_file(dir.path().join("gone.txt")).unwrap();

        let report = add(
            &repo,
            &AddOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.added, vec!["new.txt".to_string()]);
        assert_eq!(report.updated, vec!["tracked.txt".to_string()]);
        assert_eq!(report.deleted, vec!["gone.txt".to_string()]);

        // 索引未被触碰
        let index_after = repo.index().unwrap();
        assert_eq!(index_after.entries, index_before.entries);
        // 对象库只含第一次真实 add 写入的对象（dry-run 未写入任何新对象）
        let oids_after = repo.store().unwrap().all_oids().unwrap();
        assert_eq!(oids_after.len(), oids_before.len());
    }

    #[test]
    fn racy_same_size_rewrite_detected() {
        // 同 size 快速改写：文件 mtime 秒可能与索引写盘相同（racy），
        // 此时 stat 缓存不可信，必须重新哈希并检出为 updated。
        let (dir, repo) = setup_repo();
        fs::write(dir.path().join("f.txt"), "1").unwrap();
        add(&repo, &AddOptions::default()).unwrap();
        fs::write(dir.path().join("f.txt"), "2").unwrap();
        let report = add(&repo, &AddOptions::default()).unwrap();
        assert_eq!(report.updated, vec!["f.txt".to_string()]);
        let index = repo.index().unwrap();
        assert_eq!(index.entries.len(), 1);
    }

    #[test]
    fn add_dot_means_whole_workdir() {
        // `riv add .`：仓库根路径等价于整个工作区（含删除暂存）
        let (dir, repo) = setup_repo();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        fs::create_dir_all(dir.path().join("d")).unwrap();
        fs::write(dir.path().join("d/b.txt"), "b").unwrap();
        add(&repo, &AddOptions::default()).unwrap();
        // 删除一个文件后 `add .` → 暂存删除
        fs::remove_file(dir.path().join("d/b.txt")).unwrap();
        let report = add(
            &repo,
            &AddOptions {
                paths: vec![dir.path().to_path_buf()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.deleted, vec!["d/b.txt".to_string()]);
        assert_eq!(repo.index().unwrap().entries.len(), 1);
    }

    #[test]
    fn add_subdirectory_path() {
        let (dir, repo) = setup_repo();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "a").unwrap();
        fs::write(dir.path().join("top.txt"), "t").unwrap();
        let report = add(
            &repo,
            &AddOptions {
                paths: vec![dir.path().join("src")],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.added, vec!["src/a.rs".to_string()]);
        // top.txt 未被添加
        let index = repo.index().unwrap();
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].path, "src/a.rs");
    }
}
