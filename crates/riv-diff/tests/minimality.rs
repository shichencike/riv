//! Myers 引擎的最小性与有效性回归测试。
//!
//! 校验标准（对每个用例）：
//! 1. **有效性**：编辑脚本能把 a 严格变成 b；`Equal{a:i,b:j}` 满足 a[i]==b[j]；
//!    a 侧下标与 b 侧下标均从 0 开始严格递增且恰好消费完整序列。
//! 2. **最小性**：变更条数 == `n + m - 2*LCS`（朴素 DP 最优值）。
//!
//! 覆盖：多字母表穷举、二符号长序列穷举（重复元素压力）、
//! 大规模随机、块编辑（模拟真实文件修改）、病态重复模式。

use riv_diff::{diff_slices, Edit};

/// 严格校验编辑脚本：返回 (是否有效, 变更条数)。
fn check(a: &[u32], b: &[u32], edits: &[Edit]) -> (bool, usize) {
    let mut ai = 0usize;
    let mut bi = 0usize;
    let mut out = Vec::new();
    let mut changes = 0usize;
    for e in edits {
        match *e {
            Edit::Equal { a: i, b: j } => {
                if i != ai || j != bi {
                    return (false, changes); // 下标必须严格按序消费
                }
                if a[i] != b[j] {
                    return (false, changes);
                }
                out.push(a[i]);
                ai += 1;
                bi += 1;
            }
            Edit::Delete { a: i } => {
                if i != ai {
                    return (false, changes);
                }
                ai += 1;
                changes += 1;
            }
            Edit::Insert { b: j } => {
                if j != bi {
                    return (false, changes);
                }
                out.push(b[j]);
                bi += 1;
                changes += 1;
            }
        }
    }
    (ai == a.len() && bi == b.len() && out == b, changes)
}

fn lcs_len(a: &[u32], b: &[u32]) -> usize {
    // 滚动数组 DP，O(min) 内存
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut prev = vec![0usize; short.len() + 1];
    for &lv in long {
        let mut cur = vec![0usize; short.len() + 1];
        for j in 0..short.len() {
            cur[j + 1] = if lv == short[j] {
                prev[j] + 1
            } else {
                cur[j].max(prev[j + 1])
            };
        }
        prev = cur;
    }
    prev[short.len()]
}

/// 校验单个用例，失败时带完整上下文 panic。
fn verify(a: &[u32], b: &[u32]) {
    let edits = diff_slices(a, b, |x, y| x == y);
    let (valid, changes) = check(a, b, &edits);
    let expected = a.len() + b.len() - 2 * lcs_len(a, b);
    assert!(valid, "无效编辑脚本: a={:?} b={:?} edits={:?}", a, b, edits);
    assert_eq!(
        changes, expected,
        "非最小编辑脚本: a={:?} b={:?} edits={:?}",
        a, b, edits
    );
}

/// 简单线性同余随机数（避免测试依赖 rng crate）。
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// 穷举：字母表大小 2、长度 ≤ 9（重复元素压力最大的场景之一）。
#[test]
fn exhaustive_alphabet2_len9() {
    for n in 1..=9usize {
        for m in 1..=9usize {
            let total = 1u32 << n.max(m);
            for mask_a in 0..total {
                let a: Vec<u32> = (0..n).map(|i| (mask_a >> i) & 1).collect();
                for mask_b in 0..total {
                    let b: Vec<u32> = (0..m).map(|i| (mask_b >> i) & 1).collect();
                    verify(&a, &b);
                }
            }
        }
    }
}

/// 穷举：字母表大小 3、长度 ≤ 6。
#[test]
fn exhaustive_alphabet3_len6() {
    for n in 1..=6usize {
        for m in 1..=6usize {
            let total = 3u32.pow(n.max(m) as u32);
            for mask_a in 0..total {
                let a: Vec<u32> = (0..n).map(|i| (mask_a / 3u32.pow(i as u32)) % 3).collect();
                for mask_b in 0..total {
                    let b: Vec<u32> = (0..m).map(|i| (mask_b / 3u32.pow(i as u32)) % 3).collect();
                    verify(&a, &b);
                }
            }
        }
    }
}

/// 穷举：字母表大小 4、长度 ≤ 5。
#[test]
fn exhaustive_alphabet4_len5() {
    for n in 1..=5usize {
        for m in 1..=5usize {
            let total = 4u32.pow(n.max(m) as u32);
            for mask_a in 0..total {
                let a: Vec<u32> = (0..n).map(|i| (mask_a / 4u32.pow(i as u32)) % 4).collect();
                for mask_b in 0..total {
                    let b: Vec<u32> = (0..m).map(|i| (mask_b / 4u32.pow(i as u32)) % 4).collect();
                    verify(&a, &b);
                }
            }
        }
    }
}

/// 病态重复模式：周期串互相错位，Myers 最容易出错的场景。
#[test]
fn pathological_periodic_patterns() {
    for period in 2..=6usize {
        for len in 1..=12usize {
            let a: Vec<u32> = (0..len * period).map(|i| (i % period) as u32).collect();
            // 错位一位的周期串
            let b: Vec<u32> = (1..=len * period + 1)
                .map(|i| (i % period) as u32)
                .collect();
            verify(&a, &b);
            // 反转
            let mut ra = a.clone();
            ra.reverse();
            verify(&a, &ra);
            // 抽掉一段
            let mut cut = a.clone();
            let end = (len + period).min(cut.len());
            if end > len {
                cut.drain(len..end);
            }
            verify(&a, &cut);
            // 重复一遍
            let mut doubled = a.clone();
            doubled.extend_from_slice(&a);
            verify(&a, &doubled);
        }
    }
}

/// 大规模随机：多字母表、中等长度。
#[test]
fn random_medium_multi_alphabet() {
    let mut rng = Lcg(0x1234_5678_9ABC_DEF0);
    for _ in 0..400 {
        let alpha = 2 + rng.below(6) as u32;
        let n = 50 + rng.below(250) as usize;
        let m = 50 + rng.below(250) as usize;
        let a: Vec<u32> = (0..n).map(|_| rng.below(alpha as u64) as u32).collect();
        let b: Vec<u32> = (0..m).map(|_| rng.below(alpha as u64) as u32).collect();
        verify(&a, &b);
    }
}

/// 块编辑场景：模拟真实文件修改（插入/删除/移动连续块），少量大块变更。
#[test]
fn random_block_edits() {
    let mut rng = Lcg(0xDEAD_BEEF_CAFE_1234);
    for _ in 0..300 {
        let n = 200 + rng.below(400) as usize;
        let a: Vec<u32> = (0..n).map(|i| i as u32).collect();
        let mut b = a.clone();
        // 随机执行若干块操作
        let ops = 1 + rng.below(8);
        for _ in 0..ops {
            if b.is_empty() {
                break;
            }
            let pos = rng.below(b.len() as u64) as usize;
            let len = 1 + rng.below(20) as usize;
            let end = (pos + len).min(b.len());
            match rng.below(3) {
                0 => {
                    // 删除块
                    b.drain(pos..end);
                }
                1 => {
                    // 插入块（复制已有内容，制造重复）
                    let src = b[rng.below(b.len() as u64) as usize];
                    let ins: Vec<u32> = (0..len).map(|k| src + k as u32).collect();
                    b.splice(pos..pos, ins);
                }
                _ => {
                    // 替换块
                    let base = rng.below(1000) as u32;
                    let rep: Vec<u32> = (0..end - pos).map(|k| base + k as u32).collect();
                    b.splice(pos..end, rep);
                }
            }
        }
        verify(&a, &b);
    }
}

/// 极端形状：极长 vs 极短、完全包含、完全错开。
#[test]
fn extreme_shapes() {
    let long: Vec<u32> = (0..2000).map(|i| (i % 97) as u32).collect();
    // 空前缀 + 全不同
    let disjoint: Vec<u32> = (1000..3000).map(|i| (i % 97 + 1000) as u32).collect();
    verify(&long, &disjoint);
    // 完整子序列
    let sub: Vec<u32> = long.iter().step_by(3).copied().collect();
    verify(&long, &sub);
    // 完全相同
    verify(&long, &long);
    // 单元素
    verify(&[42], &[]);
    verify(&[], &[42]);
    verify(&[42], &[42]);
    verify(&[42], &[43]);
}
