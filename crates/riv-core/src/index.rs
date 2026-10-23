//! riv 的索引（暂存区）：`.riv/index`。
//!
//! 记录每个被跟踪文件的路径、Oid、mode 与 stat 缓存（size + mtime），
//! 使 `status` 无需重新哈希未变化的文件。
//!
//! 二进制格式（小端）：
//! ```text
//! magic   b"RIVIDX\x01\x00"
//! version u32 = 2
//! count   u32
//! entries (按路径排序):
//!   path_len   u16
//!   path       [u8]
//!   oid        [u8; 32]
//!   mode       u32
//!   size       u64
//!   mtime_sec  i64
//!   mtime_nsec i32
//!   flags      u32
//! checksum u64   (v2: blake3 前 8 字节，覆盖之前所有字节；v1: FNV-1a，只读兼容)
//! ```
//!
//! racy 检查（对齐 git）：`load` 时额外记录索引文件自身的 mtime（仅运行时，
//! 不序列化）。`add` 时若某文件的 mtime 秒 >= 索引文件 mtime 秒，则其 stat
//! 缓存不可信（可能与索引写盘同秒），强制重新哈希，防止粗粒度 mtime 文件
//! 系统上的漏检。

use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::error::{Error, Result};
use crate::oid::Oid;

const MAGIC: &[u8; 8] = b"RIVIDX\x01\x00";
/// v1 用 FNV-1a 校验和（只读兼容）；v2 起用 blake3 前 8 字节。
const VERSION: u32 = 2;
const LEGACY_VERSION: u32 = 1;
/// 索引条目的最小序列化长度（path_len 2 + oid 32 + mode 4 + size 8 +
/// mtime_sec 8 + mtime_nsec 4 + flags 4 + 空路径 0）。
const MIN_ENTRY_BYTES: usize = 62;
/// 损坏/恶意索引的条数上限：count 不可能超过「文件大小能容纳的条目数」。
const MAX_PREALLOC: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub path: String,
    pub oid: Oid,
    pub mode: u32,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: i32,
    pub flags: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Index {
    pub entries: Vec<IndexEntry>,
    /// 索引文件自身的 mtime（unix 秒）；仅 `load` 时填充，不序列化。
    /// 用于 racy 检查：文件 mtime >= 该值时 stat 缓存不可信。
    pub index_mtime_sec: Option<i64>,
}

impl Index {
    pub fn new() -> Self {
        Index {
            entries: Vec::new(),
            index_mtime_sec: None,
        }
    }

    pub fn load(path: &Path) -> Result<Index> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Index::new()),
            Err(e) => return Err(e.into()),
        };
        let mut index = Self::decode(&bytes)?;
        index.index_mtime_sec = fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        Ok(index)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = self.encode();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // 原子写：tmp + rename
        let tmp = path.with_extension("idx.tmp");
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn get(&self, path: &str) -> Option<&IndexEntry> {
        self.entries
            .binary_search_by(|e| e.path.as_str().cmp(path))
            .ok()
            .map(|i| &self.entries[i])
    }

    pub fn insert(&mut self, entry: IndexEntry) {
        match self
            .entries
            .binary_search_by(|e| e.path.as_str().cmp(&entry.path))
        {
            Ok(i) => self.entries[i] = entry,
            Err(i) => self.entries.insert(i, entry),
        }
    }

    pub fn remove(&mut self, path: &str) -> Option<IndexEntry> {
        match self.entries.binary_search_by(|e| e.path.as_str().cmp(path)) {
            Ok(i) => Some(self.entries.remove(i)),
            Err(_) => None,
        }
    }

    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.path.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    // ---- 序列化 ----

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in &self.entries {
            let pb = e.path.as_bytes();
            out.extend_from_slice(&(pb.len() as u16).to_le_bytes());
            out.extend_from_slice(pb);
            out.extend_from_slice(e.oid.as_bytes());
            out.extend_from_slice(&e.mode.to_le_bytes());
            out.extend_from_slice(&e.size.to_le_bytes());
            out.extend_from_slice(&e.mtime_sec.to_le_bytes());
            out.extend_from_slice(&e.mtime_nsec.to_le_bytes());
            out.extend_from_slice(&e.flags.to_le_bytes());
        }
        out.extend_from_slice(&checksum_v2(&out).to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Result<Index> {
        if bytes.len() < 20 || &bytes[..8] != MAGIC {
            return Err(Error::CorruptIndex("bad magic".into()));
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != VERSION && version != LEGACY_VERSION {
            return Err(Error::CorruptIndex(format!(
                "unsupported version {version}"
            )));
        }
        let count = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        // count 上限：每条目至少 62 字节，count 超过文件能容纳的量必为损坏数据
        // （否则 Vec::with_capacity(count) 会被伪造的 count 放大成 OOM）。
        let max_entries = bytes.len().saturating_sub(24) / MIN_ENTRY_BYTES;
        if count > max_entries {
            return Err(Error::CorruptIndex(format!(
                "entry count {count} exceeds file capacity {max_entries}"
            )));
        }
        let body = &bytes[..bytes.len() - 8];
        let stored_checksum = u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap());
        let computed = if version == LEGACY_VERSION {
            fnv1a(body)
        } else {
            checksum_v2(body)
        };
        if computed != stored_checksum {
            return Err(Error::CorruptIndex("checksum mismatch".into()));
        }
        let mut off = 16;
        let mut entries = Vec::with_capacity(count.min(MAX_PREALLOC));
        for _ in 0..count {
            if off + 2 > bytes.len() - 8 {
                return Err(Error::CorruptIndex("truncated entry".into()));
            }
            let path_len = u16::from_le_bytes(bytes[off..off + 2].try_into().unwrap()) as usize;
            off += 2;
            if off + path_len + 32 + 4 + 8 + 8 + 4 + 4 > bytes.len() - 8 {
                return Err(Error::CorruptIndex("truncated entry".into()));
            }
            let path = String::from_utf8(bytes[off..off + path_len].to_vec())
                .map_err(|_| Error::CorruptIndex("path not utf-8".into()))?;
            off += path_len;
            let oid = Oid::from_bytes(bytes[off..off + 32].try_into().unwrap());
            off += 32;
            let mode = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
            off += 4;
            let size = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
            off += 8;
            let mtime_sec = i64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
            off += 8;
            let mtime_nsec = i32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
            off += 4;
            let flags = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
            off += 4;
            entries.push(IndexEntry {
                path,
                oid,
                mode,
                size,
                mtime_sec,
                mtime_nsec,
                flags,
            });
        }
        Ok(Index {
            entries,
            index_mtime_sec: None,
        })
    }
}

/// FNV-1a 64 位（v1 索引遗留校验和，仅解码时兼容使用）。
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// v2 索引校验和：blake3 的前 8 字节（小端）。
/// 与对象库同一哈希族，避免再维护一套校验码。
pub fn checksum_v2(data: &[u8]) -> u64 {
    let h = blake3::hash(data);
    u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, seed: u8) -> IndexEntry {
        IndexEntry {
            path: path.into(),
            oid: Oid::from_bytes([seed; 32]),
            mode: 0o100644,
            size: 100 + seed as u64,
            mtime_sec: 1_700_000_000 + seed as i64,
            mtime_nsec: 123,
            flags: 0,
        }
    }

    #[test]
    fn roundtrip() {
        let mut idx = Index::new();
        idx.insert(entry("src/main.rs", 1));
        idx.insert(entry("README.md", 2));
        idx.insert(entry("a/b/c.txt", 3));
        let bytes = idx.encode();
        let decoded = Index::decode(&bytes).unwrap();
        assert_eq!(decoded.entries.len(), 3);
        assert_eq!(decoded.get("a/b/c.txt").unwrap().size, 103);
        assert_eq!(decoded.get("README.md").unwrap().mtime_sec, 1_700_000_002);
        assert!(decoded.get("nope").is_none());
    }

    #[test]
    fn insert_replaces_and_keeps_sorted() {
        let mut idx = Index::new();
        idx.insert(entry("b", 1));
        idx.insert(entry("a", 2));
        idx.insert(entry("b", 3));
        let paths: Vec<&str> = idx.paths().collect();
        assert_eq!(paths, vec!["a", "b"]);
        assert_eq!(idx.get("b").unwrap().size, 103);
    }

    #[test]
    fn remove_works() {
        let mut idx = Index::new();
        idx.insert(entry("x", 1));
        idx.insert(entry("y", 2));
        idx.remove("x");
        assert!(idx.get("x").is_none());
        assert_eq!(idx.entries.len(), 1);
    }

    #[test]
    fn corrupt_detected() {
        let mut idx = Index::new();
        idx.insert(entry("x", 1));
        let mut bytes = idx.encode();
        let last = bytes.len() - 9;
        bytes[last] ^= 0xff;
        assert!(Index::decode(&bytes).is_err());
        assert!(Index::decode(b"garbage").is_err());
    }

    #[test]
    fn legacy_v1_index_still_readable() {
        // v1（FNV-1a 校验和）的索引文件必须仍能解码——老仓库升级不丢暂存区
        let mut idx = Index::new();
        idx.insert(entry("legacy.txt", 7));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&LEGACY_VERSION.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        let e = idx.get("legacy.txt").unwrap();
        let pb = e.path.as_bytes();
        bytes.extend_from_slice(&(pb.len() as u16).to_le_bytes());
        bytes.extend_from_slice(pb);
        bytes.extend_from_slice(e.oid.as_bytes());
        bytes.extend_from_slice(&e.mode.to_le_bytes());
        bytes.extend_from_slice(&e.size.to_le_bytes());
        bytes.extend_from_slice(&e.mtime_sec.to_le_bytes());
        bytes.extend_from_slice(&e.mtime_nsec.to_le_bytes());
        bytes.extend_from_slice(&e.flags.to_le_bytes());
        bytes.extend_from_slice(&fnv1a(&bytes).to_le_bytes());
        let decoded = Index::decode(&bytes).unwrap();
        assert_eq!(decoded.get("legacy.txt").unwrap().size, 107);
    }

    #[test]
    fn absurd_count_rejected_without_oom() {
        // 伪造一个巨大 count 的小文件：必须在分配前拒绝
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]); // 空 body + 校验和位
        let err = Index::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "unexpected: {err}");
    }

    #[test]
    fn empty_roundtrip() {
        let idx = Index::new();
        let decoded = Index::decode(&idx.encode()).unwrap();
        assert!(decoded.is_empty());
    }
}
