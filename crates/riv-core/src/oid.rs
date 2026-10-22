//! Oid：riv 的内容寻址对象标识（blake3，32 字节）。

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// 对象 ID：blake3 哈希值（32 字节）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid([u8; 32]);

impl Oid {
    pub const ZERO: Oid = Oid([0u8; 32]);

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Oid(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// 完整 64 位十六进制字符串。
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0xf) as usize] as char);
        }
        s
    }

    /// 短哈希（默认 7 位，与 git 习惯一致）。
    pub fn short(&self, len: usize) -> String {
        let full = self.to_hex();
        full.chars().take(len).collect()
    }

    /// 是否是某个十六进制前缀的前缀。
    pub fn starts_with_hex(&self, prefix: &str) -> bool {
        if prefix.is_empty() {
            return true;
        }
        let prefix = prefix.to_ascii_lowercase();
        let full = self.to_hex();
        full.starts_with(&prefix)
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.short(7))
    }
}

impl FromStr for Oid {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.len() != 64 {
            return Err(Error::InvalidOid(format!(
                "expected 64 hex chars, got {}",
                s.len()
            )));
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = hex_val(chunk[0])?;
            let lo = hex_val(chunk[1])?;
            bytes[i] = (hi << 4) | lo;
        }
        Ok(Oid(bytes))
    }
}

fn hex_val(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(Error::InvalidOid(format!(
            "invalid hex char: {}",
            c as char
        ))),
    }
}

/// 对完整序列化后的对象字节计算 blake3。
pub fn hash_bytes(data: &[u8]) -> Oid {
    let h = blake3::hash(data);
    Oid(*h.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_hex() {
        let oid = hash_bytes(b"hello riv");
        let s = oid.to_string();
        assert_eq!(s.len(), 64);
        let parsed: Oid = s.parse().unwrap();
        assert_eq!(parsed, oid);
    }

    #[test]
    fn short_and_prefix() {
        let oid = hash_bytes(b"test");
        let short = oid.short(7);
        assert_eq!(short.len(), 7);
        assert!(oid.starts_with_hex(&short));
        assert!(!oid.starts_with_hex("zzzz"));
    }

    #[test]
    fn invalid_oid() {
        assert!("abc".parse::<Oid>().is_err());
        assert!(("g".repeat(64)).parse::<Oid>().is_err());
    }
}
