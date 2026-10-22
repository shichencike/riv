//! 内容寻址对象存储：`.riv/objects/`。
//!
//! - 寻址：blake3(object bytes) → Oid
//! - 布局：`objects/<前2位hex>/<剩余62位hex>`（fan-out，避免单目录文件过多）
//! - 压缩：zstd（默认级别 3，可用 core.compression 调整）
//! - 原子写：先写 `objects/tmp/` 再 rename，保证不会出现半个对象

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::object::Object;
use crate::oid::Oid;

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

    /// 读取对象。
    pub fn read(&self, oid: &Oid) -> Result<Object> {
        let path = self.path_for(oid);
        let compressed = fs::read(&path).map_err(|_| Error::ObjectNotFound(oid.to_hex()))?;
        let bytes = zstd::decode_all(&compressed[..])
            .map_err(|e| Error::CorruptObject(format!("{}: {e}", oid.short(7))))?;
        Object::decode(&bytes)
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
        let mut file = fs::File::open(path)?;
        let size = file.metadata()?.len();
        let mut hasher = blake3::Hasher::new();
        hasher.update(&crate::object::Object::blob_header(size));
        let mut encoder = zstd::Encoder::new(Vec::new(), self.level)
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
    fn all_oids_lists_objects() {
        let (_d, store) = test_store();
        let o1 = store.write(&Object::Blob(b"a".to_vec())).unwrap();
        let o2 = store.write(&Object::Blob(b"b".to_vec())).unwrap();
        let all = store.all_oids().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&o1) && all.contains(&o2));
    }
}
