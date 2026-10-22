//! riv 的配置文件 `.riv/config`（INI 风格）与仓库配置封装。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::error::Result;

/// 简单的 INI 解析器：`[section]` + `key = value`，`#`/`;` 注释。
#[derive(Debug, Clone, Default)]
pub struct Config {
    sections: BTreeMap<String, BTreeMap<String, String>>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e.into()),
        };
        Ok(Self::parse(&text))
    }

    pub fn parse(text: &str) -> Config {
        let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut current = String::new();
        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                current = rest.trim().to_string();
                sections.entry(current.clone()).or_default();
                continue;
            }
            if let Some(eq) = line.find('=') {
                let key = line[..eq].trim().to_string();
                let value = line[eq + 1..].trim().to_string();
                sections
                    .entry(current.clone())
                    .or_default()
                    .insert(key, value);
            }
        }
        Config { sections }
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections
            .get(section)
            .and_then(|s| s.get(key))
            .map(|s| s.as_str())
    }

    pub fn set(&mut self, section: &str, key: &str, value: &str) {
        self.sections
            .entry(section.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut out = String::new();
        for (section, kv) in &self.sections {
            if kv.is_empty() {
                continue;
            }
            out.push_str(&format!("[{section}]\n"));
            for (k, v) in kv {
                out.push_str(&format!("\t{k} = {v}\n"));
            }
            out.push('\n');
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, out)?;
        Ok(())
    }
}

/// 仓库级配置（ riv 需要的部分）。
pub struct RepoConfig {
    pub user_name: String,
    pub user_email: String,
    pub compression: i32,
    pub default_branch: String,
}

impl RepoConfig {
    pub fn from_config(config: &Config) -> RepoConfig {
        RepoConfig {
            user_name: config.get("user", "name").unwrap_or("riv").to_string(),
            user_email: config
                .get("user", "email")
                .unwrap_or("riv@localhost")
                .to_string(),
            compression: config
                .get("core", "compression")
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
            default_branch: config
                .get("core", "defaultbranch")
                .unwrap_or("main")
                .to_string(),
        }
    }

    pub fn default_config_text() -> String {
        "\
# riv repository configuration
[user]
	name = riv
	email = riv@localhost

[core]
	compression = 3
	defaultbranch = main
"
        .to_string()
    }

    pub fn signature_now(&self) -> crate::object::Signature {
        crate::object::Signature::now(&self.user_name, &self.user_email)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn parse_basic() {
        let cfg =
            Config::parse("[user]\n\tname = Alice\n\temail = a@b.c\n[core]\n\tcompression = 5\n");
        assert_eq!(cfg.get("user", "name"), Some("Alice"));
        assert_eq!(cfg.get("user", "email"), Some("a@b.c"));
        assert_eq!(cfg.get("core", "compression"), Some("5"));
        assert_eq!(cfg.get("nope", "x"), None);
    }

    #[test]
    fn comments_and_blank() {
        let cfg = Config::parse("# comment\n\n[user]\n; another\nname = Bob\n");
        assert_eq!(cfg.get("user", "name"), Some("Bob"));
    }

    #[test]
    fn roundtrip_save_load() {
        let mut cfg = Config::default();
        cfg.set("user", "name", "Carol");
        cfg.set("core", "compression", "7");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.get("user", "name"), Some("Carol"));
        assert_eq!(loaded.get("core", "compression"), Some("7"));
    }

    #[test]
    fn repo_config_defaults() {
        let cfg = Config::default();
        let rc = RepoConfig::from_config(&cfg);
        assert_eq!(rc.user_name, "riv");
        assert_eq!(rc.compression, 3);
        assert_eq!(rc.default_branch, "main");
    }

    #[test]
    fn load_missing_file_ok() {
        let cfg = Config::load(Path::new("/nonexistent/riv/config")).unwrap();
        assert!(cfg.get("user", "name").is_none());
    }

    #[test]
    fn default_config_text_parses() {
        let cfg = Config::parse(&RepoConfig::default_config_text());
        assert_eq!(cfg.get("user", "name"), Some("riv"));
        assert_eq!(cfg.get("core", "compression"), Some("3"));
        assert_eq!(cfg.get("core", "defaultbranch"), Some("main"));
    }

    #[test]
    fn error_variant_usable() {
        let e = Error::Config("bad".into());
        assert!(e.to_string().contains("bad"));
    }
}
