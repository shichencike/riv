#!/usr/bin/env bash
# riv 全命令演示脚本。用法：./demo.sh [riv 二进制路径]
set -euo pipefail

RIV="${1:-$(dirname "$0")/target/release/riv}"
# 脚本后续会 cd 到临时目录，先把 riv 路径解析为绝对路径
RIV="$(cd "$(dirname "$RIV")" && pwd)/$(basename "$RIV")"
if [ ! -x "$RIV" ]; then
  echo "找不到 riv 可执行文件：$RIV" >&2
  echo "请先执行 cargo build --release，或用 ./demo.sh /path/to/riv 指定" >&2
  exit 1
fi
D="$(mktemp -d)"
cd "$D"

echo "═══ init ═══"
"$RIV" init

echo; echo "═══ add + status ═══"
printf 'line1\nline2\nline3\n' > a.txt
mkdir -p lib && printf 'fn main() {}\n' > lib/x.rs
"$RIV" add .
"$RIV" status --porcelain

echo; echo "═══ commit ═══"
"$RIV" commit -m "initial"

echo; echo "═══ status clean ═══"
"$RIV" status

echo; echo "═══ log ═══"
"$RIV" log --oneline

echo; echo "═══ 分支：创建 / 列表 / 切换 ═══"
"$RIV" branch dev
"$RIV" branch
"$RIV" checkout dev
printf 'fn main() { println!("dev"); }\n' > lib/x.rs
"$RIV" commit -a -m "dev change"
"$RIV" checkout main
echo "main 分支内容: $(cat lib/x.rs)"

echo; echo "═══ diff：rev vs 工作区 / 树间 / --cached ═══"
"$RIV" diff dev
"$RIV" diff main dev
printf 'fn main() { /* wip */ }\n' > lib/x.rs
"$RIV" add lib/x.rs
"$RIV" diff --cached

echo; echo "═══ checkout -- 路径恢复（从索引）═══"
printf 'dirty\n' > a.txt
"$RIV" checkout -- a.txt
echo "a.txt 恢复为: $(head -1 a.txt)"

echo; echo "═══ branch -d（已合并可删）═══"
"$RIV" branch merged-candidate
"$RIV" branch -d merged-candidate

echo; echo "═══ 错误处理 ═══"
"$RIV" log nope 2>&1 || true
"$RIV" checkout -- ghost.txt 2>&1 || true

echo; echo "═══ 最终状态 ═══"
"$RIV" status --porcelain
"$RIV" log --oneline
