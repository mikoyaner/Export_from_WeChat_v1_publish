//! 表 / 快照 / 库列表 / 通用查询函数组（docs/protocol/04-tables-misc.md）。
//!
//! 覆盖：list_message_dbs、list_media_dbs、get_logs、exec_query、list_tables、
//! get_table_schema、message_table_columns、message_table_time_range、message_meta、
//! message_tables、message_table_stats、message_dates、media_schema_summary、
//! get_db_status、export/import_table_snapshot(_with_schema)。
//! 监控管道与 cloud_* 见 misc.rs。

use crate::ffi::*;
use crate::{take_logs_json, with_account};
use anyhow::{bail, Result};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Map, Value};
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use weflow_core::db::Account;
use weflow_core::schema::{detect_time_column, sanitize_ident, SessionTables};

use crate::query::{
    exec_query_json, path_to_json_string, query_rows_json, resolve_db_path,
};

// ── 会话消息表路由辅助 ──

struct TableRef {
    db_path: PathBuf,
    table: String,
    /// 共享表模式下的 (talker 列, sessionId) 过滤；专属分表为 None。
    filter: Option<(String, String)>,
}

fn session_table_refs(account: &Account, session_id: &str) -> Result<Vec<TableRef>> {
    let index = account.msg_index()?;
    let located = index.session_tables(account, session_id)?;
    let mut out = Vec::new();
    match located {
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
    Ok(out)
}

/// 组装 `WHERE "col" = ?` 片段（列名经白名单校验）。
fn where_clause(t: &TableRef) -> Result<(String, Option<String>)> {
    match &t.filter {
        Some((col, val)) => Ok((format!(" WHERE \"{}\" = ?", sanitize_ident(col)?), Some(val.clone()))),
        None => Ok((String::new(), None)),
    }
}

fn detect_time_col(account: &Account, t: &TableRef) -> Option<String> {
    let cols = account.columns(&t.db_path, &t.table).ok()?;
    detect_time_column(&cols)
}

// ── 库列表 ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_list_message_dbs(handle: i64, out_json: *mut *mut c_void) -> i32 {
    with_account(handle, |account| {
        let paths: Vec<Value> = account
            .layout
            .message_dbs
            .iter()
            .map(|p| Value::String(path_to_json_string(p)))
            .collect();
        emit_json(out_json, Ok(Value::Array(paths).to_string()), ERR_GENERIC)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_list_media_dbs(handle: i64, out_json: *mut *mut c_void) -> i32 {
    with_account(handle, |account| {
        let paths: Vec<Value> = account
            .layout
            .media_dbs
            .iter()
            .map(|p| Value::String(path_to_json_string(p)))
            .collect();
        emit_json(out_json, Ok(Value::Array(paths).to_string()), ERR_GENERIC)
    })
}

// ── 日志（无 handle） ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_logs(out_json: *mut *mut c_void) -> i32 {
    guard(|| {
        write_out(out_json, &take_logs_json());
        OK
    })
}

// ── 通用 SQL 逃生门 ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_exec_query(
    handle: i64,
    kind: *const c_char,
    path: *const c_char,
    sql: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let kind = cstr(kind);
        let path = cstr(path);
        let sql = cstr(sql);
        emit_json(out_json, exec_query_json(account, kind, path, sql), ERR_GENERIC)
    })
}

// ── 表清单 / 建表 SQL ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_list_tables(
    handle: i64,
    kind: *const c_char,
    db_path: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let kind = cstr(kind);
        let db_path = cstr(db_path);
        let result = (|| -> Result<String> {
            let db = resolve_db_path(account, kind, db_path)?;
            let tables = account.with_conn(&db, |conn| weflow_core::db::list_tables(conn))?;
            Ok(Value::Array(tables.into_iter().map(Value::String).collect()).to_string())
        })();
        // kind 不适用/库不存在时返回空数组也算成功（备份流程跳过空表库）
        match result {
            Ok(s) => emit_json(out_json, Ok(s), ERR_GENERIC),
            Err(_) => emit_json(out_json, Ok("[]".to_string()), ERR_GENERIC),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_table_schema(
    handle: i64,
    kind: *const c_char,
    db_path: *const c_char,
    table_name: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let kind = cstr(kind);
        let db_path = cstr(db_path);
        let table = cstr(table_name);
        // 双通道：rc 与 data.success 都判失败，失败也写 outJson
        let result = (|| -> Result<String> {
            let db = resolve_db_path(account, kind, db_path)?;
            let schema = account.with_conn(&db, |conn| {
                let mut stmt = conn
                    .prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name=?")?;
                let sql: Option<String> = stmt
                    .query_map([table], |r| r.get::<_, Option<String>>(0))?
                    .next()
                    .transpose()?
                    .flatten();
                Ok(sql.unwrap_or_default())
            })?;
            Ok(json!({ "success": true, "schema": schema }).to_string())
        })();
        match result {
            Ok(s) => {
                write_out(out_json, &s);
                OK
            }
            Err(e) => {
                write_out(out_json, &json!({ "success": false, "error": e.to_string() }).to_string());
                OK
            }
        }
    })
}

// ── 消息表列 / 时间范围 / 元数据 ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_table_columns(
    handle: i64,
    db_path: *const c_char,
    table_name: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let db_path = cstr(db_path);
        let table = cstr(table_name);
        let result = (|| -> Result<String> {
            let db = PathBuf::from(db_path);
            let cols = account.columns(&db, table)?;
            Ok(Value::Array(cols.iter().map(|c| Value::String(c.clone())).collect()).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_table_time_range(
    handle: i64,
    db_path: *const c_char,
    table_name: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let db_path = cstr(db_path);
        let table = cstr(table_name);
        let result = (|| -> Result<String> {
            let db = PathBuf::from(db_path);
            let safe_table = sanitize_ident(table)?;
            let cols = account.columns(&db, table)?;
            let Some(tc) = detect_time_column(&cols) else {
                return Ok(json!({ "column": "", "first_ts": 0, "last_ts": 0 }).to_string());
            };
            let safe_tc = sanitize_ident(&tc)?;
            let sql = format!("SELECT MIN(\"{safe_tc}\"), MAX(\"{safe_tc}\") FROM \"{safe_table}\"");
            let (first, last) = account.with_conn(&db, |conn| -> Result<(Option<i64>, Option<i64>)> {
                Ok(conn.query_row(&sql, [], |r| {
                    Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?))
                })?)
            })?;
            Ok(json!({
                "column": tc,
                "first_ts": first.unwrap_or(0),
                "last_ts": last.unwrap_or(0),
            })
            .to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_meta(
    handle: i64,
    db_path: *const c_char,
    table_name: *const c_char,
    limit: i32,
    offset: i32,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let db_path = cstr(db_path);
        let table = cstr(table_name);
        let result = (|| -> Result<String> {
            let db = PathBuf::from(db_path);
            let safe_table = sanitize_ident(table)?;
            let lim = limit.max(0);
            let off = offset.max(0);
            let sql = format!("SELECT * FROM \"{safe_table}\" LIMIT {lim} OFFSET {off}");
            let rows = account.with_conn(&db, |conn| query_rows_json(conn, &sql))?;
            Ok(Value::Array(rows).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}

// ── 会话消息表清单 / 统计 / 日期 ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_tables(
    handle: i64,
    session_id: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        let result = (|| -> Result<String> {
            let refs = session_table_refs(account, sid)?;
            let arr: Vec<Value> = refs
                .iter()
                .map(|t| {
                    json!({
                        "db_path": path_to_json_string(&t.db_path),
                        "table_name": t.table,
                    })
                })
                .collect();
            Ok(Value::Array(arr).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_table_stats(
    handle: i64,
    session_id: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        let result = (|| -> Result<String> {
            let refs = session_table_refs(account, sid)?;
            let mut arr: Vec<Value> = Vec::with_capacity(refs.len());
            for t in &refs {
                let (where_sql, param) = where_clause(t)?;
                let safe_table = sanitize_ident(&t.table)?;
                let tc = detect_time_col(account, t);
                let (count, first, last) = account.with_conn(&t.db_path, |conn| -> Result<(i64, i64, i64)> {
                    if let Some(tc) = &tc {
                        let safe_tc = sanitize_ident(tc)?;
                        let sql = format!(
                            "SELECT COUNT(*), MIN(\"{safe_tc}\"), MAX(\"{safe_tc}\") FROM \"{safe_table}\"{where_sql}"
                        );
                        let mut stmt = conn.prepare(&sql)?;
                        let row = if let Some(p) = &param {
                            stmt.query_row([p.as_str()], |r| {
                                Ok((
                                    r.get::<_, i64>(0)?,
                                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                                ))
                            })?
                        } else {
                            stmt.query_row([], |r| {
                                Ok((
                                    r.get::<_, i64>(0)?,
                                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                                ))
                            })?
                        };
                        Ok(row)
                    } else {
                        let sql = format!("SELECT COUNT(*) FROM \"{safe_table}\"{where_sql}");
                        let mut stmt = conn.prepare(&sql)?;
                        let count = if let Some(p) = &param {
                            stmt.query_row([p.as_str()], |r| r.get::<_, i64>(0))?
                        } else {
                            stmt.query_row([], |r| r.get::<_, i64>(0))?
                        };
                        Ok((count, 0, 0))
                    }
                })?;
                arr.push(json!({
                    "db_path": path_to_json_string(&t.db_path),
                    "table_name": t.table,
                    "count": count,
                    "first_timestamp": first,
                    "last_timestamp": last,
                    // messagePushService 只读 last_time（防撤回候选表过滤），必须同时输出
                    "last_time": last,
                }));
            }
            Ok(Value::Array(arr).to_string())
        })();
        emit_json(out_json, result, ERR_SCHEMA_MISMATCH)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_dates(
    handle: i64,
    session_id: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        // 特殊：任何失败都按"无消息"处理，返回 success:true + []（协议 04）
        let result = (|| -> Result<Vec<String>> {
            let refs = session_table_refs(account, sid)?;
            let mut dates: Vec<String> = Vec::new();
            for t in &refs {
                let Some(tc) = detect_time_col(account, t) else { continue };
                let safe_tc = sanitize_ident(&tc)?;
                let safe_table = sanitize_ident(&t.table)?;
                let (where_sql, param) = where_clause(t)?;
                let sql = format!(
                    "SELECT DISTINCT strftime('%Y-%m-%d', \"{safe_tc}\", 'unixepoch', 'localtime') \
                     FROM \"{safe_table}\"{where_sql}"
                );
                let mut got = account.with_conn(&t.db_path, |conn| -> Result<Vec<String>> {
                    let mut stmt = conn.prepare(&sql)?;
                    let rows = if let Some(p) = &param {
                        stmt.query_map([p.as_str()], |r| r.get::<_, Option<String>>(0))?
                            .flatten()
                            .filter_map(|s| s)
                            .collect()
                    } else {
                        stmt.query_map([], |r| r.get::<_, Option<String>>(0))?
                            .flatten()
                            .filter_map(|s| s)
                            .collect()
                    };
                    Ok(rows)
                })?;
                dates.append(&mut got);
            }
            dates.sort();
            dates.dedup();
            Ok(dates)
        })();
        let dates = result.unwrap_or_default();
        write_out(out_json, &Value::Array(dates.into_iter().map(Value::String).collect()).to_string());
        OK
    })
}

// ── 媒体库结构摘要 / DB 状态 ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_media_schema_summary(
    handle: i64,
    db_path: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let db_path = cstr(db_path);
        let result = (|| -> Result<String> {
            let db = resolve_db_path(account, "media", db_path)?;
            let tables = account.with_conn(&db, |conn| weflow_core::db::list_tables(conn))?;
            let mut obj = Map::new();
            for t in &tables {
                if let Ok(cols) = account.columns(&db, t) {
                    obj.insert(
                        t.clone(),
                        Value::Array(cols.iter().map(|c| Value::String(c.clone())).collect()),
                    );
                }
            }
            Ok(Value::Object(obj).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_db_status(handle: i64, out_json: *mut *mut c_void) -> i32 {
    with_account(handle, |account| {
        // 内容自定（TS 只写进诊断日志），给出账号库可打开性摘要
        let status = json!({
            "session_db": path_to_json_string(&account.layout.session_db),
            "message_dbs": account.layout.message_dbs.len(),
            "media_dbs": account.layout.media_dbs.len(),
            "has_contact": account.layout.contact_db.is_some(),
            "my_account_id": account.my_account_id.lock().ok().and_then(|g| g.clone()),
        });
        emit_json(out_json, Ok(status.to_string()), ERR_GENERIC)
    })
}

// ── 表快照 导出 / 导入 ──

const SNAP_FORMAT: &str = "wfsnap/1";

#[no_mangle]
pub unsafe extern "C" fn wcdb_export_table_snapshot(
    handle: i64,
    kind: *const c_char,
    db_path: *const c_char,
    table_name: *const c_char,
    output_path: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let kind = cstr(kind);
        let db_path = cstr(db_path);
        let table = cstr(table_name);
        let output = cstr(output_path);
        let result = export_snapshot(account, kind, db_path, table, output);
        match result {
            Ok(v) => {
                write_out(out_json, &v.to_string());
                OK
            }
            Err(e) => {
                write_out(out_json, &json!({ "success": false, "error": e.to_string() }).to_string());
                OK
            }
        }
    })
}

fn export_snapshot(
    account: &Account,
    kind: &str,
    db_path: &str,
    table: &str,
    output: &str,
) -> Result<Value> {
    let db = resolve_db_path(account, kind, db_path)?;
    let safe_table = sanitize_ident(table)?;
    let columns = account.columns(&db, table)?;
    let ncols = columns.len();
    let (schema, rows) = account.with_conn(&db, |conn| -> Result<(String, Vec<Vec<Value>>)> {
        let schema = {
            let mut stmt = conn.prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name=?")?;
            let sql: Option<String> = stmt
                .query_map([table], |r| r.get::<_, Option<String>>(0))?
                .next()
                .transpose()?
                .flatten();
            sql.unwrap_or_default()
        };
        let mut stmt = conn.prepare(&format!("SELECT * FROM \"{safe_table}\""))?;
        let mut rows = Vec::new();
        let mut raw = stmt.query([])?;
        while let Some(row) = raw.next()? {
            let mut vals = Vec::with_capacity(ncols);
            for i in 0..ncols {
                vals.push(sql_value_to_snap(row.get_ref(i)?));
            }
            rows.push(vals);
        }
        Ok((schema, rows))
    })?;
    let row_count = rows.len();
    let col_count = columns.len();
    let rows_value = Value::Array(rows.into_iter().map(Value::Array).collect());
    let cols_value = Value::Array(columns.iter().map(|c| Value::String(c.clone())).collect());
    let snap = json!({
        "format": SNAP_FORMAT,
        "table": table,
        "schema": schema,
        "columns": cols_value,
        "rows": rows_value,
    });
    if let Some(parent) = Path::new(output).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(output, snap.to_string())?;
    Ok(json!({ "success": true, "rows": row_count, "columns": col_count }))
}

fn sql_value_to_snap(v: rusqlite::types::ValueRef) -> Value {
    match v {
        rusqlite::types::ValueRef::Null => Value::Null,
        rusqlite::types::ValueRef::Integer(i) => Value::Number(i.into()),
        rusqlite::types::ValueRef::Real(f) => {
            serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
        }
        rusqlite::types::ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        rusqlite::types::ValueRef::Blob(b) => json!({ "$blob": hex::encode(b) }),
    }
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_import_table_snapshot(
    handle: i64,
    kind: *const c_char,
    db_path: *const c_char,
    table_name: *const c_char,
    input_path: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let result = import_snapshot(
            account,
            cstr(kind),
            cstr(db_path),
            cstr(table_name),
            cstr(input_path),
            "",
        );
        write_import_result(out_json, result)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_import_table_snapshot_with_schema(
    handle: i64,
    kind: *const c_char,
    db_path: *const c_char,
    table_name: *const c_char,
    input_path: *const c_char,
    create_table_sql: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let result = import_snapshot(
            account,
            cstr(kind),
            cstr(db_path),
            cstr(table_name),
            cstr(input_path),
            cstr(create_table_sql),
        );
        write_import_result(out_json, result)
    })
}

fn write_import_result(out_json: *mut *mut c_void, result: Result<Value>) -> i32 {
    match result {
        Ok(v) => {
            write_out(out_json, &v.to_string());
            OK
        }
        Err(e) => {
            write_out(out_json, &json!({ "success": false, "error": e.to_string() }).to_string());
            OK
        }
    }
}

fn import_snapshot(
    account: &Account,
    kind: &str,
    db_path: &str,
    table: &str,
    input: &str,
    create_table_sql: &str,
) -> Result<Value> {
    let raw = std::fs::read_to_string(input)?;
    let snap: Value = serde_json::from_str(&raw)?;
    if snap.get("format").and_then(Value::as_str) != Some(SNAP_FORMAT) {
        bail!("快照格式不受支持");
    }
    let columns: Vec<String> = snap
        .get("columns")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let rows = snap.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
    let db = resolve_db_path(account, kind, db_path)?;
    let safe_table = sanitize_ident(table)?;

    let schema_sql = if !create_table_sql.trim().is_empty() {
        create_table_sql.to_string()
    } else {
        snap.get("schema").and_then(Value::as_str).unwrap_or_default().to_string()
    };

    let col_sql: Vec<String> = columns.iter().map(|c| format!("\"{}\"", sanitize_ident(c).unwrap_or_else(|_| c.clone()))).collect();
    let placeholders = vec!["?"; columns.len()].join(",");
    let insert_sql = format!(
        "INSERT OR IGNORE INTO \"{safe_table}\" ({}) VALUES ({placeholders})",
        col_sql.join(",")
    );

    account.with_conn(&db, |conn| -> Result<Value> {
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
            [table],
            |r| Ok(r.get::<_, i64>(0)? > 0),
        )?;
        if !exists && !schema_sql.trim().is_empty() {
            conn.execute_batch(&schema_sql)?;
        }
        let mut inserted = 0i64;
        let mut ignored = 0i64;
        let mut malformed = 0i64;
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(&insert_sql)?;
            for row in &rows {
                let Some(arr) = row.as_array() else {
                    malformed += 1;
                    continue;
                };
                if arr.len() != columns.len() {
                    malformed += 1;
                    continue;
                }
                let params: Vec<SqlValue> = arr.iter().map(snap_to_sql_value).collect();
                let refs: Vec<&dyn rusqlite::ToSql> =
                    params.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
                match stmt.execute(refs.as_slice()) {
                    Ok(1) => inserted += 1,
                    Ok(_) => ignored += 1,
                    Err(_) => malformed += 1,
                }
            }
        }
        tx.commit()?;
        Ok(json!({
            "success": true,
            "rows": rows.len(),
            "inserted": inserted,
            "ignored": ignored,
            "malformed": malformed,
            "columns": columns.len(),
        }))
    })
}

fn snap_to_sql_value(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                SqlValue::Real(f)
            } else {
                SqlValue::Null
            }
        }
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Object(o) => {
            if let Some(hexstr) = o.get("$blob").and_then(Value::as_str) {
                SqlValue::Blob(hex::decode(hexstr).unwrap_or_default())
            } else {
                SqlValue::Text(v.to_string())
            }
        }
        Value::Array(_) => SqlValue::Text(v.to_string()),
    }
}
