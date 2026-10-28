//! 用用户显式配置路径下的真实数据校验密钥（校验，不提取）。
//!
//! 这里的候选密钥全部来自用户自己导入到本地密钥库（store.rs）的条目，
//! 校验目标库是用户在 WeFlow 设置里自选的 dbPath —— 均为用户自有、明确授权。
//! 组件本身不做任何进程内存扫描或路径猜测式枚举。
//!
//! - DB 密钥：`weflow_core::db::open_sqlcipher` 以 `PRAGMA key="x'<hex>'"` 打开并读
//!   `sqlite_master`，能读即密钥正确（微信 4.x 单账号所有分库共用同一把 key）。
//! - 图片密钥：对用户提供的样例 `.dat` 跑 `decrypt_dat`，成功且魔数可识别即视为通过。

use std::path::{Path, PathBuf};

/// 是否为合法的 64 位 hex SQLCipher 密钥（32 字节）。
pub fn is_hex_key(s: &str) -> bool {
    let s = s.trim();
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 宽松 hex 校验：允许首尾空白。返回规范化后的小写 hex，非法返回 None。
pub fn normalize_hex_key(s: &str) -> Option<String> {
    let t = s.trim();
    if is_hex_key(t) {
        Some(t.to_ascii_lowercase())
    } else {
        None
    }
}

/// 把用户输入的 xor key 字符串解析为 u64：支持 `0x..` 十六进制、十进制、或十六进制回退。
pub fn parse_xor_key(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(hexpart) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(hexpart, 16).ok();
    }
    t.parse::<u64>().ok().or_else(|| u64::from_str_radix(t, 16).ok())
}

/// 把用户给定路径解析为一个具体的 `.db` 文件：接受 .db 文件本身，
/// 或其所在目录（按微信 4.x 已知命名择一）。不指定时返回 None。
pub fn resolve_db_file(input: Option<&str>) -> Option<PathBuf> {
    let raw = input?.trim();
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(raw);
    if p.is_file() {
        return Some(p);
    }
    if !p.is_dir() {
        return None;
    }
    // 目录：按用户自选根目录下的已知位置依次尝试（均在用户给定路径内）。
    let candidates = [
        p.join("db_storage").join("session").join("session.db"),
        p.join("session").join("session.db"),
        p.join("session.db"),
        p.join("db_storage").join("contact").join("contact.db"),
        p.join("contact").join("contact.db"),
    ];
    for c in &candidates {
        if c.is_file() {
            return Some(c.clone());
        }
    }
    find_any_db(&p, 4)
}

fn find_any_db(dir: &Path, depth: u32) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    let mut deferred: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_any_db(&path, depth - 1) {
                return Some(found);
            }
        } else if path.extension().map(|e| e.eq_ignore_ascii_case("db")).unwrap_or(false) {
            deferred.push(path);
        }
    }
    deferred.into_iter().next()
}

/// 逐个用候选密钥尝试打开目标库，返回 (命中的密钥?, 逐条日志)。
/// 全部失败时返回 (None, logs)，logs 供上层作为 progress 展示。
pub fn pick_valid_db_key(db_file: &Path, candidates: &[String]) -> (Option<String>, Vec<String>) {
    let mut logs = Vec::new();
    for key in candidates {
        match weflow_core::db::open_sqlcipher(db_file, key) {
            Ok(_) => {
                logs.push(format!("密钥校验通过: {} (库 {})", mask(key), db_file.display()));
                return (Some(key.clone()), logs);
            }
            Err(e) => {
                logs.push(format!("候选密钥 {} 不适用: {e}", mask(key)));
            }
        }
    }
    (None, logs)
}

/// 遮蔽密钥，仅保留首尾各 4 位，避免日志明文回显。
pub fn mask(key: &str) -> String {
    let k = key.trim();
    if k.len() <= 8 {
        return "****".to_string();
    }
    format!("{}…{}", &k[..4], &k[k.len() - 4..])
}

/// 校验单把用户给定密钥能否打开目标库，返回结果与日志（doctor 用）。
pub fn check_db_key(db_file: &Path, key: &str) -> (bool, Vec<String>) {
    let (hit, logs) = pick_valid_db_key(db_file, std::slice::from_ref(&key.to_string()));
    (hit.is_some(), logs)
}

/// 校验图片密钥：尝试解密用户指定的样例 .dat 文件。
pub fn verify_dat(path: &Path, xor_key: u64, aes_key: Option<&str>) -> (bool, String) {
    match weflow_core::dat::decrypt_dat_file(&path.to_string_lossy(), xor_key as u32, aes_key) {
        Ok(dec) => {
            let ext = if dec.ext.is_empty() {
                weflow_core::dat::detect_ext(&dec.data)
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "未知".to_string())
            } else {
                dec.ext.clone()
            };
            (true, format!("样例 .dat 解密成功，识别类型 .{ext}"))
        }
        Err(e) => (false, format!("样例 .dat 解密失败: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_key_strict() {
        assert!(is_hex_key(&"a".repeat(64)));
        assert!(!is_hex_key(&"a".repeat(63)));
        assert!(!is_hex_key("0x1234"));
        assert!(!is_hex_key("zz"));
        assert_eq!(normalize_hex_key("  ABCD "), None::<String>);
        assert_eq!(normalize_hex_key(&"AB".repeat(32)).unwrap().chars().count(), 64);
    }

    #[test]
    fn xor_parsing_variants() {
        assert_eq!(parse_xor_key("0x5a"), Some(0x5a));
        assert_eq!(parse_xor_key("90"), Some(90));
        assert_eq!(parse_xor_key(""), None);
    }

    #[test]
    fn mask_key() {
        assert_eq!(mask("abcd1234efgh"), "abcd…efgh");
        assert_eq!(mask("short"), "****");
    }

    #[test]
    fn pick_reports_failure_logs() {
        // 指向一个不存在/非法的"库"，候选密钥应全部失败且给出日志。
        let tmp = std::env::temp_dir().join("kp_bad_notadb.dat");
        let _ = std::fs::write(&tmp, b"junk");
        let (hit, logs) = pick_valid_db_key(&tmp, &["a".repeat(64)]);
        assert!(hit.is_none());
        assert!(!logs.is_empty());
        let _ = std::fs::remove_file(&tmp);
    }

    /// 端到端密码学校验：真造一个 SQLCipher 加密库，验证 pick_valid_db_key
    /// 能在一堆候选里挑出正确密钥，且错误密钥全部失败。
    #[test]
    fn pick_finds_real_sqlcipher_key() {
        use rusqlite::Connection;
        let good = "1a".repeat(32); // 64 hex
        let bad = "2b".repeat(32);
        let db = std::env::temp_dir().join("kp_real_enc_test.db");
        let _ = std::fs::remove_file(&db);
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(&format!("PRAGMA key = \"x'{good}'\";")).unwrap();
            conn.execute_batch("CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (1);").unwrap();
            drop(conn);
        }
        // 正确密钥在候选中（含一个错误密钥在前）
        let (hit, _) = pick_valid_db_key(&db, &[bad.clone(), good.clone()]);
        assert_eq!(hit.as_deref(), Some(good.as_str()));
        // 只给错误密钥 -> 全部失败
        let (miss, logs) = pick_valid_db_key(&db, &[bad.clone()]);
        assert!(miss.is_none());
        assert_eq!(logs.len(), 1);
        // check_db_key 单独校验正确密钥应通过
        assert!(check_db_key(&db, &good).0);
        assert!(!check_db_key(&db, &bad).0);
        let _ = std::fs::remove_file(&db);
    }
}
