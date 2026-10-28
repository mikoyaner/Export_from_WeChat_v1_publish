//! 消息读写函数组（docs/protocol/01-core-sessions-messages.md §11-§21）。
//!
//! schema 未定档（需 dbKey 用 <微信数据根> 校准，任务 #7），因此采取
//! "SELECT * + 候选列名归一"策略：原样保留数据库列，同时补齐协议要求的 snake_case
//! 输出键（local_id/server_id/create_time/... 与 computed_is_send），最大化兼容 TS
//! 消费方的候选字段回退。

use crate::ffi::*;
use crate::{push_log, with_account};
use crate::query::path_to_json_string;
use anyhow::Result;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use weflow_core::db::{list_tables, table_columns, Account};
use weflow_core::schema::{find_candidate, sanitize_ident, SessionTables};

// 逻辑字段 → 数据库真实列名候选（大小写不敏感，取第一个命中）
const C_LOCAL_ID: &[&str] = &["local_id", "localId", "id", "msg_local_id", "rowid"];
pub(crate) const C_SERVER_ID: &[&str] = &["server_id", "serverId", "svr_id", "svrid", "msg_server_id", "server_msg_id"];
pub(crate) const C_LOCAL_TYPE: &[&str] = &["local_type", "localType", "type", "msg_type", "message_type"];
const C_STATUS: &[&str] = &["status", "msg_status", "local_status"];
pub(crate) const C_CREATE_TIME: &[&str] = &["create_time", "createTime", "msg_create_time", "create_timestamp", "time", "msg_time", "timestamp"];
const C_SORT_SEQ: &[&str] = &["sort_seq", "sortSeq", "sequence", "seq", "msg_seq"];
const C_CONTENT: &[&str] = &["message_content", "messageContent", "content", "str_content", "msg_content"];
const C_COMPRESS: &[&str] = &["compress_content", "compressContent", "compressed_content"];
pub(crate) const C_IS_SEND: &[&str] = &["is_send", "isSend", "send", "is_sender"];
pub(crate) const C_SENDER: &[&str] = &["sender_username", "sender", "str_sender", "sender_id", "talker"];

// 协议输出键 → 候选源列
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

pub(crate) struct TableRef {
    pub(crate) db_path: PathBuf,
    pub(crate) table: String,
    pub(crate) filter: Option<(String, String)>,
}

pub(crate) fn session_table_refs(account: &Account, session_id: &str) -> Result<Vec<TableRef>> {
    let index = account.msg_index()?;
    let mut out = Vec::new();
    match index.session_tables(account, session_id)? {
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
    // 无 Msg_ 表 = 该会话合法地 0 条消息（历史已清空/仅临时内容/公众号等）。
    // 返回空引用列表，让上层得到 [] / count=0，而非 schema mismatch 错误。
    Ok(out)
}

pub(crate) fn ci_get<'a>(map: &'a Map<String, Value>, key_lower: &str) -> Option<&'a Value> {
    map.iter().find(|(k, _)| k.eq_ignore_ascii_case(key_lower)).map(|(_, v)| v)
}

pub(crate) fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

// Name2Id 名称列候选（该库 rowid → user_name 解析真实发送者）
const N2I_NAME: &[&str] = &["user_name", "username", "user_name_", "name"];

fn n2i_cache() -> &'static Mutex<HashMap<String, Arc<HashMap<i64, String>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<HashMap<i64, String>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 该消息库的 Name2Id：rowid → user_name（缓存；失败不缓存以便重试）。
fn name2id_map(account: &Account, db: &Path) -> Option<Arc<HashMap<i64, String>>> {
    let key = db.to_string_lossy().replace('\\', "/").to_lowercase();
    if let Ok(g) = n2i_cache().lock() {
        if let Some(hit) = g.get(&key) {
            return Some(Arc::clone(hit));
        }
    }
    let tables = account.with_conn(db, |c| list_tables(c)).ok()?;
    let table = tables
        .iter()
        .find(|t| t.eq_ignore_ascii_case("Name2Id"))
        .or_else(|| tables.iter().find(|t| t.to_ascii_lowercase().contains("name2id")))?
        .clone();
    let map: HashMap<i64, String> = account
        .with_conn(db, |c| -> Result<HashMap<i64, String>> {
            let cols = table_columns(c, &table)?;
            let name_col =
                find_candidate(&cols, N2I_NAME).ok_or_else(|| anyhow::anyhow!("Name2Id 无名称列"))?;
            let sql = format!(
                "SELECT rowid, \"{}\" FROM \"{}\"",
                sanitize_ident(&name_col)?,
                sanitize_ident(&table)?
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            Ok(rows.flatten().collect())
        })
        .ok()?;
    let arc = Arc::new(map);
    if let Ok(mut g) = n2i_cache().lock() {
        g.insert(key, Arc::clone(&arc));
    }
    Some(arc)
}

/// 归一单行：保留原始列，补齐协议 snake_case 键与 computed_is_send。
pub(crate) fn normalize_row(
    raw: Value,
    db_path: &str,
    table: &str,
    my_id: Option<&str>,
    account: &Account,
    db: &Path,
) -> Value {
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
            continue; // 原始列已叫协议名
        }
        // 按候选找源列
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
    // 真实 schema 常无 sender_username/is_send，方向靠 real_sender_id 指向本库 Name2Id 解析
    let has_sender = matches!(ci_get(&map, "sender_username"), Some(Value::String(s)) if !s.is_empty());
    if !has_sender {
        let rid = ci_get(&map, "real_sender_id")
            .or_else(|| ci_get(&map, "realSenderId"))
            .map(as_i64)
            .unwrap_or(0);
        if rid != 0 {
            if let Some(n2i) = name2id_map(account, db) {
                if let Some(uname) = n2i.get(&rid) {
                    map.insert("sender_username".to_string(), Value::String(uname.clone()));
                }
            }
        }
    }
    // computed_is_send：优先 is_send，否则用 sender==my_id 推断
    let computed = match ci_get(&map, "is_send") {
        Some(v) => v.clone(),
        None => match (ci_get(&map, "sender_username"), my_id) {
            (Some(Value::String(s)), Some(me)) if !me.is_empty() => {
                Value::Number(((s == me) as i64).into())
            }
            _ => Value::Null,
        },
    };
    map.insert("computed_is_send".to_string(), computed);
    map.insert("db_path".to_string(), Value::String(db_path.to_string()));
    map.insert("table_name".to_string(), Value::String(table.to_string()));
    if let Some(name) = PathBuf::from(db_path)
        .file_stem()
        .and_then(|s| s.to_str())
    {
        map.insert("db_name".to_string(), Value::String(name.to_string()));
    }
    Value::Object(map)
}

pub(crate) fn sort_key(v: &Value) -> i128 {
    let map = match v { Value::Object(m) => m, _ => return 0 };
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

/// 取一张表的行（SELECT * + 可选 WHERE + ORDER BY 时间列 + LIMIT）。
pub(crate) fn fetch_table(
    account: &Account,
    t: &TableRef,
    extra_where: Option<(&str, Vec<String>)>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<Value>> {
    let cols = account.columns(&t.db_path, &t.table)?;
    let time_col = find_candidate(&cols, C_SORT_SEQ)
        .or_else(|| find_candidate(&cols, C_CREATE_TIME));
    let safe_table = sanitize_ident(&t.table)?;
    let mut wheres: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    if let Some((col, val)) = &t.filter {
        wheres.push(format!("\"{}\" = ?", sanitize_ident(col)?));
        params.push(val.clone());
    }
    if let Some((col, vals)) = &extra_where {
        wheres.push(col.to_string());
        params.extend(vals.iter().cloned());
    }
    let where_sql = if wheres.is_empty() { String::new() } else { format!(" WHERE {}", wheres.join(" AND ")) };
    let order_sql = match &time_col {
        Some(tc) => format!(
            " ORDER BY \"{}\" {}",
            sanitize_ident(tc)?,
            if ascending { "ASC" } else { "DESC" }
        ),
        None => String::new(),
    };
    let limit_sql = if limit > 0 { format!(" LIMIT {limit}") } else { String::new() };
    let sql = format!("SELECT * FROM \"{safe_table}\"{where_sql}{order_sql}{limit_sql}");
    account.with_conn(&t.db_path, |conn| {
        // 参数按位置绑定（均为文本；SQLite 会做数值亲和比较）
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
        let rows = crate::query::statement_rows_json(&mut stmt, refs.as_slice())?;
        Ok(rows)
    })
}

// ── §11 wcdb_get_messages ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_messages(
    handle: i64,
    username: *const c_char,
    limit: i32,
    offset: i32,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(username);
        let result = (|| -> Result<String> {
            let refs = session_table_refs(account, sid)?;
            let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());
            let lim = limit.max(0) as i64;
            let off = offset.max(0) as i64;
            // 每表最多取 offset+limit 行，合并后按时间降序排序再切片
            let per = off + lim;
            let mut all: Vec<Value> = Vec::new();
            for t in &refs {
                let rows = fetch_table(account, t, None, false, per.max(1))?;
                let dbp = path_to_json_string(&t.db_path);
                for r in rows {
                    all.push(normalize_row(r, &dbp, &t.table, my_id.as_deref(), account, &t.db_path));
                }
            }
            all.sort_by(|a, b| sort_key(b).cmp(&sort_key(a)));
            let sliced: Vec<Value> = all.into_iter().skip(off as usize).take(lim.max(0) as usize).collect();
            Ok(Value::Array(sliced).to_string())
        })();
        emit_json(out_json, result, ERR_SCHEMA_MISMATCH)
    })
}

// ── §12 wcdb_get_messages_by_type ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_messages_by_type(
    handle: i64,
    session_id: *const c_char,
    local_type: i64,
    ascending: i32,
    limit: i32,
    offset: i32,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        let result = (|| -> Result<String> {
            let refs = session_table_refs(account, sid)?;
            let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());
            let asc = ascending != 0;
            let lim = limit.max(0) as i64;
            let off = offset.max(0) as i64;
            let type_val = local_type.to_string();
            let mut all: Vec<Value> = Vec::new();
            for t in &refs {
                let cols = account.columns(&t.db_path, &t.table)?;
                let Some(tc) = find_candidate(&cols, C_LOCAL_TYPE) else { continue };
                let cond = format!("\"{}\" = ?", sanitize_ident(&tc)?);
                // limit=0 表示不限量；为合并分页仍取足量
                let per = if lim > 0 { off + lim } else { 0 };
                let rows = fetch_table(account, t, Some((&cond, vec![type_val.clone()])), asc, per)?;
                let dbp = path_to_json_string(&t.db_path);
                for r in rows {
                    all.push(normalize_row(r, &dbp, &t.table, my_id.as_deref(), account, &t.db_path));
                }
            }
            all.sort_by(|a, b| if asc { sort_key(a).cmp(&sort_key(b)) } else { sort_key(b).cmp(&sort_key(a)) });
            let sliced: Vec<Value> = if lim > 0 {
                all.into_iter().skip(off as usize).take(lim as usize).collect()
            } else {
                all.into_iter().skip(off as usize).collect()
            };
            Ok(Value::Array(sliced).to_string())
        })();
        emit_json(out_json, result, ERR_SCHEMA_MISMATCH)
    })
}

// ── §13 wcdb_get_message_count ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_count(
    handle: i64,
    username: *const c_char,
    out_count: *mut i32,
) -> i32 {
    with_account(handle, |account| {
        if out_count.is_null() {
            return ERR_GENERIC;
        }
        unsafe { *out_count = 0 };
        let sid = cstr(username);
        let result = (|| -> Result<i64> {
            let refs = session_table_refs(account, sid)?;
            let mut total = 0i64;
            for t in &refs {
                let safe_table = sanitize_ident(&t.table)?;
                let (where_sql, param) = match &t.filter {
                    Some((col, val)) => (format!(" WHERE \"{}\" = ?", sanitize_ident(col)?), Some(val.clone())),
                    None => (String::new(), None),
                };
                let sql = format!("SELECT COUNT(*) FROM \"{safe_table}\"{where_sql}");
                let c = account.with_conn(&t.db_path, |conn| -> Result<i64> {
                    let mut stmt = conn.prepare(&sql)?;
                    let n = if let Some(p) = &param {
                        stmt.query_row([p.as_str()], |r| r.get::<_, i64>(0))?
                    } else {
                        stmt.query_row([], |r| r.get::<_, i64>(0))?
                    };
                    Ok(n)
                })?;
                total += c;
            }
            Ok(total)
        })();
        match result {
            Ok(n) => {
                let clamped = n.clamp(0, i32::MAX as i64) as i32;
                unsafe { *out_count = clamped };
                OK
            }
            Err(e) => {
                push_log(&format!("get_message_count: {e}"));
                ERR_SCHEMA_MISMATCH
            }
        }
    })
}

// ── §14/§15 单条查询（by local_id / by server_id） ──

fn find_one(account: &Account, sid: &str, cands: &[&str], value: &str) -> Result<Option<Value>> {
    let refs = session_table_refs(account, sid)?;
    let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());
    for t in &refs {
        let cols = account.columns(&t.db_path, &t.table)?;
        let Some(idc) = find_candidate(&cols, cands) else { continue };
        let cond = format!("\"{}\" = ?", sanitize_ident(&idc)?);
        let rows = fetch_table(account, t, Some((&cond, vec![value.to_string()])), false, 1)?;
        if let Some(r) = rows.into_iter().next() {
            let dbp = path_to_json_string(&t.db_path);
            return Ok(Some(normalize_row(r, &dbp, &t.table, my_id.as_deref(), account, &t.db_path)));
        }
    }
    Ok(None)
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_by_id(
    handle: i64,
    session_id: *const c_char,
    local_id: i32,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        let result = (|| -> Result<String> {
            match find_one(account, sid, C_LOCAL_ID, &local_id.to_string())? {
                Some(row) => Ok(row.to_string()),
                None => Ok("{}".to_string()),
            }
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_by_svrid(
    handle: i64,
    session_id: *const c_char,
    svrid: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        let svr = cstr(svrid).trim().to_string();
        let result = (|| -> Result<String> {
            if svr.is_empty() || svr == "0" {
                return Ok("{}".to_string());
            }
            match find_one(account, sid, C_SERVER_ID, &svr)? {
                Some(row) => Ok(row.to_string()),
                None => Ok("{}".to_string()),
            }
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}
