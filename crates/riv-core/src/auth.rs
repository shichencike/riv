//! Token 凭证管理——riv 的「token 保险柜」。
//!
//! riv 连接远程时需要身份凭证。本模块按优先级从高到低读取：
//!
//! 1. **环境变量 `RIV_TOKEN`**：CI/CD 与临时覆盖首选，不落盘、用即焚；
//! 2. **系统钥匙串**：Windows 凭据管理器 / macOS 钥匙串 / Linux Secret Service；
//! 3. **本地文件** `<config_dir>/riv/credentials`（Unix 权限 600）：
//!    Termux 等无 DBus 服务、或没有钥匙串的极简 Linux 环境。
//!
//! 保存时优先写系统钥匙串；钥匙串不可用（如 Termux 缺 DBus）时自动降级到
//! 本地文件，并在返回中报告实际落点。
//!
//! 默认主机为 GitHub 预设（`https://github.com`），可用环境变量
//! `RIV_BASE_URL` 或 CLI 的 `--host` 覆盖。内部一律按「主机名」寻址
//! （如 `github.com`、`git.example.com:8443`），因此同一套机制将来到任意
//! riv 远程都可以复用，不必绑死在 GitHub 上。
//!
//! 凭证文件格式（每行一条，`#` 起始为注释，第一个 `=` 分隔）：
//!
//! ```text
//! # riv credentials: host=token
//! github.com=ghp_xxxx
//! git.example.com:8443=token
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// 默认远程预设：GitHub。
pub const DEFAULT_BASE_URL: &str = "https://github.com";
/// 环境变量：Token 本体（最高优先级，CI 用）。
pub const ENV_TOKEN: &str = "RIV_TOKEN";
/// 环境变量：默认 base_url（覆盖 GitHub 预设）。
pub const ENV_BASE_URL: &str = "RIV_BASE_URL";
/// 钥匙串中的服务名。
const KEYRING_SERVICE: &str = "riv";

/// Token 来源（按优先级从高到低）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// 环境变量 `RIV_TOKEN`。
    Env,
    /// 系统钥匙串。
    Keyring,
    /// 本地凭证文件。
    File,
}

impl TokenSource {
    /// 展示用名称（与 CLI 输出一致）。
    pub fn label(&self) -> &'static str {
        match self {
            TokenSource::Env => "environment variable RIV_TOKEN",
            TokenSource::Keyring => "system keyring",
            TokenSource::File => "credentials file",
        }
    }
}

/// 一条凭证：主机 + token + 来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    /// 归一化后的主机名（小写、无 scheme、可带端口）。
    pub host: String,
    pub token: String,
    pub source: TokenSource,
}

/// 某个来源的查询结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceState {
    /// 有 token。
    Present(String),
    /// 查过，确实没有。
    Absent,
    /// 来源本身不可用（如无 DBus 的 Termux 钥匙串）。
    Unavailable,
}

/// `riv auth status` 的完整状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStatus {
    pub host: String,
    pub env: SourceState,
    pub keyring: SourceState,
    pub file: SourceState,
}

impl AuthStatus {
    /// 当前生效的（来源, token）：优先级最高且有值的那一个。
    pub fn effective(&self) -> Option<(TokenSource, &str)> {
        for (src, state) in [
            (TokenSource::Env, &self.env),
            (TokenSource::Keyring, &self.keyring),
            (TokenSource::File, &self.file),
        ] {
            if let SourceState::Present(t) = state {
                return Some((src, t.as_str()));
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// 主机名归一化
// ---------------------------------------------------------------------------

/// 把 base_url 或裸主机归一化为「主机名」：去 scheme、去路径、小写、保留端口。
///
/// - `https://github.com` → `github.com`
/// - `github.com/` → `github.com`
/// - `HTTPS://GitHub.com/riv/riv` → `github.com`
/// - `git.example.com:8443` → `git.example.com:8443`
pub fn normalize_host(input: &str) -> String {
    // 先整体转小写：scheme 匹配必须大小写不敏感（HTTPS:// 与 https:// 等价）
    let s = input.trim().to_ascii_lowercase();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(&s);
    s.split('/').next().unwrap_or(s).to_string()
}

/// 默认主机：`RIV_BASE_URL` > GitHub 预设。
pub fn default_host() -> String {
    match std::env::var(ENV_BASE_URL) {
        Ok(v) if !v.trim().is_empty() => normalize_host(&v),
        _ => normalize_host(DEFAULT_BASE_URL),
    }
}

/// 凭证文件路径：`<config_dir>/riv/credentials`。
pub fn credentials_path() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| Error::msg("cannot locate config directory (is $HOME/APPDATA set?)"))?;
    Ok(dir.join("riv").join("credentials"))
}

// ---------------------------------------------------------------------------
// 公开 API（带显式凭证文件路径，便于测试与调用方控制）
// ---------------------------------------------------------------------------

/// 读取 token：环境变量 > 系统钥匙串 > 凭证文件。
pub fn get_token_with(host: &str, cred_path: &Path) -> Result<Option<Credential>> {
    let host = normalize_host(host);
    if let Some(token) = token_from_env() {
        return Ok(Some(Credential {
            host,
            token,
            source: TokenSource::Env,
        }));
    }
    // 钥匙串不可用时静默降级（Termux 无 DBus 是常态，不该每次报警）。
    // 注意：错误必须在这里吞掉并继续下一个来源，不能向上传播。
    if let Ok(Some(token)) = keyring_get(&host) {
        return Ok(Some(Credential {
            host,
            token,
            source: TokenSource::Keyring,
        }));
    }
    if let Some(token) = file_get(cred_path, &host)? {
        return Ok(Some(Credential {
            host,
            token,
            source: TokenSource::File,
        }));
    }
    Ok(None)
}

/// 保存 token：优先系统钥匙串，不可用时降级到凭证文件（Unix 600）。
/// 返回实际落点；同时清理另一处的旧条目，避免过期凭证被优先读到。
pub fn set_token_with(host: &str, token: &str, cred_path: &Path) -> Result<TokenSource> {
    let host = normalize_host(host);
    let token = token.trim();
    if token.is_empty() {
        return Err(Error::msg("token must not be empty"));
    }
    match keyring_set(&host, token) {
        Ok(()) => {
            let _ = file_delete(cred_path, &host); // 清理可能存在的旧文件条目
            Ok(TokenSource::Keyring)
        }
        Err(_) => {
            file_set(cred_path, &host, token)?;
            let _ = keyring_delete(&host); // 清理可能存在的旧钥匙串条目
            Ok(TokenSource::File)
        }
    }
}

/// 删除 token：钥匙串 + 凭证文件（环境变量属于父 shell，riv 无权清除）。
/// 返回**确实删除了**的位置——无条目的来源不计入（NoEntry 不是删除成功）。
pub fn delete_token_with(host: &str, cred_path: &Path) -> Result<Vec<TokenSource>> {
    let host = normalize_host(host);
    let mut cleared = Vec::new();
    // 钥匙串不可用时静默跳过（riv 无权访问）；只有确实删除了才计入
    if matches!(keyring_delete(&host), Ok(true)) {
        cleared.push(TokenSource::Keyring);
    }
    if file_delete(cred_path, &host)? {
        cleared.push(TokenSource::File);
    }
    Ok(cleared)
}

/// 三来源状态快照（供 `riv auth status` 展示）。
pub fn status_with(host: &str, cred_path: &Path) -> Result<AuthStatus> {
    let host = normalize_host(host);
    let env = match token_from_env() {
        Some(t) => SourceState::Present(t),
        None => SourceState::Absent,
    };
    let keyring = match keyring_get(&host) {
        Ok(Some(t)) => SourceState::Present(t),
        Ok(None) => SourceState::Absent,
        Err(_) => SourceState::Unavailable,
    };
    let file = match file_get(cred_path, &host)? {
        Some(t) => SourceState::Present(t),
        None => SourceState::Absent,
    };
    Ok(AuthStatus {
        host,
        env,
        keyring,
        file,
    })
}

/// 脱敏：保留前 4 后 4 个字符，中间打码；过短则全码。
pub fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len().max(3));
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}

// ---------------------------------------------------------------------------
// 环境变量
// ---------------------------------------------------------------------------

/// 读取环境变量 `RIV_TOKEN`（空值视为未设置）。
pub fn token_from_env() -> Option<String> {
    match std::env::var(ENV_TOKEN) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 系统钥匙串（任何错误都视为「不可用」，由调用方降级）
// ---------------------------------------------------------------------------

fn keyring_entry(host: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, host)
        .map_err(|e| Error::msg(format!("keyring unavailable: {e}")))
}

fn keyring_get(host: &str) -> Result<Option<String>> {
    let entry = keyring_entry(host)?;
    match entry.get_password() {
        Ok(t) if !t.trim().is_empty() => Ok(Some(t)),
        Ok(_) => Ok(None),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(Error::msg(format!("keyring unavailable: {e}"))),
    }
}

fn keyring_set(host: &str, token: &str) -> Result<()> {
    let entry = keyring_entry(host)?;
    entry
        .set_password(token)
        .map_err(|e| Error::msg(format!("keyring write failed: {e}")))
}

/// 删除钥匙串条目；返回是否确有删除（无条目 = false，不可用 = Err）。
fn keyring_delete(host: &str) -> Result<bool> {
    let entry = keyring_entry(host)?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(Error::msg(format!("keyring delete failed: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// 本地凭证文件：host=token 行格式，tmp+rename 原子写，Unix 600
// ---------------------------------------------------------------------------

const FILE_HEADER: &str = "# riv credentials: host=token (one entry per line)";

/// 读取并解析凭证文件。文件不存在 = 空表。
fn file_read(path: &Path) -> Result<BTreeMap<String, String>> {
    let body = match fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    let mut map = BTreeMap::new();
    for (i, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((host, token)) = line.split_once('=') else {
            return Err(Error::msg(format!(
                "malformed credentials file: {} line {} (expected host=token)",
                path.display(),
                i + 1
            )));
        };
        let host = normalize_host(host);
        let token = token.trim();
        if host.is_empty() || token.is_empty() {
            return Err(Error::msg(format!(
                "malformed credentials file: {} line {} (empty host or token)",
                path.display(),
                i + 1
            )));
        }
        map.insert(host, token.to_string()); // 重复主机：后者覆盖
    }
    Ok(map)
}

/// 原子写入凭证文件（tmp + rename），Unix 上设置 600 权限。
fn file_write(path: &Path, map: &BTreeMap<String, String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    body.push_str(FILE_HEADER);
    body.push('\n');
    for (host, token) in map {
        body.push_str(host);
        body.push('=');
        body.push_str(token);
        body.push('\n');
    }
    let tmp = path.with_file_name(format!(
        ".{}.tmp.{}",
        path.file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "credentials".to_string()),
        std::process::id()
    ));
    fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn file_get(path: &Path, host: &str) -> Result<Option<String>> {
    let host = normalize_host(host);
    Ok(file_read(path)?.remove(&host))
}

fn file_set(path: &Path, host: &str, token: &str) -> Result<()> {
    let host = normalize_host(host);
    let mut map = file_read(path)?;
    map.insert(host, token.trim().to_string());
    file_write(path, &map)
}

/// 删除条目；返回是否确有删除。删空后移除文件本身。
fn file_delete(path: &Path, host: &str) -> Result<bool> {
    let host = normalize_host(host);
    let mut map = file_read(path)?;
    if map.remove(&host).is_none() {
        return Ok(false);
    }
    if map.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(e) => return Err(e.into()),
        }
    }
    file_write(path, &map)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    /// 环境变量是进程级共享状态：所有碰 env 的测试都必须持锁串行。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 设置/清理环境变量，Drop 时恢复原值（panic 安全）。
    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn apply(vars: &[(&str, Option<&str>)]) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = vars
                .iter()
                .map(|(k, _)| ((*k).to_string(), std::env::var(k).ok()))
                .collect();
            for (k, v) in vars {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
            EnvGuard { _lock: lock, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn cred_path(dir: &TempDir) -> PathBuf {
        dir.path().join("riv").join("credentials")
    }

    // ---- 纯函数 ----

    #[test]
    fn normalize_host_variants() {
        assert_eq!(normalize_host("https://github.com"), "github.com");
        assert_eq!(normalize_host("http://github.com/"), "github.com");
        assert_eq!(normalize_host("github.com"), "github.com");
        assert_eq!(normalize_host("github.com/"), "github.com");
        assert_eq!(normalize_host("  GitHub.com  "), "github.com");
        assert_eq!(normalize_host("HTTPS://GitHub.com/riv/riv"), "github.com");
        assert_eq!(
            normalize_host("https://git.example.com:8443/base"),
            "git.example.com:8443"
        );
        assert_eq!(
            normalize_host("git.example.com:8443/"),
            "git.example.com:8443"
        );
    }

    #[test]
    fn mask_token_variants() {
        assert_eq!(mask_token("ghp_abcdefghijklmnopqrstuvwxyz"), "ghp_****wxyz");
        assert_eq!(mask_token("short"), "*****");
        assert_eq!(mask_token(""), "***");
        assert_eq!(mask_token("12345678"), "********");
        assert_eq!(mask_token("123456789"), "1234****6789");
    }

    #[test]
    fn default_host_preset() {
        let _g = EnvGuard::apply(&[(ENV_BASE_URL, None), (ENV_TOKEN, None)]);
        assert_eq!(default_host(), "github.com");
    }

    #[test]
    fn default_host_env_override() {
        let _g = EnvGuard::apply(&[(ENV_BASE_URL, Some("https://git.example.com:8443"))]);
        assert_eq!(default_host(), "git.example.com:8443");
    }

    // ---- 凭证文件层 ----

    #[test]
    fn file_set_get_delete_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        assert_eq!(file_get(&path, "github.com").unwrap(), None);

        file_set(&path, "https://github.com", "ghp_abc123").unwrap();
        assert_eq!(
            file_get(&path, "github.com").unwrap(),
            Some("ghp_abc123".to_string())
        );
        assert_eq!(file_get(&path, "other.com").unwrap(), None);

        // 覆盖写
        file_set(&path, "github.com", "ghp_new").unwrap();
        assert_eq!(
            file_get(&path, "github.com").unwrap(),
            Some("ghp_new".to_string())
        );

        // 删除
        assert!(file_delete(&path, "github.com").unwrap());
        assert_eq!(file_get(&path, "github.com").unwrap(), None);
        // 删空后文件被移除
        assert!(!path.exists());
        // 重复删除 = 无操作
        assert!(!file_delete(&path, "github.com").unwrap());
    }

    #[test]
    fn file_preserves_other_hosts() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        file_set(&path, "github.com", "t1").unwrap();
        file_set(&path, "git.example.com:8443", "t2").unwrap();
        assert!(file_delete(&path, "github.com").unwrap());
        assert_eq!(
            file_get(&path, "git.example.com:8443").unwrap(),
            Some("t2".to_string())
        );
    }

    #[test]
    fn file_parses_comments_and_blank_lines() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "# 注释\n\ngithub.com=ghp_hand\n  \n# 又一注释\ngit.example.com=tok\n",
        )
        .unwrap();
        assert_eq!(
            file_get(&path, "github.com").unwrap(),
            Some("ghp_hand".to_string())
        );
        assert_eq!(
            file_get(&path, "git.example.com").unwrap(),
            Some("tok".to_string())
        );
    }

    #[test]
    fn file_rejects_malformed_line() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "github.com\n").unwrap();
        let err = file_get(&path, "github.com").unwrap_err();
        assert!(
            err.to_string().contains("malformed credentials file"),
            "got: {err}"
        );
    }

    #[test]
    fn file_rejects_empty_token() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "github.com=\n").unwrap();
        assert!(file_get(&path, "github.com").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn file_permissions_are_600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        file_set(&path, "github.com", "ghp_secret").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "expected 600, got {mode:o}");
    }

    // ---- 三级优先级 ----

    #[test]
    fn env_token_beats_file() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        file_set(&path, "github.com", "ghp_from_file").unwrap();
        let _g = EnvGuard::apply(&[(ENV_TOKEN, Some("ghp_from_env")), (ENV_BASE_URL, None)]);
        let cred = get_token_with("github.com", &path).unwrap().unwrap();
        assert_eq!(cred.source, TokenSource::Env);
        assert_eq!(cred.token, "ghp_from_env");
    }

    #[test]
    fn empty_env_falls_through_to_file() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        file_set(&path, "github.com", "ghp_from_file").unwrap();
        let _g = EnvGuard::apply(&[(ENV_TOKEN, Some("")), (ENV_BASE_URL, None)]);
        let cred = get_token_with("github.com", &path).unwrap().unwrap();
        assert_eq!(cred.source, TokenSource::File);
        assert_eq!(cred.token, "ghp_from_file");
    }

    #[test]
    fn no_token_anywhere() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        let _g = EnvGuard::apply(&[(ENV_TOKEN, None), (ENV_BASE_URL, None)]);
        assert!(get_token_with("github.com", &path).unwrap().is_none());
    }

    #[test]
    fn status_reports_env_present() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        let _g = EnvGuard::apply(&[(ENV_TOKEN, Some("ghp_envtok1234")), (ENV_BASE_URL, None)]);
        let st = status_with("github.com", &path).unwrap();
        assert_eq!(st.env, SourceState::Present("ghp_envtok1234".to_string()));
        // 无 DBus 环境（如本 CI）下钥匙串报 Unavailable，有 DBus 且未登录时 Absent
        assert!(matches!(
            st.keyring,
            SourceState::Absent | SourceState::Unavailable
        ));
        assert_eq!(st.file, SourceState::Absent);
        let (src, tok) = st.effective().unwrap();
        assert_eq!(src, TokenSource::Env);
        assert_eq!(tok, "ghp_envtok1234");
    }

    /// 与钥匙串是否可用无关：set→get 必然闭环（桌面落钥匙串，无 DBus 落文件）。
    #[test]
    fn set_get_roundtrip_regardless_of_keyring() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        let _g = EnvGuard::apply(&[(ENV_TOKEN, None), (ENV_BASE_URL, None)]);
        let source = set_token_with("github.com", "ghp_roundtrip", &path).unwrap();
        assert!(matches!(source, TokenSource::Keyring | TokenSource::File));
        let cred = get_token_with("github.com", &path).unwrap().unwrap();
        assert_eq!(cred.token, "ghp_roundtrip");
        // 生效来源应与保存来源一致（另一处的旧条目已在保存时清理）
        assert_eq!(cred.source, source);
        // 删除后读不到
        let cleared = delete_token_with("github.com", &path).unwrap();
        assert!(cleared.contains(&source));
        assert!(get_token_with("github.com", &path).unwrap().is_none());
    }

    #[test]
    fn set_token_rejects_empty() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        assert!(set_token_with("github.com", "   ", &path).is_err());
    }

    /// 回归：无条目可删时 cleared 必须为空——钥匙串的 NoEntry 不是「删除成功」，
    /// 否则 logout 会在什么都没删的情况下谎报 Removed（wine 实测暴露）。
    #[test]
    fn delete_nothing_reports_empty() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        let _g = EnvGuard::apply(&[(ENV_TOKEN, None), (ENV_BASE_URL, None)]);
        let cleared = delete_token_with("delete-empty.example.com", &path).unwrap();
        assert!(cleared.is_empty(), "got: {cleared:?}");
    }

    /// 无 DBus 环境（如本 CI）下，钥匙串报错必须表现为 Unavailable 而非崩溃。
    #[test]
    fn keyring_failure_is_graceful() {
        let dir = TempDir::new().unwrap();
        let path = cred_path(&dir);
        let _g = EnvGuard::apply(&[(ENV_TOKEN, None), (ENV_BASE_URL, None)]);
        let st = status_with("github.com", &path).unwrap();
        assert!(matches!(
            st.keyring,
            SourceState::Absent | SourceState::Unavailable
        ));
    }
}
