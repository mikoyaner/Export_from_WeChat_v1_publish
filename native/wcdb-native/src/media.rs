//! 媒体定位函数组：图片 hardlink 解析 + 语音数据读取（docs/protocol/04-tables-misc.md）。
//!
//! - 图片：WeFlow 以 hardlinkOnly 流程调 `wcdb_resolve_image_hardlink(md5)` 拿
//!   `{file_name, full_path}`，full_path 必须磁盘存在。md5→file_name 查
//!   hardlink.db 的 `*_hardlink_info_v4`；file_name→磁盘路径经一次 `msg/attach` 递归
//!   建立的"文件名→完整路径"全局索引（按账号目录缓存）解析。
//! - 语音：`wcdb_get_voice_data` 跨 media_*.db 的 `VoiceInfo` 取 silk blob，
//!   按 svr_id / local_id / create_time 命中，经 Name2Id 关联用户名收窄，出参为十六进制字符串。

use crate::ffi::*;
use crate::query::path_to_json_string;
use crate::with_account;
use anyhow::Result;
use rusqlite::{types::ValueRef, Connection};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use weflow_core::db::{list_tables, table_columns, Account};
use weflow_core::schema::{find_candidate, sanitize_ident};

// 逻辑列 → 真实列候选（大小写不敏感）
const HL_MD5: &[&str] = &["md5"];
const HL_FILE_NAME: &[&str] = &["file_name", "fileName", "filename"];
const VOICE_DATA: &[&str] = &["voice_data", "buf", "voicebuf", "voice_buf", "data"];
const VOICE_SVR: &[&str] = &["svr_id", "svrId", "server_id", "svrid", "msg_server_id"];
const VOICE_LOCAL: &[&str] = &["local_id", "localId", "msg_local_id", "local_id_"];
const VOICE_CREATE: &[&str] = &["create_time", "createTime", "create_time_", "time"];
const VOICE_CHAT: &[&str] = &["chat_name_id", "chat_name", "name_id", "chat_id"];
const N2I_NAME: &[&str] = &["user_name", "username", "user_name_", "name"];

// ── 附件文件名 → 磁盘完整路径 索引（按账号 msg/attach 根缓存） ──

fn attach_cache() -> &'static Mutex<HashMap<String, Arc<HashMap<String, PathBuf>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<HashMap<String, PathBuf>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取得（必要时构建）指定账号 attach 根的文件名索引。文件名小写 → 首个匹配完整路径。
fn attach_index(account: &Account) -> Arc<HashMap<String, PathBuf>> {
    let root = match account.account_dir() {
        Some(d) => d.join("msg").join("attach"),
        None => return Arc::new(HashMap::new()),
    };
    if !root.is_dir() {
        return Arc::new(HashMap::new());
    }
    let key = root.to_string_lossy().replace('\\', "/").to_lowercase();
    if let Ok(g) = attach_cache().lock() {
        if let Some(hit) = g.get(&key) {
            return Arc::clone(hit);
        }
    }
    let mut map = HashMap::new();
    walk_attach(&root, &mut map);
    let arc = Arc::new(map);
    if let Ok(mut g) = attach_cache().lock() {
        g.insert(key, Arc::clone(&arc));
    }
    arc
}

fn walk_attach(dir: &Path, map: &mut HashMap<String, PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = path.is_dir();
        if is_dir {
            walk_attach(&path, map);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            // 保留首个命中；同名文件在 attach 树里极少冲突，主图/缩略图后缀不同不会冲突
            map.entry(name.to_ascii_lowercase()).or_insert_with(|| path.clone());
        }
    }
}

// ── 图片 hardlink 解析 ──

/// 微信 3.x 时代迁移过来的图片按 `<datName>_w.dat`（原图）/ `<datName>_t_w.dat`（缩略图）
/// 落盘，而 hardlink 表里登记的是"文件自身 md5"，消息侧只有 datName/imageMd5（原图 md5），
/// 按 md5 查不到；WeFlow 侧的文件名扫描也只认 `.dat`/`_t`/`_h` 三类，于是整张图判为"未找到"。
/// 这里按文件名变体直接查 attach 索引，顺序 = 质量优先（原图 > 高清 > 迁移原图 > 缩略图）。
const DAT_VARIANT_NAMES: &[&str] = &[
    "{k}.dat",
    "{k}_h.dat",
    "{k}_hd.dat",
    "{k}_w.dat",
    "{k}_b.dat",
    "{k}_c.dat",
    "{k}_t.dat",
    "{k}_t_w.dat",
    "{k}.jpg",
    "{k}.png",
];

/// 由消息图片 md5 解析出 (file_name, full_path)。full_path 保证磁盘存在。
fn resolve_image_dat(account: &Account, md5: &str) -> Option<(String, String)> {
    let md5 = md5.trim().to_ascii_lowercase();
    if md5.is_empty() {
        return None;
    }
    let index = attach_index(account);

    // 1) 经 hardlink.db 各 *_hardlink_info 表：md5 → file_name
    if let Some(hl) = account.layout.hardlink_db.as_ref() {
        let tables: Vec<String> = account
            .with_conn(hl, |c| list_tables(c))
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                let l = t.to_ascii_lowercase();
                l.contains("hardlink_info") || l.contains("hardlink")
            })
            .collect();
        for t in tables {
            if let Some(hit) = lookup_hardlink(account, hl, &t, &md5, &index) {
                return Some(hit);
            }
        }
    }

    // 2) 兜底：以传入的键作为文件名基，按变体后缀在索引里找
    if !is_md5_hex(&md5) {
        return None;
    }
    for tmpl in DAT_VARIANT_NAMES {
        let cand = tmpl.replace("{k}", &md5);
        if let Some(p) = index.get(&cand) {
            return Some((cand, path_to_json_string(p)));
        }
    }
    None
}

fn is_md5_hex(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 在单张 hardlink 表里按 md5 找 file_name，并在 attach 索引里定位完整路径。
fn lookup_hardlink(
    account: &Account,
    hl: &Path,
    table: &str,
    md5: &str,
    index: &HashMap<String, PathBuf>,
) -> Option<(String, String)> {
    let cols = account.columns(hl, table).ok()?;
    let md5_col = find_candidate(&cols, HL_MD5)?;
    let name_col = find_candidate(&cols, HL_FILE_NAME)?;
    let safe_table = sanitize_ident(table).ok()?;
    let sql = format!(
        "SELECT \"{name}\" FROM \"{tbl}\" WHERE lower(\"{m}\") = ?1",
        name = sanitize_ident(&name_col).ok()?,
        tbl = safe_table,
        m = sanitize_ident(&md5_col).ok()?,
    );
    let names: Vec<String> = account
        .with_conn(hl, |c| -> Result<Vec<String>> {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([md5], |r| r.get::<_, String>(0))?;
            Ok(rows.flatten().collect())
        })
        .ok()?;
    for name in names {
        let key = name.to_ascii_lowercase();
        if let Some(p) = index.get(&key) {
            return Some((name, path_to_json_string(p)));
        }
    }
    None
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_resolve_image_hardlink(
    handle: i64,
    md5: *const c_char,
    _account_dir: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let md5 = cstr(md5);
        let result = (|| -> Result<String> {
            match resolve_image_dat(account, md5) {
                Some((file_name, full_path)) => Ok(json!({
                    "file_name": file_name,
                    "full_path": full_path,
                    "md5": md5.trim().to_ascii_lowercase(),
                })
                .to_string()),
                None => Err(anyhow::anyhow!("未找到图片 hardlink: {md5}")),
            }
        })();
        // 未命中时返回非 0，交由上层记录 miss / 回退
        emit_json(out_json, result, ERR_GENERIC)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_resolve_image_hardlink_batch(
    handle: i64,
    requests_json: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let reqs = cstr(requests_json);
        let result = (|| -> Result<String> {
            let parsed: Value = serde_json::from_str(if reqs.trim().is_empty() { "[]" } else { reqs })
                .unwrap_or(Value::Array(Vec::new()));
            let arr = parsed.as_array().cloned().unwrap_or_default();
            let mut rows = Vec::with_capacity(arr.len());
            for (i, r) in arr.iter().enumerate() {
                let md5 = r["md5"].as_str().unwrap_or("").trim().to_ascii_lowercase();
                if md5.is_empty() {
                    rows.push(json!({ "index": i, "md5": "", "success": false, "error": "md5 为空" }));
                    continue;
                }
                match resolve_image_dat(account, &md5) {
                    Some((file_name, full_path)) => rows.push(json!({
                        "index": i,
                        "md5": md5,
                        "success": true,
                        "data": { "file_name": file_name, "full_path": full_path },
                    })),
                    None => rows.push(json!({
                        "index": i, "md5": md5, "success": false, "error": "未找到图片文件"
                    })),
                }
            }
            Ok(Value::Array(rows).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}

// ── 语音数据读取 ──

/// 从 Name2Id 解析候选用户名对应的 rowid 集合。
fn name2id_rowids(conn: &Connection, table: &str, names: &[String]) -> Vec<i64> {
    if names.is_empty() {
        return Vec::new();
    }
    let cols = match table_columns(conn, table) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let name_col = match find_candidate(&cols, N2I_NAME) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let safe = match sanitize_ident(&name_col) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let safe_table = match sanitize_ident(table) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let placeholders = vec!["?"; names.len()].join(",");
    let sql = format!("SELECT rowid FROM \"{safe_table}\" WHERE \"{safe}\" IN ({placeholders})");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let refs: Vec<&dyn rusqlite::ToSql> =
        names.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
    let out: Vec<i64> = match stmt.query_map(refs.as_slice(), |r| r.get::<_, i64>(0)) {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    };
    out
}

/// 读取语音 silk blob 的 hex。命中不到返回 None。
fn find_voice_hex(
    account: &Account,
    session_id: &str,
    create_time: i64,
    local_id: i64,
    svr_id: i64,
    candidates: &[String],
) -> Option<String> {
    let mut names: Vec<String> = candidates
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let sid = session_id.trim().to_string();
    if !sid.is_empty() && !names.iter().any(|n| n == &sid) {
        names.push(sid);
    }
    names.sort();
    names.dedup();

    for db in &account.layout.media_dbs {
        let tables: Vec<String> = account
            .with_conn(db, |c| list_tables(c))
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.to_ascii_lowercase().contains("voice"))
            .collect();
        for vt in tables {
            if let Some(hexv) = voice_in_table(account, db, &vt, &names, create_time, local_id, svr_id) {
                return Some(hexv);
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn voice_in_table(
    account: &Account,
    db: &Path,
    vtable: &str,
    names: &[String],
    create_time: i64,
    local_id: i64,
    svr_id: i64,
) -> Option<String> {
    let cols = account.columns(db, vtable).ok()?;
    let data_col = find_candidate(&cols, VOICE_DATA)?;
    let svr_col = find_candidate(&cols, VOICE_SVR);
    let local_col = find_candidate(&cols, VOICE_LOCAL);
    let create_col = find_candidate(&cols, VOICE_CREATE);
    let chat_col = find_candidate(&cols, VOICE_CHAT);
    let safe_data = sanitize_ident(&data_col).ok()?;
    let safe_table = sanitize_ident(vtable).ok()?;

    // 定位谓词：svr_id > local_id > create_time（单一绑定参数用 ?1）
    let (id_where, id_param): (Option<String>, Option<i64>) = match (&svr_col, &local_col, &create_col) {
        (Some(sc), _, _) if svr_id != 0 => (Some(format!("\"{}\" = ?1", sanitize_ident(sc).ok()?)), Some(svr_id)),
        (_, Some(lc), _) if local_id != 0 => (Some(format!("\"{}\" = ?1", sanitize_ident(lc).ok()?)), Some(local_id)),
        (_, _, Some(cc)) if create_time != 0 => (Some(format!("\"{}\" = ?1", sanitize_ident(cc).ok()?)), Some(create_time)),
        _ => (None, None),
    };
    if id_where.is_none() {
        return None;
    }

    let conn = account.conn(db).ok()?;
    let conn = conn.lock().ok()?;

    // 收窄：优先带 chat_name_id 过滤，命中不到再仅用 id 谓词
    let rowids: Vec<i64> = if let Some(cc) = &chat_col {
        let n2i = find_name2id(&cols_of_tables(&conn))?;
        let _ = cc;
        name2id_rowids(&conn, &n2i, names)
    } else {
        Vec::new()
    };

    let run = |with_chat: bool| -> Option<String> {
        let base = id_where.clone()?;
        let (where_sql, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = if with_chat && !rowids.is_empty() {
            let inlist = rowids.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(",");
            let cc = chat_col.as_ref().and_then(|c| sanitize_ident(c).ok())?;
            (format!("\"{cc}\" IN ({inlist}) AND {base}"), vec![Box::new(id_param.unwrap_or(0))])
        } else {
            (base, vec![Box::new(id_param.unwrap_or(0))])
        };
        let sql = format!("SELECT hex(\"{safe_data}\") FROM \"{safe_table}\" WHERE {where_sql} LIMIT 1");
        let mut stmt = conn.prepare(&sql).ok()?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(|p| p.as_ref() as &dyn rusqlite::ToSql).collect();
        let mut rows = stmt.query(refs.as_slice()).ok()?;
        match rows.next().ok()? {
            Some(row) => match row.get_ref(0).ok()? {
                ValueRef::Text(t) => Some(String::from_utf8_lossy(t).to_ascii_lowercase()),
                ValueRef::Blob(b) => Some(hex::encode(b).to_ascii_lowercase()),
                _ => None,
            },
            None => None,
        }
    };

    run(true).or_else(|| run(false))
}

fn cols_of_tables(conn: &Connection) -> Vec<String> {
    list_tables(conn).unwrap_or_default()
}

/// 在库中定位 Name2Id 表（名字含 name2id）。
fn find_name2id(tables: &[String]) -> Option<String> {
    tables
        .iter()
        .find(|t| t.eq_ignore_ascii_case("Name2Id"))
        .or_else(|| tables.iter().find(|t| t.to_ascii_lowercase().contains("name2id")))
        .cloned()
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_voice_data(
    handle: i64,
    session_id: *const c_char,
    create_time: i32,
    local_id: i32,
    svr_id: i64,
    candidates_json: *const c_char,
    out_hex: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        let cands_json = cstr(candidates_json);
        let candidates: Vec<String> = serde_json::from_str(if cands_json.trim().is_empty() { "[]" } else { cands_json })
            .unwrap_or_default();
        let hexv = find_voice_hex(account, sid, create_time.max(0) as i64, local_id.max(0) as i64, svr_id, &candidates);
        match hexv {
            Some(hex) => {
                write_out(out_hex, &hex);
                OK
            }
            None => {
                // 未命中：给出空串（TS 端 `hex || undefined`），但仍回 0 让其进入 exec_query 兜底前先不报错
                write_out(out_hex, "");
                ERR_GENERIC
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_voice_data_batch(
    handle: i64,
    requests_json: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let reqs = cstr(requests_json);
        let result = (|| -> Result<String> {
            let parsed: Value = serde_json::from_str(if reqs.trim().is_empty() { "[]" } else { reqs })
                .unwrap_or(Value::Array(Vec::new()));
            let arr = parsed.as_array().cloned().unwrap_or_default();
            let mut rows = Vec::with_capacity(arr.len());
            for (i, r) in arr.iter().enumerate() {
                let sid = r["session_id"].as_str().unwrap_or("").to_string();
                let ct = r["create_time"].as_i64().unwrap_or(0);
                let lid = r["local_id"].as_i64().unwrap_or(0);
                let svr = r["svr_id"].as_i64().or_else(|| r["svr_id"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
                let cands: Vec<String> = r["candidates"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
                let hexv = find_voice_hex(account, &sid, ct, lid, svr, &cands);
                let mut obj = json!({ "index": i });
                if let Some(h) = hexv {
                    obj["hex"] = Value::String(h);
                }
                rows.push(obj);
            }
            Ok(Value::Array(rows).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
    })
}
