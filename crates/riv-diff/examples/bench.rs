use riv_diff::{diff_slices, split_lines};
use std::time::Instant;

fn main() {
    // 场景 1：10 万行文件，修改 1%
    let n = 100_000;
    let a: Vec<String> = (0..n)
        .map(|i| format!("line {} of a fairly long source file\n", i))
        .collect();
    let mut b = a.clone();
    for i in (0..n).step_by(100) {
        b[i] = format!("line {} MODIFIED\n", i);
    }
    let al: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    let bl: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
    let t = Instant::now();
    let edits = diff_slices(&al, &bl, |x, y| x == y);
    let d = t.elapsed();
    let changes = edits.iter().filter(|e| e.is_change()).count();
    println!(
        "100k lines, 1% changed: {:?} edits={} (expect {}) in {:?}",
        edits.len(),
        changes,
        2 * n / 100,
        d
    );

    // 场景 2：完全不同的两个 5 万行文件（D = N+M 最坏情况）
    let a2: Vec<u32> = (0..50_000).map(|i| i as u32).collect();
    let b2: Vec<u32> = (100_000..150_000).map(|i| i as u32).collect();
    let t = Instant::now();
    let edits = diff_slices(&a2, &b2, |x, y| x == y);
    let d = t.elapsed();
    println!(
        "50k vs 50k fully disjoint: {} edits (expect 100000) in {:?}",
        edits.iter().filter(|e| e.is_change()).count(),
        d
    );

    // 场景 3：周期串病态 case（abab... vs babab...，5万行）
    let a3: Vec<u32> = (0..100_000).map(|i| (i % 2) as u32).collect();
    let b3: Vec<u32> = (1..=100_000).map(|i| (i % 2) as u32).collect();
    let t = Instant::now();
    let edits = diff_slices(&a3, &b3, |x, y| x == y);
    let d = t.elapsed();
    println!(
        "100k periodic shift: {} edits in {:?}",
        edits.iter().filter(|e| e.is_change()).count(),
        d
    );

    // 场景 4：文本行 diff（常见场景）
    let text_a: String = (0..20_000)
        .map(|i| format!("fn f{}() {{ return {}; }}\n", i, i))
        .collect();
    let mut text_b = text_a.clone();
    text_b = text_b.replace("fn f19999()", "fn f19999_v2()");
    let t = Instant::now();
    let edits = diff_slices(&split_lines(&text_a), &split_lines(&text_b), |x, y| x == y);
    println!(
        "20k-line text, 1 line changed: {} edits (expect 2) in {:?}",
        edits.iter().filter(|e| e.is_change()).count(),
        t.elapsed()
    );
}
