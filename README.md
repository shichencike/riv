# riv

**riv** 是一个用 Rust 编写的分布式版本控制系统。它实现了与 git 等价的核心工作流
（`init` / `add` / `commit` / `status` / `log` / `branch` / `checkout` / `diff`），
但采用**完全自有的存储格式**——不与 git 互操作，也不背负 git 的历史包袱。

riv 的定位是「实际生产力工具」：大文件友好、并行哈希、增量 diff。
```
$ riv --help
riv - a fast, distributed version control system

Usage: riv <COMMAND>

Commands:
  init      Initialize a new repository
  add       Stage file changes into the index
  commit    Create a commit from the index
  status    Show working tree status
  log       Show commit history
  branch    Manage branches
  checkout  Switch branches/commits, or restore files
  diff      Show differences
  help      Print this message or the help of the given subcommand(s)
```

## 设计要点

### 对象模型

所有内容寻址对象共享一个二进制帧：

```
RIVOBJ\x01\x00 | type(u8) | payload_len(u64 LE) | payload
```

- `type`：`1 = blob`，`2 = tree`，`3 = commit`
- 对象 id = `blake3(完整序列化字节)`，即对头部+内容整体哈希
- payload 经 **zstd** 压缩后按 fan-out 目录（前 2 字节）存放，tmp+rename 原子写入

### 索引与 racy 检查

索引（`.riv/index`）记录每个路径的 `oid / mode / size / mtime_sec / mtime_nsec`。
status 与 add 依赖 stat 缓存避免重复读文件；当文件 mtime 秒 ≥ 索引文件自身
mtime 秒时（粗粒度时间戳文件系统上的经典竞态），强制重新哈希——与 git 的
racy-clean 检查一致。

### diff：对齐 git xdiff 的 Myers 算法

`riv-diff` 实现了 Myers O(ND) 的**线性空间分治**版本，与 git 的 xdiff 同构：

- 每层递归先剥离公共前后缀，空维直接返回
- 前向/后向搜索**边界首次重叠时的空蛇**作为分裂点
- 进度保证：剥离后 D ≥ 2，递归必然收敛
- 成本安全阀 `max(256, sqrt(n+m))`：差异极大时退化为合法但非最小的编辑脚本
  （与 git 的 mxcost 行为一致）

输出为 git 风格 unified diff（`--- a/x` / `+++ b/x` / `@@` hunk）。

### 性能取向

- **并行哈希**：`add` 用 rayon 并行处理文件，大文件（≥1MB）走 mmap 零拷贝
- **流式写入**：blob 经 zstd Encoder 边算边写，内存占用与文件大小无关
- **增量 status**：只重算 stat 缓存失效的路径

## 跨平台一致性

「同一份源码、同一种构建命令、同一种体验」是逐项对齐的结果，而非巧合：

| 平台差异 | riv 的处理 |
|---|---|
| 路径分隔符 | Windows 的 `\` 与 verbatim 路径（`\\?\C:\...`）在仓库入口统一按**路径组件**归一化（不能按字符串剥离前缀），仓库内路径一律以 `/` 表示；`add ./src` / `add src` / `add .\src` 行为一致 |
| 执行位 | Windows 无 exec 位，非 Unix 平台显式忽略 exec 位（`(a & !0o111) == (b & !0o111)`），对齐 git `core.filemode=false`，杜绝 Linux 提交的可执行文件在 Windows 上永久显示 modified |
| 符号链接 | Unix / Termux 完整支持；Windows 回退为普通文件，对齐 git `core.symlinks=false` |
| 编辑器回退 | `commit` 无 `-m` 且无 `$EDITOR`/`$VISUAL`：Unix 回退 `vi`，Windows 回退 `notepad`；stdin 非 TTY 时明确报错指引 `-m/-F`，绝不挂起 |

验证矩阵：

| 平台 | 编译 | clippy | 测试 |
|---|---|---|---|
| Linux x86_64 | ✓ | 0 警告 | 全部通过 |
| Windows（GNU，wine 运行） | ✓ | 0 警告 | core 全过 + CLI 冒烟（含路径分隔符回归） |
| Android aarch64（NDK 交叉编译） | ✓ | 0 警告 | 编译级验证（Termux 直接编译同构） |

## 安装与构建

riv **不提供预编译二进制**——所有平台使用同一份源码、同一条命令 `cargo build --release`
直接编译，保证「代码本身可移植」，而非「打包好的东西支持」。唯一前置条件是 Rust
工具链（rust-version ≥ 1.85，推荐 rustup 稳定版）。

### Linux / macOS

```bash
tar xzf riv-0.1.0.tar.gz
cd riv-0.1.0
cargo build --release        # 产物：target/release/riv
```

### Windows

安装 Rust（rustup）+ 任一工具链（MinGW-w64 或 MSVC Build Tools），然后：

```powershell
tar xzf riv-0.1.0.tar.gz
cd riv-0.1.0
cargo build --release        # 产物：target\release\riv.exe
```

### Termux (Android)

```bash
pkg update && pkg install rust clang
tar xzf riv-0.1.0.tar.gz
cd riv-0.1.0
cargo build --release        # 产物：target/release/riv
```

> Termux 的 `$HOME` 位于 ext4/f2fs，exec 位与符号链接均可用，走完整 Unix 语义。
> 请勿把仓库放在 `/sdcard`（FAT/exFAT 无 Unix 权限位，riv 会退化为
> git `core.filemode=false` 的安全语义，不影响仓库正确性）。

完整图文教程见压缩包内 `docs/index.html`（离线单文件网页，双击即可打开）。

## 快速开始

```bash
cd my-project
../riv/target/release/riv init

riv add .                    # 暂存整个工作区（尊重 .rivignore）
riv status
riv commit -m "first commit"

riv log --oneline
riv branch feature           # 创建分支
riv checkout feature         # 切换
riv diff --stat              # 工作区 vs 索引统计
```

`.rivignore` 的语法与 `.gitignore` 相同（底层使用 `ignore` crate）。

## 命令语义对照

| 命令 | 行为 |
|---|---|
| `riv init` | 创建 `.riv/`（objects/ refs/ index HEAD config） |
| `riv add [paths]` | 无参数 = 整个工作区（含删除）；`-u` 只更新已跟踪；`-f` 强制添加被忽略文件；`-n` 预览 |
| `riv commit -m/-F` | 无信息时回退 `$EDITOR`（非交互环境明确报错而非挂起）；`-a` 等价先 `add -u` |
| `riv status` | 两列模型（staged/unstaged）+ untracked；`--porcelain` 机器可读 |
| `riv log` | 第一父链；`--oneline` / `-n N` / `<rev>` 起始 |
| `riv branch` | 列表 / 创建 / `-d`（未合并拒绝）/ `-D` / `-m` 重命名 |
| `riv checkout` | `-b` 创建切换；`<branch|rev>` 切换（脏工作区安全检查）；`-- <paths>` 从索引恢复；`<rev> -- <paths>` 从树恢复 |
| `riv diff` | 默认 index vs 工作区；`--cached` = HEAD vs 索引；`<rev>` = rev vs 工作区；`<rev1> <rev2>` = 树间比；`--stat` |

修订表达式（revparse）支持：`HEAD`、分支名、oid 前缀（≥4 位十六进制）、
`^N`（第 N 父）与 `~N`（回溯 N 代）。

## 项目结构

```
crates/
  riv-diff/    Myers O(ND) 线性空间 diff（零依赖，可独立使用）
  riv-core/    对象库、索引、引用、快照、状态、检出、日志、分支
  riv-cli/     clap 命令行界面
```

## 测试

```bash
cargo test           # 144 个测试全部通过
```

- `riv-core`：104 个单元测试（对象/索引/引用/快照/状态/检出/提交/日志/分支）
- `riv-diff`：17 个单元测试 + 7 个最小性属性测试（与暴力 DP 对拍）
- `riv-cli`：16 个端到端集成测试（对真实二进制跑完整命令流）

代码质量：`cargo clippy --all-targets` 零警告，`cargo fmt --check` 干净。

## 当前范围与后续

已实现本地核心工作流。**尚未实现**：远程协议（fetch/push/pull）、
合并（merge）与三方合并、对象库垃圾回收、`stash`、交互式 add。
