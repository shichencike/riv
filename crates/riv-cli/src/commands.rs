//! riv 各子命令的实现：参数解析后的仓库操作与输出格式化。
//!
//! 所有命令（除 init）都通过 `Repo::discover` 定位仓库，
//! 业务逻辑全部委托 riv-core，本模块只负责编排与展示。

use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcCommand;

use riv_core::auth;
use riv_core::branch as branch_ops;
use riv_core::checkout::{self, CheckoutOptions};
use riv_core::commit::{self, CommitOptions};
use riv_core::filediff;
use riv_core::log as log_ops;
use riv_core::object::Object;
use riv_core::repo::Repo;
use riv_core::revparse;
use riv_core::snapshot::TreeMap;
use riv_core::snapshot::{self, AddOptions};
use riv_core::status::{self, Status};
use riv_core::treediff::{self, diff_tree_maps};
use riv_core::{Error, Oid, Result};

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

pub fn init(path: Option<&Path>, branch: &str) -> Result<()> {
    let dir = path.unwrap_or_else(|| Path::new("."));
    let repo = Repo::init(dir, Some(branch))?;
    println!(
        "Initialized empty riv repository in {}/",
        repo.riv_dir.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

pub fn add(paths: &[PathBuf], all: bool, update: bool, force: bool, dry_run: bool) -> Result<()> {
    let repo = discover()?;
    let opts = AddOptions {
        paths: paths.to_vec(),
        all,
        update_only: update,
        force,
        dry_run,
    };
    let report = snapshot::add(&repo, &opts)?;
    if dry_run {
        for p in &report.added {
            println!("add '{p}'");
        }
        for p in &report.updated {
            println!("update '{p}'");
        }
        for p in &report.deleted {
            println!("remove '{p}'");
        }
        if report.is_empty() {
            println!("nothing to add");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// commit
// ---------------------------------------------------------------------------

pub fn commit(
    message: Option<&str>,
    file: Option<&Path>,
    all: bool,
    allow_empty: bool,
) -> Result<()> {
    let repo = discover()?;
    // -a：先自动暂存已跟踪文件的修改与删除（等价 riv add -u）
    if all {
        snapshot::add(
            &repo,
            &AddOptions {
                update_only: true,
                ..Default::default()
            },
        )?;
    }
    let msg = match (message, file) {
        (Some(m), _) => m.to_string(),
        (None, Some(f)) => read_message_file(f)?,
        (None, None) => edit_message()?,
    };
    // 提交前的 HEAD 树（用于统计变更文件数）
    let old_tree = repo.head_tree()?;
    let out = commit::commit(
        &repo,
        &CommitOptions {
            message: msg,
            allow_empty,
            author: None,
        },
    )?;
    let branch = repo
        .refs()
        .current_branch()?
        .unwrap_or_else(|| "(detached)".to_string());
    let branch_disp = if out.parents == 0 {
        format!("{branch} (root-commit)")
    } else {
        branch
    };
    let changed = treediff::diff_trees(&repo.store()?, old_tree, Some(out.commit.tree))?.len();
    println!(
        "[{} {}] {}",
        branch_disp,
        out.oid.short(7),
        out.commit.summary()
    );
    if changed > 0 {
        println!(
            " {changed} file{} changed",
            if changed == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// 从文件读取提交信息（`-` 表示 stdin）。
fn read_message_file(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| Error::msg(format!("cannot read stdin: {e}")))?;
        Ok(buf)
    } else {
        fs::read_to_string(path)
            .map_err(|e| Error::msg(format!("cannot read {}: {e}", path.display())))
    }
}

/// 打开 $EDITOR 编辑提交信息（无 -m/-F 时的回退）。
fn edit_message() -> Result<String> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .ok();
    let editor = match editor {
        Some(e) => e,
        None => {
            // 与 git 类似：无 EDITOR/VISUAL 时回退平台缺省编辑器。但在非交互
            // 环境（CI、管道、测试）里编辑器只会挂起等待永不到来的输入，
            // 因此直接给出可操作的错误，指引改用 -m/-F。
            if !std::io::stdin().is_terminal() {
                return Err(Error::msg(
                    "no EDITOR/VISUAL is set and stdin is not a terminal; \
                     use -m <message> or -F <file> instead",
                ));
            }
            // Unix 回退 vi，Windows 回退 notepad（git 同思路）
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        }
    };
    let tmp = std::env::temp_dir().join(format!("riv-commit-{}.txt", std::process::id()));
    let template = "\n# 请填写提交信息；以 # 开头的行将被忽略。\n";
    fs::write(&tmp, template).map_err(|e| Error::msg(format!("cannot write temp file: {e}")))?;
    let status = ProcCommand::new(&editor)
        .arg(&tmp)
        .status()
        .map_err(|e| Error::msg(format!("cannot launch editor '{editor}': {e}")))?;
    if !status.success() {
        let _ = fs::remove_file(&tmp);
        return Err(Error::msg("editor exited with failure"));
    }
    let text =
        fs::read_to_string(&tmp).map_err(|e| Error::msg(format!("cannot read temp file: {e}")))?;
    let _ = fs::remove_file(&tmp);
    // 剥离注释行
    let stripped: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();
    Ok(stripped.join("\n"))
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

pub fn status(porcelain: bool) -> Result<()> {
    let repo = discover()?;
    let s = status::status(&repo)?;
    if porcelain {
        for line in status::porcelain(&s) {
            println!("{line}");
        }
        return Ok(());
    }
    print_long_status(&s);
    Ok(())
}

fn print_long_status(s: &Status) {
    println!("On branch {}", s.branch);
    if s.head.is_none() {
        println!();
        println!("No commits yet");
    }

    if !s.staged.is_empty() {
        println!();
        println!("Changes to be committed:");
        for (path, kind) in &s.staged {
            let label = match kind {
                status::StagedStatus::New => "new file:  ",
                status::StagedStatus::Modified => "modified:  ",
                status::StagedStatus::Deleted => "deleted:   ",
                status::StagedStatus::TypeChanged => "typechange:",
            };
            println!("\t{label} {path}");
        }
    }

    if !s.unstaged.is_empty() {
        println!();
        println!("Changes not staged for commit:");
        println!("  (use \"riv add <file>...\" to update what will be committed)");
        for (path, kind) in &s.unstaged {
            let label = match kind {
                status::UnstagedStatus::Modified => "modified:  ",
                status::UnstagedStatus::Deleted => "deleted:   ",
                status::UnstagedStatus::TypeChanged => "typechange:",
            };
            println!("\t{label} {path}");
        }
    }

    if !s.untracked.is_empty() {
        println!();
        println!("Untracked files:");
        println!("  (use \"riv add <file>...\" to include in what will be committed)");
        for path in &s.untracked {
            println!("\t{path}");
        }
    }

    if s.is_clean() {
        println!();
        if s.head.is_none() {
            println!("nothing to commit (create/copy files and use \"riv add\" to track)");
        } else {
            println!("nothing to commit, working tree clean");
        }
    }
}

// ---------------------------------------------------------------------------
// log
// ---------------------------------------------------------------------------

pub fn log(rev: Option<&str>, oneline: bool, max_count: usize) -> Result<()> {
    let repo = discover()?;
    let start = match rev {
        Some(spec) => Some(revparse::resolve(&repo, spec)?),
        None => None,
    };
    // -n N 直接透传：N=0 表示显示 0 条（对齐 git log -n 0 输出为空、退出 0），
    // 而不是「不限」——省略 -n 才是不限。
    let entries = log_ops::log(&repo, start, Some(max_count))?;
    if entries.is_empty() && max_count > 0 {
        return Err(Error::InvalidRevision(
            "your current branch does not have any commits yet".into(),
        ));
    }
    for e in &entries {
        if oneline {
            println!("{} {}", e.oid.short(7), e.commit.summary());
        } else {
            println!("commit {}", e.oid);
            if e.commit.parents.len() > 1 {
                let parents: Vec<String> = e.commit.parents.iter().map(|p| p.short(7)).collect();
                println!("Merge: {}", parents.join(" "));
            }
            println!(
                "Author: {} <{}>",
                e.commit.author.name, e.commit.author.email
            );
            println!("Date:   {}", e.commit.author.format_time());
            println!();
            for line in e.commit.message.lines() {
                println!("    {line}");
            }
            println!();
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// branch
// ---------------------------------------------------------------------------

pub fn branch(
    name: Option<&str>,
    start_or_new: Option<&str>,
    delete: Option<&str>,
    force_delete: Option<&str>,
    rename: bool,
) -> Result<()> {
    let repo = discover()?;

    if rename {
        let (old, new) = match (name, start_or_new) {
            (Some(o), Some(n)) => (o.to_string(), n.to_string()),
            (Some(n), None) => {
                // -m <new>：重命名当前分支
                let cur = repo
                    .refs()
                    .current_branch()?
                    .ok_or_else(|| Error::Msg("cannot rename in detached HEAD".into()))?;
                branch_ops::rename(&repo, &cur, n)?;
                println!("Renamed {cur} to {n}");
                return Ok(());
            }
            _ => return Err(Error::Msg("usage: riv branch -m <old> <new>".into())),
        };
        branch_ops::rename(&repo, &old, &new)?;
        println!("Renamed {old} to {new}");
        return Ok(());
    }

    if let Some(d) = delete.or(force_delete) {
        let force = force_delete.is_some();
        branch_ops::delete(&repo, d, force)?;
        println!("Deleted branch {d}");
        return Ok(());
    }

    match name {
        None => {
            let branches = branch_ops::list(&repo)?;
            if branches.is_empty() {
                println!("(no branches)");
                return Ok(());
            }
            for b in &branches {
                let marker = if b.is_current { "* " } else { "  " };
                println!("{marker}{} {}", b.name, b.oid.short(7));
                if !b.summary.is_empty() {
                    println!("    {}", b.summary);
                }
            }
        }
        Some(n) => {
            let oid = branch_ops::create(&repo, n, start_or_new)?;
            println!("Created branch {n} at {}", oid.short(7));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// checkout
// ---------------------------------------------------------------------------

pub fn checkout(
    target: Option<&str>,
    create: Option<&str>,
    start_point: Option<&str>,
    force: bool,
    paths: &[PathBuf],
) -> Result<()> {
    let repo = discover()?;

    // -b：创建并切换
    if let Some(new_branch) = create {
        branch_ops::create(&repo, new_branch, start_point)?;
        checkout::checkout(
            &repo,
            Some(new_branch),
            &CheckoutOptions {
                paths: Vec::new(),
                force,
            },
        )?;
        let start = start_point.unwrap_or("HEAD");
        println!("Switched to a new branch '{new_branch}' (from {start})");
        return Ok(());
    }

    let opts = CheckoutOptions {
        paths: paths.to_vec(),
        force,
    };
    let report = checkout::checkout(&repo, target, &opts)?;

    if !opts.paths.is_empty() {
        // 路径恢复模式：无 target 时源是索引（git `checkout -- <path>` 语义），
        // 有 target 时源是目标树（git `checkout <tree-ish> -- <path>` 语义）。
        let n = report.updated.len();
        if n == 0 && report.removed.is_empty() {
            println!("no changes");
        } else {
            let source = if target.is_some() {
                "the tree"
            } else {
                "the index"
            };
            println!(
                "Updated {n} path{} from {source}",
                if n == 1 { "" } else { "s" }
            );
        }
        return Ok(());
    }

    match &report.switched_to {
        Some(b) => println!("Switched to branch '{b}'"),
        None => {
            let head = repo
                .refs()
                .resolve_head()?
                .ok_or_else(|| Error::InvalidRevision("HEAD points to no commit".into()))?;
            let summary = repo
                .store()?
                .read(&head)?
                .as_commit()
                .map(|c| c.summary().to_string())
                .unwrap_or_default();
            println!("HEAD is now at {} {summary}", head.short(7));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

pub fn diff(revs: &[String], cached: bool, stat_only: bool, paths: &[PathBuf]) -> Result<()> {
    let repo = discover()?;
    let store = repo.store()?;

    let diffs: Vec<filediff::FileDiff> = match (revs.len(), cached) {
        (0, false) => diff_workdir_vs_index(&repo, paths)?,
        (0, true) => diff_head_vs_index(&repo, paths)?,
        (1, false) => {
            let oid = revparse::resolve(&repo, &revs[0])?;
            diff_tree_vs_workdir(&repo, Some(oid), paths)?
        }
        (1, true) => diff_head_vs_index(&repo, paths)?,
        (2, _) => {
            let a = revparse::resolve(&repo, &revs[0])?;
            let b = revparse::resolve(&repo, &revs[1])?;
            let ta = commit_tree(&repo, a)?;
            let tb = commit_tree(&repo, b)?;
            let changes = treediff::diff_trees(&store, Some(ta), Some(tb))?;
            changes
                .into_iter()
                .filter(|c| path_matches(&c.path, paths, &repo))
                .filter_map(|c| filediff::diff_blobs(&store, &c.path, c.old, c.new).transpose())
                .collect::<Result<Vec<_>>>()?
        }
        _ => return Err(Error::Msg("too many revisions".into())),
    };

    if diffs.is_empty() {
        return Ok(());
    }
    if stat_only {
        print!("{}", filediff::render_stat(&diffs));
    } else {
        print!("{}", filediff::render(&diffs));
    }
    Ok(())
}

/// 工作区 vs 索引（`riv diff` 默认）。
fn diff_workdir_vs_index(repo: &Repo, paths: &[PathBuf]) -> Result<Vec<filediff::FileDiff>> {
    // 索引只加载一次：逐路径各自 repo.index() 会把同一文件反复解码（O(n²) IO）
    let index = repo.index()?;
    let mut out = Vec::new();
    for entry in &index.entries {
        if let Some(d) = filediff::diff_index_vs_workdir(repo, &index, &entry.path)? {
            out.push(d);
        }
    }
    filter_by_paths(out, paths, repo)
}

/// HEAD 树 vs 索引（`riv diff --cached`）。
fn diff_head_vs_index(repo: &Repo, paths: &[PathBuf]) -> Result<Vec<filediff::FileDiff>> {
    let store = repo.store()?;
    let index = repo.index()?;
    let head_tree = match repo.head_tree()? {
        Some(t) => Some(riv_core::snapshot::flatten_tree(&store, Some(t))?),
        None => None,
    };
    let index_map: TreeMap = index
        .entries
        .iter()
        .map(|e| (e.path.clone(), (e.oid, e.mode)))
        .collect();
    let changes = diff_tree_maps(&head_tree.unwrap_or_default(), &index_map);
    let mut out = Vec::new();
    for c in changes {
        if let Some(d) = filediff::diff_blobs(&store, &c.path, c.old, c.new)? {
            out.push(d);
        }
    }
    filter_by_paths(out, paths, repo)
}

/// 树 vs 工作区（`riv diff <rev>`）。
///
/// 除目标树的路径外，**索引中新增的路径**（该 rev 之后 `add` 进去的）也要
/// 出现在 diff 里——git 的 `git diff <rev>` 语义是「rev 与工作区的全部
/// 已跟踪内容」之比，漏掉它们会让暂存的新文件看起来不存在。
fn diff_tree_vs_workdir(
    repo: &Repo,
    rev: Option<Oid>,
    paths: &[PathBuf],
) -> Result<Vec<filediff::FileDiff>> {
    let store = repo.store()?;
    let tree = match rev {
        Some(oid) => Some(commit_tree(repo, oid)?),
        None => repo.head_tree()?,
    };
    let flat = match tree {
        Some(t) => Some(riv_core::snapshot::flatten_tree(&store, Some(t))?),
        None => None,
    };
    let flat = flat.unwrap_or_default();
    let index = repo.index()?;
    let mut out = Vec::new();
    for (path, (oid, mode)) in &flat {
        let abs = repo.workdir.join(path);
        if let Some(d) = filediff::diff_blob_workdir(&store, path, Some((*oid, *mode)), &abs)? {
            out.push(d);
        }
    }
    // 索引有、目标树没有的路径：以「新增文件」身份参与比较
    for entry in &index.entries {
        if !flat.contains_key(&entry.path) {
            let abs = repo.workdir.join(&entry.path);
            if let Some(d) = filediff::diff_blob_workdir(&store, &entry.path, None, &abs)? {
                out.push(d);
            }
        }
    }
    filter_by_paths(out, paths, repo)
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

/// 路径过滤：空 pathspec 全收；否则精确匹配或**目录前缀**匹配
/// （`riv diff -- src` 应收 `src/` 下所有文件，与 checkout/add 的前缀语义一致）。
fn path_matches(path: &str, paths: &[PathBuf], repo: &Repo) -> bool {
    if paths.is_empty() {
        return true;
    }
    paths.iter().any(|p| {
        repo.to_repo_relative(p)
            .map(|rel| {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                path == rel_str || path.starts_with(&format!("{rel_str}/"))
            })
            .unwrap_or(false)
    })
}

fn filter_by_paths(
    diffs: Vec<filediff::FileDiff>,
    paths: &[PathBuf],
    repo: &Repo,
) -> Result<Vec<filediff::FileDiff>> {
    if paths.is_empty() {
        return Ok(diffs);
    }
    Ok(diffs
        .into_iter()
        .filter(|d| path_matches(&d.path, paths, repo))
        .collect())
}

// ---------------------------------------------------------------------------
// auth（token 凭证管理）
// ---------------------------------------------------------------------------

/// `riv auth login`：隐藏输入并保存 token（优先系统钥匙串，失败降级文件）。
pub fn auth_login(host: Option<&str>) -> Result<()> {
    let host = resolve_host(host);
    let cred_path = auth::credentials_path()?;
    if let Some(existing) = auth::get_token_with(&host, &cred_path)? {
        println!(
            "Replacing existing token for {} (from {}).",
            host,
            existing.source.label()
        );
    }
    let token = if io::stdin().is_terminal() {
        // 交互终端：隐藏输入（rpassword 走 /dev/tty，屏幕不回显）
        rpassword::prompt_password(format!("Token for {} (input hidden): ", host))
            .map_err(|e| Error::msg(format!("failed to read token: {e}")))?
    } else {
        // 非交互（管道 / 重定向）：无 TTY 可隐藏，直接从 stdin 读一行。
        // CI 场景更推荐用 RIV_TOKEN 环境变量（不落盘）。
        print!("Token for {} (from stdin): ", host);
        io::stdout().flush().ok();
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| Error::msg(format!("failed to read token: {e}")))?;
        line
    };
    let source = auth::set_token_with(&host, &token, &cred_path)?;
    match source {
        auth::TokenSource::Keyring => println!("Token for {} saved to system keyring.", host),
        auth::TokenSource::File => println!(
            "System keyring unavailable; token for {} saved to {} (mode 600).",
            host,
            cred_path.display()
        ),
        auth::TokenSource::Env => {}
    }
    if auth::token_from_env().is_some() {
        println!(
            "Note: {} is set and takes precedence over the saved token.",
            auth::ENV_TOKEN
        );
    }
    Ok(())
}

/// `riv auth status`：三来源状态 + 当前生效项（token 脱敏展示）。
pub fn auth_status(host: Option<&str>) -> Result<()> {
    let host = resolve_host(host);
    let cred_path = auth::credentials_path()?;
    let st = auth::status_with(&host, &cred_path)?;

    println!("Token status for {}:", host);
    print_state("environment variable RIV_TOKEN", &st.env);
    print_state("system keyring", &st.keyring);
    let file_label = format!("credentials file ({})", cred_path.display());
    print_state(&file_label, &st.file);
    println!();
    match st.effective() {
        Some((src, token)) => println!(
            "Effective token: {} (from {})",
            auth::mask_token(token),
            src.label()
        ),
        None => println!("No token configured. Run `riv auth login` to save one."),
    }
    Ok(())
}

/// `riv auth logout`：清除钥匙串与凭证文件中的 token（环境变量需用户自行 unset）。
pub fn auth_logout(host: Option<&str>) -> Result<()> {
    let host = resolve_host(host);
    let cred_path = auth::credentials_path()?;
    let cleared = auth::delete_token_with(&host, &cred_path)?;
    if cleared.is_empty() {
        println!(
            "No saved token found for {} (checked system keyring and credentials file).",
            host
        );
    } else {
        let labels: Vec<&str> = cleared.iter().map(|s| s.label()).collect();
        println!("Removed token for {} from: {}.", host, labels.join(", "));
    }
    if auth::token_from_env().is_some() {
        println!(
            "Note: {} is set in your shell environment; riv cannot unset it. Run `unset {}` to clear.",
            auth::ENV_TOKEN,
            auth::ENV_TOKEN
        );
    }
    Ok(())
}

/// 解析目标主机：`--host` > `$RIV_BASE_URL` > GitHub 预设。
fn resolve_host(host: Option<&str>) -> String {
    match host {
        Some(h) => auth::normalize_host(h),
        None => auth::default_host(),
    }
}

fn print_state(label: &str, state: &auth::SourceState) {
    let desc = match state {
        auth::SourceState::Present(t) => format!("set ({})", auth::mask_token(t)),
        auth::SourceState::Absent => "not set".to_string(),
        auth::SourceState::Unavailable => "unavailable".to_string(),
    };
    println!("  {label:<36} {desc}");
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn discover() -> Result<Repo> {
    Repo::discover(None)
}
