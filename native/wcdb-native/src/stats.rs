//! 统计与游标函数组（docs/protocol/02-stats-cursors-reports.md）。
//!
//! 覆盖必需符号：wcdb_get_aggregate_stats、wcdb_open_message_cursor、
//! wcdb_fetch_message_batch、wcdb_close_message_cursor。
//!
//! 游标策略：open 时把会话在 [begin,end] 内的全部消息物化为已排序的 Vec<Value>，
//! 存入全局游标表；fetch 顺序切片返回。这与 wcdb_get_messages 的合并-排序模型一致，
//! 且天然满足「跨批次单调推进」「batch_size=1」「空数组+has_more」等协议约束。

use crate::ffi::*;
use crate::messages::{
    as_i64, ci_get, fetch_table, normalize_row, session_table_refs, sort_key, TableRef,
};
use crate::query::path_to_json_string;
use crate::{push_log, with_account};
use anyhow::Result;
use chrono::{DateTime, Local, TimeZone, Utc};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use weflow_core::db::Account;

// ── 全局游标表 ──

struct Cursor {
    rows: Vec<Value>,
    pos: usize,
    batch: usize,
}

fn cursors() -> &'static Mutex<HashMap<i64, Cursor>> {
    static C: OnceLock<Mutex<HashMap<i64, Cursor>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_CURSOR: AtomicI64 = AtomicI64::new(1);

/// 物化会话消息：跨全部分表取行 → 归一 → 时间过滤 → 排序。
fn collect_rows(
    account: &Account,
    sid: &str,
    begin: i64,
    end: i64,
    ascending: bool,
) -> Result<Vec<Value>> {
    let refs: Vec<TableRef> = session_table_refs(account, sid)?;
    let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());
    let mut all: Vec<Value> = Vec::new();
    for t in &refs {
        // limit=0 → 取全表，时间过滤在 Rust 侧做（避免依赖 SQL 时间列探测）
        let rows = fetch_table(account, t, None, ascending, 0)?;
        let dbp = path_to_json_string(&t.db_path);
        for r in rows {
            let row = normalize_row(r, &dbp, &t.table, my_id.as_deref(), account, &t.db_path);
            let ct = row_create_time(&row);
            if begin > 0 && ct < begin {
                continue;
            }
            if end > 0 && ct > end {
                continue;
            }
            all.push(row);
        }
    }
    all.sort_by(|a, b| {
        if ascending {
            sort_key(a).cmp(&sort_key(b))
        } else {
            sort_key(b).cmp(&sort_key(a))
        }
    });
    Ok(all)
}

fn row_create_time(row: &Value) -> i64 {
    match row {
        Value::Object(m) => ci_get(m, "create_time").map(as_i64).unwrap_or(0),
        _ => 0,
    }
}

fn row_is_send(row: &Value, my_id: Option<&str>) -> bool {
    let m = match row {
        Value::Object(m) => m,
        _ => return false,
    };
    if let Some(v) = ci_get(m, "computed_is_send") {
        match v {
            Value::Number(n) => return n.as_i64().unwrap_or(0) != 0,
            Value::Bool(b) => return *b,
            Value::String(s) => {
                if let Ok(n) = s.trim().parse::<i64>() {
                    return n != 0;
                }
            }
            _ => {}
        }
    }
    match (ci_get(m, "sender_username"), my_id) {
        (Some(Value::String(s)), Some(me)) if !me.is_empty() => s == me,
        _ => false,
    }
}

fn row_local_type(row: &Value) -> i64 {
    match row {
        Value::Object(m) => ci_get(m, "local_type").map(as_i64).unwrap_or(1),
        _ => 1,
    }
}

// ── wcdb_open_message_cursor ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_open_message_cursor(
    handle: i64,
    session_id: *const c_char,
    batch_size: i32,
    ascending: i32,
    begin_timestamp: i32,
    end_timestamp: i32,
    out_cursor: *mut i64,
) -> i32 {
    with_account(handle, |account| {
        if out_cursor.is_null() {
            return ERR_GENERIC;
        }
        unsafe { *out_cursor = 0 };
        let sid = cstr(session_id);
        let begin = begin_timestamp.max(0) as i64;
        let end = end_timestamp.max(0) as i64;
        let asc = ascending != 0;
        let batch = (batch_size.max(1)) as usize;

        // 消息库缓存为空 → -3（TS 触发 forceReopen 重试）
        if account.layout.message_dbs.is_empty() {
            push_log("open_message_cursor: no message db");
            return ERR_NO_MESSAGE_DB;
        }

        let rows = match collect_rows(account, sid, begin, end, asc) {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                push_log(&format!("open_message_cursor: {msg}"));
                // schema mismatch（无消息表/无法路由）→ -7
                return if msg.contains("schema mismatch") || msg.contains("无消息表") || msg.contains("消息分表") {
                    ERR_SCHEMA_MISMATCH
                } else {
                    ERR_GENERIC
                };
            }
        };

        let id = NEXT_CURSOR.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = cursors().lock() {
            map.insert(id, Cursor { rows, pos: 0, batch });
            unsafe { *out_cursor = id };
            OK
        } else {
            ERR_GENERIC
        }
    })
}

// ── wcdb_fetch_message_batch ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_fetch_message_batch(
    handle: i64,
    cursor: i64,
    out_json: *mut *mut c_void,
    out_has_more: *mut i32,
) -> i32 {
    // 游标表是全局的；handle 仅用于 panic 防护与一致性校验（不强制匹配）。
    let _ = handle;
    guard(|| {
        if !out_has_more.is_null() {
            unsafe { *out_has_more = 0 };
        }
        let mut map = match cursors().lock() {
            Ok(m) => m,
            Err(_) => return ERR_GENERIC,
        };
        let c = match map.get_mut(&cursor) {
            Some(c) => c,
            None => {
                push_log(&format!("fetch_message_batch: unknown cursor {cursor}"));
                return ERR_GENERIC;
            }
        };
        let start = c.pos;
        let end = (start + c.batch).min(c.rows.len());
        let slice: Vec<Value> = c.rows[start..end].to_vec();
        c.pos = end;
        let has_more = if c.pos < c.rows.len() { 1 } else { 0 };
        drop(map);
        write_out(out_json, &Value::Array(slice).to_string());
        if !out_has_more.is_null() {
            unsafe { *out_has_more = has_more };
        }
        OK
    })
}

// ── wcdb_close_message_cursor ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_close_message_cursor(handle: i64, cursor: i64) -> i32 {
    let _ = handle;
    guard(|| {
        // 容忍未知/重复 close：一律返回 OK，绝不崩溃。
        if let Ok(mut map) = cursors().lock() {
            map.remove(&cursor);
        }
        OK
    })
}

/// shutdown 时清空全部游标。
pub(crate) fn clear_cursors() {
    if let Ok(mut map) = cursors().lock() {
        map.clear();
    }
}

// ── wcdb_get_aggregate_stats ──

/// 解析 session_ids_json：既接受字符串数组也接受数字数组（协议双形态）。
fn parse_session_ids(json_str: &str) -> Vec<String> {
    match serde_json::from_str::<Value>(json_str) {
        Ok(Value::Array(arr)) => arr
            .into_iter()
            .map(|v| match v {
                Value::String(s) => s,
                Value::Number(n) => n.to_string(),
                other => other.to_string(),
            })
            .filter(|s| !s.trim().is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn local_parts(secs: i64) -> (u32, u32, String, String) {
    // 返回 (hour 0-23, weekday 0=周日..6=周六, "YYYY-MM", "YYYY-MM-DD")，按本地时区。
    let dt: DateTime<Local> = match Utc.timestamp_opt(secs, 0).single() {
        Some(d) => d.with_timezone(&Local),
        None => Utc.timestamp_opt(0, 0).unwrap().with_timezone(&Local),
    };
    let hour = dt.format("%H").to_string().parse::<u32>().unwrap_or(0);
    // chrono weekday: Mon=0..Sun=6；协议要 SQLite strftime('%w'): Sun=0..Sat=6
    let wd = dt.format("%w").to_string().parse::<u32>().unwrap_or(0);
    let month = dt.format("%Y-%m").to_string();
    let day = dt.format("%Y-%m-%d").to_string();
    (hour, wd, month, day)
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_aggregate_stats(
    handle: i64,
    session_ids_json: *const c_char,
    begin_timestamp: i32,
    end_timestamp: i32,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let raw = cstr(session_ids_json);
        let begin = begin_timestamp.max(0) as i64;
        let end = end_timestamp.max(0) as i64;
        let result = (|| -> Result<String> {
            let sids = parse_session_ids(raw);
            let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());

            let mut total: i64 = 0;
            let mut sent: i64 = 0;
            let mut received: i64 = 0;
            let mut first_time: i64 = 0;
            let mut last_time: i64 = 0;
            let mut type_counts: HashMap<String, i64> = HashMap::new();
            let mut hourly = [0i64; 24];
            let mut weekday: HashMap<String, i64> = HashMap::new();
            let mut daily: HashMap<String, i64> = HashMap::new();
            let mut sent_daily: HashMap<String, i64> = HashMap::new();
            let mut monthly: HashMap<String, i64> = HashMap::new();
            let mut sessions: Map<String, Value> = Map::new();
            let id_map: Map<String, Value> = Map::new();

            for sid in &sids {
                let rows = match collect_rows(account, sid, begin, end, true) {
                    Ok(r) => r,
                    Err(_) => continue, // 单会话无表/路由失败按空处理，不整体失败
                };
                let mut s_total: i64 = 0;
                let mut s_sent: i64 = 0;
                let mut s_recv: i64 = 0;
                let mut s_last: i64 = 0;
                for row in &rows {
                    let ct = row_create_time(row);
                    if ct <= 0 {
                        continue;
                    }
                    let is_send = row_is_send(row, my_id.as_deref());
                    total += 1;
                    s_total += 1;
                    if is_send {
                        sent += 1;
                        s_sent += 1;
                    } else {
                        received += 1;
                        s_recv += 1;
                    }
                    if first_time == 0 || ct < first_time {
                        first_time = ct;
                    }
                    if ct > last_time {
                        last_time = ct;
                    }
                    if ct > s_last {
                        s_last = ct;
                    }
                    *type_counts.entry(row_local_type(row).to_string()).or_insert(0) += 1;
                    let (h, wd, mk, dk) = local_parts(ct);
                    hourly[h as usize] += 1;
                    *weekday.entry(wd.to_string()).or_insert(0) += 1;
                    *daily.entry(dk.clone()).or_insert(0) += 1;
                    *monthly.entry(mk).or_insert(0) += 1;
                    if is_send {
                        *sent_daily.entry(dk).or_insert(0) += 1;
                    }
                }
                if s_total > 0 {
                    sessions.insert(
                        sid.clone(),
                        json!({
                            "total": s_total,
                            "sent": s_sent,
                            "received": s_recv,
                            "lastTime": s_last,
                        }),
                    );
                }
            }

            let out = json!({
                "total": total,
                "sent": sent,
                "received": received,
                "firstTime": first_time,
                "lastTime": last_time,
                "typeCounts": type_counts,
                "hourly": hourly,
                "weekday": weekday,
                "daily": daily,
                "sentDaily": sent_daily,
                "monthly": monthly,
                "sessions": Value::Object(sessions),
                "idMap": Value::Object(id_map),
            });
            Ok(out.to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}
