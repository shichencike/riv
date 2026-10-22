//! riv-diff：riv 的差异引擎。
//!
//! 提供：
//! - Myers O(ND) 最小编辑脚本（线性空间「中间蛇」分治变体，结构对齐 git xdiff）
//! - 行级文本差异、unified diff / hunk 生成
//! - 差异统计（`--stat` 风格）
//!
//! 设计要点：
//! - 内存 O(N+M)：不像朴素回溯版 Myers 那样随编辑距离平方增长，可处理超大文件
//! - 每层递归先剥离公共前后缀（对齐 xdiff `xdl_recs_cmp`），常见「少量修改」
//!   场景几乎瞬时完成
//! - 前向/后向边界首次重叠时返回**边界点**作为分裂点（对齐 xdiff `xdl_split`），
//!   该点位于某条最优路径上，递归结果为最小编辑脚本
//! - 成本安全阀：搜索深度超过 `max(256, sqrt(n+m))`（差异极大的病态输入）时
//!   返回当时最远的前向边界点，保证最坏情况 O((N+M)^1.5) 而非 O((N+M)·D)
//! - 跨递归层级复用同一块 scratch 缓冲，避免反复分配

use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// 编辑脚本
// ---------------------------------------------------------------------------

/// 一条编辑操作。`Equal`/`Delete` 携带 a 侧下标，`Equal`/`Insert` 携带 b 侧下标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// a[i] 与 b[j] 相等
    Equal { a: usize, b: usize },
    /// a[i] 被删除
    Delete { a: usize },
    /// b[j] 被新增
    Insert { b: usize },
}

impl Edit {
    #[inline]
    pub fn is_equal(&self) -> bool {
        matches!(self, Edit::Equal { .. })
    }
    #[inline]
    pub fn is_change(&self) -> bool {
        !self.is_equal()
    }
}

/// 计算两个序列的最小编辑脚本（Myers O(ND)，线性空间变体）。
///
/// - 时间 O((N+M)·D)，D 为编辑距离
/// - 空间 O(N+M)
pub fn diff_slices<T, F>(a: &[T], b: &[T], eq: F) -> Vec<Edit>
where
    F: Fn(&T, &T) -> bool,
{
    let mut out = Vec::with_capacity(a.len() + b.len());
    diff_into(a, b, &eq, &mut out);
    out
}

fn diff_into<T, F: Fn(&T, &T) -> bool>(a: &[T], b: &[T], eq: &F, out: &mut Vec<Edit>) {
    let mut scratch = Scratch::new(a.len() + b.len());
    diff_recursive(a, b, 0, 0, eq, &mut scratch, out);
}

/// 分治递归，结构对齐 git xdiff 的 `xdl_recs_cmp`：
///
/// 1. 剥离公共前缀 / 公共后缀（输出 `Equal`）—— 每层都做，保证进入中间蛇的
///    盒子既无公共前后缀、编辑距离也 ≥ 2；
/// 2. 某一维为空 → 另一维全部为 `Insert` / `Delete`（对应 xdiff 的叶子标记）；
/// 3. 否则求中间蛇：**返回前向/后向边界在重叠对角线上的边界点（空蛇）**，
///    以该点把盒子分裂为两个严格更小的子盒子分别递归。
///
/// `a_off`/`b_off` 为子序列在原序列中的偏移，用于输出全局下标。
fn diff_recursive<T, F: Fn(&T, &T) -> bool>(
    a: &[T],
    b: &[T],
    mut a_off: usize,
    mut b_off: usize,
    eq: &F,
    scratch: &mut Scratch,
    out: &mut Vec<Edit>,
) {
    // ---- 剥离公共前缀 ----
    let mut pre = 0usize;
    let limit = a.len().min(b.len());
    while pre < limit && eq(&a[pre], &b[pre]) {
        out.push(Edit::Equal {
            a: a_off + pre,
            b: b_off + pre,
        });
        pre += 1;
    }
    let a = &a[pre..];
    let b = &b[pre..];
    a_off += pre;
    b_off += pre;

    // ---- 剥离公共后缀（先计算，输出推迟到主体之后以保持脚本有序）----
    let mut suf = 0usize;
    while suf < a.len() && suf < b.len() && eq(&a[a.len() - 1 - suf], &b[b.len() - 1 - suf]) {
        suf += 1;
    }
    let a = &a[..a.len() - suf];
    let b = &b[..b.len() - suf];
    let suf_base_a = a_off + a.len();
    let suf_base_b = b_off + b.len();

    // ---- 主体 ----
    if a.is_empty() {
        // 对应 xdiff 的叶子：off1 == lim1，b 侧全部为新增
        for j in 0..b.len() {
            out.push(Edit::Insert { b: b_off + j });
        }
    } else if b.is_empty() {
        for i in 0..a.len() {
            out.push(Edit::Delete { a: a_off + i });
        }
    } else {
        let (x, y, u, v) = middle_snake(a, b, eq, scratch);
        if x == u && y == v {
            // 空蛇：以边界点分裂。正常路径下边界点严格位于盒子内部
            // （进入此处的盒子编辑距离 ≥ 2），两侧均严格变小。
            // 角点分支仅为理论不可达的防御，保证任何情况下都能终止。
            if x == 0 && y == 0 {
                out.push(Edit::Delete { a: a_off });
                diff_recursive(&a[1..], b, a_off + 1, b_off, eq, scratch, out);
            } else if x == a.len() && y == b.len() {
                out.push(Edit::Delete {
                    a: a_off + a.len() - 1,
                });
                diff_recursive(&a[..a.len() - 1], b, a_off, b_off, eq, scratch, out);
            } else {
                diff_recursive(&a[..x], &b[..y], a_off, b_off, eq, scratch, out);
                diff_recursive(&a[x..], &b[y..], a_off + x, b_off + y, eq, scratch, out);
            }
        } else {
            // 非空蛇：仅出现在「整个盒子相等」的安全网路径
            diff_recursive(&a[..x], &b[..y], a_off, b_off, eq, scratch, out);
            for k in 0..(u - x) {
                out.push(Edit::Equal {
                    a: a_off + x + k,
                    b: b_off + y + k,
                });
            }
            diff_recursive(&a[u..], &b[v..], a_off + u, b_off + v, eq, scratch, out);
        }
    }

    // ---- 后缀 Equal ----
    for i in 0..suf {
        out.push(Edit::Equal {
            a: suf_base_a + i,
            b: suf_base_b + i,
        });
    }
}

// ---------------------------------------------------------------------------
// 中间蛇（Myers 线性空间核心）
// ---------------------------------------------------------------------------

const F_UNSET: isize = isize::MIN / 4;
const B_UNSET: isize = isize::MAX / 4;

/// 跨递归层级复用的边界数组。对角线 κ 的实际下标为 `κ + offset`。
///
/// 容量推导：后向阶段 κ 的范围是 `[delta - d, delta + d]`，读取时还会取
/// `κ ± 1`；`|delta| <= max(n, m)`、`d <= (n+m+1)/2`，故
/// `|κ ± 1| <= max(n,m) + (n+m+1)/2 + 1 <= 1.5(n+m) + 2.5`。
/// 取 `offset = 1.5(n+m) + 4`、`size = 2*offset + 8` 可完整覆盖，
/// 越界对角线读到的是哨兵值（不可达），与 xdiff 的域钳制等效。
struct Scratch {
    vf: Vec<isize>,
    vb: Vec<isize>,
    offset: isize,
}

impl Scratch {
    fn new(n_plus_m: usize) -> Self {
        let s = n_plus_m as isize;
        let offset = (3 * s) / 2 + 4;
        let size = (2 * offset + 8) as usize;
        Scratch {
            vf: vec![F_UNSET; size],
            vb: vec![B_UNSET; size],
            offset,
        }
    }
}

/// 寻找中间蛇，返回 `(x, y, u, v)`。
///
/// 正常路径返回**空蛇** `(x, y) == (u, v)`：即前向/后向边界首次重叠时，
/// 重叠对角线上的**边界点**（对齐 git xdiff `xdl_split` 的 `spl->i1/i2`）。
/// 以该点分裂可保证两个子盒子严格变小，且该点位于某条最优路径上，
/// 因此递归结果为最小编辑脚本。
///
/// 例外（安全网）：若深度 0 的前向扩展即到达终点（整个盒子相等），
/// 返回完整蛇 `(0, 0) -> (n, m)`。正常流程中调用方已剥离公共前后缀，
/// 该分支不会触发。
///
/// 约定：
/// - 前向边界 `VF[κ]` = 对角线 κ 上距原点最远的 x（d 步内可达）
/// - 后向边界 `VB[κ]` = 对角线 κ 上距终点最近的 x（d 步内可达）
/// - `delta` 为奇数时在前向阶段检测重叠（对比深度 d-1 的后向边界）
/// - `delta` 为偶数时在后向阶段检测重叠（对比深度 d 的前向边界）
fn middle_snake<T, F: Fn(&T, &T) -> bool>(
    a: &[T],
    b: &[T],
    eq: &F,
    scratch: &mut Scratch,
) -> (usize, usize, usize, usize) {
    let n = a.len() as isize;
    let m = b.len() as isize;
    if n == 0 || m == 0 {
        return (0, 0, 0, 0);
    }
    let delta = n - m;
    let odd = delta % 2 != 0;
    let max = (n + m) as usize;
    let off = scratch.offset;
    let vf = &mut scratch.vf;
    let vb = &mut scratch.vb;

    // 记录本次调用写过的区间，返回前复位，保证数组对下次调用是「干净」的
    let mut f_lo = 0isize;
    let mut f_hi = 0isize;
    let mut b_lo = 0isize;
    let mut b_hi = 0isize;

    let result = (|| {
        // ---- 深度 0 ----
        let (mut x, mut y) = (0isize, 0isize);
        while x < n && y < m && eq(&a[x as usize], &b[y as usize]) {
            x += 1;
            y += 1;
        }
        vf[off as usize] = x;
        f_lo = 0;
        f_hi = 0;
        // 安全网：深度 0 前向扩展即到达终点（整个盒子相等，仅当 delta == 0）。
        // 正常流程中调用方已剥离公共前后缀，此处不会触发。
        if x == n && y == m {
            return (0, 0, n as usize, m as usize);
        }
        let (mut xb, mut yb) = (n, m);
        while xb > 0 && yb > 0 && eq(&a[(xb - 1) as usize], &b[(yb - 1) as usize]) {
            xb -= 1;
            yb -= 1;
        }
        vb[(delta + off) as usize] = xb;
        b_lo = delta;
        b_hi = delta;

        let d_max = max.div_ceil(2);
        // 成本安全阀（对齐 xdiff 的 mxcost）：搜索深度超过 max(256, sqrt(n+m))
        // 时说明两个序列差异极大（接近最坏情况 O((N+M)·D)），此时返回当前
        // 最远的前向边界点作为分裂点。该点严格位于盒子内部（进度有保证），
        // 输出仍是合法编辑脚本，但不再保证最小——与 git 的行为一致。
        // 由于检测深度 <= (D+1)/2，仅当 D > 2*mxcost 时才可能触发。
        let mxcost = (((n + m) as f64).sqrt() as usize).max(256);
        for d in 1..=d_max {
            let dd = d as isize;

            // ---- 前向：深度 d ----
            let mut k = -dd;
            while k <= dd {
                let fa = vf[(k + 1 + off) as usize]; // 来自对角线 k+1（下移）
                let fb = vf[(k - 1 + off) as usize]; // 来自对角线 k-1（右移）
                let mut cx = F_UNSET;
                if k == -dd {
                    // 只能来自 k+1
                    if fa != F_UNSET && fa - (k + 1) < m {
                        cx = fa;
                    }
                } else if k == dd {
                    // 只能来自 k-1
                    if fb != F_UNSET && fb < n {
                        cx = fb + 1;
                    }
                } else {
                    let a_ok = fa != F_UNSET && fa - (k + 1) < m;
                    let b_ok = fb != F_UNSET && fb < n;
                    if a_ok && (!b_ok || fa > fb) {
                        cx = fa;
                    } else if b_ok {
                        cx = fb + 1;
                    }
                }
                if cx != F_UNSET {
                    let mut cy = cx - k;
                    // 沿对角线向前扩展（极大相等段），扩展后即为深度 d 的边界点
                    while cx < n && cy < m && eq(&a[cx as usize], &b[cy as usize]) {
                        cx += 1;
                        cy += 1;
                    }
                    vf[(k + off) as usize] = cx;
                    if k < f_lo {
                        f_lo = k;
                    }
                    if k > f_hi {
                        f_hi = k;
                    }
                    // 重叠检测：delta 奇数，对比深度 d-1 的后向边界。
                    // 返回前向边界点本身（空蛇），即 xdiff 的 split point。
                    if odd {
                        let shift = k - delta;
                        if shift >= -(dd - 1) && shift <= (dd - 1) {
                            let bv = vb[(k + off) as usize];
                            if bv != B_UNSET && cx >= bv {
                                return (cx as usize, cy as usize, cx as usize, cy as usize);
                            }
                        }
                    }
                } else {
                    vf[(k + off) as usize] = F_UNSET;
                    if k < f_lo {
                        f_lo = k;
                    }
                    if k > f_hi {
                        f_hi = k;
                    }
                }
                k += 2;
            }

            // ---- 后向：深度 d ----
            // 后向走步等价于在反转序列上的前向走步：
            // 左移 (x-1) ↔ 反转坐标右移；上移 (y-1) ↔ 反转坐标下移。
            let mut k = delta - dd;
            while k <= delta + dd {
                let ba = vb[(k + 1 + off) as usize]; // 来自对角线 k+1（左移，x-1）
                let bb = vb[(k - 1 + off) as usize]; // 来自对角线 k-1（上移，y-1）
                let mut cx = B_UNSET;
                if k == delta - dd {
                    // 只能来自 k+1
                    if ba != B_UNSET && ba > 0 {
                        cx = ba - 1;
                    }
                } else if k == delta + dd {
                    // 只能来自 k-1
                    if bb != B_UNSET && (bb - (k - 1)) > 0 {
                        cx = bb;
                    }
                } else {
                    let a_ok = ba != B_UNSET && ba > 0;
                    let b_ok = bb != B_UNSET && (bb - (k - 1)) > 0;
                    if a_ok && (!b_ok || ba - 1 <= bb) {
                        cx = ba - 1;
                    } else if b_ok {
                        cx = bb;
                    }
                }
                if cx != B_UNSET {
                    let mut cy = cx - k;
                    // 沿对角线向后扩展（极大相等段），扩展后即为深度 d 的边界点
                    while cx > 0 && cy > 0 && eq(&a[(cx - 1) as usize], &b[(cy - 1) as usize]) {
                        cx -= 1;
                        cy -= 1;
                    }
                    vb[(k + off) as usize] = cx;
                    if k < b_lo {
                        b_lo = k;
                    }
                    if k > b_hi {
                        b_hi = k;
                    }
                    // 重叠检测：delta 偶数，对比深度 d 的前向边界。
                    // 返回后向边界点本身（空蛇），即 xdiff 的 split point。
                    if !odd && k >= -dd && k <= dd {
                        let fv = vf[(k + off) as usize];
                        if fv != F_UNSET && cx <= fv {
                            return (cx as usize, cy as usize, cx as usize, cy as usize);
                        }
                    }
                } else {
                    vb[(k + off) as usize] = B_UNSET;
                    if k < b_lo {
                        b_lo = k;
                    }
                    if k > b_hi {
                        b_hi = k;
                    }
                }
                k += 2;
            }

            // ---- 成本安全阀：超过阈值仍未检测到重叠 ----
            if d >= mxcost {
                // 取前向边界中 x+y 最大（推进最远）的点作为分裂点。
                // 该点严格位于盒子内部：x+y >= d >= 1（非原点），且到达终点
                // 需要深度 >= D > 2*mxcost > d（非终点），两侧严格变小。
                let mut best: Option<(isize, isize)> = None;
                let mut best_sum = -1isize;
                let mut k = -dd;
                while k <= dd {
                    let fv = vf[(k + off) as usize];
                    if fv != F_UNSET {
                        let mut cx = fv;
                        if cx > n {
                            cx = n;
                        }
                        let mut cy = cx - k;
                        if cy > m {
                            cy = m;
                            cx = cy + k;
                        }
                        if cx + cy > best_sum {
                            best_sum = cx + cy;
                            best = Some((cx, cy));
                        }
                    }
                    k += 2;
                }
                if let Some((bx, by)) = best {
                    return (bx as usize, by as usize, bx as usize, by as usize);
                }
            }
        }
        // 理论不可达：返回空蛇（调用方有防御分支）
        (0, 0, 0, 0)
    })();

    // 复位本次写过的区间，保持 scratch 对后续调用干净
    for idx in f_lo..=f_hi {
        vf[(idx + off) as usize] = F_UNSET;
    }
    for idx in b_lo..=b_hi {
        vb[(idx + off) as usize] = B_UNSET;
    }
    result
}

// ---------------------------------------------------------------------------
// 文本行差异
// ---------------------------------------------------------------------------

/// 按行切分文本，保留行尾 `\n`；最后一行无换行时也作为独立一行。
pub fn split_lines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, c) in s.bytes().enumerate() {
        if c == b'\n' {
            lines.push(&s[start..=i]);
            start = i + 1;
        }
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

/// 行级文本差异。
pub fn diff_lines(a: &str, b: &str) -> Vec<Edit> {
    let al = split_lines(a);
    let bl = split_lines(b);
    diff_slices(&al, &bl, |x, y| x == y)
}

/// 前 8000 字节内出现 NUL 视为二进制（与 git 判定一致）。
pub fn is_binary(data: &[u8]) -> bool {
    let end = data.len().min(8000);
    data[..end].contains(&0)
}

// ---------------------------------------------------------------------------
// unified diff
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Delete,
    Insert,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkLine {
    pub kind: LineKind,
    /// 行内容（不含行尾换行）
    pub text: String,
    /// 原文件中该行末尾没有换行
    pub no_newline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<HunkLine>,
}

/// 归一化编辑序列中变更段的顺序：每个极大连续变更组内，Delete 稳定排在
/// Insert 之前。Myers 分治产出的序列不保证这一顺序，而 git/xdiff 的规范
/// 输出是同组内先删后增——统一顺序让 riv 的 diff 与 git 逐字节一致。
fn normalize_change_order(edits: &mut [Edit]) {
    let n = edits.len();
    let mut i = 0;
    while i < n {
        if !edits[i].is_change() {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && edits[i].is_change() {
            i += 1;
        }
        let group = &mut edits[start..i];
        // 稳定分区：Delete 前移，Insert 后移（各自内部相对顺序不变）
        group.sort_by_key(|e| matches!(e, Edit::Insert { .. }));
    }
}

/// 生成 unified diff 的 hunk 列表（不含文件头）。
pub fn unified_hunks(old: &str, new: &str, context: usize) -> Vec<Hunk> {
    let mut edits = diff_lines(old, new);
    normalize_change_order(&mut edits);
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);

    let last_no_nl = |lines: &[&str]| -> bool {
        match lines.last() {
            Some(l) => !l.ends_with('\n'),
            None => false,
        }
    };
    let old_no_nl = last_no_nl(&old_lines);
    let new_no_nl = last_no_nl(&new_lines);

    // 1. 收集「变更组」（极大连续非 Equal 段）及其起止 edit 下标
    struct Group {
        start: usize,
        end: usize,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut i = 0;
    while i < edits.len() {
        if edits[i].is_change() {
            let start = i;
            while i < edits.len() && edits[i].is_change() {
                i += 1;
            }
            groups.push(Group { start, end: i });
        } else {
            i += 1;
        }
    }
    if groups.is_empty() {
        return Vec::new();
    }

    // 2. 合并间隔 ≤ 2*context 的组
    let mut merged: Vec<Group> = Vec::new();
    for g in groups {
        if let Some(last) = merged.last_mut() {
            let gap = g.start - last.end; // 组间 Equal 条数
            if gap <= 2 * context {
                last.end = g.end;
                continue;
            }
        }
        merged.push(g);
    }

    // 3. 每个组生成 hunk（带上下文修剪）
    let mut hunks = Vec::new();
    for (gi, g) in merged.iter().enumerate() {
        // 前导上下文
        let lead = if gi == 0 {
            g.start.min(context)
        } else {
            // 不越过上一个 hunk 已消费的位置（简化：直接取 context，合并后组间 gap > 2*context 已保证不重叠）
            g.start.min(context)
        };
        let start_idx = g.start - lead;
        let mut end_idx = g.end;
        // 尾随上下文：延伸到下一个变更组（受 context 限制）
        let mut trail = 0usize;
        while trail < context && end_idx < edits.len() && edits[end_idx].is_equal() {
            // 若下一个组就在 context 之内，已被合并；此处只需补足 context 条 Equal
            trail += 1;
            end_idx += 1;
        }
        // 但如果后面还有组且间距很小……上面已合并，无需处理

        let mut lines = Vec::new();
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        let mut old_first: Option<usize> = None;
        let mut new_first: Option<usize> = None;
        for e in &edits[start_idx..end_idx] {
            match *e {
                Edit::Equal { a, b } => {
                    if old_first.is_none() {
                        old_first = Some(a);
                    }
                    if new_first.is_none() {
                        new_first = Some(b);
                    }
                    old_count += 1;
                    new_count += 1;
                    let is_last_old = a + 1 == old_lines.len();
                    let is_last_new = b + 1 == new_lines.len();
                    lines.push(HunkLine {
                        kind: LineKind::Context,
                        text: old_lines[a]
                            .strip_suffix('\n')
                            .unwrap_or(old_lines[a])
                            .to_string(),
                        no_newline: (old_no_nl && is_last_old) || (new_no_nl && is_last_new),
                    });
                }
                Edit::Delete { a } => {
                    if old_first.is_none() {
                        old_first = Some(a);
                    }
                    old_count += 1;
                    let is_last = a + 1 == old_lines.len();
                    lines.push(HunkLine {
                        kind: LineKind::Delete,
                        text: old_lines[a]
                            .strip_suffix('\n')
                            .unwrap_or(old_lines[a])
                            .to_string(),
                        no_newline: old_no_nl && is_last,
                    });
                }
                Edit::Insert { b } => {
                    if new_first.is_none() {
                        new_first = Some(b);
                    }
                    new_count += 1;
                    let is_last = b + 1 == new_lines.len();
                    lines.push(HunkLine {
                        kind: LineKind::Insert,
                        text: new_lines[b]
                            .strip_suffix('\n')
                            .unwrap_or(new_lines[b])
                            .to_string(),
                        no_newline: new_no_nl && is_last,
                    });
                }
            }
        }
        let old_start = old_first.unwrap_or(0);
        let new_start = new_first.unwrap_or(0);
        hunks.push(Hunk {
            old_start: if old_count == 0 {
                old_start
            } else {
                old_start + 1
            },
            old_count,
            new_start: if new_count == 0 {
                new_start
            } else {
                new_start + 1
            },
            new_count,
            lines,
        });
    }
    hunks
}

/// 格式化 hunk 头，如 `@@ -1,4 +1,5 @@`。
pub fn format_hunk_header(h: &Hunk) -> String {
    let old_range = if h.old_count == 1 {
        format!("{}", h.old_start)
    } else {
        format!("{},{}", h.old_start, h.old_count)
    };
    let new_range = if h.new_count == 1 {
        format!("{}", h.new_start)
    } else {
        format!("{},{}", h.new_start, h.new_count)
    };
    format!("@@ -{} +{} @@", old_range, new_range)
}

/// 将 hunk 渲染为文本行（不含颜色）。
pub fn render_hunk(h: &Hunk) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{}", format_hunk_header(h));
    for l in &h.lines {
        let prefix = match l.kind {
            LineKind::Context => ' ',
            LineKind::Delete => '-',
            LineKind::Insert => '+',
        };
        let _ = writeln!(s, "{}{}", prefix, l.text);
        if l.no_newline {
            let _ = writeln!(s, "\\ No newline at end of file");
        }
    }
    s
}

/// 完整的文件级 diff（git 风格文件头 + hunk）。
/// `old`/`new` 为 `None` 表示文件不存在（新增/删除）。
pub fn format_file_diff(
    path: &str,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
    context: usize,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "diff --riv a/{path} b/{path}");

    match (old, new) {
        (None, Some(d)) => {
            let _ = writeln!(out, "new file mode 100644");
            let _ = writeln!(out, "--- /dev/null");
            let _ = writeln!(out, "+++ b/{path}");
            if is_binary(d) {
                let _ = writeln!(out, "Binary file b/{path} added");
            } else {
                let text = String::from_utf8_lossy(d);
                let hunks = unified_hunks("", &text, context);
                for h in &hunks {
                    out.push_str(&render_hunk(h));
                }
            }
        }
        (Some(d), None) => {
            let _ = writeln!(out, "deleted file mode 100644");
            let _ = writeln!(out, "--- a/{path}");
            let _ = writeln!(out, "+++ /dev/null");
            if is_binary(d) {
                let _ = writeln!(out, "Binary file a/{path} deleted");
            } else {
                let text = String::from_utf8_lossy(d);
                let hunks = unified_hunks(&text, "", context);
                for h in &hunks {
                    out.push_str(&render_hunk(h));
                }
            }
        }
        (Some(od), Some(nd)) => {
            if is_binary(od) || is_binary(nd) {
                let _ = writeln!(out, "Binary files a/{path} and b/{path} differ");
                return out;
            }
            let _ = writeln!(out, "--- a/{path}");
            let _ = writeln!(out, "+++ b/{path}");
            let ot = String::from_utf8_lossy(od);
            let nt = String::from_utf8_lossy(nd);
            let hunks = unified_hunks(&ot, &nt, context);
            for h in &hunks {
                out.push_str(&render_hunk(h));
            }
        }
        (None, None) => {}
    }
    out
}

// ---------------------------------------------------------------------------
// 统计（--stat）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct FileStat {
    pub path: String,
    pub insertions: usize,
    pub deletions: usize,
}

impl FileStat {
    pub fn total(&self) -> usize {
        self.insertions + self.deletions
    }
}

/// 计算单个文件的增删行数（二进制文件返回 None）。
pub fn file_stat(path: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> Option<FileStat> {
    if old.map(is_binary).unwrap_or(false) || new.map(is_binary).unwrap_or(false) {
        return None;
    }
    match (old, new) {
        (Some(od), Some(nd)) => {
            let ot = String::from_utf8_lossy(od);
            let nt = String::from_utf8_lossy(nd);
            let edits = diff_lines(&ot, &nt);
            let ins = edits
                .iter()
                .filter(|e| matches!(e, Edit::Insert { .. }))
                .count();
            let del = edits
                .iter()
                .filter(|e| matches!(e, Edit::Delete { .. }))
                .count();
            Some(FileStat {
                path: path.to_string(),
                insertions: ins,
                deletions: del,
            })
        }
        (None, Some(nd)) => Some(FileStat {
            path: path.to_string(),
            insertions: split_lines(&String::from_utf8_lossy(nd)).len(),
            deletions: 0,
        }),
        (Some(od), None) => Some(FileStat {
            path: path.to_string(),
            insertions: 0,
            deletions: split_lines(&String::from_utf8_lossy(od)).len(),
        }),
        (None, None) => None,
    }
}

/// 渲染 `--stat` 输出。
pub fn format_stat(stats: &[FileStat], graph_width: usize) -> String {
    if stats.is_empty() {
        return String::new();
    }
    let max_total = stats.iter().map(|s| s.total()).max().unwrap_or(0).max(1);
    let name_width = stats
        .iter()
        .map(|s| s.path.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for s in stats {
        let total = s.total();
        // git 语义：最大变更未超界时每个变更行一个字符；超界才按比例缩放到 graph_width
        let (plus, minus) = if max_total > graph_width {
            (
                (s.insertions * graph_width).div_ceil(max_total),
                (s.deletions * graph_width).div_ceil(max_total),
            )
        } else {
            (s.insertions, s.deletions)
        };
        let bar: String = "+".repeat(plus.min(graph_width)) + &"-".repeat(minus.min(graph_width));
        let total_disp = if total == 0 {
            String::new()
        } else {
            format!("{}", total)
        };
        let _ = writeln!(
            out,
            " {:<nw$} | {:>4} {}",
            s.path,
            total_disp,
            bar,
            nw = name_width
        );
    }
    let files = stats.len();
    let ins: usize = stats.iter().map(|s| s.insertions).sum();
    let del: usize = stats.iter().map(|s| s.deletions).sum();
    let mut summary = format!(
        " {} file{} changed",
        files,
        if files == 1 { "" } else { "s" }
    );
    if ins > 0 {
        summary.push_str(&format!(
            ", {} insertion{}(+)",
            ins,
            if ins == 1 { "" } else { "s" }
        ));
    }
    if del > 0 {
        summary.push_str(&format!(
            ", {} deletion{}(-)",
            del,
            if del == 1 { "" } else { "s" }
        ));
    }
    let _ = writeln!(out, "{}", summary);
    out
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 应用编辑脚本，验证它确实把 a 变成 b
    fn apply<T: Clone>(a: &[T], b: &[T], edits: &[Edit]) -> Vec<T> {
        let mut out = Vec::new();
        let mut ai = 0;
        let mut bi = 0;
        for e in edits {
            match *e {
                Edit::Equal { .. } => {
                    out.push(a[ai].clone());
                    ai += 1;
                    bi += 1;
                }
                Edit::Delete { .. } => {
                    ai += 1;
                }
                Edit::Insert { .. } => {
                    out.push(b[bi].clone());
                    bi += 1;
                }
            }
        }
        assert_eq!(ai, a.len(), "a 未被完全消费");
        assert_eq!(bi, b.len(), "b 未被完全消费");
        out
    }

    fn lcs_len(a: &[u32], b: &[u32]) -> usize {
        let n = a.len();
        let m = b.len();
        let mut dp = vec![vec![0usize; m + 1]; n + 1];
        for i in 0..n {
            for j in 0..m {
                dp[i + 1][j + 1] = if a[i] == b[j] {
                    dp[i][j] + 1
                } else {
                    dp[i][j + 1].max(dp[i + 1][j])
                };
            }
        }
        dp[n][m]
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn identical_sequences_all_equal() {
        let a: Vec<u32> = vec![1, 2, 3];
        let b = a.clone();
        let edits = diff_slices(&a, &b, |x, y| x == y);
        assert!(edits.iter().all(|e| e.is_equal()));
        assert_eq!(edits.len(), 3);
    }

    #[test]
    fn empty_vs_nonempty() {
        let a: Vec<u32> = vec![];
        let b = vec![1u32, 2];
        let edits = diff_slices(&a, &b, |x, y| x == y);
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|e| matches!(e, Edit::Insert { .. })));
        assert_eq!(apply(&a, &b, &edits), b);
    }

    #[test]
    fn simple_line_diff() {
        let a = "a\nb\nc\n";
        let b = "a\nB\nc\n";
        let edits = diff_lines(a, b);
        let del = edits
            .iter()
            .filter(|e| matches!(e, Edit::Delete { .. }))
            .count();
        let ins = edits
            .iter()
            .filter(|e| matches!(e, Edit::Insert { .. }))
            .count();
        assert_eq!((del, ins), (1, 1));
        assert_eq!(
            apply(&split_lines(a), &split_lines(b), &edits),
            split_lines(b)
        );
    }

    #[test]
    fn all_different_no_common() {
        let a: Vec<u32> = vec![1, 2, 3];
        let b: Vec<u32> = vec![4, 5];
        let edits = diff_slices(&a, &b, |x, y| x == y);
        assert_eq!(edits.iter().filter(|e| e.is_change()).count(), 5);
        assert_eq!(apply(&a, &b, &edits), b);
    }

    #[test]
    fn empty_middle_snake_case() {
        // [X, Y] vs [Z]：没有任何公共行，中间蛇为空
        let a: Vec<u32> = vec![1, 2];
        let b: Vec<u32> = vec![3];
        let edits = diff_slices(&a, &b, |x, y| x == y);
        assert_eq!(edits.iter().filter(|e| e.is_change()).count(), 3);
        assert_eq!(apply(&a, &b, &edits), b);
    }

    #[test]
    fn repeated_elements() {
        let a: Vec<u32> = vec![1, 1, 1, 2];
        let b: Vec<u32> = vec![1, 2, 1];
        let edits = diff_slices(&a, &b, |x, y| x == y);
        assert_eq!(apply(&a, &b, &edits), b);
        let changes = edits.iter().filter(|e| e.is_change()).count();
        assert_eq!(changes, 3); // 最优：3 次编辑
    }

    /// 属性测试：随机输入下，编辑脚本必须（1）能把 a 变成 b；（2）编辑次数等于
    /// 朴素 DP 求得的最优值 n+m-2*LCS。
    #[test]
    fn property_minimal_and_valid() {
        let mut rng = Lcg(0x5EED_1234_ABCD_EF01);
        for _ in 0..3000 {
            let n = rng.below(9) as usize;
            let m = rng.below(9) as usize;
            let a: Vec<u32> = (0..n).map(|_| rng.below(4) as u32).collect();
            let b: Vec<u32> = (0..m).map(|_| rng.below(4) as u32).collect();
            let edits = diff_slices(&a, &b, |x, y| x == y);
            // 有效性
            assert_eq!(
                apply(&a, &b, &edits),
                b,
                "a={:?} b={:?} edits={:?}",
                a,
                b,
                edits
            );
            // 最小性
            let expected = n + m - 2 * lcs_len(&a, &b);
            let actual = edits.iter().filter(|e| e.is_change()).count();
            assert_eq!(
                actual, expected,
                "非最小编辑脚本: a={:?} b={:?} edits={:?}",
                a, b, edits
            );
        }
    }

    /// 更大规模的随机属性测试（长序列，验证线性空间实现在大输入下的正确性）
    #[test]
    fn property_larger_inputs() {
        let mut rng = Lcg(0xC0FF_EE00_1122_3344);
        for _ in 0..60 {
            let n = 50 + rng.below(150) as usize;
            let m = 50 + rng.below(150) as usize;
            let a: Vec<u32> = (0..n).map(|_| rng.below(8) as u32).collect();
            let b: Vec<u32> = (0..m).map(|_| rng.below(8) as u32).collect();
            let edits = diff_slices(&a, &b, |x, y| x == y);
            assert_eq!(apply(&a, &b, &edits), b);
            let expected = n + m - 2 * lcs_len(&a, &b);
            let actual = edits.iter().filter(|e| e.is_change()).count();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn unified_hunks_basic() {
        let old = "line1\nline2\nline3\nline4\nline5\n";
        let new = "line1\nline2 changed\nline3\nline4\nline5\nline6\n";
        let hunks = unified_hunks(old, new, 3);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_count, 5);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_count, 6);
        assert_eq!(format_hunk_header(h), "@@ -1,5 +1,6 @@");
    }

    #[test]
    fn unified_hunks_no_changes() {
        let s = "a\nb\n";
        assert!(unified_hunks(s, s, 3).is_empty());
    }

    #[test]
    fn unified_hunks_two_separate() {
        let old: String = (0..20).map(|i| format!("line{}\n", i)).collect();
        let mut new = old.clone();
        new = new.replace("line1\n", "line1 modified\n");
        new = new.replace("line18\n", "line18 modified\n");
        let hunks = unified_hunks(&old, &new, 3);
        assert_eq!(hunks.len(), 2);
    }

    #[test]
    fn unified_hunks_deletes_before_inserts() {
        // git/xdiff 规范：同一变更组内先删后增。Myers 分治不保证顺序，
        // normalize_change_order 必须统一（含混合增删插空洞的场景）。
        let old = "def hello():\n    print(\"hello\")\n";
        let new = "def hello(name, greeting=\"hi\"):\n    print(f\"{greeting} {name}\")\n";
        let hunks = unified_hunks(old, new, 3);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        let first_delete = h
            .lines
            .iter()
            .position(|l| matches!(l.kind, LineKind::Delete));
        let first_insert = h
            .lines
            .iter()
            .position(|l| matches!(l.kind, LineKind::Insert));
        assert!(
            first_delete < first_insert,
            "删除行应先于新增行: {first_delete:?} vs {first_insert:?}"
        );
        // 增删计数不变（仍是 2 换 2）
        assert_eq!(
            h.lines
                .iter()
                .filter(|l| matches!(l.kind, LineKind::Delete))
                .count(),
            2
        );
        assert_eq!(
            h.lines
                .iter()
                .filter(|l| matches!(l.kind, LineKind::Insert))
                .count(),
            2
        );
    }

    #[test]
    fn no_newline_marker() {
        let old = "a\nb";
        let new = "a\nb\n";
        let hunks = unified_hunks(old, new, 3);
        assert_eq!(hunks.len(), 1);
        let rendered = render_hunk(&hunks[0]);
        assert!(rendered.contains("\\ No newline at end of file"));
    }

    #[test]
    fn file_diff_new_and_deleted() {
        let out = format_file_diff("new.txt", None, Some(b"hello\n"), 3);
        assert!(out.contains("new file mode 100644"));
        assert!(out.contains("--- /dev/null"));
        assert!(out.contains("+++ b/new.txt"));
        assert!(out.contains("+hello"));

        let out = format_file_diff("old.txt", Some(b"hello\n"), None, 3);
        assert!(out.contains("deleted file mode 100644"));
        assert!(out.contains("-hello"));
    }

    #[test]
    fn file_diff_binary() {
        let out = format_file_diff("bin", Some(b"a\x00b"), Some(b"c\x00d"), 3);
        assert!(out.contains("Binary files"));
    }

    #[test]
    fn stat_output() {
        let stats = vec![
            FileStat {
                path: "a.txt".into(),
                insertions: 10,
                deletions: 2,
            },
            FileStat {
                path: "b.txt".into(),
                insertions: 0,
                deletions: 5,
            },
        ];
        let out = format_stat(&stats, 20);
        assert!(out.contains("a.txt"));
        assert!(out.contains("2 files changed"));
        assert!(out.contains("10 insertions(+)"));
        assert!(out.contains("7 deletions(-)"));
    }

    #[test]
    fn split_lines_handles_trailing_newline() {
        assert_eq!(split_lines("a\nb\n"), vec!["a\n", "b\n"]);
        assert_eq!(split_lines("a\nb"), vec!["a\n", "b"]);
        assert_eq!(split_lines(""), Vec::<&str>::new());
        assert_eq!(split_lines("\n"), vec!["\n"]);
    }
}
