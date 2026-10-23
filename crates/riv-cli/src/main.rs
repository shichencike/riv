//! riv：分布式版本控制系统的命令行界面。
//!
//! 子命令与 riv-core 操作层的对应关系：
//! - `init` → `Repo::init`
//! - `add` → `snapshot::add`
//! - `commit` → `commit::commit`
//! - `status` → `status::status`
//! - `log` → `log::log`
//! - `branch` → `branch::{list, create, delete, rename}`
//! - `checkout` → `checkout::checkout`
//! - `diff` → `filediff::*` / `treediff::diff_trees`
//! - `auth` → `auth::{get, set, delete, status}_token_with`（token 凭证管理）

mod commands;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "riv",
    version,
    about = "riv - a fast, distributed version control system",
    long_about = None,
    disable_help_subcommand = true,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 初始化一个新仓库
    Init {
        /// 目录（缺省为当前目录）
        path: Option<PathBuf>,
        /// 初始分支名
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// 把文件变更加入索引
    Add {
        /// 要添加的文件或目录（缺省为整个工作区）
        paths: Vec<PathBuf>,
        /// 包含已跟踪文件的删除
        #[arg(short = 'A', long)]
        all: bool,
        /// 只更新已跟踪的文件（忽略未跟踪文件）
        #[arg(short = 'u', long)]
        update: bool,
        /// 添加强制忽略的文件
        #[arg(short, long)]
        force: bool,
        /// 只预览，不实际写入索引
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// 用索引内容创建提交
    Commit {
        /// 提交信息
        #[arg(short, long, conflicts_with = "file")]
        message: Option<String>,
        /// 从文件读取提交信息（- 表示标准输入）
        #[arg(short = 'F', long)]
        file: Option<PathBuf>,
        /// 自动暂存已跟踪文件的修改与删除（等价于先执行 riv add -u）
        #[arg(short, long)]
        all: bool,
        /// 索引无变化时也允许提交
        #[arg(long)]
        allow_empty: bool,
    },
    /// 显示工作区状态
    Status {
        /// 机器可读的两列格式
        #[arg(long)]
        porcelain: bool,
        /// 简短格式（porcelain 的别名）
        #[arg(short, long)]
        short: bool,
    },
    /// 显示提交历史
    Log {
        /// 起始修订（缺省 HEAD）
        rev: Option<String>,
        /// 每个提交只显示一行
        #[arg(long)]
        oneline: bool,
        /// 最多显示 N 条（缺省不限；N=0 输出为空，对齐 git log -n 0）
        #[arg(short = 'n', long, default_value_t = usize::MAX)]
        max_count: usize,
    },
    /// 管理分支
    Branch {
        /// 分支名（创建），或 -m 时的旧名
        name: Option<String>,
        /// 创建时的起始点修订，或 -m 时的新名
        start_or_new: Option<String>,
        /// 删除分支（已合并到当前 HEAD 才允许）
        #[arg(short, long)]
        delete: Option<String>,
        /// 强制删除分支（跳过合并检查）
        #[arg(short = 'D', long)]
        force_delete: Option<String>,
        /// 重命名分支：-m <旧名> <新名>
        #[arg(short = 'm', long)]
        rename: bool,
    },
    /// 切换分支 / 提交，或恢复文件
    Checkout {
        /// 目标：分支名或修订表达式
        target: Option<String>,
        /// 创建并切换到新分支
        #[arg(short = 'b', long, conflicts_with = "paths")]
        create: Option<String>,
        /// 新分支的起始点（配合 -b，缺省 HEAD）
        #[arg(long)]
        start_point: Option<String>,
        /// 丢弃本地修改，强制执行
        #[arg(short, long)]
        force: bool,
        /// 只恢复这些路径（git checkout [<tree-ish>] -- <paths> 语义）
        #[arg(last = true)]
        paths: Vec<PathBuf>,
    },
    /// 显示差异
    Diff {
        /// 比较的修订：一个 = 与工作区比；两个 = 树间比
        revs: Vec<String>,
        /// 比较 HEAD 树与索引（--staged 的别名）；与显式修订互斥
        #[arg(long, alias = "staged", conflicts_with = "revs")]
        cached: bool,
        /// 只显示增删统计
        #[arg(long)]
        stat: bool,
        /// 只显示这些路径的差异
        #[arg(last = true)]
        paths: Vec<PathBuf>,
    },
    /// 管理连接远程用的 Token 凭证
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// 输入并保存 Token（屏幕不回显）
    Login {
        /// 目标主机或 base URL（缺省 github.com，或 $RIV_BASE_URL）
        #[arg(long)]
        host: Option<String>,
    },
    /// 查看当前 Token 的来源与状态
    Status {
        /// 目标主机或 base URL（缺省 github.com，或 $RIV_BASE_URL）
        #[arg(long)]
        host: Option<String>,
    },
    /// 清除已保存的 Token（系统钥匙串与凭证文件）
    Logout {
        /// 目标主机或 base URL（缺省 github.com，或 $RIV_BASE_URL）
        #[arg(long)]
        host: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> riv_core::Result<()> {
    match cli.command {
        Command::Init { path, branch } => commands::init(path.as_deref(), &branch),
        Command::Add {
            paths,
            all,
            update,
            force,
            dry_run,
        } => commands::add(&paths, all, update, force, dry_run),
        Command::Commit {
            message,
            file,
            all,
            allow_empty,
        } => commands::commit(message.as_deref(), file.as_deref(), all, allow_empty),
        Command::Status { porcelain, short } => commands::status(porcelain || short),
        Command::Log {
            rev,
            oneline,
            max_count,
        } => commands::log(rev.as_deref(), oneline, max_count),
        Command::Branch {
            name,
            start_or_new,
            delete,
            force_delete,
            rename,
        } => commands::branch(
            name.as_deref(),
            start_or_new.as_deref(),
            delete.as_deref(),
            force_delete.as_deref(),
            rename,
        ),
        Command::Checkout {
            target,
            create,
            start_point,
            force,
            paths,
        } => commands::checkout(
            target.as_deref(),
            create.as_deref(),
            start_point.as_deref(),
            force,
            &paths,
        ),
        Command::Diff {
            revs,
            cached,
            stat,
            paths,
        } => commands::diff(&revs, cached, stat, &paths),
        Command::Auth { action } => match action {
            AuthAction::Login { host } => commands::auth_login(host.as_deref()),
            AuthAction::Status { host } => commands::auth_status(host.as_deref()),
            AuthAction::Logout { host } => commands::auth_logout(host.as_deref()),
        },
    }
}
