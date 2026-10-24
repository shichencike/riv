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

    // EDITOR 回退路径（stdin 已置为 null，任何情况下都不会挂起）。
    // 注：Unix 用 true/false 这两个空操作程序验证「空信息拒绝」与
    // 「编辑器失败传播」；Windows 没有等价的内置非 GUI 程序
    // （notepad 会弹窗，无头环境无法驱动），这两个子用例仅在 Unix 类平台运行。
    #[cfg(unix)]
    {
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
    }

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

    // -n 0：输出为空但退出码为 0（对齐 git log -n 0，而非「不限」）
    let out = ok(dir, &["log", "--oneline", "-n", "0"]);
    assert_eq!(out, "", "-n 0 应为空输出: {out:?}");

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

#[test]
fn diff_cached_conflicts_with_explicit_rev() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a1\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c1"]);

    // --cached 与显式修订语义重叠，clap 层直接互斥
    let se = err(dir, &["diff", "--cached", "HEAD"]);
    assert!(
        se.contains("cannot be used with") || se.contains("conflicts"),
        "got: {se}"
    );
    // --staged 是 --cached 的别名，同样互斥
    let se2 = err(dir, &["diff", "--staged", "HEAD"]);
    assert!(
        se2.contains("cannot be used with") || se2.contains("conflicts"),
        "got: {se2}"
    );
}

#[test]
fn diff_rev_includes_index_added_paths() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a1\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c1"]);

    // c1 之后 add 的新文件：进了索引但不在 HEAD 树里。
    // git 的 `git diff HEAD` 会显示它，riv 也必须显示（曾漏掉索引新增路径）。
    write(dir, "added.txt", "new\n");
    ok(dir, &["add", "added.txt"]);
    let out = ok(dir, &["diff", "HEAD"]);
    assert!(out.contains("added.txt"), "索引新增文件应出现: {out}");
    assert!(out.contains("new file"), "应标记为新文件: {out}");

    // 工作区再改一个已跟踪文件：两种差异同时出现
    write(dir, "a.txt", "a2\n");
    let out = ok(dir, &["diff", "HEAD"]);
    assert!(out.contains("added.txt") && out.contains("a.txt"), "{out}");
}

#[test]
fn diff_directory_pathspec_matches_prefix() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "src/lib.rs", "fn a() {}\n");
    write(dir, "src/util/mod.rs", "fn b() {}\n");
    write(dir, "README.md", "# readme\n");
    ok(dir, &["add", "."]);
    ok(dir, &["commit", "-m", "c1"]);
    write(dir, "src/lib.rs", "fn a2() {}\n");
    write(dir, "src/util/mod.rs", "fn b2() {}\n");
    write(dir, "README.md", "# readme2\n");

    // 目录 pathspec：只收 src/ 前缀（含嵌套子目录），不收 README.md
    let out = ok(dir, &["diff", "--", "src"]);
    assert!(out.contains("src/lib.rs"), "{out}");
    assert!(out.contains("src/util/mod.rs"), "{out}");
    assert!(!out.contains("README.md"), "目录前缀不应收其他文件: {out}");
}

#[cfg(unix)]
#[test]
fn diff_shows_real_modes() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "script.sh", "#!/bin/sh\necho hi\n");
    ok(dir, &["add", "script.sh"]);
    ok(dir, &["commit", "-m", "c1"]);

    // 内容不变、只加可执行位：输出 old/new mode，无 hunk
    let abs = dir.join("script.sh");
    fs::set_permissions(&abs, fs::Permissions::from_mode(0o755)).unwrap();
    let out = ok(dir, &["diff"]);
    assert!(out.contains("old mode 100644"), "{out}");
    assert!(out.contains("new mode 100755"), "{out}");
    assert!(!out.contains("@@"), "mode-only 变更不应有 hunk: {out}");

    // 新文件带可执行位：new file mode 行反映真实 mode
    write(dir, "run.sh", "#!/bin/sh\necho run\n");
    fs::set_permissions(dir.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    ok(dir, &["add", "run.sh"]);
    let out = ok(dir, &["diff", "--cached"]);
    assert!(out.contains("new file mode 100755"), "{out}");
}

#[test]
fn checkout_create_conflicts_with_paths() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a1\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c1"]);

    // -b 建分支与 pathspec 恢复文件是两个互斥意图，clap 层直接报错
    let se = err(dir, &["checkout", "-b", "feature", "--", "a.txt"]);
    assert!(
        se.contains("cannot be used with") || se.contains("conflicts"),
        "got: {se}"
    );
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

// ---------------------------------------------------------------------------
// auth（token 凭证管理）
// ---------------------------------------------------------------------------

/// auth 测试专用执行器：隔离配置目录（HOME/XDG/APPDATA/USERPROFILE），
/// 默认移除 RIV_TOKEN / RIV_BASE_URL，可选从 stdin 喂入 token。
///
/// 注意：各 auth 测试使用**互不重复的主机名**——Windows 下 dirs 走 KnownFolder
/// 而非 HOME 环境变量，配置目录可能落到真实用户目录，唯一主机名保证并行测试
/// 互不干扰；断言也只依赖 status 输出，不依赖具体存储后端（钥匙串或文件）。
fn riv_auth(
    home: &Path,
    args: &[&str],
    input: Option<&str>,
    extra: &[(&str, Option<&str>)],
) -> Output {
    let mut cmd = Command::new(RIV);
    cmd.args(args).current_dir(home);
    cmd.env("HOME", home);
    cmd.env("XDG_CONFIG_HOME", home.join(".config"));
    cmd.env("APPDATA", home.join("AppData").join("Roaming"));
    cmd.env("USERPROFILE", home);
    cmd.env_remove("RIV_TOKEN");
    cmd.env_remove("RIV_BASE_URL");
    for (k, v) in extra {
        match v {
            Some(val) => {
                cmd.env(k, val);
            }
            None => {
                cmd.env_remove(k);
            }
        }
    }
    if input.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    } else {
        cmd.stdin(std::process::Stdio::null());
    }
    // wait_with_output 只收集已存在的管道：stdout/stderr 必须显式 piped，
    // 否则默认 inherit，输出会漏到测试进程的控制台而 out.stdout 为空
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn riv");
    if let Some(input) = input {
        use std::io::Write as _;
        child
            .stdin
            .as_mut()
            .expect("stdin not piped")
            .write_all(input.as_bytes())
            .expect("failed to write stdin");
    }
    drop(child.stdin.take());
    child.wait_with_output().expect("failed to wait riv")
}

/// auth 执行并断言成功，返回 stdout。
fn auth_ok(home: &Path, args: &[&str], input: Option<&str>) -> String {
    let out = riv_auth(home, args, input, &[]);
    assert!(
        out.status.success(),
        "riv {:?} 失败: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// 完整流程：login（默认主机 github.com）→ status → logout → status。
/// 唯一使用默认主机的测试，其余 auth 测试一律用唯一主机名。
#[test]
fn auth_full_flow_default_host() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();

    // login：token 经管道喂入（非 TTY 路径），落钥匙串或文件（后端无关）
    let out = auth_ok(home, &["auth", "login"], Some("ghp_abcdefgh123456\n"));
    assert!(out.contains("Token for github.com"), "got: {out}");
    assert!(out.contains("saved to"), "got: {out}");

    // status：脱敏展示 + 生效项
    let st = auth_ok(home, &["auth", "status"], None);
    assert!(st.contains("Token status for github.com:"), "got: {st}");
    assert!(st.contains("ghp_****3456"), "got: {st}");
    assert!(st.contains("Effective token"), "got: {st}");

    // logout：清除后 status 显示未配置；重复 logout 无保存凭证
    let out = auth_ok(home, &["auth", "logout"], None);
    assert!(
        out.contains("Removed token for github.com from:"),
        "got: {out}"
    );
    let st = auth_ok(home, &["auth", "status"], None);
    assert!(st.contains("No token configured"), "got: {st}");
    let out = auth_ok(home, &["auth", "logout"], None);
    assert!(out.contains("No saved token found"), "got: {out}");
}

#[test]
fn auth_env_token_takes_precedence() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    // 唯一主机：Windows 下配置目录/钥匙串是进程间共享的（dirs 走 KnownFolder，
    // 不认 HOME 环境变量），唯一主机名保证并行测试互不干扰
    auth_ok(
        home,
        &["auth", "login", "--host", "auth-env-precedence.example.com"],
        Some("ghp_filetoken9999\n"),
    );
    let out = riv_auth(
        home,
        &[
            "auth",
            "status",
            "--host",
            "auth-env-precedence.example.com",
        ],
        None,
        &[("RIV_TOKEN", Some("ghp_envtoken12345"))],
    );
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(
        so.contains("from environment variable RIV_TOKEN"),
        "got: {so}"
    );
    assert!(so.contains("ghp_****2345"), "got: {so}");
    // 清理钥匙串/文件中的条目（真实 Windows 上这是共享的系统存储）
    auth_ok(
        home,
        &[
            "auth",
            "logout",
            "--host",
            "auth-env-precedence.example.com",
        ],
        None,
    );
}

#[test]
fn auth_custom_host_isolated() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    // --host 接受完整 base_url，归一化为主机名（含端口）
    let out = auth_ok(
        home,
        &["auth", "login", "--host", "https://git.example.com:8443"],
        Some("tok8443\n"),
    );
    assert!(out.contains("git.example.com:8443"), "got: {out}");
    let st = auth_ok(
        home,
        &["auth", "status", "--host", "git.example.com:8443"],
        None,
    );
    assert!(
        st.contains("Token status for git.example.com:8443:"),
        "got: {st}"
    );
    assert!(st.contains("Effective token"), "got: {st}");
    // 另一个主机不受影响（隔离性；不用默认主机，避免与并行测试共享 github.com）
    let st2 = auth_ok(
        home,
        &["auth", "status", "--host", "other-isolated.example.org"],
        None,
    );
    assert!(st2.contains("No token configured"), "got: {st2}");
    // 清理钥匙串/文件中的条目（真实 Windows 上这是共享的系统存储）
    auth_ok(
        home,
        &["auth", "logout", "--host", "git.example.com:8443"],
        None,
    );
}

#[test]
fn auth_login_empty_token_fails() {
    let tmp = TempDir::new().unwrap();
    let out = riv_auth(tmp.path(), &["auth", "login"], Some("\n"), &[]);
    assert!(!out.status.success(), "空 token 不应成功");
    let se = String::from_utf8_lossy(&out.stderr);
    assert!(se.contains("token must not be empty"), "got: {se}");
}

#[test]
fn auth_logout_notes_unsettable_env() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    auth_ok(
        home,
        &["auth", "login", "--host", "auth-logout-env.example.com"],
        Some("ghp_envnote12345\n"),
    );
    // logout 时环境里仍有 RIV_TOKEN：必须提示用户自行 unset
    let out = riv_auth(
        home,
        &["auth", "logout", "--host", "auth-logout-env.example.com"],
        None,
        &[("RIV_TOKEN", Some("ghp_fromenv12345"))],
    );
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(so.contains("unset RIV_TOKEN"), "got: {so}");
}

// ---------------------------------------------------------------------------
// 本轮加固新增：git 行为对拍回归
// ---------------------------------------------------------------------------

#[test]
fn commit_honors_git_author_env() {
    // 与 git 一致：GIT_AUTHOR_NAME/EMAIL 覆盖 config 身份（CI 无法改 config
    // 时这是唯一注入身份的方式）。
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a\n");
    ok(dir, &["add", "a.txt"]);
    let out = riv_env(
        dir,
        &["commit", "-m", "by env"],
        &[
            ("GIT_AUTHOR_NAME", Some("Env Author")),
            ("GIT_AUTHOR_EMAIL", Some("env@example.com")),
        ],
    );
    assert!(
        out.status.success(),
        "commit 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = ok(dir, &["log"]);
    assert!(
        log.contains("Author: Env Author <env@example.com>"),
        "env 身份未生效: {log}"
    );
}

#[test]
fn commit_empty_author_env_errors() {
    // git 语义：环境变量一旦设置（哪怕是空串）就采用其值——空 name 直接
    // 报错，而不是静默回落 config（否则 CI 里误设空变量会静默产出
    // 错误身份的提交）。
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a\n");
    ok(dir, &["add", "a.txt"]);
    let se = err_env(
        dir,
        &["commit", "-m", "empty env"],
        &[
            ("GIT_AUTHOR_NAME", Some("")),
            ("GIT_AUTHOR_EMAIL", Some("")),
        ],
    );
    assert!(
        se.contains("empty ident name (for <>) not allowed"),
        "got: {se}"
    );
}

#[test]
fn commit_whitespace_only_author_name_errors() {
    // 纯空白 name：git 报 "name consists only of disallowed characters"
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a\n");
    ok(dir, &["add", "a.txt"]);
    let se = err_env(
        dir,
        &["commit", "-m", "ws env"],
        &[("GIT_AUTHOR_NAME", Some("   "))],
    );
    assert!(
        se.contains("name consists only of disallowed characters"),
        "got: {se}"
    );
}

#[test]
fn commit_author_env_is_crud_ed() {
    // git ident.c 的 crud：首尾空白去掉、`<` `>` `\n` 剔除
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a\n");
    ok(dir, &["add", "a.txt"]);
    let out = riv_env(
        dir,
        &["commit", "-m", "crud"],
        &[
            ("GIT_AUTHOR_NAME", Some("  J<o>hn  ")),
            ("GIT_AUTHOR_EMAIL", Some("  j@x.io  ")),
        ],
    );
    assert!(out.status.success());
    let log = ok(dir, &["log"]);
    assert!(
        log.contains("Author: John <j@x.io>"),
        "crud 后的身份不符: {log}"
    );
}

#[test]
fn commit_empty_email_env_allowed() {
    // git 对空 email 不做校验：提交成功，Author 行 email 为空
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a\n");
    ok(dir, &["add", "a.txt"]);
    let out = riv_env(
        dir,
        &["commit", "-m", "no email"],
        &[
            ("GIT_AUTHOR_NAME", Some("No Email")),
            ("GIT_AUTHOR_EMAIL", Some("")),
        ],
    );
    assert!(out.status.success());
    let log = ok(dir, &["log"]);
    assert!(
        log.contains("Author: No Email <>"),
        "空 email 应被接受: {log}"
    );
}

#[test]
fn commit_committer_env_fallback() {
    // 只设 GIT_COMMITTER_* 时同样生效（riv 作者=提交者，取先到者）
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a\n");
    ok(dir, &["add", "a.txt"]);
    let out = riv_env(
        dir,
        &["commit", "-m", "by committer env"],
        &[
            ("GIT_AUTHOR_NAME", None),
            ("GIT_AUTHOR_EMAIL", None),
            ("GIT_COMMITTER_NAME", Some("CI Bot")),
            ("GIT_COMMITTER_EMAIL", Some("ci@example.com")),
        ],
    );
    assert!(out.status.success());
    let log = ok(dir, &["log"]);
    assert!(
        log.contains("Author: CI Bot <ci@example.com>"),
        "committer env 未生效: {log}"
    );
}

#[test]
fn commit_author_env_beats_committer_env() {
    // 两者都设时 author 优先（git 优先级：author 变量 > committer 变量）
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a\n");
    ok(dir, &["add", "a.txt"]);
    let out = riv_env(
        dir,
        &["commit", "-m", "both envs"],
        &[
            ("GIT_AUTHOR_NAME", Some("The Author")),
            ("GIT_AUTHOR_EMAIL", Some("author@example.com")),
            ("GIT_COMMITTER_NAME", Some("The Committer")),
            ("GIT_COMMITTER_EMAIL", Some("committer@example.com")),
        ],
    );
    assert!(out.status.success());
    let log = ok(dir, &["log"]);
    assert!(
        log.contains("Author: The Author <author@example.com>"),
        "author env 应优先: {log}"
    );
    assert!(
        !log.contains("The Committer"),
        "author env 优先时不应出现 committer 身份: {log}"
    );
}

#[test]
fn log_n_zero_on_empty_repo_errors() {
    // git：空仓库 `log -n 0` 同样报 does not have any commits yet（rc=128）——
    // -n 只是上限，不改变「无提交可看」的事实。
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    let se = err(dir, &["log", "-n", "0"]);
    assert!(se.contains("does not have any commits yet"), "got: {se}");
}

#[test]
fn checkout_rev_expression_detaches_head() {
    // `checkout HEAD~1` 是合法 rev 表达式：必须走 revparse 进入分离头指针，
    // 而不是被当成非法分支名拒绝（git 行为）。
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a1\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c1"]);
    write(dir, "a.txt", "a2\n");
    ok(dir, &["add", "a.txt"]);
    ok(dir, &["commit", "-m", "c2"]);

    let c1 = ok(dir, &["log", "--oneline"]);
    let c1_oid = c1
        .lines()
        .last()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    let out = ok(dir, &["checkout", "HEAD~1"]);
    assert!(out.contains("HEAD is now at"), "分离头指针提示缺失: {out}");
    assert!(out.contains(c1_oid), "应检出 c1（{c1_oid}）: {out}");
    // 工作区回到 c1 内容
    assert_eq!(read(dir, "a.txt"), "a1\n");
}

#[test]
fn add_nonexistent_pathspec_errors() {
    // git：`add <不存在的路径>` 报 fatal pathspec did not match any files。
    // 静默成功会让用户误以为文件已暂存。
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "real.txt", "r\n");
    let se = err(dir, &["add", "nope.txt"]);
    assert!(
        se.contains("pathspec did not match any files") && se.contains("nope.txt"),
        "got: {se}"
    );
    // 合法路径不受影响（ok 内部已断言成功）
    ok(dir, &["add", "real.txt"]);
}

#[test]
fn commit_summary_shows_create_delete_modes() {
    // git commit 摘要含 create/delete mode 行（按路径排序）
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    write(dir, "a.txt", "a\n");
    ok(dir, &["add", "a.txt"]);
    let out = ok(dir, &["commit", "-m", "c1"]);
    assert!(
        out.contains("create mode 100644 a.txt"),
        "create mode 行缺失: {out}"
    );

    fs::remove_file(dir.join("a.txt")).unwrap();
    ok(dir, &["add", "-u"]);
    let out = ok(dir, &["commit", "-m", "c2"]);
    assert!(
        out.contains("delete mode 100644 a.txt"),
        "delete mode 行缺失: {out}"
    );
}

#[test]
fn diff_stat_binary_shows_bin_line() {
    // 二进制变更在 --stat 里必须可见（Bin X -> Y bytes），
    // 静默跳过会让用户以为没有变更。
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    ok(dir, &["init"]);
    fs::write(dir.join("img.bin"), [0u8, 1, 2, 3]).unwrap();
    ok(dir, &["add", "img.bin"]);
    ok(dir, &["commit", "-m", "c1"]);
    fs::write(dir.join("img.bin"), [0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap();
    let out = ok(dir, &["diff", "--stat"]);
    assert!(
        out.contains("Bin 4 -> 8 bytes"),
        "二进制 --stat 行缺失: {out}"
    );
    assert!(out.contains("img.bin"), "路径缺失: {out}");
}
