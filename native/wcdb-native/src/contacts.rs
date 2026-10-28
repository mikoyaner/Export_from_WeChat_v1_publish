//! 联系人 / 群聊 / 头像 / 表情 CDN 函数组（docs/protocol/03、05）。
//!
//! 覆盖必需符号：wcdb_get_contact、wcdb_get_display_names、wcdb_get_avatar_urls、
//! wcdb_get_group_member_count、wcdb_get_group_members、wcdb_get_emoticon_cdn_url，
//! 及可选的 wcdb_get_group_member_counts。
//!
//! `contact` 表名与列语义由 wcdbCore.ts 的 darwin SQL 路径确证（表名单数 `contact`，
//! 列 username/remark/nick_name/alias/big_head_img_url/...）。群成员与表情库表结构
//! 未经 dbKey 校准，故对这两者采取"运行时内省 + 候选表/列名"策略，未命中时返回合法空
//! 结果（`[]`/count=0/空串）而非报错，确保 initialize 不因 schema 差异失败。

use crate::ffi::*;
use crate::query::query_rows_json;
use crate::{push_log, with_account};
use anyhow::{bail, Result};
use rusqlite::Connection;
use serde_json::{Map, Value};
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use weflow_core::db::{list_tables, table_columns, Account};
use weflow_core::schema::{find_candidate, sanitize_ident};

// ── 通用内省辅助 ──

fn ci_get<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    map.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}

fn pick_string(map: &Map<String, Value>, candidates: &[&str]) -> Option<String> {
    for c in candidates {
        if let Some(v) = ci_get(map, c) {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => continue,
            };
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// 在库中按名字提示定位表：优先精确匹配，其次包含匹配。
fn locate_table(conn: &Connection, hints: &[&str]) -> Option<String> {
    let tables = list_tables(conn).ok()?;
    for h in hints {
        if let Some(t) = tables.iter().find(|t| t.eq_ignore_ascii_case(h)) {
            return Some(t.clone());
        }
    }
    for h in hints {
        if let Some(t) = tables.iter().find(|t| t.to_ascii_lowercase().contains(*h)) {
            return Some(t.clone());
        }
    }
    None
}

fn contact_db_path(account: &Account) -> Option<PathBuf> {
    if let Some(p) = &account.layout.contact_db {
        if p.is_file() {
            return Some(p.clone());
        }
    }
    // 回退：账号目录/db_storage/contact/contact.db
    let dir = account.account_dir()?;
    let p = dir.join("db_storage").join("contact").join("contact.db");
    p.is_file().then_some(p)
}

/// 批量取 contact 行（按 username IN (...)），返回 Vec<原始行对象>。
fn fetch_contacts(account: &Account, db: &Path, usernames: &[String]) -> Result<Vec<Value>> {
    let table = account.with_conn(db, |c| {
        locate_table(c, &["contact", "contacts", "friend", "Contact"])
            .ok_or_else(|| anyhow::anyhow!("contact 库中未找到联系人表"))
    })?;
    let safe_table = sanitize_ident(&table)?;
    account.with_conn(db, |conn| {
        if usernames.is_empty() {
            let sql = format!("SELECT * FROM \"{safe_table}\"");
            return query_rows_json(conn, &sql);
        }
        let cols = table_columns(conn, &table)?;
        let Some(name_col) = find_candidate(
            &cols,
            &["username", "user_name", "userName", "usrName", "str_username", "name"],
        ) else {
            bail!("contact 表无 username 列");
        };
        let safe_col = sanitize_ident(&name_col)?;
        let placeholders = std::iter::repeat("?").take(usernames.len()).collect::<Vec<_>>().join(",");
        let sql = format!("SELECT * FROM \"{safe_table}\" WHERE \"{safe_col}\" IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            usernames.iter().map(|u| u as &dyn rusqlite::ToSql).collect();
        crate::query::statement_rows_json(&mut stmt, refs.as_slice())
    })
}

const CONTACT_DISPLAY: &[&str] = &[
    "remark", "Remark", "nick_name", "nickName", "nickname", "NickName", "alias", "Alias",
];
const CONTACT_NICK: &[&str] = &["nick_name", "nickName", "nickname", "NickName"];
const CONTACT_AVATAR: &[&str] = &[
    "big_head_img_url", "bigHeadImgUrl", "big_head_url", "bigHeadUrl",
    "small_head_img_url", "smallHeadImgUrl", "small_head_url", "smallHeadUrl",
    "head_img_url", "headImgUrl", "avatar_url", "avatarUrl",
];

/// 给 contact 行补齐协议要求的推荐键（nickName camelCase 为硬要求；local_type/flag/quan_pin snake_case）。
fn add_contact_aliases(map: &mut Map<String, Value>) {
    let username = pick_string(map, &["username", "user_name", "userName", "usrName"]);
    if let Some(u) = &username {
        map.entry("username").or_insert_with(|| Value::String(u.clone()));
    }
    if let Some(alias) = pick_string(map, &["alias", "Alias"]) {
        map.entry("alias").or_insert_with(|| Value::String(alias));
    }
    if let Some(remark) = pick_string(map, &["remark", "Remark"]) {
        map.entry("remark").or_insert_with(|| Value::String(remark));
    }
    // nickName（camelCase）硬要求：格式化器只读此键
    if let Some(nick) = pick_string(map, CONTACT_NICK) {
        map.insert("nickName".to_string(), Value::String(nick));
    } else if !map.contains_key("nickName") {
        if let Some(u) = &username {
            map.insert("nickName".to_string(), Value::String(u.clone()));
        }
    }
    // 分类判定三键（snake_case 首选）
    if let Some(v) = pick_string(map, &["local_type", "localType", "type", "ntype", "contact_type"]) {
        map.entry("local_type").or_insert_with(|| Value::String(v));
    }
    if let Some(v) = pick_string(map, &["flag", "contact_flag", "contactFlag"]) {
        map.entry("flag").or_insert_with(|| Value::String(v));
    }
    if let Some(v) = pick_string(map, &["quan_pin", "quanPin", "quanpin", "QuanPin"]) {
        map.entry("quan_pin").or_insert_with(|| Value::String(v));
    }
}

fn parse_usernames(json_str: &str) -> Vec<String> {
    match serde_json::from_str::<Value>(json_str) {
        Ok(Value::Array(arr)) => arr
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.trim().to_string()),
                other => Some(other.to_string().trim_matches('"').to_string()),
            })
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

// ── wcdb_get_contact ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_contact(
    handle: i64,
    username: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let uname = cstr(username).trim().to_string();
        let result = (|| -> Result<String> {
            let Some(db) = contact_db_path(account) else {
                bail!("未找到 contact 库");
            };
            let rows = fetch_contacts(account, &db, std::slice::from_ref(&uname))?;
            match rows.into_iter().next() {
                Some(Value::Object(mut m)) => {
                    add_contact_aliases(&mut m);
                    Ok(Value::Object(m).to_string())
                }
                Some(other) => Ok(other.to_string()),
                // 未找到：返回空对象（rc=0），消费方按空联系人处理，避免触发噪声 db dump
                None => Ok("{}".to_string()),
            }
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}

// ── wcdb_get_display_names ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_display_names(
    handle: i64,
    usernames_json: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let names = parse_usernames(cstr(usernames_json));
        let result = (|| -> Result<String> {
            let mut out = Map::new();
            if names.is_empty() {
                return Ok(Value::Object(out).to_string());
            }
            let Some(db) = contact_db_path(account) else {
                // 无 contact 库：回退 username 本身（TS 亦会回退）
                for n in &names {
                    out.insert(n.clone(), Value::String(n.clone()));
                }
                return Ok(Value::Object(out).to_string());
            };
            let rows = fetch_contacts(account, &db, &names)?;
            for row in rows {
                let Value::Object(m) = row else { continue };
                let Some(uname) = pick_string(&m, &["username", "user_name", "userName", "usrName"]) else {
                    continue;
                };
                let display = pick_string(&m, CONTACT_DISPLAY).unwrap_or_else(|| uname.clone());
                out.insert(uname, Value::String(display));
            }
            Ok(Value::Object(out).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}

// ── wcdb_get_avatar_urls ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_avatar_urls(
    handle: i64,
    usernames_json: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let names = parse_usernames(cstr(usernames_json));
        let result = (|| -> Result<String> {
            let mut out = Map::new();
            if names.is_empty() {
                return Ok(Value::Object(out).to_string());
            }
            let Some(db) = contact_db_path(account) else {
                return Ok(Value::Object(out).to_string());
            };
            let rows = fetch_contacts(account, &db, &names)?;
            for row in rows {
                let Value::Object(m) = row else { continue };
                let Some(uname) = pick_string(&m, &["username", "user_name", "userName", "usrName"]) else {
                    continue;
                };
                if let Some(url) = pick_string(&m, CONTACT_AVATAR) {
                    // 必须是 http(s) URL；拒绝 hex/base64 图片字节（协议 isValidAvatarUrl）
                    let u = url.trim();
                    if u.starts_with("http://") || u.starts_with("https://") {
                        out.insert(uname, Value::String(u.to_string()));
                    }
                }
            }
            Ok(Value::Object(out).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}

// ── 群成员解析（内省，双模型）──

const MEMBER_LIST_COLS: &[&str] = &[
    "user_name_list", "username_list", "chatroom_user_name_list", "chat_room_user_name_list",
    "usernamelist", "member_list", "user_list", "userNameList",
];
const MEMBER_DISPLAY_COLS: &[&str] = &[
    "display_name_list", "displayname_list", "chatroom_display_name_list",
    "chat_room_display_name_list", "displayNameList",
];
const SEPARATORS: &[char] = &['^', '\n', ';', '|', ',', '\u{1}', '\u{2}'];

fn split_members(s: &str) -> Vec<String> {
    s.split(SEPARATORS)
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

struct MemberRow {
    username: String,
    display: Option<String>,
}

/// 解析群成员：
/// 模型 A —— 群"联系人"行内的 user_name_list / display_name_list（分隔符切分）；
/// 模型 B —— 每成员一行的表（含 chatroom_id + username 列）。
fn resolve_group_members(account: &Account, chatroom_id: &str) -> Result<Vec<MemberRow>> {
    let Some(db) = contact_db_path(account) else {
        return Ok(Vec::new());
    };
    // 模型 A
    if let Ok(rows) = fetch_contacts(account, &db, &[chatroom_id.to_string()]) {
        for row in rows {
            let Value::Object(m) = row else { continue };
            if let Some(list) = pick_string(&m, MEMBER_LIST_COLS) {
                let names = split_members(&list);
                let displays = pick_string(&m, MEMBER_DISPLAY_COLS)
                    .map(|d| split_members(&d))
                    .unwrap_or_default();
                let members = names
                    .into_iter()
                    .enumerate()
                    .map(|(i, username)| MemberRow {
                        display: displays.get(i).cloned(),
                        username,
                    })
                    .collect();
                return Ok(members);
            }
        }
    }
    // 模型 B：在 contact 库中找带 chatroom_id 列的成员表
    let found = account.with_conn(&db, |conn| -> Result<Vec<MemberRow>> {
        let tables = list_tables(conn)?;
        for t in &tables {
            let cols = match table_columns(conn, t) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let room_col = find_candidate(
                &cols,
                &["chatroom_id", "chat_room_id", "room_id", "chatroomid", "chatRoomId"],
            );
            let user_col = find_candidate(&cols, &["username", "user_name", "userName", "usr_name"]);
            if let (Some(rc), Some(uc)) = (room_col, user_col) {
                let safe_t = sanitize_ident(t)?;
                let safe_rc = sanitize_ident(&rc)?;
                let safe_uc = sanitize_ident(&uc)?;
                let disp_col = find_candidate(&cols, &["display_name", "displayName", "nickname", "nick_name"]);
                let disp_sql = disp_col
                    .as_ref()
                    .map(|d| format!(", \"{}\"", sanitize_ident(d).unwrap_or_default()))
                    .unwrap_or_default();
                let sql = format!(
                    "SELECT \"{safe_uc}\"{disp_sql} FROM \"{safe_t}\" WHERE \"{safe_rc}\" = ?"
                );
                let mut stmt = conn.prepare(&sql)?;
                let mut rows = stmt.query([chatroom_id])?;
                let mut out = Vec::new();
                while let Some(r) = rows.next()? {
                    let username: String = r.get(0)?;
                    if username.trim().is_empty() {
                        continue;
                    }
                    let display: Option<String> = if disp_col.is_some() { r.get(1).ok() } else { None };
                    out.push(MemberRow { username, display });
                }
                if !out.is_empty() {
                    return Ok(out);
                }
            }
        }
        Ok(Vec::new())
    })?;
    Ok(found)
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_group_member_count(
    handle: i64,
    chatroom_id: *const c_char,
    out_count: *mut i32,
) -> i32 {
    with_account(handle, |account| {
        if out_count.is_null() {
            return ERR_GENERIC;
        }
        unsafe { *out_count = 0 };
        let room = cstr(chatroom_id).trim().to_string();
        match resolve_group_members(account, &room) {
            Ok(members) => {
                let n = members.len().clamp(0, i32::MAX as usize) as i32;
                unsafe { *out_count = n };
                OK
            }
            Err(e) => {
                push_log(&format!("get_group_member_count: {e}"));
                // 未知 schema 返回 0 而非错误，避免上层降级噪声
                unsafe { *out_count = 0 };
                OK
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_group_member_counts(
    handle: i64,
    chatroom_ids_json: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let ids = parse_usernames(cstr(chatroom_ids_json));
        let result = (|| -> Result<String> {
            let mut out = Map::new();
            for id in &ids {
                let n = resolve_group_members(account, id).map(|m| m.len()).unwrap_or(0);
                out.insert(id.clone(), Value::Number((n as i64).into()));
            }
            Ok(Value::Object(out).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_group_members(
    handle: i64,
    chatroom_id: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let room = cstr(chatroom_id).trim().to_string();
        let result = (|| -> Result<String> {
            let members = resolve_group_members(account, &room).unwrap_or_default();
            let arr: Vec<Value> = members
                .into_iter()
                .map(|m| {
                    let mut o = Map::new();
                    o.insert("username".to_string(), Value::String(m.username.clone()));
                    if let Some(d) = m.display {
                        if !d.trim().is_empty() {
                            o.insert("nickname".to_string(), Value::String(d.clone()));
                            o.insert("displayName".to_string(), Value::String(d));
                        }
                    }
                    Value::Object(o)
                })
                .collect();
            Ok(Value::Array(arr).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}

// ── wcdb_get_emoticon_cdn_url（纯字符串输出）──

/// 解析表情库路径：dbPath 可能是 emoticon.db 全路径、账号根目录、db_storage 上层目录。
fn resolve_emoticon_db(account: &Account, db_hint: &str) -> Option<PathBuf> {
    let hint = db_hint.trim();
    if !hint.is_empty() {
        let p = PathBuf::from(hint);
        if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("db") {
            return Some(p);
        }
        // 目录提示：向下找 emoticon.db / emotion/emoticon.db
        for sub in [
            PathBuf::from("emoticon").join("emoticon.db"),
            PathBuf::from("emotion").join("emoticon.db"),
            PathBuf::from("db_storage").join("emoticon").join("emoticon.db"),
            PathBuf::from("emoticon.db"),
        ] {
            let cand = p.join(&sub);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    if let Some(p) = &account.layout.emoticon_db {
        if p.is_file() {
            return Some(p.clone());
        }
    }
    let dir = account.account_dir()?;
    for sub in ["emoticon", "emotion"] {
        let cand = dir.join("db_storage").join(sub).join("emoticon.db");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn lookup_emoticon_url(account: &Account, db: &Path, md5: &str) -> Option<String> {
    account
        .with_conn(db, |conn| -> Result<Option<String>> {
            let Some(table) = locate_table(conn, &["emoticon", "emotion", "Emoticon", "emoji"])
            else {
                return Ok(None);
            };
            let cols = table_columns(conn, &table)?;
            let Some(md5_col) = find_candidate(&cols, &["md5", "md5_key", "str_md5", "key", "MD5"])
            else {
                return Ok(None);
            };
            let Some(url_col) = find_candidate(
                &cols,
                &["cdn_url", "cdnurl", "cdn_url_2", "cdn_url_1", "url", "cdnUrl", "CdnUrl"],
            ) else {
                return Ok(None);
            };
            let (Ok(u), Ok(t), Ok(m)) = (
                sanitize_ident(&url_col),
                sanitize_ident(&table),
                sanitize_ident(&md5_col),
            ) else {
                return Ok(None);
            };
            let sql = format!("SELECT \"{u}\" FROM \"{t}\" WHERE \"{m}\" = ? LIMIT 1");
            let mut stmt = conn.prepare(&sql)?;
            // md5 大小写不敏感匹配
            for cand in [md5.to_string(), md5.to_ascii_lowercase(), md5.to_ascii_uppercase()] {
                if let Ok(url) = stmt.query_row([&cand], |r| r.get::<_, String>(0)) {
                    if !url.trim().is_empty() {
                        return Ok(Some(url));
                    }
                }
            }
            Ok(None)
        })
        .unwrap_or(None)
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_emoticon_cdn_url(
    handle: i64,
    db_path: *const c_char,
    md5: *const c_char,
    out_url: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let hint = cstr(db_path);
        let md5 = cstr(md5).trim().to_string();
        // 纯字符串输出：未命中写空串（rc=0，TS 视为 url=undefined）
        let url = match resolve_emoticon_db(account, hint) {
            Some(db) => lookup_emoticon_url(account, &db, &md5).unwrap_or_default(),
            None => String::new(),
        };
        write_out(out_url, &url);
        OK
    })
}
