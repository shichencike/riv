//! riv 的对象模型：Blob / Tree / Commit 与二进制序列化格式。
//!
//! 对象格式（完全独立于 git，小端序）：
//! ```text
//! magic:      b"RIVOBJ\x01\x00"        (8 字节)
//! type:       u8                        (1=blob 2=tree 3=commit)
//! payload_len: u64 LE
//! payload:    见各对象
//! ```
//! 对象 ID = blake3(完整序列化字节)，即对整个头部+负载寻址。

use crate::error::{Error, Result};
use crate::oid::{hash_bytes, Oid};
use chrono::{Datelike, TimeZone};

pub const MAGIC: &[u8; 8] = b"RIVOBJ\x01\x00";

pub const MODE_TREE: u32 = 0o040000;
pub const MODE_BLOB: u32 = 0o100644;
pub const MODE_BLOB_EXEC: u32 = 0o100755;
pub const MODE_LINK: u32 = 0o120000;

/// 当前平台语义下两个 mode 是否等价。
///
/// Unix 文件系统带执行位，逐位比较。Windows 等无执行位文件系统上，
/// `file_mode` 永远返回 `MODE_BLOB`——若逐位比较，Linux 端提交的可执行
/// 文件在 Windows 上会被永久判为 modified（幻影变更），跨平台共享仓库
/// 时还会把 mode 翻转写进树。因此这里对齐 git 的 `core.filemode=false`
/// 语义：忽略 exec 位，只比较文件类型（blob / symlink / tree）。
///
/// Termux（Android）的 `$HOME` 位于 ext4/f2fs，执行位与符号链接均可用，
/// 走完整的 Unix 语义。
pub fn mode_equivalent(a: u32, b: u32) -> bool {
    if cfg!(unix) {
        a == b
    } else {
        (a & !0o111) == (b & !0o111)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    Blob = 1,
    Tree = 2,
    Commit = 3,
}

impl ObjectType {
    pub fn from_byte(b: u8) -> Result<Self> {
        match b {
            1 => Ok(ObjectType::Blob),
            2 => Ok(ObjectType::Tree),
            3 => Ok(ObjectType::Commit),
            _ => Err(Error::CorruptObject(format!("unknown type byte {b}"))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ObjectType::Blob => "blob",
            ObjectType::Tree => "tree",
            ObjectType::Commit => "commit",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Object {
    Blob(Vec<u8>),
    Tree(Tree),
    Commit(Commit),
}

impl Object {
    pub fn otype(&self) -> ObjectType {
        match self {
            Object::Blob(_) => ObjectType::Blob,
            Object::Tree(_) => ObjectType::Tree,
            Object::Commit(_) => ObjectType::Commit,
        }
    }

    /// 序列化为完整字节（头部 + 负载）。
    pub fn encode(&self) -> Vec<u8> {
        let payload = match self {
            Object::Blob(d) => d.clone(),
            Object::Tree(t) => t.encode_payload(),
            Object::Commit(c) => c.encode_payload(),
        };
        let mut out = Vec::with_capacity(16 + payload.len());
        out.extend_from_slice(MAGIC);
        out.push(self.otype() as u8);
        out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// 对象的 ID（blake3）。
    pub fn oid(&self) -> Oid {
        hash_bytes(&self.encode())
    }

    /// blob 对象的头部（magic + type + payload_len），供流式哈希复用：
    /// 大文件无需整体载入内存即可计算对象 id。
    pub fn blob_header(payload_len: u64) -> [u8; 17] {
        let mut h = [0u8; 17];
        h[..8].copy_from_slice(MAGIC);
        h[8] = ObjectType::Blob as u8;
        h[9..17].copy_from_slice(&payload_len.to_le_bytes());
        h
    }

    /// 从完整字节解析对象。
    pub fn decode(bytes: &[u8]) -> Result<Object> {
        if bytes.len() < 17 {
            return Err(Error::CorruptObject("too short".into()));
        }
        if &bytes[..8] != MAGIC {
            return Err(Error::CorruptObject("bad magic".into()));
        }
        let otype = ObjectType::from_byte(bytes[8])?;
        let payload_len = u64::from_le_bytes(bytes[9..17].try_into().unwrap()) as usize;
        // checked_add：16 位平台上 17 + payload_len 可能溢出 usize
        let expected = usize::checked_add(17, payload_len).ok_or_else(|| {
            Error::CorruptObject(format!("payload length too large: {payload_len}"))
        })?;
        if bytes.len() != expected {
            return Err(Error::CorruptObject(format!(
                "payload length mismatch: header says {}, buffer has {}",
                payload_len,
                bytes.len() - 17
            )));
        }
        let payload = &bytes[17..];
        match otype {
            ObjectType::Blob => Ok(Object::Blob(payload.to_vec())),
            ObjectType::Tree => Ok(Object::Tree(Tree::decode_payload(payload)?)),
            ObjectType::Commit => Ok(Object::Commit(Commit::decode_payload(payload)?)),
        }
    }

    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            Object::Blob(d) => Some(d),
            _ => None,
        }
    }

    pub fn as_tree(&self) -> Option<&Tree> {
        match self {
            Object::Tree(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_commit(&self) -> Option<&Commit> {
        match self {
            Object::Commit(c) => Some(c),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Tree {
    /// 按名称字节序排序
    pub entries: Vec<TreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: u32,
    pub name: String,
    pub oid: Oid,
}

impl Tree {
    pub fn new(mut entries: Vec<TreeEntry>) -> Self {
        entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        Tree { entries }
    }

    pub fn get(&self, name: &str) -> Option<&TreeEntry> {
        self.entries
            .binary_search_by(|e| e.name.as_bytes().cmp(name.as_bytes()))
            .ok()
            .map(|i| &self.entries[i])
    }

    pub fn is_dir(&self, name: &str) -> bool {
        self.get(name).map(|e| e.mode == MODE_TREE).unwrap_or(false)
    }

    fn encode_payload(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in &self.entries {
            out.extend_from_slice(&e.mode.to_le_bytes());
            out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            out.extend_from_slice(e.name.as_bytes());
            out.extend_from_slice(e.oid.as_bytes());
        }
        out
    }

    fn decode_payload(p: &[u8]) -> Result<Tree> {
        if p.len() < 4 {
            return Err(Error::CorruptObject("tree payload too short".into()));
        }
        let count = u32::from_le_bytes(p[..4].try_into().unwrap()) as usize;
        let mut off = 4;
        // count 来自不可信字节：每个条目至少 38 字节（6 头 + 32 oid），
        // 按 payload 实际大小钳制预分配，防止损坏数据触发巨额分配。
        let mut entries = Vec::with_capacity(count.min(p.len() / 38 + 1));
        for _ in 0..count {
            if off + 6 > p.len() {
                return Err(Error::CorruptObject("tree entry truncated".into()));
            }
            let mode = u32::from_le_bytes(p[off..off + 4].try_into().unwrap());
            let name_len = u16::from_le_bytes(p[off + 4..off + 6].try_into().unwrap()) as usize;
            off += 6;
            if off + name_len + 32 > p.len() {
                return Err(Error::CorruptObject("tree entry name truncated".into()));
            }
            let name = String::from_utf8(p[off..off + name_len].to_vec())
                .map_err(|_| Error::CorruptObject("tree entry name not utf-8".into()))?;
            off += name_len;
            let oid = Oid::from_bytes(p[off..off + 32].try_into().unwrap());
            off += 32;
            entries.push(TreeEntry { mode, name, oid });
        }
        Ok(Tree { entries })
    }
}

// ---------------------------------------------------------------------------
// Commit / Signature
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Commit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    /// unix 秒
    pub seconds: i64,
    /// 时区偏移（分钟），如 +0800 → 480
    pub tz_offset_minutes: i32,
}

impl Signature {
    pub fn now(name: &str, email: &str) -> Self {
        let seconds = chrono::Local::now().timestamp();
        let tz = chrono::Local::now().offset().local_minus_utc() / 60;
        Signature {
            name: name.to_string(),
            email: email.to_string(),
            seconds,
            tz_offset_minutes: tz,
        }
    }

    /// `name <email> 1699999999 +0800`
    fn encode(&self) -> String {
        let sign = if self.tz_offset_minutes < 0 { '-' } else { '+' };
        let abs = self.tz_offset_minutes.abs();
        format!(
            "{} <{}> {} {}{:02}{:02}",
            self.name,
            self.email,
            self.seconds,
            sign,
            abs / 60,
            abs % 60
        )
    }

    fn decode(s: &str) -> Result<Signature> {
        // name <email> seconds tz
        let lt = s
            .rfind('<')
            .ok_or_else(|| Error::CorruptObject("signature missing '<'".into()))?;
        let gt = s
            .rfind('>')
            .ok_or_else(|| Error::CorruptObject("signature missing '>'".into()))?;
        if gt < lt || gt + 1 > s.len() {
            return Err(Error::CorruptObject("signature malformed".into()));
        }
        let name = s[..lt].trim_end().to_string();
        let email = s[lt + 1..gt].to_string();
        let rest = s[gt + 1..].trim();
        let mut parts = rest.split_whitespace();
        let seconds: i64 = parts
            .next()
            .ok_or_else(|| Error::CorruptObject("signature missing timestamp".into()))?
            .parse()
            .map_err(|_| Error::CorruptObject("signature bad timestamp".into()))?;
        let tz_str = parts
            .next()
            .ok_or_else(|| Error::CorruptObject("signature missing tz".into()))?;
        let tz_offset_minutes = parse_tz(tz_str)?;
        Ok(Signature {
            name,
            email,
            seconds,
            tz_offset_minutes,
        })
    }

    /// 格式化为展示用字符串，与 git `Date:` 行逐字节同款：
    /// `Sun Jan 5 09:03:07 2020 +0800`——星期/月份为英文三字母缩写，
    /// 日不补零（单空格分隔），时区偏移 `+HHMM`。
    pub fn format_time(&self) -> String {
        let offset_secs = self.tz_offset_minutes * 60;
        match chrono::FixedOffset::east_opt(offset_secs) {
            Some(off) => match off.timestamp_opt(self.seconds, 0).single() {
                Some(dt) => format!(
                    "{} {} {} {:02}:{:02}:{:02} {} {}",
                    dt.format("%a"),
                    dt.format("%b"),
                    dt.day(),
                    dt.format("%H"),
                    dt.format("%M"),
                    dt.format("%S"),
                    dt.format("%Y"),
                    dt.format("%z"),
                ),
                None => self.seconds.to_string(),
            },
            None => self.seconds.to_string(),
        }
    }
}

fn parse_tz(s: &str) -> Result<i32> {
    let s = s.trim();
    if s.len() != 5 {
        return Err(Error::CorruptObject(format!("bad tz: {s}")));
    }
    let sign = match &s[..1] {
        "+" => 1,
        "-" => -1,
        _ => return Err(Error::CorruptObject(format!("bad tz: {s}"))),
    };
    let h: i32 = s[1..3]
        .parse()
        .map_err(|_| Error::CorruptObject(format!("bad tz: {s}")))?;
    let m: i32 = s[3..5]
        .parse()
        .map_err(|_| Error::CorruptObject(format!("bad tz: {s}")))?;
    Ok(sign * (h * 60 + m))
}

impl Commit {
    /// 首行摘要。
    pub fn summary(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }

    fn encode_payload(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str(&format!("tree {}\n", self.tree));
        for p in &self.parents {
            out.push_str(&format!("parent {}\n", p));
        }
        out.push_str(&format!("author {}\n", self.author.encode()));
        out.push_str(&format!("committer {}\n", self.committer.encode()));
        out.push('\n');
        out.push_str(&self.message);
        out.into_bytes()
    }

    fn decode_payload(p: &[u8]) -> Result<Commit> {
        let text = String::from_utf8(p.to_vec())
            .map_err(|_| Error::CorruptObject("commit not utf-8".into()))?;
        let (header, message) = match text.find("\n\n") {
            Some(i) => (&text[..i], text[i + 2..].to_string()),
            None => (text.as_str(), String::new()),
        };
        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = None;
        let mut committer = None;
        for line in header.lines() {
            if let Some(rest) = line.strip_prefix("tree ") {
                tree = Some(
                    rest.trim()
                        .parse()
                        .map_err(|_| Error::CorruptObject("commit bad tree".into()))?,
                );
            } else if let Some(rest) = line.strip_prefix("parent ") {
                parents.push(
                    rest.trim()
                        .parse()
                        .map_err(|_| Error::CorruptObject("commit bad parent".into()))?,
                );
            } else if let Some(rest) = line.strip_prefix("author ") {
                author = Some(Signature::decode(rest)?);
            } else if let Some(rest) = line.strip_prefix("committer ") {
                committer = Some(Signature::decode(rest)?);
            }
        }
        Ok(Commit {
            tree: tree.ok_or_else(|| Error::CorruptObject("commit missing tree".into()))?,
            parents,
            author: author.ok_or_else(|| Error::CorruptObject("commit missing author".into()))?,
            committer: committer
                .ok_or_else(|| Error::CorruptObject("commit missing committer".into()))?,
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_equivalent_platform_semantics() {
        // 所有平台：文件类型位不同 → 不等价
        assert!(mode_equivalent(MODE_BLOB, MODE_BLOB));
        assert!(mode_equivalent(MODE_LINK, MODE_LINK));
        assert!(!mode_equivalent(MODE_BLOB, MODE_LINK));
        assert!(!mode_equivalent(MODE_LINK, MODE_TREE));
        if cfg!(unix) {
            // Unix：exec 位是真实差异（Termux 的 $HOME 在 ext4 上同理）
            assert!(!mode_equivalent(MODE_BLOB, MODE_BLOB_EXEC));
        } else {
            // Windows 无执行位：忽略 exec 位（对齐 git core.filemode=false），
            // 避免 Linux 端提交的可执行文件被永久判为 modified。
            assert!(mode_equivalent(MODE_BLOB, MODE_BLOB_EXEC));
            assert!(mode_equivalent(MODE_BLOB_EXEC, MODE_BLOB));
        }
    }

    #[test]
    fn blob_roundtrip() {
        let obj = Object::Blob(b"hello riv\n".to_vec());
        let bytes = obj.encode();
        let decoded = Object::decode(&bytes).unwrap();
        assert_eq!(decoded.as_blob(), Some(&b"hello riv\n"[..]));
        assert_eq!(decoded.oid(), obj.oid());
        assert_eq!(decoded.otype(), ObjectType::Blob);
    }

    #[test]
    fn tree_roundtrip_and_sorting() {
        let t = Tree::new(vec![
            TreeEntry {
                mode: MODE_BLOB,
                name: "b.txt".into(),
                oid: Oid::ZERO,
            },
            TreeEntry {
                mode: MODE_BLOB,
                name: "a.txt".into(),
                oid: Oid::ZERO,
            },
            TreeEntry {
                mode: MODE_TREE,
                name: "src".into(),
                oid: Oid::ZERO,
            },
        ]);
        let names: Vec<&str> = t.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "src"]);
        let obj = Object::Tree(t);
        let decoded = Object::decode(&obj.encode()).unwrap();
        assert_eq!(decoded.as_tree().unwrap().entries.len(), 3);
        assert!(decoded.as_tree().unwrap().get("src").is_some());
        assert!(decoded.as_tree().unwrap().is_dir("src"));
    }

    #[test]
    fn commit_roundtrip() {
        let sig = Signature {
            name: "Riv Dev".into(),
            email: "dev@riv.local".into(),
            seconds: 1_700_000_000,
            tz_offset_minutes: 480,
        };
        let c = Commit {
            tree: Oid::ZERO,
            parents: vec![Oid::ZERO],
            author: sig.clone(),
            committer: sig,
            message: "first commit\n\nbody line\n".into(),
        };
        let obj = Object::Commit(c);
        let decoded = Object::decode(&obj.encode()).unwrap();
        let c2 = decoded.as_commit().unwrap();
        assert_eq!(c2.tree, Oid::ZERO);
        assert_eq!(c2.parents.len(), 1);
        assert_eq!(c2.author.name, "Riv Dev");
        assert_eq!(c2.author.tz_offset_minutes, 480);
        assert_eq!(c2.summary(), "first commit");
        assert_eq!(c2.message, "first commit\n\nbody line\n");
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(Object::decode(b"not an object").is_err());
        let mut bytes = Object::Blob(vec![1, 2, 3]).encode();
        bytes[9] = 99; // 破坏长度字段
        assert!(Object::decode(&bytes).is_err());
    }

    #[test]
    fn signature_time_format() {
        let sig = Signature {
            name: "n".into(),
            email: "e".into(),
            seconds: 1_700_000_000,
            tz_offset_minutes: -300,
        };
        // 1_700_000_000 = 2023-11-14 22:13:20 UTC，-0500 时区本地时间为 17:13:20
        assert_eq!(sig.format_time(), "Tue Nov 14 17:13:20 2023 -0500");
    }

    #[test]
    fn signature_time_format_single_digit_day() {
        // 对齐 git 对个位数日期的处理：日不补零、单空格分隔（`Jan 5` 而非 `Jan  5`）
        let sig = Signature {
            name: "n".into(),
            email: "e".into(),
            seconds: 1_578_186_187, // 2020-01-05 01:03:07 UTC，+0800 本地时间为 09:03:07
            tz_offset_minutes: 480,
        };
        assert_eq!(sig.format_time(), "Sun Jan 5 09:03:07 2020 +0800");
    }
}
