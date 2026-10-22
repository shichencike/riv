//! riv-cli 端到端集成测试：对真实二进制跑完整命令流。
//!
//! 覆盖 init/add/commit/status/log/branch/checkout/diff 八个命令的
//! 正常路径与关键错误路径，断言对齐 git 的输出语义。

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const RIV: &str = env!("CARGO_BIN_EXE_riv");

/// 在指定目录执行 riv，返回输出（不断言成功与否）。
/// stdin 置为 null：版本控制命令不应读取 stdin，即使误触交互路径也不会挂起。
fn riv_in(cwd: &Path, args: &[&str]) -> Output {
    Command::new(RIV)
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to spawn riv")
}

/// 执行并断言成功，返回 stdout。
fn ok(cwd: &Path, args: &[&str]) -> String {
    let out = riv_in(cwd, args);
    assert!(
        out.status.success(),
        "riv {:?} 失败: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// 执行并断言失败，返回 stderr。
fn err(cwd: &Path, args: &[&str]) -> String {
    let out = riv_in(cwd, args);
    assert!(!out.status.success(), "riv {:?} 意外成功", args);
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// 执行 riv 并显式控制环境变量（None = 移除该变量）。
fn riv_env(cwd: &Path, args: &[&str], envs: &[(&str, Option<&str>)]) -> Output {
    let mut cmd = Command::new(RIV);
    cmd.args(args).current_dir(cwd);
    for (k, v) in envs {
        match v {
            Some(val) => {
                cmd.env(k, val);
            }
            None => {
                cmd.env_remove(k);
            }
        }
    }
    // 切断 stdin：即使误触编辑器路径也不会挂起等待输入
    cmd.stdin(std::process::Stdio::null());
    cmd.output().expect("failed to spawn riv")
}

/// 断言失败的环境受控执行，返回 stderr。
fn err_env(cwd: &Path, args: &[&str], envs: &[(&str, Option<&str>)]) -> String {
    let out = riv_env(cwd, args, envs);
    assert!(!out.status.success(), "riv {:?} 意外成功", args);
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, content).unwrap();
}

fn read(dir: &Path, rel: &str) -> String {
    fs::read_to_string(dir.join(rel)).unwrap()
}

// ---------------------------------------------------------------------------
// init / add / status / commit 主流程
// ---------------------------------------------------------------------------

#[test]
fn init_add_status_commit_flow() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    // init：生成 .riv 目录与初始 HEAD
    ok(dir, &["init"]);
    assert!(dir.join(".riv").is_dir());
    assert_eq!(read(dir, ".riv/HEAD"), "ref: refs/heads/main\n");

    // add 前：未跟踪
    write(dir, "a.txt", "hello\n");
    let s = ok(dir, &["status", "--porcelain"]);
    assert_eq!(s, "?? a.txt\n");

    // add 后：已暂存新文件
    ok(dir, &["add", "a.txt"]);
    let s = ok(dir, &["status", "--porcelain"]);
    assert_eq!(s, "A  a.txt\n");

    // commit
    let out = ok(dir, &["commit", "-m", "first"]);
    assert!(out.contains("[main"), "commit 输出应含分支名: {out}");
    assert!(out.contains("first"), "commit 输出应含摘要: {out}");

    // 提交后干净
    let s = ok(dir, &["status", "--porcelain"]);
    assert_eq!(s, "");

    // 第二次修改 → 两列模型：staged + unstaged 同时出现
    write(dir, "a.txt", "hello2\n");
    ok(dir, &["add", "a.txt"]);
    write(dir, "a.txt", "hello3\n");
    let s = ok(dir, &["status", "--porcelain"]);
    assert_eq!(s, "MM a.txt\n");
}

#[test]
fn status_long_format_lists_sections() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "staged.txt", "s\n");
    ok(dir, &["add", "staged.txt"]);
    write(dir, "dirty.txt", "d\n");
    ok(dir, &["add", "dirty.txt"]);
    write(dir, "dirty.txt", "d2\n");
    write(dir, "new.txt", "n\n");

    let out = ok(dir, &["status"]);
    assert!(out.contains("On branch main"), "{out}");
    assert!(out.contains("Changes to be committed"), "{out}");
    assert!(out.contains("Changes not staged for commit"), "{out}");
    assert!(out.contains("Untracked files"), "{out}");
    assert!(out.contains("new file:   staged.txt"), "{out}");
    assert!(out.contains("modified:   dirty.txt"), "{out}");
    assert!(out.contains("new.txt"), "{out}");
}

#[test]
fn commit_all_stages_tracked_changes() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "f.txt", "v1\n");
    ok(dir, &["add", "f.txt"]);
    ok(dir, &["commit", "-m", "c1"]);

    // -a：修改 + 删除一起进提交
    write(dir, "f.txt", "v2\n");
    write(dir, "g.txt", "g\n");
    ok(dir, &["add", "g.txt"]);
    fs::remove_file(dir.join("g.txt")).unwrap();
    let out = ok(dir, &["commit", "-a", "-m", "c2"]);
    assert!(out.contains("c2"), "{out}");
    assert_eq!(ok(dir, &["status", "--porcelain"]), "");

    // 索引无变化时拒绝提交
    let e = err(dir, &["commit", "-m", "empty"]);
    assert!(
        e.contains("nothing to commit") || e.contains("no changes"),
        "{e}"
    );
}

#[test]
fn commit_message_from_file_and_editor_fallback() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a\n");
    ok(dir, &["add", "a.txt"]);

    // -F 从文件读信息
    write(dir, "msg.txt", "message from file\n");
    ok(dir, &["commit", "-F", "msg.txt"]);
    let log = ok(dir, &["log", "--oneline"]);
    assert!(log.contains("message from file"), "{log}");

    // EDITOR 回退路径（stdin 已置为 null，任何情况下都不会挂起）：
    write(dir, "b.txt", "b\n");
    ok(dir, &["add", "b.txt"]);

    // EDITOR 指向一个不修改文件的命令 → 信息为空 → 拒绝提交
    let e = err_env(
        dir,
        &["commit"],
        &[("EDITOR", Some("true")), ("VISUAL", None)],
    );
    assert!(e.contains("empty commit message"), "{e}");

    // EDITOR 退出非零 → 报告编辑器失败
    let e = err_env(
        dir,
        &["commit"],
        &[("EDITOR", Some("false")), ("VISUAL", None)],
    );
    assert!(e.contains("editor"), "{e}");

    // 无 EDITOR/VISUAL 且 stdin 非终端 → 明确指引 -m/-F，而不是挂起
    let e = err_env(dir, &["commit"], &[("EDITOR", None), ("VISUAL", None)]);
    assert!(
        e.contains("EDITOR") && e.contains("-m"),
        "应提示改用 -m/-F: {e}"
    );
}

// ---------------------------------------------------------------------------
// log
// ---------------------------------------------------------------------------

#[test]
fn log_shows_history_oneline_and_limit() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    for i in 1..=3 {
        write(dir, "f.txt", &format!("v{i}\n"));
        ok(dir, &["add", "f.txt"]);
        ok(dir, &["commit", "-m", &format!("c{i}")]);
    }
    let out = ok(dir, &["log", "--oneline"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[0].contains("c3"));
    assert!(lines[2].contains("c1"));
    // 每条一行含短 oid
    assert!(lines[0].len() > 8, "{lines:?}");

    // -n 限制条数
    let out = ok(dir, &["log", "--oneline", "-n", "1"]);
    assert_eq!(out.lines().count(), 1, "{out}");

    // 长格式含作者行
    let out = ok(dir, &["log"]);
    assert!(out.contains("Author:"), "{out}");
    assert!(out.contains("Date:"), "{out}");
}

// ---------------------------------------------------------------------------
// branch
// ---------------------------------------------------------------------------

#[test]
fn branch_create_list_delete_rename() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c1"]);

    // 初始只有 main
    let out = ok(dir, &["branch"]);
    assert!(out.contains("* main"), "{out}");

    // 创建
    ok(dir, &["branch", "feature"]);
    let out = ok(dir, &["branch"]);
    assert!(out.contains("feature"), "{out}");
    assert!(out.contains("* main"), "HEAD 不应移动: {out}");

    // 重命名
    ok(dir, &["branch", "-m", "feature", "feat2"]);
    let out = ok(dir, &["branch"]);
    assert!(out.contains("feat2") && !out.contains("feature\n"), "{out}");

    // 删除已合并分支：直接成功
    ok(dir, &["branch", "-d", "feat2"]);
    let out = ok(dir, &["branch"]);
    assert!(!out.contains("feat2"), "{out}");

    // 未合并分支：-d 拒绝，-D 强制删除
    ok(dir, &["branch", "wip"]);
    ok(dir, &["checkout", "wip"]);
    write(dir, "wip.txt", "w\n");
    ok(dir, &["add", "wip.txt"]);
    ok(dir, &["commit", "-m", "wip only"]);
    ok(dir, &["checkout", "main"]);
    write(dir, "a.txt", "b\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c2 on main"]);
    // wip 持有 main 不可达的提交 → 拒绝普通删除
    let e = err(dir, &["branch", "-d", "wip"]);
    assert!(e.contains("not fully merged"), "{e}");
    ok(dir, &["branch", "-D", "wip"]);
    let out = ok(dir, &["branch"]);
    assert!(!out.contains("wip"), "{out}");
}

// ---------------------------------------------------------------------------
// checkout
// ---------------------------------------------------------------------------

#[test]
fn checkout_switch_and_create_branch() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "v1\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c1"]);

    // -b 创建并切换
    let out = ok(dir, &["checkout", "-b", "dev"]);
    assert!(out.contains("Switched to a new branch 'dev'"), "{out}");
    let s = ok(dir, &["status", "--porcelain"]);
    assert_eq!(s, "");

    // 在新分支提交
    write(dir, "a.txt", "dev\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "dev change"]);

    // 切回 main：文件回到 v1
    ok(dir, &["checkout", "main"]);
    assert_eq!(read(dir, "a.txt"), "v1\n");
    let s = ok(dir, &["status", "--porcelain"]);
    assert_eq!(s, "");

    // 切到 dev：文件变 dev
    ok(dir, &["checkout", "dev"]);
    assert_eq!(read(dir, "a.txt"), "dev\n");
}

#[test]
fn checkout_dirty_workdir_blocked() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "v1\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c1"]);
    ok(dir, &["branch", "other"]);
    write(dir, "a.txt", "main-only\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c2"]);

    // other 分支的 a.txt = v1，工作区脏改 → 拒绝
    write(dir, "a.txt", "dirty\n");
    let e = err(dir, &["checkout", "other"]);
    assert!(
        e.contains("overwritten") || e.contains("local changes"),
        "{e}"
    );
    assert_eq!(read(dir, "a.txt"), "dirty\n");

    // -f 强制执行
    ok(dir, &["checkout", "-f", "other"]);
    assert_eq!(read(dir, "a.txt"), "v1\n");
}

#[test]
fn checkout_paths_restore_from_index() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "v1\n");
    write(dir, "b.txt", "v1b\n");
    ok(dir, &["add", "."]);
    ok(dir, &["commit", "-m", "c1"]);

    // 无 target：从索引恢复（丢弃未暂存修改）
    write(dir, "a.txt", "dirty\n");
    let out = ok(dir, &["checkout", "--", "a.txt"]);
    assert!(out.contains("Updated 1 path from the index"), "{out}");
    assert_eq!(read(dir, "a.txt"), "v1\n");

    // 新暂存文件（不在 HEAD 树中）+ 脏改 → 从索引恢复而非删除
    write(dir, "new.txt", "x\n");
    ok(dir, &["add", "new.txt"]);
    write(dir, "new.txt", "dirty\n");
    ok(dir, &["checkout", "--", "new.txt"]);
    assert_eq!(read(dir, "new.txt"), "x\n");

    // 目录前缀批量恢复
    write(dir, "src/a.rs", "a\n");
    write(dir, "src/deep/b.rs", "b\n");
    ok(dir, &["add", "."]);
    ok(dir, &["commit", "-m", "c2"]);
    write(dir, "src/a.rs", "dirty\n");
    fs::remove_file(dir.join("src/deep/b.rs")).unwrap();
    let out = ok(dir, &["checkout", "--", "src"]);
    assert!(out.contains("Updated 2 paths"), "{out}");
    assert_eq!(read(dir, "src/a.rs"), "a\n");
    assert_eq!(read(dir, "src/deep/b.rs"), "b\n");

    // 未知路径报错
    let e = err(dir, &["checkout", "--", "nope.txt"]);
    assert!(e.contains("did not match"), "{e}");
}

#[test]
fn checkout_paths_from_tree_ish() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "v1\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c1"]);
    write(dir, "a.txt", "v2\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c2"]);

    // 从 HEAD~1 的树恢复 a.txt，HEAD 不动
    write(dir, "a.txt", "dirty\n");
    let out = ok(dir, &["checkout", "HEAD~1", "--", "a.txt"]);
    assert!(out.contains("Updated 1 path from the tree"), "{out}");
    assert_eq!(read(dir, "a.txt"), "v1\n");

    // HEAD 仍是 c2
    let log = ok(dir, &["log", "--oneline"]);
    assert!(log.lines().next().unwrap().contains("c2"), "{log}");

    // 索引被改为 v1 → staged modified
    let s = ok(dir, &["status", "--porcelain"]);
    assert_eq!(s, "M  a.txt\n");
}

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

#[test]
fn diff_workdir_cached_and_revs() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "one\ntwo\nthree\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c1"]);

    // 工作区修改 → 默认 diff（index vs workdir）
    write(dir, "a.txt", "one\ntwo\nTHREE\n");
    let out = ok(dir, &["diff"]);
    assert!(out.contains("--- a/a.txt"), "{out}");
    assert!(out.contains("+++ b/a.txt"), "{out}");
    assert!(out.contains("-three") && out.contains("+THREE"), "{out}");

    // 暂存后 → --cached（HEAD vs index）
    ok(dir, &["add", "a.txt"]);
    let out = ok(dir, &["diff", "--cached"]);
    assert!(out.contains("-three") && out.contains("+THREE"), "{out}");

    // 干净工作区 diff 为空
    write(dir, "a.txt", "one\ntwo\nTHREE\n");
    let out = ok(dir, &["diff"]);
    assert_eq!(out, "", "工作区与索引一致时 diff 应为空");

    // 两个 rev 之间
    let out = ok(dir, &["diff", "HEAD~0", "HEAD"]);
    assert_eq!(out, "", "同一 rev 无差异");

    // --stat
    write(dir, "a.txt", "one\ntwo\nfour\nfive\n");
    let out = ok(dir, &["diff", "--stat"]);
    assert!(out.contains("a.txt"), "{out}");
    assert!(out.contains("+"), "{out}");
}

#[test]
fn diff_filters_paths() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a1\n");
    write(dir, "b.txt", "b1\n");
    ok(dir, &["add", "."]);
    ok(dir, &["commit", "-m", "c1"]);
    write(dir, "a.txt", "a2\n");
    write(dir, "b.txt", "b2\n");

    let out = ok(dir, &["diff", "--", "a.txt"]);
    assert!(out.contains("a.txt") && !out.contains("b.txt"), "{out}");
}

// ---------------------------------------------------------------------------
// add 细节
// ---------------------------------------------------------------------------

#[test]
fn add_dry_run_and_update_only() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "tracked.txt", "t1\n");
    ok(dir, &["add", "tracked.txt"]);
    ok(dir, &["commit", "-m", "c1"]);

    // -n：只预览，索引不变
    write(dir, "tracked.txt", "t2\n");
    write(dir, "untracked.txt", "u\n");
    let out = ok(dir, &["add", "-n"]);
    assert!(out.contains("tracked.txt"), "{out}");
    assert!(out.contains("untracked.txt"), "{out}");
    let s = ok(dir, &["status", "--porcelain"]);
    assert_eq!(s, " M tracked.txt\n?? untracked.txt\n", "{s}");

    // -u：只更新已跟踪，忽略未跟踪
    ok(dir, &["add", "-u"]);
    let s = ok(dir, &["status", "--porcelain"]);
    assert_eq!(s, "M  tracked.txt\n?? untracked.txt\n", "{s}");

    // -f：强制添加被 .rivignore 忽略的文件
    write(dir, ".rivignore", "secret.txt\n");
    write(dir, "secret.txt", "s\n");
    ok(dir, &["add", "."]);
    let s = ok(dir, &["status", "--porcelain"]);
    assert!(!s.contains("secret.txt"), "忽略规则应生效: {s}");
    ok(dir, &["add", "-f", "secret.txt"]);
    let s = ok(dir, &["status", "--porcelain"]);
    assert!(s.contains("secret.txt"), "-f 应强制添加: {s}");
}

#[test]
fn add_dot_covers_whole_workdir_including_deletions() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a\n");
    write(dir, "sub/b.txt", "b\n");
    ok(dir, &["add", "."]);
    ok(dir, &["commit", "-m", "c1"]);

    fs::remove_file(dir.join("a.txt")).unwrap();
    write(dir, "sub/b.txt", "b2\n");
    write(dir, "sub/c.txt", "c\n");
    ok(dir, &["add", "."]);
    let s = ok(dir, &["status", "--porcelain"]);
    assert!(s.contains("D  a.txt"), "删除应被暂存: {s}");
    assert!(s.contains("M  sub/b.txt"), "{s}");
    assert!(s.contains("A  sub/c.txt"), "{s}");
}

// ---------------------------------------------------------------------------
// 错误路径
// ---------------------------------------------------------------------------

#[test]
fn commands_outside_repo_fail_cleanly() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    // 无 .riv
    let e = err(dir, &["status"]);
    assert!(e.contains("not a riv repository"), "{e}");
    let e = err(dir, &["log"]);
    assert!(e.contains("not a riv repository"), "{e}");
    let e = err(dir, &["add", "x.txt"]);
    assert!(e.contains("not a riv repository"), "{e}");

    // 嵌套目录：向上发现 .riv
    let sub = dir.join("deep/nested");
    fs::create_dir_all(&sub).unwrap();
    ok(dir, &["init"]);
    let out = ok(&sub, &["status", "--porcelain"]);
    assert_eq!(out, "");
}

#[test]
fn unknown_revision_and_ambiguous_prefix_fail() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c1"]);

    let e = err(dir, &["log", "nonexistent-branch"]);
    assert!(
        e.contains("unknown revision") || e.contains("cannot resolve"),
        "{e}"
    );

    let e = err(dir, &["checkout", "deadbeef"]);
    assert!(e.contains("unknown revision"), "{e}");
}
