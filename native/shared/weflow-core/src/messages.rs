//! 会话消息读取（跨分表合并 + 归一），供 wcdb.dll 与 welive-export 共用。

use crate::db::Account;
use crate::schema::{find_candidate, sanitize_ident, SessionTables};
use anyhow::Result;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

const C_LOCAL_ID: &[&str] = &["local_id", "localId", "id", "msg_local_id", "rowid"];
const C_SERVER_ID: &[&str] = &["server_id", "serverId", "svr_id", "svrid", "msg_server_id", "server_msg_id"];
const C_LOCAL_TYPE: &[&str] = &["local_type", "localType", "type", "msg_type", "message_type"];
const C_STATUS: &[&str] = &["status", "msg_status", "local_status"];
const C_CREATE_TIME: &[&str] = &["create_time", "createTime", "msg_create_time", "create_timestamp", "time", "msg_time", "timestamp"];
const C_SORT_SEQ: &[&str] = &["sort_seq", "sortSeq", "sequence", "seq", "msg_seq"];
const C_CONTENT: &[&str] = &["message_content", "messageContent", "content", "str_content", "msg_content"];
const C_COMPRESS: &[&str] = &["compress_content", "compressContent", "compressed_content"];
const C_IS_SEND: &[&str] = &["is_send", "isSend", "send", "is_sender"];
const C_SENDER: &[&str] = &["sender_username", "sender", "str_sender", "sender_id", "talker"];

const FIELD_MAP: &[(&str, &[&str])] = &[
    ("local_id", C_LOCAL_ID),
    ("server_id", C_SERVER_ID),
    ("local_type", C_LOCAL_TYPE),
    ("status", C_STATUS),
    ("create_time", C_CREATE_TIME),
    ("sort_seq", C_SORT_SEQ),
    ("message_content", C_CONTENT),
    ("compress_content", C_COMPRESS),
    ("is_send", C_IS_SEND),
    ("sender_username", C_SENDER),
];

struct TableRef {
    db_path: PathBuf,
    table: String,
    filter: Option<(String, String)>,
}

fn ci_get<'a>(map: &'a Map<String, Value>, key_lower: &str) -> Option<&'a Value> {
    map.iter().find(|(k, _)| k.eq_ignore_ascii_case(key_lower)).map(|(_, v)| v)
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

fn value_ref_to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Number(i.into()),
        ValueRef::Real(f) => serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(hex::encode(b)),
    }
}

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")
}

fn normalize_row(raw: Value, db_path: &str, table: &str, my_id: Option<&str>) -> Value {
    let mut map = match raw {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };
    let keys: Vec<String> = map.keys().cloned().collect();
    for (proto, cands) in FIELD_MAP {
        if ci_get(&map, proto).is_some() {
            continue;
        }
        let mut found: Option<Value> = None;
        for cand in *cands {
            if let Some(k) = keys.iter().find(|k| k.eq_ignore_ascii_case(cand)) {
                if let Some(v) = map.get(k) {
                    found = Some(v.clone());
                    break;
                }
            }
        }
        if let Some(v) = found {
            map.insert(proto.to_string(), v);
        }
    }
    let computed = match ci_get(&map, "is_send") {
        Some(v) => v.clone(),
        None => match (ci_get(&map, "sender_username"), my_id) {
            (Some(Value::String(s)), Some(me)) if !me.is_empty() => Value::Number(((s == me) as i64).into()),
            _ => Value::Null,
        },
    };
    map.insert("computed_is_send".to_string(), computed);
    map.insert("db_path".to_string(), Value::String(db_path.to_string()));
    map.insert("table_name".to_string(), Value::String(table.to_string()));
    if let Some(name) = PathBuf::from(db_path).file_stem().and_then(|s| s.to_str()) {
        map.insert("db_name".to_string(), Value::String(name.to_string()));
    }
    Value::Object(map)
}

fn sort_key(v: &Value) -> i128 {
    let map = match v {
        Value::Object(m) => m,
        _ => return 0,
    };
    if let Some(sq) = ci_get(map, "sort_seq") {
        let n = as_i64(sq);
        if n != 0 {
            return n as i128;
        }
    }
    if let Some(ct) = ci_get(map, "create_time") {
        return as_i64(ct) as i128 * 1000;
    }
    0
}

fn session_table_refs(account: &Account, session_id: &str) -> Result<Vec<TableRef>> {
    session_table_refs_routed(account, session_id).map(|(v, _)| v)
}

/// 同 `session_table_refs`，把命中的路由（md5/mapping/shared/none）一起带出来。
fn session_table_refs_routed(account: &Account, session_id: &str) -> Result<(Vec<TableRef>, &'static str)> {
    let index = account.msg_index()?;
    let mut out = Vec::new();
    let (tables, route) = index.session_tables_routed(account, session_id)?;
    match tables {
        SessionTables::Located(list) => {
            for (db_idx, table) in list {
                if let Some(db) = index.db_path(db_idx) {
                    out.push(TableRef { db_path: db.to_path_buf(), table, filter: None });
                }
            }
        }
        SessionTables::Shared { tables, talker_column } => {
            for (db_idx, table) in tables {
                if let Some(db) = index.db_path(db_idx) {
                    out.push(TableRef {
                        db_path: db.to_path_buf(),
                        table,
                        filter: Some((talker_column.clone(), session_id.to_string())),
                    });
                }
            }
        }
    }
    Ok((out, route))
}

/// 单表游标：每条原始行立刻交给 `on_row`，只在内存里保留当前一行。
/// `limit` 仅在该表**有时间列**时才可安全下推（否则 LIMIT 取到的是任意前 n 行，不是最近 n 行）。
fn stream_table<F>(account: &Account, t: &TableRef, ascending: bool, limit: Option<u64>, mut on_row: F) -> Result<u64>
where
    F: FnMut(Value) -> Result<()>,
{
    let cols = account.columns(&t.db_path, &t.table)?;
    let time_col = find_candidate(&cols, C_SORT_SEQ).or_else(|| find_candidate(&cols, C_CREATE_TIME));
    let safe_table = sanitize_ident(&t.table)?;
    let mut wheres: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    if let Some((col, val)) = &t.filter {
        wheres.push(format!("\"{}\" = ?", sanitize_ident(col)?));
        params.push(val.clone());
    }
    let where_sql = if wheres.is_empty() { String::new() } else { format!(" WHERE {}", wheres.join(" AND ")) };
    let order_sql = match &time_col {
        Some(tc) => format!(" ORDER BY \"{}\" {}", sanitize_ident(tc)?, if ascending { "ASC" } else { "DESC" }),
        None => String::new(),
    };
    // limit 是 u64，格式化出来只可能是数字，不参与注入
    let limit_sql = match (limit, &time_col) {
        (Some(l), Some(_)) => format!(" LIMIT {l}"),
        _ => String::new(),
    };
    let sql = format!("SELECT * FROM \"{safe_table}\"{where_sql}{order_sql}{limit_sql}");
    account.with_conn(&t.db_path, move |conn: &Connection| {
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = stmt.query(refs.as_slice())?;
        let mut count = 0u64;
        while let Some(row) = rows.next()? {
            let mut obj = Map::with_capacity(names.len());
            for (idx, name) in names.iter().enumerate() {
                obj.insert(name.clone(), value_ref_to_json(row.get_ref(idx)?));
            }
            on_row(Value::Object(obj))?;
            count += 1;
        }
        Ok(count)
    })
}

/// 流式读取会话在 [begin,end]（秒，0=不限）内的消息，逐行回调，内存与整会话大小无关。
///
/// 顺序为「分库顺序 + 库内 sort_seq/create_time 顺序」，**不再做跨分库全局排序**。
/// 这不影响最终导出：WeFlow 读 raw JSONL 后总会按 create_time→local_id 重排
/// （ExportContext.ts::collectMessagesFromWeliveRaw 末尾的 rows.sort）。
/// 返回真正交给 `on_row` 的行数（已按 [begin,end] 过滤）。
pub fn stream_session_message_rows<F>(
    account: &Account,
    session_id: &str,
    begin: i64,
    end: i64,
    ascending: bool,
    mut on_row: F,
) -> Result<u64>
where
    F: FnMut(Value) -> Result<()>,
{
    let refs = session_table_refs(account, session_id)?;
    let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());
    let mut emitted = 0u64;
    for t in &refs {
        let dbp = path_str(&t.db_path);
        let table = t.table.clone();
        let mut kept = 0u64;
        stream_table(account, t, ascending, None, |raw| {
            let row = normalize_row(raw, &dbp, &table, my_id.as_deref());
            let ct = match &row {
                Value::Object(m) => ci_get(m, "create_time").map(as_i64).unwrap_or(0),
                _ => 0,
            };
            if (begin > 0 && ct < begin) || (end > 0 && ct > end) {
                return Ok(());
            }
            on_row(row)?;
            kept += 1;
            Ok(())
        })?;
        emitted += kept;
    }
    Ok(emitted)
}

/// 读取会话在 [begin,end]（秒，0=不限）内的全部消息，归一并按时间排序。
/// 整会话进内存，只适合小批量/校验用途；导出大流请用 [`stream_session_message_rows`]。
pub fn session_message_rows(
    account: &Account,
    session_id: &str,
    begin: i64,
    end: i64,
    ascending: bool,
) -> Result<Vec<Value>> {
    let mut all: Vec<Value> = Vec::new();
    stream_session_message_rows(account, session_id, begin, end, ascending, |row| {
        all.push(row);
        Ok(())
    })?;
    all.sort_by(|a, b| {
        if ascending {
            sort_key(a).cmp(&sort_key(b))
        } else {
            sort_key(b).cmp(&sort_key(a))
        }
    });
    Ok(all)
}

fn table_has_time(account: &Account, t: &TableRef) -> bool {
    match account.columns(&t.db_path, &t.table) {
        Ok(cols) => {
            find_candidate(&cols, C_SORT_SEQ).is_some() || find_candidate(&cols, C_CREATE_TIME).is_some()
        }
        Err(_) => false,
    }
}

/// 只取会话**最近 n 条**（新→旧）。
///
/// 每个分库各自 `ORDER BY 时间 DESC LIMIT n` 取回，再合并排序截断：全局最近 n 条必然落在
/// 各库最近 n 条里，所以结果与「整会话读进内存再截」一致，但内存与耗时的量级从
/// 「整会话行数」（本机实测单会话可到数万行）降到「库数 × n」。
/// 表没有时间列时（异常结构）LIMIT 下推不安全 ⇒ 退回全量读，宁可慢也不能取错行。
pub fn session_message_rows_recent(account: &Account, session_id: &str, n: usize) -> Result<Vec<Value>> {
    session_message_rows_recent_routed(account, session_id, n).map(|(v, _, _)| v)
}

/// 同 `session_message_rows_recent`，额外返回：
/// - `route`：会话→消息表命中的路由（md5/mapping/shared/none）
/// - `time_fallback`：是否因为分表缺时间列而退回"全量读再截断"（慢路）
pub fn session_message_rows_recent_routed(
    account: &Account,
    session_id: &str,
    n: usize,
) -> Result<(Vec<Value>, &'static str, bool)> {
    if n == 0 {
        return Ok((Vec::new(), "none", false));
    }
    let (refs, route) = session_table_refs_routed(account, session_id)?;
    if refs.iter().any(|t| !table_has_time(account, t)) {
        let all = session_message_rows(account, session_id, 0, 0, false)?;
        return Ok((all.into_iter().take(n).collect(), route, true));
    }
    let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());
    let cap = n as u64;
    let mut all: Vec<Value> = Vec::new();
    for t in &refs {
        let dbp = path_str(&t.db_path);
        let table = t.table.clone();
        stream_table(account, t, false, Some(cap), |raw| {
            all.push(normalize_row(raw, &dbp, &table, my_id.as_deref()));
            Ok(())
        })?;
    }
    all.sort_by(|a, b| sort_key(b).cmp(&sort_key(a)));
    all.truncate(n);
    Ok((all, route, false))
}
