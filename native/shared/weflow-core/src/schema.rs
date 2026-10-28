//! 微信 4.x 消息库 schema 发现与会话→消息表路由。
//!
//! 真实表结构需拿到 dbKey 后用 <微信数据根> 验证（任务 #7），因此这里采取
//! "运行时内省 + 多候选列名"策略：
//! - 消息分表：名字形如 `MSG_*`/`Msg_*`/`message_*`（排除 FTS 辅助表）；
//! - 会话路由两种模式：
//!   A. 映射表模式：存在 Name2ID 类表（username → int id），消息表按 id 命名（MSG_{id}）；
//!   B. 共享表模式：消息表内有 talker 列（str_talker/talker/session_id/username 等），
//!      用 WHERE 过滤。
//! 两种都不满足时返回 schema mismatch（状态码 -7 由调用方映射）。

use crate::db::{list_tables, Account};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const NAME_COL_CANDIDATES: &[&str] = &[
    "usr_name",
    "usrname",
    "user_name",
    "username",
    "str_session_id",
    "strsessionid",
    "session_id_text",
];
const ID_COL_CANDIDATES: &[&str] = &["rowid", "id", "usr_id", "usrid", "session_id", "sessionid", "talker_id", "talkerid"];
const TALKER_COL_CANDIDATES: &[&str] = &[
    "str_talker",
    "strtalker",
    "talker",
    "session_id",
    "sessionid",
    "str_session_id",
    "username",
    "user_name",
];
/// annualReportService.detectTimeColumn 的候选序（04 规格 §message_table_columns）。
const TIME_COL_CANDIDATES: &[&str] = &[
    "create_time",
    "createtime",
    "msg_create_time",
    "msg_time",
    "msgtime",
    "time",
];

fn is_msg_table(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("sqlite_") {
        return false;
    }
    // 排除 FTS 辅助表（*_content/*_segments/*_idx/*_docsize/*_config/*_data 等）
    let is_fts_part = ["_content", "_segments", "_segdir", "_idx", "_docsize", "_config", "_data", "_stat"]
        .iter()
        .any(|suffix| lower.ends_with(suffix));
    if is_fts_part {
        return false;
    }
    lower.starts_with("msg") || lower.starts_with("message")
}

/// 会话 username 对应的消息表名后缀：md5(username) 的小写十六进制（微信 4.x 实测规律）。
/// 真实表名形如 `Msg_<md5hex>`，落在包含它的那个 message_N.db 分库中。
pub fn msg_table_hash(session_id: &str) -> String {
    format!("{:x}", md5::compute(session_id.as_bytes()))
}

#[derive(Debug, Clone)]
pub struct MappingTable {
    pub db_idx: usize,
    pub table: String,
    pub name_col: String,
    /// None = 用 SQLite 隐式 rowid。
    pub id_col: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MsgIndex {
    /// 与 Account.layout.message_dbs 对齐。
    pub dbs: Vec<PathBuf>,
    pub tables_per_db: Vec<Vec<String>>,
    /// 全部消息分表 (db_idx, table)。
    pub msg_tables: Vec<(usize, String)>,
    pub mapping: Option<MappingTable>,
    /// 共享表模式下的 talker 过滤列。
    pub talker_column: Option<String>,
}

/// 会话的消息表定位结果。
#[derive(Debug, Clone)]
pub enum SessionTables {
    /// 模式 A：专属分表已定位。
    Located(Vec<(usize, String)>),
    /// 模式 B：所有消息分表 + talker 过滤列。
    Shared { tables: Vec<(usize, String)>, talker_column: String },
}

impl MsgIndex {
    pub fn build(account: &Account) -> Result<Arc<MsgIndex>> {
        let dbs = account.layout.message_dbs.clone();
        if dbs.is_empty() {
            bail!("未发现 message_*.db 分库");
        }
        let mut tables_per_db = Vec::with_capacity(dbs.len());
        let mut msg_tables = Vec::new();
        let mut mapping: Option<MappingTable> = None;

        for (db_idx, db) in dbs.iter().enumerate() {
            let tables = account.with_conn(db, |conn| list_tables(conn))?;
            for t in &tables {
                let lower = t.to_ascii_lowercase();
                if mapping.is_none() && lower.contains("name2id") {
                    if let Ok(cols) = account.columns(db, t) {
                        if let Some(m) = resolve_mapping(t, &cols, db_idx) {
                            mapping = Some(m);
                        }
                    }
                }
                if is_msg_table(t) {
                    msg_tables.push((db_idx, t.clone()));
                }
            }
            tables_per_db.push(tables);
        }

        // talker 列探测：抽样第一张消息分表
        let mut talker_column = None;
        if let Some((db_idx, table)) = msg_tables.first() {
            if let Ok(cols) = account.columns(&dbs[*db_idx], table) {
                talker_column = find_candidate(&cols, TALKER_COL_CANDIDATES);
            }
        }

        Ok(Arc::new(MsgIndex { dbs, tables_per_db, msg_tables, mapping, talker_column }))
    }

    /// 定位 sessionId 的消息分表。定位不到（映射无此会话）返回空向量而非错误。
    pub fn session_tables(&self, account: &Account, session_id: &str) -> Result<SessionTables> {
        self.session_tables_routed(account, session_id).map(|(t, _)| t)
    }

    /// 同 `session_tables`，但把**命中的是哪一级路由**带出来：
    /// `md5`（主路由）/ `mapping`（映射表）/ `shared`（共享表 + talker 列）/ `none`（合法空）。
    /// 微信下次改布局时，这条就是"从哪一级退化的"的第一现场证据。
    pub fn session_tables_routed(
        &self,
        account: &Account,
        session_id: &str,
    ) -> Result<(SessionTables, &'static str)> {
        // 主路由（微信 4.x 实测）：消息表名 = `Msg_<md5(session_id)>`，按哈希后缀在各分库中定位。
        let hash = msg_table_hash(session_id);
        let mut located: Vec<(usize, String)> = Vec::new();
        for (db_idx, tables) in self.tables_per_db.iter().enumerate() {
            for t in tables {
                if is_msg_table(t) && t.to_ascii_lowercase().ends_with(&hash) {
                    located.push((db_idx, t.clone()));
                }
            }
        }
        if !located.is_empty() {
            located.sort();
            located.dedup();
            return Ok((SessionTables::Located(located), "md5"));
        }

        if let Some(mapping) = &self.mapping {
            let db = &self.dbs[mapping.db_idx];
            let id_col = mapping.id_col.clone().unwrap_or_else(|| "rowid".to_string());
            let sql = format!(
                "SELECT \"{}\" FROM \"{}\" WHERE \"{}\" = ? LIMIT 1",
                sanitize_ident(&id_col)?,
                sanitize_ident(&mapping.table)?,
                sanitize_ident(&mapping.name_col)?
            );
            let found: Option<i64> = account.with_conn(db, |conn| {
                let mut stmt = conn.prepare(&sql)?;
                let found = stmt
                    .query_map([session_id], |r| r.get::<_, i64>(0))?
                    .next()
                    .transpose()?;
                Ok(found)
            })?;
            if let Some(id) = found {
                let mut located = Vec::new();
                for prefix in ["MSG_", "Msg_", "msg_", "MESSAGE_", "message_"] {
                    let name = format!("{prefix}{id}");
                    for (db_idx, tables) in self.tables_per_db.iter().enumerate() {
                        if tables.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
                            let real = tables
                                .iter()
                                .find(|t| t.eq_ignore_ascii_case(&name))
                                .cloned()
                                .unwrap_or(name.clone());
                            located.push((db_idx, real));
                        }
                    }
                }
                located.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
                if !located.is_empty() {
                    return Ok((SessionTables::Located(located), "mapping"));
                }
            }
            // 映射表存在但没查到该会话：可能仍是共享表模式，继续向下尝试
        }
        if let Some(col) = &self.talker_column {
            return Ok((
                SessionTables::Shared { tables: self.msg_tables.clone(), talker_column: col.clone() },
                "shared",
            ));
        }
        if self.msg_tables.is_empty() {
            bail!("消息库中未发现消息分表");
        }
        // md5 主路由与旧兜底都未命中：该会话在本机没有对应消息表（如历史已清空、
        // 服务号/资讯号等），属合法的"0 条消息"，返回空定位而非错误。
        Ok((SessionTables::Located(Vec::new()), "none"))
    }

    pub fn db_path(&self, db_idx: usize) -> Option<&Path> {
        self.dbs.get(db_idx).map(PathBuf::as_path)
    }
}

fn resolve_mapping(table: &str, cols: &[String], db_idx: usize) -> Option<MappingTable> {
    let name_col = find_candidate(cols, NAME_COL_CANDIDATES)?;
    let id_col = find_candidate(cols, ID_COL_CANDIDATES);
    Some(MappingTable { db_idx, table: table.to_string(), name_col, id_col })
}

/// 按候选序在列名中查找（大小写不敏感），返回数据库中的原始列名。
pub fn find_candidate(cols: &[String], candidates: &[&str]) -> Option<String> {
    for cand in candidates {
        if let Some(hit) = cols.iter().find(|c| c.eq_ignore_ascii_case(cand)) {
            return Some(hit.clone());
        }
    }
    None
}

pub fn detect_time_column(cols: &[String]) -> Option<String> {
    find_candidate(cols, TIME_COL_CANDIDATES)
}

#[cfg(test)]
mod tests {
    use super::msg_table_hash;

    #[test]
    fn msg_hash_matches_wechat4_md5_scheme() {
        // 微信 4.x 实测：消息表名后缀 = md5(username) 小写十六进制。
        assert_eq!(msg_table_hash("9000000001@chatroom"), "7d6d974f81e451e2c39dc66f5d3690fc");
        assert_eq!(msg_table_hash("wxid_fixture_1"), "03f539eac3ce8b4298e2ed633474ce04");
    }
}

/// SQL 标识符白名单校验（表名/列名无法参数化，拼接前必须过滤）。
pub fn sanitize_ident(name: &str) -> Result<String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'#')
    {
        bail!("非法 SQL 标识符: {name}");
    }
    Ok(name.to_string())
}
