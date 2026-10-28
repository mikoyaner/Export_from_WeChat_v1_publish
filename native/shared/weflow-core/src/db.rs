//! SQLCipher 账号上下文：连接打开/缓存、微信 4.x db_storage 布局发现、schema 内省。
//!
//! 真实库表结构在拿到 dbKey 前无法最终确认（任务 #7 用 <微信数据根> 验证），
//! 因此 schema 解析采取"运行时内省 + 多候选列名"策略，不做过度硬编码。

use anyhow::{anyhow, bail, Result};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// db_storage 下各类库文件的位置。
#[derive(Debug, Clone, Default)]
pub struct DbLayout {
    pub session_db: PathBuf,
    /// message_0.db .. message_N.db，按序号升序。
    pub message_dbs: Vec<PathBuf>,
    /// media_0.db ..（媒体/硬链接元数据分库）。
    pub media_dbs: Vec<PathBuf>,
    pub contact_db: Option<PathBuf>,
    pub sns_db: Option<PathBuf>,
    pub emoticon_db: Option<PathBuf>,
    pub hardlink_db: Option<PathBuf>,
    pub message_fts_db: Option<PathBuf>,
    pub message_resource_db: Option<PathBuf>,
}

impl DbLayout {
    /// 布局探测报告：每一项 =（名称，是否找到，说明）。
    /// 微信改目录/文件名的第一现场就在这里——先让人看见"哪个库没找到"，
    /// 而不是等到语音全 not_local、昵称全变空才怀疑。
    pub fn summary(&self) -> Vec<(String, bool, String)> {
        let count = |v: &Vec<PathBuf>| {
            if v.is_empty() {
                (false, "0 个".to_string())
            } else {
                (true, format!("{} 个：{}", v.len(),
                               v.iter().filter_map(|p| p.file_name()).map(|s| s.to_string_lossy().to_string())
                                .collect::<Vec<_>>().join(",")))
            }
        };
        let one = |o: &Option<PathBuf>| match o {
            Some(p) => (true, p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()),
            None => (false, "缺失".to_string()),
        };
        let mut out = Vec::new();
        out.push(("session.db".into(), self.session_db.is_file(),
                  self.session_db.to_string_lossy().to_string()));
        let (m_ok, m_d) = count(&self.message_dbs);
        out.push(("message 分库".into(), m_ok, m_d));
        let (v_ok, v_d) = count(&self.media_dbs);
        out.push(("media 分库".into(), v_ok, v_d));
        for (name, opt) in [("contact.db", &self.contact_db), ("hardlink.db", &self.hardlink_db),
                            ("sns.db", &self.sns_db), ("emoticon.db", &self.emoticon_db),
                            ("message_fts.db", &self.message_fts_db),
                            ("message_resource.db", &self.message_resource_db)] {
            let (ok, detail) = one(opt);
            out.push((name.into(), ok, detail));
        }
        out
    }
}

/// 从 session.db 路径反推账号根目录（.../wxid_xxx/db_storage/session/session.db）。
pub fn derive_account_dir(session_db: &Path) -> Option<PathBuf> {
    let mut dir = session_db.parent()?;
    loop {
        if dir.file_name()?.eq_ignore_ascii_case("db_storage") {
            return dir.parent().map(PathBuf::from);
        }
        dir = dir.parent()?;
    }
}

fn numbered_dbs(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut out: Vec<(i64, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            let Some(rest) = lower.strip_prefix(prefix) else {
                continue;
            };
            let Some(rest) = rest.strip_suffix(".db") else {
                continue;
            };
            if let Ok(idx) = rest.parse::<i64>() {
                out.push((idx, path));
            }
        }
    }
    out.sort_by_key(|(idx, _)| *idx);
    out.into_iter().map(|(_, p)| p).collect()
}

fn find_db(db_dir: &Path, name: &str) -> Option<PathBuf> {
    let p = db_dir.join(name);
    p.is_file().then_some(p)
}

/// 以 session.db 为入口发现同账号的全部业务库。
pub fn discover_layout(session_db: &Path) -> DbLayout {
    let mut layout = DbLayout { session_db: session_db.to_path_buf(), ..Default::default() };
    let account_dir = match derive_account_dir(session_db) {
        Some(d) => d,
        None => return layout,
    };
    let storage = account_dir.join("db_storage");
    let msg_dir = storage.join("message");
    layout.message_dbs = numbered_dbs(&msg_dir, "message_");
    if layout.message_dbs.is_empty() {
        // 候选兜底：有的版本不分片（message.db）
        if let Some(p) = find_db(&msg_dir, "message.db") {
            layout.message_dbs.push(p);
        }
    }
    layout.media_dbs = numbered_dbs(&msg_dir, "media_");
    if layout.media_dbs.is_empty() {
        // 候选兜底：不分片的 media.db / Media.db（大小写不敏感文件系统上同名，仍列出备查）
        for name in ["media.db", "Media.db"] {
            if let Some(p) = find_db(&msg_dir, name) {
                layout.media_dbs.push(p);
                break;
            }
        }
    }
    layout.message_fts_db = find_db(&msg_dir, "message_fts.db");
    layout.message_resource_db = find_db(&msg_dir, "message_resource.db");
    layout.contact_db = find_db(&storage.join("contact"), "contact.db");
    if layout.contact_db.is_none() {
        // contact 目录改名时，扫 db_storage 下一层找 contact.db（代价很小）
        layout.contact_db = ls_subdir_db(&storage, "contact.db");
    }
    layout.sns_db = find_db(&storage.join("sns"), "sns.db");
    layout.emoticon_db = find_db(&storage.join("emoticon"), "emoticon.db");
    layout.hardlink_db = find_db(&storage.join("hardlink"), "hardlink.db");
    layout
}

/// 在 `root` 的直接子目录里找名为 `name` 的库（只扫一层，防目录重命名后彻底静默）。
fn ls_subdir_db(root: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            let cand = p.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// 用 SQLCipher hex 密钥打开数据库并验证可读。
pub fn open_sqlcipher(path: &Path, key_hex: &str) -> Result<Connection> {
    let conn = Connection::open(path).map_err(|e| anyhow!("open {}: {e}", path.display()))?;
    let key = key_hex.trim();
    // 只接受 hex 密钥（WeFlow 约定：64 位 hex 字符串）；PRAGMA 值必须原样拼接，
    // 因此先做严格白名单校验，杜绝 SQL 注入。
    if key.is_empty()
        || key.len() % 2 != 0
        || !key.bytes().all(|b| b.is_ascii_hexdigit())
    {
        bail!("dbKey 不是合法的十六进制字符串");
    }
    conn.execute_batch(&format!("PRAGMA key = \"x'{key}'\";"))?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
        .map_err(|e| anyhow!("密钥错误或数据库损坏: {e}"))?;
    Ok(conn)
}

pub fn list_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    // 表名无法参数化，先白名单校验再拼接
    if table.is_empty() || !table.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        bail!("非法表名: {table}");
    }
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(cols)
}

/// 一个账号的全部状态：会话连接、按需缓存的分库连接、我的账号 ID。
pub struct Account {
    pub layout: DbLayout,
    pub key_hex: String,
    pub account_dir: Option<PathBuf>,
    pub keystore: crate::keystore::KeyStore,
    pub my_account_id: Mutex<Option<String>>,
    conns: Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>>,
    columns_cache: Mutex<HashMap<(PathBuf, String), Arc<Vec<String>>>>,
    msg_index: Mutex<Option<Arc<crate::schema::MsgIndex>>>,
}

impl Account {
    /// 打开账号（session.db + 密钥），失败原因由调用方映射为状态码。
    pub fn open(session_db_path: &str, key_hex: &str) -> Result<Arc<Account>> {
        let path = PathBuf::from(session_db_path);
        if !path.is_file() {
            bail!("session.db 不存在: {session_db_path}");
        }
        let layout = discover_layout(&path);
        let account_dir = derive_account_dir(&path);
        let keystore = crate::keystore::KeyStore::load_default();
        let account = Arc::new(Account {
            layout,
            key_hex: key_hex.to_string(),
            account_dir,
            keystore,
            my_account_id: Mutex::new(None),
            conns: Mutex::new(HashMap::new()),
            columns_cache: Mutex::new(HashMap::new()),
            msg_index: Mutex::new(None),
        });
        // 立即打开 session 库验证密钥
        account.conn(&path)?;
        Ok(account)
    }

    pub fn account_dir(&self) -> Option<PathBuf> {
        self.account_dir.clone()
    }

    /// 该库应使用的密钥：每库表命中则用之，否则回退主 key。
    fn key_for(&self, path: &Path) -> String {
        self.keystore.resolve_key(path, self.account_dir.as_deref(), &self.key_hex)
    }

    /// 取（或建立）到指定库文件的连接句柄。连接按路径缓存，Mutex 串行化访问。
    pub fn conn(&self, path: &Path) -> Result<Arc<Mutex<Connection>>> {
        let mut cache = self
            .conns
            .lock()
            .map_err(|_| anyhow!("connection cache poisoned"))?;
        if let Some(c) = cache.get(path) {
            return Ok(Arc::clone(c));
        }
        let key = self.key_for(path);
        let conn = open_sqlcipher(path, &key)?;
        let c = Arc::new(Mutex::new(conn));
        cache.insert(path.to_path_buf(), Arc::clone(&c));
        Ok(c)
    }

    /// 在指定库上执行一段只读/写入逻辑，自动加解锁。
    pub fn with_conn<T>(&self, path: &Path, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let arc = self.conn(path)?;
        let conn = arc.lock().map_err(|_| anyhow!("connection poisoned"))?;
        f(&conn)
    }

    pub fn session_conn(&self) -> Result<Arc<Mutex<Connection>>> {
        self.conn(&self.layout.session_db)
    }

    /// PRAGMA table_info 结果缓存（列名保持数据库原始大小写）。
    pub fn columns(&self, db: &Path, table: &str) -> Result<Arc<Vec<String>>> {
        let key = (db.to_path_buf(), table.to_string());
        if let Ok(cache) = self.columns_cache.lock() {
            if let Some(hit) = cache.get(&key) {
                return Ok(Arc::clone(hit));
            }
        }
        let cols = self.with_conn(db, |conn| table_columns(conn, table))?;
        let arc = Arc::new(cols);
        if let Ok(mut cache) = self.columns_cache.lock() {
            cache.insert(key, Arc::clone(&arc));
        }
        Ok(arc)
    }

    /// 消息库分表索引（懒构建，失败不缓存以便下次重试）。
    pub fn msg_index(&self) -> Result<Arc<crate::schema::MsgIndex>> {
        if let Ok(cache) = self.msg_index.lock() {
            if let Some(hit) = cache.as_ref() {
                return Ok(Arc::clone(hit));
            }
        }
        let index = crate::schema::MsgIndex::build(self)?;
        if let Ok(mut cache) = self.msg_index.lock() {
            *cache = Some(Arc::clone(&index));
        }
        Ok(index)
    }

    /// 消息库布局变化（新增分库/分表）后重建索引。
    pub fn invalidate_msg_index(&self) {
        if let Ok(mut cache) = self.msg_index.lock() {
            *cache = None;
        }
        if let Ok(mut cache) = self.columns_cache.lock() {
            cache.clear();
        }
    }

    /// wcdb_purge_memory：释放页缓存，不关闭活动连接、不影响游标。
    pub fn shrink_memory(&self) {
        if let Ok(cache) = self.conns.lock() {
            for conn in cache.values() {
                if let Ok(c) = conn.lock() {
                    let _ = c.execute_batch("PRAGMA shrink_memory;");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_account_dir_walks_up() {
        let p = Path::new(r"C:\wechat_data\wxid_abc_1234\db_storage\session\session.db");
        assert_eq!(
            derive_account_dir(p),
            Some(PathBuf::from(r"C:\wechat_data\wxid_abc_1234"))
        );
    }

    #[test]
    fn rejects_non_hex_key() {
        let tmp = std::env::temp_dir().join("weflow_core_test_nonhex.db");
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(&tmp, b"not a db").unwrap();
        assert!(open_sqlcipher(&tmp, "zz12").is_err());
        assert!(open_sqlcipher(&tmp, "").is_err());
        assert!(open_sqlcipher(&tmp, "0x1234").is_err());
        let _ = std::fs::remove_file(&tmp);
    }
}
