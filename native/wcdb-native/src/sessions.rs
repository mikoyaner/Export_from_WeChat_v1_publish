//! 会话函数组（docs/protocol/01-core-sessions-messages.md §8-§10）。
//!
//! get_sessions 读 session.db 的会话表；schema 未定档，用候选列名归一到协议 snake_case。

use crate::ffi::*;
use crate::query::query_rows_json;
use crate::{push_log, with_account};
use anyhow::{bail, Result};
use serde_json::{Map, Value};
use std::ffi::c_void;
use weflow_core::db::{list_tables, table_columns, Account};
use weflow_core::schema::find_candidate;

const C_USERNAME: &[&str] = &["username", "user_name", "userName", "usrName", "UsrName", "name", "str_name", "talker", "str_username"];
const C_TYPE: &[&str] = &["type", "session_type", "local_type", "ntype"];
const C_LOCAL_TYPE: &[&str] = &["local_type", "localType", "contact_type"];
const C_UNREAD: &[&str] = &["unread_count", "unreadCount", "unreadcount", "unread", "n_unread", "un_read_count"];
const C_SORT_TS: &[&str] = &["sort_timestamp", "sortTimestamp", "sort_time", "sortTime", "ntimestamp", "update_time", "order_time"];
const C_LAST_TS: &[&str] = &["last_timestamp", "lastTimestamp", "last_msg_time", "lastMsgTime", "create_time", "createTime"];
const C_SUMMARY: &[&str] = &["digest", "summary", "last_msg", "lastMsg", "str_digest", "content", "last_message"];
const C_LAST_TYPE: &[&str] = &["last_msg_type", "lastMsgType", "digest_type", "last_type"];
const C_MSG_COUNT: &[&str] = &["message_count", "messageCount", "msg_count", "msgCount", "total_count", "totalCount"];
const C_DISPLAY: &[&str] = &["display_name", "displayName", "nick_name", "nickname", "nickName", "remark", "str_nick_name", "str_remark"];

const FIELD_MAP: &[(&str, &[&str])] = &[
    ("username", C_USERNAME),
    ("type", C_TYPE),
    ("local_type", C_LOCAL_TYPE),
    ("unread_count", C_UNREAD),
    ("sort_timestamp", C_SORT_TS),
    ("last_timestamp", C_LAST_TS),
    ("summary", C_SUMMARY),
    ("last_msg_type", C_LAST_TYPE),
    ("message_count", C_MSG_COUNT),
    ("display_name", C_DISPLAY),
];

/// 在 session.db 中定位主会话表。
///
/// 不能只按表名匹配：WeChat 4.x 的真身叫 `SessionTable`，而
/// `SessionDeleteTable` / `SessionDraft` / `SessionUnread*` 等辅助表同样含 "session"，
/// 且字典序排在 `SessionTable` 之前——按名字选会挑到 1 行的删除表，导致会话列表几乎全丢。
/// 因此按"形状"评分：须有 username 列，有排序/时间戳列加分，行数多者优先。
fn locate_session_table(account: &Account) -> Result<String> {
    let db = account.layout.session_db.clone();
    let tables = account.with_conn(&db, |c| list_tables(c))?;
    let mut best: Option<(i64, String)> = None; // (score, table)
    for t in &tables {
        if !t.to_ascii_lowercase().contains("session") {
            continue;
        }
        let scored = account.with_conn(&db, |c| -> Result<Option<i64>> {
            let cols = table_columns(c, t)?;
            // 必须有 username 列（主会话表才有；否则可能是辅助表）
            if find_candidate(&cols, C_USERNAME).is_none() {
                return Ok(None);
            }
            let mut score = 1i64;
            // 精确/近精确名字强加分
            let lower = t.to_ascii_lowercase();
            if lower == "session" {
                score += 1_000_000;
            } else if lower == "sessiontable" || lower == "session_table" {
                score += 100_000;
            }
            // 辅助表降权（删除/草稿/未读/无联系人信息/统计等）
            for bad in ["delete", "draft", "unread", "nocontact", "noinfo", "stat", "hidden", "oplog", "del"] {
                if lower.contains(bad) {
                    score -= 50_000;
                }
            }
            // 有排序时间戳列加分（主会话表的标志）
            if find_candidate(&cols, C_SORT_TS).is_some() || find_candidate(&cols, C_LAST_TS).is_some() {
                score += 10_000;
            }
            // 行数兜底：正常主会话表远多于辅助表
            let cnt: i64 = c
                .query_row(&format!("SELECT COUNT(*) FROM \"{}\"", t.replace('"', "\"\"")), [], |r| r.get(0))
                .unwrap_or(0);
            score += cnt.min(100_000);
            Ok(Some(score))
        })?;
        if let Some(score) = scored {
            if best.as_ref().map_or(true, |(bs, _)| score > *bs) {
                best = Some((score, t.clone()));
            }
        }
    }
    match best {
        Some((_, t)) => Ok(t),
        None => bail!("session.db 中未找到主会话表"),
    }
}

fn ci_get<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    map.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}

fn normalize_session(raw: Value) -> Value {
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
        for cand in *cands {
            if let Some(k) = keys.iter().find(|k| k.eq_ignore_ascii_case(cand)) {
                if let Some(v) = map.get(k) {
                    map.insert(proto.to_string(), v.clone());
                    break;
                }
            }
        }
    }
    // last_timestamp 缺失时回退 sort_timestamp（chatService 约定）
    let has_last = ci_get(&map, "last_timestamp").map(|v| !v.is_null()).unwrap_or(false);
    if !has_last {
        if let Some(st) = ci_get(&map, "sort_timestamp").cloned() {
            map.insert("last_timestamp".to_string(), st);
        }
    }
    Value::Object(map)
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_sessions(handle: i64, out_json: *mut *mut c_void) -> i32 {
    with_account(handle, |account| {
        let result = (|| -> Result<String> {
            let table = locate_session_table(account)?;
            let db = account.layout.session_db.clone();
            let cols = account.with_conn(&db, |c| table_columns(c, &table))?;
            let sort_col = find_candidate(&cols, C_SORT_TS).or_else(|| find_candidate(&cols, C_LAST_TS));
            let order = match &sort_col {
                Some(sc) => format!(
                    " ORDER BY \"{}\" DESC",
                    weflow_core::schema::sanitize_ident(sc)?
                ),
                None => String::new(),
            };
            let sql = format!("SELECT * FROM \"{}\"{order}", weflow_core::schema::sanitize_ident(&table)?);
            let rows = account.with_conn(&db, |c| query_rows_json(c, &sql))?;
            let out: Vec<Value> = rows
                .into_iter()
                .map(normalize_session)
                .filter(|v| {
                    // username 为空的行丢弃（chatService 约定）
                    ci_get(v.as_object().unwrap_or(&Map::new()), "username")
                        .and_then(Value::as_str)
                        .map(|s| !s.is_empty())
                        .unwrap_or(false)
                })
                .collect();
            Ok(Value::Array(out).to_string())
        })();
        emit_json(out_json, result, ERR_NO_SESSION_TABLE)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_mark_all_sessions_read(handle: i64, out_error: *mut *mut c_void) -> i32 {
    with_account(handle, |account| {
        let result = (|| -> Result<()> {
            let table = locate_session_table(account)?;
            let db = account.layout.session_db.clone();
            let cols = account.with_conn(&db, |c| table_columns(c, &table))?;
            let Some(unread) = find_candidate(&cols, C_UNREAD) else {
                bail!("会话表无未读数列");
            };
            let sql = format!(
                "UPDATE \"{}\" SET \"{}\" = 0",
                weflow_core::schema::sanitize_ident(&table)?,
                weflow_core::schema::sanitize_ident(&unread)?
            );
            account.with_conn(&db, |c| {
                c.execute(&sql, [])?;
                Ok(())
            })?;
            Ok(())
        })();
        match result {
            Ok(()) => OK,
            Err(e) => {
                let msg = e.to_string();
                push_log(&format!("mark_all_sessions_read: {msg}"));
                write_out(out_error, &msg);
                ERR_GENERIC
            }
        }
    })
}
