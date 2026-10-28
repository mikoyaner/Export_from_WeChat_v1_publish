//! 通用查询层：SQL 值→JSON 归一、任意 SQL 执行、库路径解析。
//!
//! 协议依据 `docs/protocol/04-tables-misc.md` §wcdb_exec_query：
//! 返回"行对象数组"（非"列名+行"两段式）；文本→string、整数/浮点→number、
//! NULL→null、BLOB→hex 字符串（消费方 decodeVoiceBlob 接受 hex/base64）。
//! 空结果集必须是 `[]`。

use anyhow::{bail, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Params, Statement};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use weflow_core::db::Account;

/// 单个 SQLite 值 → JSON。BLOB 走 hex（协议约定消费方按 hex/base64 解码）。
pub fn value_ref_to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Number(i.into()),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(hex::encode(b)),
    }
}

/// 执行已 prepare 的语句，返回行对象数组（key = SQL 输出列名/别名，原样保留）。
pub fn statement_rows_json(stmt: &mut Statement, params: impl Params) -> Result<Vec<Value>> {
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut obj = Map::with_capacity(names.len());
        for (idx, name) in names.iter().enumerate() {
            obj.insert(name.clone(), value_ref_to_json(row.get_ref(idx)?));
        }
        out.push(Value::Object(obj));
    }
    Ok(out)
}

/// 无参查询便捷封装。
pub fn query_rows_json(conn: &Connection, sql: &str) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(sql)?;
    statement_rows_json(&mut stmt, [])
}

/// 按 kind + 显式路径解析目标库文件；路径为空时回退到 handle 账号目录下该 kind 的默认库
/// （协议 04：TS 找不到文件会传空串，DLL 需自行定位）。
pub fn resolve_db_path(account: &Account, kind: &str, path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    if !trimmed.is_empty() {
        let p = PathBuf::from(trimmed);
        if p.is_file() {
            return Ok(p);
        }
        // 显式路径给了但文件不存在：仍返回它，让 open 报"数据库无法打开"
        return Ok(p);
    }
    let layout = &account.layout;
    let fallback = match kind.trim().to_ascii_lowercase().as_str() {
        "session" => Some(layout.session_db.clone()),
        "contact" => layout.contact_db.clone(),
        "emoticon" => layout.emoticon_db.clone(),
        "sns" => layout.sns_db.clone(),
        "hardlink" => layout.hardlink_db.clone(),
        "message" => layout.message_dbs.first().cloned(),
        "media" => layout.media_dbs.first().cloned(),
        _ => None,
    };
    match fallback {
        Some(p) => Ok(p),
        None => bail!("无法解析 kind={kind} 的默认库路径"),
    }
}

/// 通用 SQL 逃生门：解析库路径 → 执行 → 行对象数组 JSON 字符串。
pub fn exec_query_json(account: &Account, kind: &str, path: &str, sql: &str) -> Result<String> {
    let db = resolve_db_path(account, kind, path)?;
    let rows = account.with_conn(&db, |conn| query_rows_json(conn, sql))?;
    Ok(Value::Array(rows).to_string())
}

/// 便捷：把库路径标准化为正斜杠绝对路径字符串（消费方直接 basename / 文件存在性检查）。
pub fn path_to_json_string(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}
