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

索引格式 v2 的校验和从 FNV-1a 升级为 **blake3 前 8 字节**，并对条目数量设上限
（防伪造 count 触发预分配 OOM）；v1 文件仍可正常读取。

### 完整性与加固

- **读取即校验**：对象读回后重算 blake3 与寻址 oid 比对，位翻转/坏盘/误写立即
  报错，对齐 git 松散对象校验
- **压缩炸弹防护**：zstd 流式解压，上限 4 GiB，超出即拒
- **分支名校验**：创建/读取/删除全链路拦截 `..`、`~^:?*[`、尾部 `.lock` 等
  非法名，路径穿越（`refs/heads/../../etc/x`）在构造路径前即被拒绝
- **修订后缀校验**：`^` / `~` 后仅接受数字，`HEAD^abc` 这类输入报
  `InvalidRevision` 而非静默忽略
- **提交完整性**：空仓库且索引为空时 `commit` 拒绝创建空根提交
  （`--allow-empty` 显式放行）

### diff：对齐 git xdiff 的 Myers 算法

`riv-diff` 实现了 Myers O(ND) 的**线性空间分治**版本，与 git 的 xdiff 同构：

- 每层递归先剥离公共前后缀，空维直接返回
- 前向/后向搜索**边界首次重叠时的空蛇**作为分裂点
- 进度保证：剥离后 D ≥ 2，递归必然收敛
- 成本安全阀 `max(256, sqrt(n+m))`：差异极大时退化为合法但非最小的编辑脚本
  （与 git 的 mxcost 行为一致）

输出为 git 风格 unified diff（`--- a/x` / `+++ b/x` / `@@` hunk），mode 变化
输出 `old mode` / `new mode` 行（仅 mode 变化时无 hunk）。

### 性能取向

- **并行哈希**：`add` 用 rayon 并行处理文件，大文件（≥1MB）走 mmap 零拷贝
- **流式写入**：blob 经 zstd Encoder 边算边写，内存占用与文件大小无关
- **增量 status**：只重算 stat 缓存失效的路径
- **惰性检出**：checkout 时索引条目与目标树一致、且工作区经 freshness 校验为
  干净的文件直接跳过写入，大仓库切换分支不再全量重写；只清理被删文件所在的
  已空目录，不误删用户自建空目录
- **单次加载索引**：`diff` 等命令把索引解码一次传入，消除逐路径重复加载的
  O(n²) IO

## 跨平台一致性

「同一份源码、同一种构建命令、同一种体验」是逐项对齐的结果，而非巧合：

| 平台差异 | riv 的处理 |
|---|---|
| 路径分隔符 | Windows 的 `\` 与 verbatim 路径（`\\?\C:\...`）在仓库入口统一按**路径组件**归一化（不能按字符串剥离前缀），仓库内路径一律以 `/` 表示；`add ./src` / `add src` / `add .\src` 行为一致 |
| 执行位 | Windows 无 exec 位，非 Unix 平台显式忽略 exec 位（`(a & !0o111) == (b & !0o111)`），对齐 git `core.filemode=false`，杜绝 Linux 提交的可执行文件在 Windows 上永久显示 modified |
| 符号链接 | Unix / Termux 完整支持；Windows 回退为普通文件，对齐 git `core.symlinks=false` |
| 编辑器回退 | `commit` 无 `-m` 且无 `$EDITOR`/`$VISUAL`：Unix 回退 `vi`，Windows 回退 `notepad`；stdin 非 TTY 时明确报错指引 `-m/-F`，绝不挂起 |
| Token 存储 | 环境变量 > 系统钥匙串（macOS Keychain / Windows 凭据管理器 / Linux Secret Service）> 文件 `~/.config/riv/credentials`（chmod 600）；无 DBus 的 Termux 自动降级到文件层，流程不中断 |

验证矩阵：

| 平台 | 编译 | clippy | 测试 |
|---|---|---|---|
| Linux x86_64 | ✓ | 0 警告 | 全部通过（188 个：core 134 + CLI 26 + diff 21 + 最小性属性 7） |
| Windows（GNU，wine 运行） | ✓ | 0 警告 | core 128/128 + diff 21/21 + 最小性 7/7 + CLI 25/25（Unix 专属测试按设计排除；Windows 凭据管理器真实生效） |
| Android aarch64（NDK 交叉编译） | ✓ | 0 警告 | 编译级验证（Termux 直接编译同构） |

## 安装与构建

riv **不提供预编译二进制**——所有平台使用同一份源码、同一条命令 `cargo build --release`
直接编译，保证「代码本身可移植」，而非「打包好的东西支持」。唯一前置条件是 Rust
工具链（rust-version ≥ 1.85，推荐 rustup 稳定版）。

### Linux / macOS

```bash
tar xzf riv-0.3.1.tar.gz
cd riv-0.3.1
cargo build --release        # 产物：target/release/riv
```

### Windows

安装 Rust（rustup）+ 任一工具链（MinGW-w64 或 MSVC Build Tools），然后：

```powershell
tar xzf riv-0.3.1.tar.gz
cd riv-0.3.1
cargo build --release        # 产物：target\release\riv.exe
```

### Termux (Android)

```bash
pkg update && pkg install rust clang
tar xzf riv-0.3.1.tar.gz
cd riv-0.3.1
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
| `riv log` | 第一父链；`--oneline` / `-n N` / `<rev>` 起始（`-n 0` 输出为空，对齐 git） |
| `riv branch` | 列表 / 创建 / `-d`（未合并拒绝）/ `-D` / `-m` 重命名；分支名在创建/读取/删除全链路校验 |
| `riv checkout` | `-b` 创建切换（与 paths 互斥）；`<branch|rev>` 切换（脏工作区安全检查）；`-- <paths>` 从索引恢复；`<rev> -- <paths>` 从树恢复 |
| `riv diff` | 默认 index vs 工作区；`--cached` = HEAD vs 索引（与显式 rev 互斥）；`<rev>` = rev vs 工作区（含索引新增路径）；`<rev1> <rev2>` = 树间比；`--stat`；pathspec 支持目录前缀；输出真实 mode（`old mode`/`new mode`） |
| `riv auth` | `login` 隐藏输入并保存 token / `status` 三来源状态 / `logout` 清除；均支持 `--host` |

## Token 凭证管理（riv auth）

riv 连接远程时需要身份凭证。`riv auth` 按优先级从高到低读取：

1. **环境变量 `RIV_TOKEN`**：CI/CD 与临时覆盖首选，不落盘、用即焚；
2. **系统钥匙串**：Windows 凭据管理器 / macOS 钥匙串 / Linux Secret Service；
3. **本地文件** `<config_dir>/riv/credentials`（Unix 权限 600）：Termux 等无 DBus
   的环境自动降级到此。

默认主机为 GitHub 预设（`https://github.com`），可用 `RIV_BASE_URL` 或 `--host`
覆盖；内部按主机名寻址（含端口），同一套机制将来到任意 riv 远程都可复用。

```bash
riv auth login                  # 输入不可见；落钥匙串，失败则降级文件（600）
riv auth status                 # 三来源状态 + 生效项（token 脱敏展示）
riv auth logout                 # 清除钥匙串与文件；环境变量需自行 unset
riv auth login --host https://git.example.com:8443
```

保存时优先写系统钥匙串，并清理另一处的旧条目，避免过期凭证被优先读到；
日志与错误输出中的 token 一律打码（保留前 4 后 4）。

修订表达式（revparse）支持：`HEAD`、分支名、oid 前缀（≥4 位十六进制）、
`^N`（第 N 父）与 `~N`（回溯 N 代）。后缀部分仅接受数字，其他字符
（如 `HEAD^abc`）报 `InvalidRevision`。

## 项目结构

```
crates/
  riv-diff/    Myers O(ND) 线性空间 diff（零依赖，可独立使用）
  riv-core/    对象库、索引、引用、快照、状态、检出、日志、分支
  riv-cli/     clap 命令行界面
```

## 测试

```bash
cargo test           # 188 个测试全部通过
```

- `riv-core`：134 个单元测试（对象/索引/引用/快照/状态/检出/提交/日志/分支/auth）
- `riv-diff`：21 个单元测试 + 7 个最小性属性测试（与暴力 DP 对拍）
- `riv-cli`：26 个端到端集成测试（对真实二进制跑完整命令流，含 auth 全流程）

代码质量：`cargo clippy --all-targets` 零警告，`cargo fmt --check` 干净。

## 许可证

双许可：[MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE)，与 `Cargo.toml` 中
`license = "MIT OR Apache-2.0"` 一致，可任选其一使用。

## 0.3.1：git 行为对齐修复

基于 git-vs-riv 对拍（同一命令序列跑两种实现、归一化输出后 diff）逐条修复的真实分歧：

- **checkout 分离头指针**：`checkout HEAD~1` 等合法 rev 表达式现在走 revparse 进入分离头指针，不再被当成非法分支名拒绝。
- **`add` 不存在路径**：`add <不存在的路径>` 报 `pathspec did not match any files`（静默成功会误导用户以为已暂存）。多 pathspec 只报第一个，与 git 一致。
- **`log -n 0` 空仓库**：与 git 一致——无论 `-n` 多少，空仓库都报 `does not have any commits yet`（rc=128）。
- **`log` 日期格式**：`Date:` 行改为 git 同款 `Sun Jan 5 09:03:07 2020 +0800`（星期/月份英文缩写、日不补零、偏移 `+HHMM`）。
- **作者身份环境变量**：`GIT_AUTHOR_NAME/EMAIL`（及回落 `GIT_COMMITTER_NAME/EMAIL`）覆盖 config 身份，与 git 优先级一致；空名直接报错而非静默回落（git 语义）。CI 可通过环境变量注入身份。
- **`commit` 摘要**：补齐 `create mode 100644 <path>` / `delete mode 100644 <path>` 行，与 git 逐字节一致。
- **`diff --stat` 二进制**：二进制变更显示 `Bin X -> Y bytes`；计数列宽按最大变更量动态计算；全零汇总显示 `0 insertions(+), 0 deletions(-)`。

## 当前范围与后续

已实现本地核心工作流与 token 凭证层（`riv auth`，0.2.0 新增），并在 0.3.1 完成
一轮正确性与健壮性加固：对象读取哈希校验、zstd 解压上限、流式 blob 写入头修复、
惰性检出（未变化文件不重写）、checkout 原子性与 prune 范围化、索引 v2 校验和、
分支名/修订后缀全链路校验、diff mode 语义贯通。**尚未实现**：远程协议
（fetch/push/pull，凭证层已就位待对接）、合并（merge）与三方合并、对象库垃圾
回收、`stash`、交互式 add。
