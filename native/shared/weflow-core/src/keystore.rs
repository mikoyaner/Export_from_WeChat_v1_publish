//! 每库独立密钥库（微信 4.x：db_storage 下每个 .db 各有一把 SQLCipher key，
//! 实测确认 session.db 内无 keyring，session key 打不开其它库）。
//!
//! 这是 key-provider / wcdb.dll / welive-export **共享**的私有密钥通道：
//! WeFlow 只把 session.db 的单把 key 交给 `wcdb_open_account`，其余各库的 key 由
//! 用户经 key-provider 批量导入本库，本模块在打开每个库时按相对路径解析出对应 key，
//! 找不到就回退到传入的主 key（兼容单密钥 / 微信 3.x）。
//!
//! 存储位置（优先级，与 key-provider 一致）：
//!   1. 环境变量 `WEFLOW_KEY_STORE`
//!   2. 可执行文件同目录 `key-store.json`（若存在）
//!   3. `%APPDATA%\weflow-key-provider\store.json`（Unix: `~/.config/...`）

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 账号无关的默认桶（单账号或未区分时）。
pub const DEFAULT_ACCOUNT: &str = "__default__";

/// 微信 .dat 图片密钥。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageKeyEntry {
    pub xor_key: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aes_key: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct KeyStore {
    /// accountId(or DEFAULT) -> 主(session) key。WeFlow 打开账号用的那把；也作回退。
    #[serde(default)]
    pub db_keys: BTreeMap<String, String>,
    /// normalizedAccountDir -> ( relPathUnderDbStorage -> hexKey )。每库独立密钥。
    #[serde(default)]
    pub db_key_maps: BTreeMap<String, BTreeMap<String, String>>,
    /// accountId(or DEFAULT) -> 图片密钥。
    #[serde(default)]
    pub image_keys: BTreeMap<String, ImageKeyEntry>,
}

/// 路径规范化：转正斜杠、转小写（Windows 大小写不敏感）、去尾部斜杠。
pub fn norm_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    s.trim_end_matches('/').to_string()
}

fn norm_rel(s: &str) -> String {
    s.replace('\\', "/").to_ascii_lowercase().trim_start_matches('/').to_string()
}

/// 求 `db` 相对 `<account_dir>/db_storage` 的路径（小写正斜杠），如 `message/message_0.db`。
pub fn rel_under_db_storage(db: &Path, account_dir: &Path) -> Option<String> {
    let storage = account_dir.join("db_storage");
    let rel = db.strip_prefix(&storage).ok()?;
    Some(norm_rel(&rel.to_string_lossy()))
}

/// 解析密钥库文件路径。
pub fn store_path() -> PathBuf {
    if let Ok(p) = std::env::var("WEFLOW_KEY_STORE") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("key-store.json");
            if cand.exists() {
                return cand;
            }
        }
    }
    let mut base = {
        #[cfg(windows)]
        {
            PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| ".".into()))
        }
        #[cfg(not(windows))]
        {
            match std::env::var("XDG_CONFIG_HOME") {
                Ok(x) if !x.trim().is_empty() => PathBuf::from(x),
                _ => {
                    let mut h = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                    h.push_str("/.config");
                    PathBuf::from(h)
                }
            }
        }
    };
    base.push("weflow-key-provider");
    base.push("store.json");
    base
}

impl KeyStore {
    pub fn load(path: &Path) -> KeyStore {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => KeyStore::default(),
        }
    }

    pub fn load_default() -> KeyStore {
        KeyStore::load(&store_path())
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // 原子写：半截 JSON 会让下次 load 直接静默回退成空 keystore（现象=密钥全没了）。
        // tmp 名带 pid：入口并发闸允许多条链路同时跑，两条同时落钥时固定名会互相
        // 覆盖（Windows 下 rename 打开中的文件会失败）——各写各的 tmp，rename 本身原子。
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 解析某个库文件应使用的密钥：优先每库密钥表（按 account_dir + 相对路径），
    /// 命中即用；否则回退 `fallback`（主 session key）。
    pub fn resolve_key(&self, db: &Path, account_dir: Option<&Path>, fallback: &str) -> String {
        if let Some(dir) = account_dir {
            let acct = norm_path(dir);
            if let Some(map) = self.db_key_maps.get(&acct) {
                if let Some(rel) = rel_under_db_storage(db, dir) {
                    if let Some(k) = map.get(&rel) {
                        return k.clone();
                    }
                }
            }
        }
        fallback.to_string()
    }

    /// 指定账号下所有已知密钥候选（去重）：主 key → 每库表所有值 → DEFAULT。
    /// key-provider 取 DB key 时逐个用 SQLCipher 校验，挑出能打开目标库者。
    pub fn candidate_db_keys(&self, account_id: Option<&str>, account_dir: Option<&Path>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let push = |k: &str, out: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
            if !k.is_empty() && seen.insert(k.to_ascii_lowercase()) {
                out.push(k.to_ascii_lowercase());
            }
        };
        if let Some(dir) = account_dir {
            if let Some(map) = self.db_key_maps.get(&norm_path(dir)) {
                for k in map.values() {
                    push(k, &mut out, &mut seen);
                }
            }
        }
        if let Some(acc) = account_id {
            if let Some(k) = self.db_keys.get(acc) {
                push(k, &mut out, &mut seen);
            }
        }
        if let Some(k) = self.db_keys.get(DEFAULT_ACCOUNT) {
            push(k, &mut out, &mut seen);
        }
        out
    }

    pub fn image_key(&self, account_id: Option<&str>) -> Option<&ImageKeyEntry> {
        account_id
            .and_then(|a| self.image_keys.get(a))
            .or_else(|| self.image_keys.get(DEFAULT_ACCOUNT))
    }

    /// 该 account_dir 是否已有每库密钥映射表（诊断用）。
    pub fn has_db_key_map_for(&self, account_dir: &Path) -> bool {
        self.db_key_maps.contains_key(&norm_path(account_dir))
    }

    /// 列出所有"位于 `dir` 之下（或等于 dir）"的已导入账号目录。
    /// 用于 WeFlow 只给出父级数据目录（如 <微信数据根>）时识别候选账号。
    pub fn accounts_under(&self, dir: &Path) -> Vec<PathBuf> {
        let prefix = norm_path(dir);
        self.db_key_maps
            .keys()
            .filter(|acct| *acct == &prefix || acct.starts_with(&format!("{prefix}/")))
            .map(PathBuf::from)
            .collect()
    }

    /// 全部已导入账号目录（norm 规整后的路径）。
    pub fn all_accounts(&self) -> Vec<PathBuf> {
        self.db_key_maps.keys().map(PathBuf::from).collect()
    }

    /// 某账号目录的 session 主密钥（每库表里的 session/session.db）。
    pub fn session_key_for(&self, account_dir: &Path) -> Option<String> {
        self.db_key_maps
            .get(&norm_path(account_dir))
            .and_then(|m| m.get("session/session.db"))
            .cloned()
    }

    /// 把一批"每库密钥"写入某 account_dir 的映射表（相对路径键）。返回写入条数。
    pub fn put_db_key_map(&mut self, account_dir: &Path, entries: BTreeMap<String, String>) -> usize {
        let acct = norm_path(account_dir);
        let mut norm: BTreeMap<String, String> = BTreeMap::new();
        for (rel, key) in entries {
            norm.insert(norm_rel(&rel), key.trim().to_ascii_lowercase());
        }
        let n = norm.len();
        self.db_key_maps.insert(acct, norm);
        n
    }

    /// 解析取钥工具的常见输出格式，逐行提取 `相对/db_storage 的 .db 路径 -> 64hex key`。
    /// 兼容形如：`  [Cipher-FOUND] message\message_4.db salt=.. -> b4a1..`。
    /// 返回 (relPath, key) 列表；无法识别的行忽略。
    pub fn parse_scanner_lines(text: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for line in text.lines() {
            let arrow = match line.find("->") {
                Some(i) => i,
                None => continue,
            };
            let key_raw = line[arrow + 2..].trim();
            let key: String = key_raw
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if key.len() != 64 {
                continue;
            }
            let left = line[..arrow].trim();
            // db 路径 = left 中最后一个以 .db 结尾的 token（前面可能带 [Cipher-FOUND] / salt 在后）
            let token = left
                .split_whitespace()
                .rev()
                .find(|t| t.to_ascii_lowercase().ends_with(".db"));
            let Some(tok) = token else { continue };
            let rel = tok.trim_matches(|c| c == '"' || c == '\'');
            out.push((norm_rel(rel), key.to_ascii_lowercase()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_and_rel() {
        assert_eq!(norm_path(Path::new("E:/A/B\\")), "e:/a/b");
        let dir = Path::new("E:/acct/wxid_x");
        let db = Path::new("E:/acct/wxid_x/db_storage/message/message_0.db");
        assert_eq!(rel_under_db_storage(db, dir).as_deref(), Some("message/message_0.db"));
    }

    #[test]
    fn resolve_prefers_map_then_fallback() {
        let mut s = KeyStore::default();
        let mut map = BTreeMap::new();
        map.insert("message/message_0.db".to_string(), "b".repeat(64));
        s.put_db_key_map(Path::new("E:/acct/wxid_x"), map);
        let dir = Path::new("E:/acct/wxid_x");
        let hit = s.resolve_key(Path::new("E:/acct/wxid_x/db_storage/message/message_0.db"), Some(dir), &"a".repeat(64));
        assert_eq!(hit, "b".repeat(64));
        // 未登记的库 -> 回退
        let fb = s.resolve_key(Path::new("E:/acct/wxid_x/db_storage/other/x.db"), Some(dir), &"a".repeat(64));
        assert_eq!(fb, "a".repeat(64));
    }

    #[test]
    fn parse_scanner_output() {
        let text = r#"
  [Cipher-FOUND] message\message_4.db salt=0123 -> 1111111111111111111111111111111111111111111111111111111111111111
  [Cipher-FOUND] session\session.db salt=4567 -> 2222222222222222222222222222222222222222222222222222222222222222
some noise line
[Cipher] PID=19308: 37 keys
"#;
        let got = KeyStore::parse_scanner_lines(text);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "message/message_4.db");
        assert_eq!(got[0].1, "1111111111111111111111111111111111111111111111111111111111111111");
        assert_eq!(got[1].0, "session/session.db");
    }

    #[test]
    fn roundtrip_save_load() {
        let path = std::env::temp_dir().join("kp_keystore_rt.json");
        let _ = std::fs::remove_file(&path);
        let mut s = KeyStore::default();
        let mut map = BTreeMap::new();
        map.insert("sns/sns.db".to_string(), "c".repeat(64));
        s.put_db_key_map(Path::new("E:/acct/a"), map);
        s.db_keys.insert(DEFAULT_ACCOUNT.into(), "d".repeat(64));
        s.save(&path).unwrap();
        let loaded = KeyStore::load(&path);
        assert!(loaded.db_key_maps.contains_key(&norm_path(Path::new("E:/acct/a"))));
        assert_eq!(loaded.db_keys.get(DEFAULT_ACCOUNT).unwrap(), &"d".repeat(64));
        let _ = std::fs::remove_file(&path);
    }
}
