//! 内容寻址对象存储：`.riv/objects/`。
//!
//! - 寻址：blake3(object bytes) → Oid
//! - 布局：`objects/<前2位hex>/<剩余62位hex>`（fan-out，避免单目录文件过多）
//! - 压缩：zstd（默认级别 3，可用 core.compression 调整）
//! - 原子写：先写 `objects/tmp/` 再 rename，保证不会出现半个对象
//! - 读校验：解压后重新序列化哈希并与寻址 oid 比对（对齐 git 对松散对象的校验），
//!   损坏或被篡改的对象在读取时即报错，不会把坏数据带进上层

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::object::Object;
use crate::oid::Oid;

/// 对象解压大小上限（4 GiB）：防止损坏/恶意构造的 zstd 流（压缩炸弹）
/// 在解码时耗尽内存。正常仓库远低于此限。
pub const MAX_DECOMPRESSED_BYTES: u64 = 4 << 30;

pub struct ObjectStore {
    dir: PathBuf,
    level: i32,
    tmp_counter: AtomicU64,
}

impl ObjectStore {
    pub fn new(riv_dir: &Path, level: i32) -> Self {
        ObjectStore {
            dir: riv_dir.join("objects"),
            level,
            tmp_counter: AtomicU64::new(0),
        }
    }

    fn path_for(&self, oid: &Oid) -> PathBuf {
        let hex = oid.to_hex();
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    pub fn contains(&self, oid: &Oid) -> bool {
        self.path_for(oid).is_file()
    }

    /// 写入对象（已存在则跳过），返回其 Oid。
    pub fn write(&self, object: &Object) -> Result<Oid> {
        let oid = object.oid();
        let path = self.path_for(&oid);
        if path.is_file() {
            return Ok(oid);
        }
        let bytes = object.encode();
        let compressed = zstd::encode_all(&bytes[..], self.level)
            .map_err(|e| Error::Msg(format!("zstd compress failed: {e}")))?;
        fs::create_dir_all(self.dir.join("tmp"))?;
        let tmp = self.dir.join("tmp").join(format!(
            "tmp-{}-{}",
            std::process::id(),
            self.tmp_counter.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&tmp, &compressed)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // rename 在同一文件系统上是原子的；并发写相同 oid 无害（内容一致）
        fs::rename(&tmp, &path)?;
        Ok(oid)
    }

    /// 读取对象（校验内容哈希与寻址一致）。
    pub fn read(&self, oid: &Oid) -> Result<Object> {
        let path = self.path_for(oid);
        let compressed = fs::read(&path).map_err(|_| Error::ObjectNotFound(oid.to_hex()))?;
        let bytes = zstd_decode_capped(&compressed, MAX_DECOMPRESSED_BYTES)
            .map_err(|e| Error::CorruptObject(format!("{}: {e}", oid.short(7))))?;
        let object = Object::decode(&bytes)?;
        // 内容寻址的闭环校验：解压出的字节重新哈希必须等于其文件名（oid）。
        // 位翻转、磁盘坏道、对象被误写到位都会在这里暴露。
        if object.oid() != *oid {
            return Err(Error::CorruptObject(format!(
                "{}: content hash mismatch",
                oid.short(7)
            )));
        }
        Ok(object)
    }

    /// 仅读取 blob 内容。
    pub fn read_blob(&self, oid: &Oid) -> Result<Vec<u8>> {
        match self.read(oid)? {
            Object::Blob(d) => Ok(d),
            other => Err(Error::CorruptObject(format!(
                "{} is a {}, not a blob",
                oid.short(7),
                other.otype().as_str()
            ))),
        }
    }

    /// 把工作区文件流式写入为 blob 对象（内存有界，适合大文件）。
    /// 返回 (oid, size)。对象已存在时仍会读一遍文件以得到 size 与 oid。
    pub fn write_blob_file(&self, path: &Path) -> Result<(Oid, u64)> {
        let file = fs::File::open(path)?;
        let size = file.metadata()?.len();
        self.write_blob_file_sized(path, size)
    }

    /// `write_blob_file` 的复用版：调用方已持有文件大小（避免重复 stat）。
    ///
    /// 流式写入**完整对象字节**（头部 + 内容）：头部漏进压缩流会让对象
    /// 在 read 时直接 bad magic——大文件路径曾因漏写头部而整体损坏，
    /// 现由 `read` 的哈希校验兜底暴露，此处从源头修掉。
    pub fn write_blob_file_sized(&self, path: &Path, size: u64) -> Result<(Oid, u64)> {
        let mut file = fs::File::open(path)?;
        let header = crate::object::Object::blob_header(size);
        let mut hasher = blake3::Hasher::new();
        hasher.update(&header);
        let mut encoder = zstd::Encoder::new(Vec::new(), self.level)
            .map_err(|e| Error::Msg(format!("zstd compress failed: {e}")))?;
        encoder
            .write_all(&header)
            .map_err(|e| Error::Msg(format!("zstd compress failed: {e}")))?;
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| Error::Msg(format!("read {}: {e}", path.display())))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder
                .write_all(&buf[..n])
                .map_err(|e| Error::Msg(format!("zstd compress failed: {e}")))?;
        }
        let compressed = encoder
            .finish()
            .map_err(|e| Error::Msg(format!("zstd compress failed: {e}")))?;
        let oid = Oid::from_bytes(*hasher.finalize().as_bytes());
        let dest = self.path_for(&oid);
        if !dest.is_file() {
            self.atomic_write_compressed(&dest, &compressed)?;
        }
        Ok((oid, size))
    }

    /// 原子写入已压缩的对象字节。
    fn atomic_write_compressed(&self, dest: &Path, compressed: &[u8]) -> Result<()> {
        fs::create_dir_all(self.dir.join("tmp"))?;
        let tmp = self.dir.join("tmp").join(format!(
            "tmp-{}-{}",
            std::process::id(),
            self.tmp_counter.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&tmp, compressed)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&tmp, dest)?;
        Ok(())
    }

    /// 遍历所有对象 Oid（用于前缀解析等）。
    pub fn all_oids(&self) -> Result<Vec<Oid>> {
        let mut out = Vec::new();
        if !self.dir.is_dir() {
            return Ok(out);
        }
        for fan in fs::read_dir(&self.dir)? {
            let fan = fan?;
            let fan_name = fan.file_name().to_string_lossy().to_string();
            if fan_name.len() != 2 || !fan_name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            if !fan.path().is_dir() {
                continue;
            }
            for entry in fs::read_dir(fan.path())? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                let full = format!("{fan_name}{name}");
                if full.len() == 64 {
                    if let Ok(oid) = full.parse::<Oid>() {
                        out.push(oid);
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

/// 带上限的 zstd 解压：流式读取，超过 `limit` 即报错，
/// 避免损坏或恶意构造的对象文件（压缩炸弹）耗尽内存。
fn zstd_decode_capped(compressed: &[u8], limit: u64) -> std::result::Result<Vec<u8>, String> {
    let mut decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(|e| format!("zstd init failed: {e}"))?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = decoder
            .read(&mut buf)
            .map_err(|e| format!("zstd decode failed: {e}"))?;
        if n == 0 {
            break;
        }
        if out.len() as u64 + n as u64 > limit {
            return Err(format!("decompressed object exceeds {limit} byte limit"));
        }
        out.extend_from_slice(&buf[..n]);
    }
    drop(decoder);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Commit, Object, Signature, Tree, TreeEntry, MODE_BLOB};

    fn test_store() -> (tempfile::TempDir, ObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path(), 3);
        (dir, store)
    }

    #[test]
    fn blob_write_read_roundtrip() {
        let (_d, store) = test_store();
        let data = b"hello riv\n".repeat(100);
        let oid = store.write(&Object::Blob(data.clone())).unwrap();
        assert!(store.contains(&oid));
        assert_eq!(store.read_blob(&oid).unwrap(), data);
        // 重复写返回相同 oid
        let oid2 = store.write(&Object::Blob(data)).unwrap();
        assert_eq!(oid, oid2);
    }

    #[test]
    fn tree_and_commit_roundtrip() {
        let (_d, store) = test_store();
        let blob = store.write(&Object::Blob(b"content\n".to_vec())).unwrap();
        let tree = Tree::new(vec![TreeEntry {
            mode: MODE_BLOB,
            name: "file.txt".into(),
            oid: blob,
        }]);
        let tree_oid = store.write(&Object::Tree(tree)).unwrap();
        let sig = Signature {
            name: "t".into(),
            email: "t@t".into(),
            seconds: 1700000000,
            tz_offset_minutes: 0,
        };
        let commit = Commit {
            tree: tree_oid,
            parents: vec![],
            author: sig.clone(),
            committer: sig,
            message: "test\n".into(),
        };
        let commit_oid = store.write(&Object::Commit(commit)).unwrap();
        let read_back = store.read(&commit_oid).unwrap();
        let c = read_back.as_commit().unwrap();
        assert_eq!(c.tree, tree_oid);
        assert_eq!(c.summary(), "test");
        let t = store.read(&tree_oid).unwrap();
        assert_eq!(t.as_tree().unwrap().get("file.txt").unwrap().oid, blob);
    }

    #[test]
    fn read_missing_object() {
        let (_d, store) = test_store();
        assert!(store.read(&Oid::ZERO).is_err());
    }

    #[test]
    fn read_detects_content_hash_mismatch() {
        // 把对象 A 的字节写到对象 B 的路径下：解压能过、格式能过，
        // 但内容哈希与文件名不符，read 必须报错（位翻转/坏盘的防线）。
        let (dir, store) = test_store();
        let victim = store.write(&Object::Blob(b"victim".to_vec())).unwrap();
        let other = store
            .write(&Object::Blob(b"other content".to_vec()))
            .unwrap();
        let hex = other.to_hex();
        let wrong_path = dir.path().join("objects").join(&hex[..2]).join(&hex[2..]);
        let good_path = {
            let h = victim.to_hex();
            dir.path().join("objects").join(&h[..2]).join(&h[2..])
        };
        fs::copy(&good_path, &wrong_path).unwrap();
        let err = store.read(&other).unwrap_err();
        assert!(
            err.to_string().contains("hash mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn write_blob_file_sized_matches_stat_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path(), 3);
        let file = dir.path().join("big.bin");
        let data: Vec<u8> = (0..(1 << 20)).map(|i| (i % 251) as u8).collect();
        fs::write(&file, &data).unwrap();
        let (oid_stat, size_stat) = store.write_blob_file(&file).unwrap();
        // 同一文件再用 sized 接口写一遍：oid 必须一致（流式与带大小路径同结果）
        let (oid_sized, size_sized) = store
            .write_blob_file_sized(&file, fs::metadata(&file).unwrap().len())
            .unwrap();
        assert_eq!(oid_stat, oid_sized);
        assert_eq!(size_stat, size_sized);
        assert_eq!(store.read_blob(&oid_stat).unwrap(), data);
    }

    #[test]
    fn zstd_decode_capped_rejects_bomb() {
        // 构造一个声称解压后极大的 zstd 流：1MB 的零数据压缩后极小，
        // 超过上限时必须报错而不是吃光内存。
        let zeros = vec![0u8; 8 << 20];
        let compressed = zstd::encode_all(&zeros[..], 3).unwrap();
        assert!(compressed.len() < zeros.len() / 100);
        // 上限设为比解压结果小 → 必须报错
        let err = zstd_decode_capped(&compressed, 1 << 20).unwrap_err();
        assert!(err.contains("exceeds"), "unexpected error: {err}");
    }

    #[test]
    fn all_oids_lists_objects() {
        let (_d, store) = test_store();
        let o1 = store.write(&Object::Blob(b"a".to_vec())).unwrap();
        let o2 = store.write(&Object::Blob(b"b".to_vec())).unwrap();
        let all = store.all_oids().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&o1) && all.contains(&o2));
    }
}
