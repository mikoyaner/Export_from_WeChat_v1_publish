//! 独立诊断工具：以 WeFlow 的方式运行时加载已构建的 wcdb.dll（只读映射，即使文件被
//! 正在运行的 WeFlow 锁定也可加载），逐个会话类型读取消息条数，定位"聊天记录只显示企业微信"。
//!
//! cargo run --release -p abi-smoke -- <session.db 路径> <store.json 路径> <wcdb.dll 路径>
//! 仅在用户本机/本账号范围内运行；不打印任何密钥原文。

use libloading::{Library, Symbol};
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;

type FnInit = unsafe extern "C" fn() -> i32;
type FnOpen = unsafe extern "C" fn(*const c_char, *const c_char, *mut i64) -> i32;
type FnGetSessions = unsafe extern "C" fn(i64, *mut *mut c_void) -> i32;
type FnGetMessages = unsafe extern "C" fn(i64, *const c_char, i32, i32, *mut *mut c_void) -> i32;
type FnOpenCursor = unsafe extern "C" fn(i64, *const c_char, i32, i32, i32, i32, *mut i64) -> i32;
type FnFetchBatch = unsafe extern "C" fn(i64, i64, *mut *mut c_void, *mut i32) -> i32;
type FnCloseCursor = unsafe extern "C" fn(i64, i64) -> i32;
type FnListMsgDbs = unsafe extern "C" fn(i64, *mut *mut c_void) -> i32;
type FnFree = unsafe extern "C" fn(*mut c_void);
type FnResolveImg = unsafe extern "C" fn(i64, *const c_char, *const c_char, *mut *mut c_void) -> i32;
type FnResolveImgBatch = unsafe extern "C" fn(i64, *const c_char, *mut *mut c_void) -> i32;
type FnGetVoice = unsafe extern "C" fn(i64, *const c_char, i32, i32, i64, *const c_char, *mut *mut c_void) -> i32;
type FnGetVoiceBatch = unsafe extern "C" fn(i64, *const c_char, *mut *mut c_void) -> i32;
type FnSetMyId = unsafe extern "C" fn(i64, *const c_char) -> i32;

fn decode(out: *mut c_void) -> Option<String> {
    if out.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(out as *const c_char).to_str().ok().map(|s| s.to_string()) }
}

fn norm(p: &str) -> String {
    p.replace('\\', "/").to_lowercase()
}

fn kind_of(u: &str) -> &'static str {
    if u.ends_with("@chatroom") {
        "group"
    } else if u.starts_with("wxid_") {
        "personal"
    } else if u.starts_with("gh_")
        || u.contains("brandsessionholder")
        || u.contains("@api")
        || u.contains("appmsg")
    {
        "biz"
    } else {
        "other"
    }
}

fn find_key(store: &Value, session_db: &str) -> Option<String> {
    let want = norm(session_db);
    let maps = store["db_key_maps"].as_object()?;
    for (acct, entry) in maps {
        if want.contains(&norm(acct)) {
            if let Some(k) = entry["session/session.db"].as_str() {
                return Some(k.to_string());
            }
        }
    }
    // 兜底：任一条目含 session/session.db
    for (_a, entry) in maps {
        if let Some(k) = entry["session/session.db"].as_str() {
            return Some(k.to_string());
        }
    }
    None
}

fn json_len(j: Option<String>) -> usize {
    j.and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_array().map(|a| a.len()))
        .unwrap_or(0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    // 三个路径都要显式传（真实账号目录与构建产物都不写死在源码里）：
    //   abi-smoke <session.db> <store.json> <wcdb.dll>
    let session_db = args
        .next()
        .unwrap_or_else(|| "<微信数据根>/<wxid>_<suffix>/db_storage/session/session.db".into());
    let store_path = args
        .next()
        .unwrap_or_else(|| "C:/Users/<you>/AppData/Roaming/weflow-key-provider/store.json".into());
    let dll_path = args
        .next()
        .unwrap_or_else(|| "components/wcdb.dll".into());

    let store_txt = std::fs::read_to_string(&store_path)?;
    let store: Value = serde_json::from_str(&store_txt)?;
    let key_hex = find_key(&store, &session_db).ok_or("store.json 找不到 session key")?;
    eprintln!("[abi-smoke] key_len={}", key_hex.len());

    // ── 直接用 rusqlite/weflow-core 打开 session.db 做原始诊断 ──
    {
        use rusqlite::Connection;
        let acct = weflow_core::db::Account::open(&session_db, &key_hex)?;
        let sdb = acct.layout.session_db.clone();
        eprintln!("[raw] layout.session_db={}", sdb.display());
        acct.with_conn(&sdb, |c: &Connection| -> anyhow::Result<()> {
            let mut st = c.prepare("SELECT name FROM sqlite_master WHERE type='table' AND (lower(name) LIKE '%session%' OR lower(name) LIKE '%chat%')")?;
            let tables: Vec<String> = st.query_map([], |r| r.get::<_, String>(0))?.filter_map(|x| x.ok()).collect();
            eprintln!("[raw] candidate session/chat tables = {tables:?}");
            for t in &tables {
                let cnt: i64 = c.query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get(0)).unwrap_or(-1);
                let cols: Vec<String> = {
                    let mut ps = c.prepare(&format!("PRAGMA table_info(\"{t}\")"))?;
                    let rows: Vec<String> = ps.query_map([], |r| r.get::<_, String>(1))?.filter_map(|x| x.ok()).collect();
                    rows
                };
                let uname = ["str_username","username","UsrName","usrName","talker"].iter().find(|k| cols.iter().any(|c| c.eq_ignore_ascii_case(k))).copied();
                eprintln!("[raw] table {t}: rows={cnt} has_username_col={uname:?} cols={cols:?}");
                if let Some(uc) = uname {
                    let empties: i64 = c.query_row(&format!("SELECT COUNT(*) FROM \"{t}\" WHERE \"{uc}\" IS NULL OR \"{uc}\"=''"), [], |r| r.get(0)).unwrap_or(-1);
                    let wxid: i64 = c.query_row(&format!("SELECT COUNT(*) FROM \"{t}\" WHERE lower(\"{uc}\") LIKE 'wxid_%'"), [], |r| r.get(0)).unwrap_or(-1);
                    let room: i64 = c.query_row(&format!("SELECT COUNT(*) FROM \"{t}\" WHERE lower(\"{uc}\") LIKE '%@chatroom'"), [], |r| r.get(0)).unwrap_or(-1);
                    let gh: i64 = c.query_row(&format!("SELECT COUNT(*) FROM \"{t}\" WHERE lower(\"{uc}\") LIKE 'gh_%' OR lower(\"{uc}\") LIKE '%brandsessionholder%'"), [], |r| r.get(0)).unwrap_or(-1);
                    eprintln!("[raw]   {t} username breakdown: empty={empties} wxid={wxid} chatroom={room} gh/biz={gh}");
                }
            }
            Ok(())
        })?;
    }

    // ── 语音诊断：列出 media dbs、VoiceInfo 列与真实样例行 ──
    {
        use rusqlite::Connection;
        let acct = weflow_core::db::Account::open(&session_db, &key_hex)?;
        // 消息库 Name2Id：real_sender_id → user_name，验证方向判定
        eprintln!("[msg-diag] message_dbs={}", acct.layout.message_dbs.len());
        if let Some(db) = acct.layout.message_dbs.first() {
            let dbo = db.clone();
            let _ = acct.with_conn(&dbo, |c: &Connection| -> anyhow::Result<()> {
                let mut st = c.prepare("SELECT name FROM sqlite_master WHERE type='table' AND lower(name) LIKE '%name2id%'")?;
                let tables: Vec<String> = st.query_map([], |r| r.get::<_, String>(0))?.filter_map(|x| x.ok()).collect();
                eprintln!("[msg-diag] {} name2id tables={:?}", dbo.file_name().unwrap_or_default().to_string_lossy(), tables);
                if let Some(t) = tables.first() {
                    let cnt: i64 = c.query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get(0)).unwrap_or(-1);
                    eprintln!("[msg-diag]   {t}: rows={cnt}");
                    for rid in [111i64] {
                        let uname: Option<String> = c.query_row(&format!("SELECT user_name FROM \"{t}\" WHERE rowid=?1"), [rid], |r| r.get(0)).ok();
                        eprintln!("[msg-diag]   real_sender_id={rid} -> {uname:?}");
                    }
                    // 顶部若干
                    let mut ps = c.prepare(&format!("SELECT rowid,user_name FROM \"{t}\" ORDER BY rowid LIMIT 5"))?;
                    let mut rows = ps.query([])?;
                    while let Some(r) = rows.next()? {
                        eprintln!("[msg-diag]     rowid={} user_name={}", r.get::<_,i64>(0)?, r.get::<_,String>(1)?);
                    }
                }
                Ok(())
            });
        }
        eprintln!("[voice-diag] media_dbs={:?}", acct.layout.media_dbs.iter().map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string()).collect::<Vec<_>>());
        for db in &acct.layout.media_dbs {
            let dbowned = db.clone();
            let _ = acct.with_conn(&dbowned, |c: &Connection| -> anyhow::Result<()> {
                let mut st = c.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
                let tables: Vec<String> = st.query_map([], |r| r.get::<_, String>(0))?.filter_map(|x| x.ok()).collect();
                for t in &tables {
                    let lt = t.to_ascii_lowercase();
                    if lt.contains("voice") {
                        let mut ps = c.prepare(&format!("PRAGMA table_info(\"{t}\")"))?;
                        let cols: Vec<String> = ps.query_map([], |r| r.get::<_, String>(1))?.filter_map(|x| x.ok()).collect();
                        let cnt: i64 = c.query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get(0)).unwrap_or(-1);
                        eprintln!("[voice-diag] {}.{t}: rows={cnt} cols={cols:?}", db.file_name().unwrap_or_default().to_string_lossy());
                        let mut ds = c.prepare(&format!("SELECT * FROM \"{t}\" LIMIT 3"))?;
                        let mut idxs = ds.query([])?;
                        while let Some(row) = idxs.next()? {
                            use rusqlite::types::ValueRef;
                            let mut m = serde_json::Map::new();
                            for (i, col) in cols.iter().enumerate() {
                                let v = match row.get_ref(i)? {
                                    ValueRef::Null => serde_json::Value::Null,
                                    ValueRef::Integer(x) => serde_json::json!(x),
                                    ValueRef::Real(x) => serde_json::json!(x),
                                    ValueRef::Text(t) => serde_json::Value::String(String::from_utf8_lossy(t).chars().take(24).collect()),
                                    ValueRef::Blob(b) => serde_json::json!(format!("<blob {} bytes>", b.len())),
                                };
                                m.insert(col.clone(), v);
                            }
                            eprintln!("[voice-diag]   row={}", serde_json::Value::Object(m));
                        }
                    }
                    if lt.contains("name2id") {
                        let mut ps = c.prepare(&format!("PRAGMA table_info(\"{t}\")"))?;
                        let cols: Vec<String> = ps.query_map([], |r| r.get::<_, String>(1))?.filter_map(|x| x.ok()).collect();
                        eprintln!("[voice-diag] {}.{t}: cols={cols:?}", db.file_name().unwrap_or_default().to_string_lossy());
                    }
                }
                Ok(())
            });
        }
    }

    let lib = unsafe { Library::new(&dll_path)? };
    unsafe {
        let init: Symbol<FnInit> = lib.get(b"wcdb_init")?;
        let open: Symbol<FnOpen> = lib.get(b"wcdb_open_account")?;
        let get_sessions: Symbol<FnGetSessions> = lib.get(b"wcdb_get_sessions")?;
        let get_messages: Symbol<FnGetMessages> = lib.get(b"wcdb_get_messages")?;
        let open_cursor: Symbol<FnOpenCursor> = lib.get(b"wcdb_open_message_cursor")?;
        let fetch: Symbol<FnFetchBatch> = lib.get(b"wcdb_fetch_message_batch")?;
        let close_cursor: Symbol<FnCloseCursor> = lib.get(b"wcdb_close_message_cursor")?;
        let list_msg_dbs: Symbol<FnListMsgDbs> = lib.get(b"wcdb_list_message_dbs")?;
        let free: Option<Symbol<FnFree>> = lib.get(b"wcdb_free_string").ok();
        let freef = |p: *mut c_void| {
            if let Some(ref f) = free {
                if !p.is_null() {
                    f(p);
                }
            }
        };

        eprintln!("[abi-smoke] init rc={}", init());
        let cpath = CString::new(session_db.clone())?;
        let ckey = CString::new(key_hex)?;
        let mut handle: i64 = 0;
        let orc = open(cpath.as_ptr(), ckey.as_ptr(), &mut handle);
        eprintln!("[abi-smoke] open rc={orc} handle={handle}");
        if orc != 0 {
            return Err("open_account failed".into());
        }
        // 模拟 WeFlow：打开后设置本账号 id，使 computed_is_send 可判定方向
        let set_id: Option<Symbol<FnSetMyId>> = lib.get(b"wcdb_set_my_account_id").ok();
        if let Some(set_id) = set_id {
            // 本账号 id 不写死：需要方向判定时用 WEFLOW_ACCOUNT_ID 传进来
            let my_id = std::env::var("WEFLOW_ACCOUNT_ID").unwrap_or_default();
            let cid = CString::new(my_id)?;
            eprintln!("[abi-smoke] set_my_account_id rc={}", set_id(handle, cid.as_ptr()));
        }

        let mut mo: *mut c_void = ptr::null_mut();
        if list_msg_dbs(handle, &mut mo) == 0 {
            eprintln!("[abi-smoke] message_dbs={}", json_len(decode(mo)));
            freef(mo);
        }

        let mut so: *mut c_void = ptr::null_mut();
        let src = get_sessions(handle, &mut so);
        eprintln!("[abi-smoke] get_sessions rc={src}");
        if src != 0 {
            return Err("get_sessions failed".into());
        }
        let sj = decode(so).ok_or("sessions null")?;
        freef(so);
        let sessions: Vec<Value> = serde_json::from_str(&sj)?;
        let mut by_kind = HashMap::<&str, usize>::new();
        for s in &sessions {
            *by_kind.entry(kind_of(s["username"].as_str().unwrap_or(""))).or_insert(0) += 1;
        }
        eprintln!("[abi-smoke] sessions total={} by_kind={:?}", sessions.len(), by_kind);

        let mut seen = std::collections::HashSet::<String>::new();
        let mut dumped = std::collections::HashSet::<&'static str>::new();
        for want in ["personal", "group", "biz", "other"] {
            let mut picked = 0usize;
            for s in &sessions {
                if picked >= 4 {
                    break;
                }
                let u = match s["username"].as_str() {
                    Some(x) if !x.is_empty() => x,
                    _ => continue,
                };
                if kind_of(u) != want || !seen.insert(u.to_string()) {
                    continue;
                }
                picked += 1;
                let cu = CString::new(u)?;

                let mut go: *mut c_void = ptr::null_mut();
                let grc = get_messages(handle, cu.as_ptr(), 5, 0, &mut go);
                let gn = if grc == 0 { json_len(decode(go)) } else { usize::MAX };
                // 首个样例：打印每行方向列（不含正文），验证收发判定
                if grc == 0 && (want == "personal" || want == "group") && !dumped.contains(want) {
                    if let Some(js) = decode(go) {
                        if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(&js) {
                            if !a.is_empty() {
                                dumped.insert(want);
                                println!("\n### DIRECTION [{}] {} (rows={}) ###", want, u, a.len());
                                for (i, r) in a.iter().take(6).enumerate() {
                                    println!(
                                        "  row{i}: computed_is_send={:?} sender_username={:?} real_sender_id={:?} local_type={:?}",
                                        r["computed_is_send"],
                                        r["sender_username"],
                                        r["real_sender_id"],
                                        r["local_type"],
                                    );
                                }
                            }
                        }
                    }
                }
                freef(go);

                let mut cur: i64 = 0;
                let crc = open_cursor(handle, cu.as_ptr(), 500, 0, 0, 0, &mut cur);
                let mut ctotal = 0usize;
                let mut ferr = 0i32;
                if crc == 0 {
                    loop {
                        let mut fo: *mut c_void = ptr::null_mut();
                        let mut more: i32 = 0;
                        let frc = fetch(handle, cur, &mut fo, &mut more);
                        if frc != 0 {
                            ferr = frc;
                            freef(fo);
                            break;
                        }
                        ctotal += json_len(decode(fo));
                        freef(fo);
                        if more == 0 || ctotal > 20000 {
                            break;
                        }
                    }
                    close_cursor(handle, cur);
                } else {
                    ferr = crc;
                }

                println!(
                    "[{want:8}] {:38} get_messages rc={grc} n={gn:>3} | cursor rc={crc} ferr={ferr} total={ctotal:>5} | ts={}",
                    u,
                    s["sort_timestamp"].as_i64().unwrap_or(0),
                );
            }
        }

        // ── 媒体：图片 hardlink + 语音（用已知可用样例值验证新导出）──
        let resolve_img: Symbol<FnResolveImg> = lib.get(b"wcdb_resolve_image_hardlink")?;
        let resolve_img_batch: Symbol<FnResolveImgBatch> = lib.get(b"wcdb_resolve_image_hardlink_batch")?;
        let get_voice: Symbol<FnGetVoice> = lib.get(b"wcdb_get_voice_data")?;
        let get_voice_batch: Symbol<FnGetVoiceBatch> = lib.get(b"wcdb_get_voice_data_batch")?;

        let sample_md5 = CString::new("3bb59dccc959c3d3123104d8119da5c9").unwrap();
        let mut io: *mut c_void = ptr::null_mut();
        let irc = resolve_img(handle, sample_md5.as_ptr(), ptr::null(), &mut io);
        if irc == 0 {
            let j = decode(io).unwrap_or_default();
            let v: Value = serde_json::from_str(&j).unwrap_or(Value::Null);
            let fp = v["full_path"].as_str().unwrap_or("");
            println!("[image ] resolve rc={irc} file_name={} full_path={} exists={}",
                v["file_name"].as_str().unwrap_or("-"), fp, std::path::Path::new(fp).exists());
        } else {
            println!("[image ] resolve rc={irc} (未命中)");
        }
        freef(io);

        let reqs = CString::new(r#"[{"md5":"3bb59dccc959c3d3123104d8119da5c9","md5_bad":"x"}]"#).unwrap();
        let mut ibo: *mut c_void = ptr::null_mut();
        let ibrc = resolve_img_batch(handle, reqs.as_ptr(), &mut ibo);
        println!("[image ] batch rc={ibrc} rows={}", json_len(decode(ibo)));
        freef(ibo);

        // 语音：Name2Id user_name=fengyutianxiang local_id=56 svr_id=5506518535405883510
        let vsid = CString::new("fengyutianxiang").unwrap();
        let vcands = CString::new(r#"["fengyutianxiang"]"#).unwrap();
        let mut vo: *mut c_void = ptr::null_mut();
        let vrc = get_voice(handle, vsid.as_ptr(), 1776166914, 56, 5506518535405883510i64, vcands.as_ptr(), &mut vo);
        let hexv = decode(vo).unwrap_or_default();
        println!("[voice ] get_voice_data rc={vrc} hex_len={} head={}", hexv.len(), &hexv[..hexv.len().min(24)]);
        freef(vo);

        let vreqs = CString::new(r#"[{"session_id":"fengyutianxiang","create_time":1776166914,"local_id":56,"svr_id":5506518535405883510,"candidates":["fengyutianxiang"]}]"#).unwrap();
        let mut vbo: *mut c_void = ptr::null_mut();
        let vbrc = get_voice_batch(handle, vreqs.as_ptr(), &mut vbo);
        println!("[voice ] batch rc={vbrc} json={}", decode(vbo).unwrap_or_default().chars().take(120).collect::<String>());
        freef(vbo);
    }
    Ok(())
}
