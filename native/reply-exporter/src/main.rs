//! reply-exporter —— AI 微信回复助手 · 阶段一自研只读导出器。
//!
//! 命令触发式一次性导出：解析「联系人名/群名」→ 定位 session_id →
//! 取该会话最近 N 条消息 → 原始 local_type 转可读文本 → 结构化 JSON 输出（stdout）。
//! 密钥来自 keyscan_readonly.py 写入的 keystore（Account::open 内部自动按库解析每库密钥）。
//!
//! 全程只读：只读 session.db/contact.db/消息库，不写微信数据目录，不注入、不 admin。
//!
//! 用法：
//!   reply-exporter [数据根目录] --name "<联系人名/群名>" [--n 30] [--sid <username>] [--pretty]
//!   reply-exporter [数据根目录] --list-accounts
//! 数据根目录（如 <微信数据根>）可选，用于把账号限定在其下；缺省用 keystore 里全部账号。
//! 命中多个同名 → 打印候选 JSON 并退出码 2（调用方据此让用户选编号）；0 个 → 退出码 4。

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use weflow_core::db::{list_tables, table_columns, Account};
use weflow_core::keystore::KeyStore;
use weflow_core::messages::{session_message_rows_recent, session_message_rows_recent_routed};
use weflow_core::schema::{find_candidate, sanitize_ident};

mod cdn_emoji;
mod media;

const USER_COLS: &[&str] = &[
    "username", "user_name", "userName", "usrName", "UsrName", "name", "str_name",
    "strUsrName", "talker", "str_username",
];
const DISP_COLS: &[&str] = &[
    "nick_name", "nickName", "nickname", "NickName", "str_nick_name", "strNickName",
    "display_name", "displayName",
];
const REMARK_COLS: &[&str] = &["con_remark", "conRemark", "remark", "Remark", "str_remark"];
const ALIAS_COLS: &[&str] = &["alias", "Alias"];

#[derive(Default, Clone)]
struct NameInfo {
    username: String,
    nickname: String,
    remark: String,
    alias: String,
    session_display: String,
}
impl NameInfo {
    fn best(&self) -> &str {
        for s in [&self.remark, &self.nickname, &self.session_display, &self.alias] {
            if !s.trim().is_empty() {
                return s;
            }
        }
        &self.username
    }
}

fn ci_get<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    m.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}
fn as_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}
fn as_i64(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn is_session_db(db: &Path) -> bool {
    db.file_stem().map(|s| s.eq_ignore_ascii_case("session")).unwrap_or(false)
}

/// 通用查询：给定表 + 一组列名，返回每行 Option<String>（非文本列取不到即 None）。
fn query_rows(account: &Account, db: &Path, table: &str, cols: &[String]) -> Result<Vec<Vec<Option<String>>>> {
    let col_list = cols
        .iter()
        .map(|c| sanitize_ident(c).map(|c| format!("\"{c}\"")))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    let sql = format!("SELECT {col_list} FROM \"{}\"", sanitize_ident(table)?);
    account.with_conn(db, move |conn| {
        let mut stmt = conn.prepare(&sql)?;
        let n = cols.len();
        let mut out = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let mut row = Vec::with_capacity(n);
            for i in 0..n {
                row.push(r.get::<_, Option<String>>(i).unwrap_or(None));
            }
            out.push(row);
        }
        Ok(out)
    })
}

/// 一张候选名列表的列索引：user / disp / remark? / alias?（都是 wanted 里的位置）。
struct NameCols {
    table: String,
    wanted: Vec<String>,
    i_user: usize,
    i_disp: Option<usize>,
    i_remark: Option<usize>,
    i_alias: Option<usize>,
}

/// 找一张同时含「用户名列 + 展示名列」的表（会话表或联系人表），并给出列索引。
fn pick_name_table(account: &Account, db: &Path) -> Result<Option<NameCols>> {
    let tables = account.with_conn(db, |conn| list_tables(conn))?;
    for t in &tables {
        let cols = match account.with_conn(db, |conn| table_columns(conn, t)) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let user = match find_candidate(&cols, USER_COLS) {
            Some(u) => u,
            None => continue,
        };
        let disp = match find_candidate(&cols, DISP_COLS) {
            Some(d) => d,
            None => continue,
        };
        let remark = find_candidate(&cols, REMARK_COLS);
        let alias = find_candidate(&cols, ALIAS_COLS);
        let mut wanted: Vec<String> = vec![user.clone(), disp.clone()];
        let push_uniq = |name: &Option<String>, wanted: &mut Vec<String>| {
            if let Some(n) = name {
                if !wanted.iter().any(|x| x.eq_ignore_ascii_case(n)) {
                    wanted.push(n.clone());
                }
            }
        };
        push_uniq(&remark, &mut wanted);
        push_uniq(&alias, &mut wanted);
        let pos = |name: &Option<String>| name.as_ref().and_then(|n| wanted.iter().position(|c| c.eq_ignore_ascii_case(n)));
        return Ok(Some(NameCols {
            table: t.clone(),
            i_user: wanted.iter().position(|c| c.eq_ignore_ascii_case(&user)).unwrap_or(0),
            i_disp: pos(&Some(disp)),
            i_remark: pos(&remark),
            i_alias: pos(&alias),
            wanted,
        }));
    }
    Ok(None)
}

/// 从某个库加载 username -> NameInfo（增量合并到 map）。
fn load_names(account: &Account, db: &Path, map: &mut HashMap<String, NameInfo>) {
    let nc = match pick_name_table(account, db) {
        Ok(Some(nc)) => nc,
        _ => return,
    };
    let rows = match query_rows(account, db, &nc.table, &nc.wanted) {
        Ok(r) => r,
        Err(_) => return,
    };
    let from_session = is_session_db(db);
    let get = |row: &[Option<String>], i: Option<usize>| -> String {
        i.and_then(|i| row.get(i).and_then(|v| v.clone())).unwrap_or_default()
    };
    for row in rows {
        let username = get(&row, Some(nc.i_user));
        if username.trim().is_empty() {
            continue;
        }
        let disp = get(&row, nc.i_disp);
        let remark = get(&row, nc.i_remark);
        let alias = get(&row, nc.i_alias);
        let e = map.entry(username.clone()).or_insert_with(|| NameInfo { username: username.clone(), ..Default::default() });
        if !disp.trim().is_empty() {
            if from_session {
                if e.session_display.trim().is_empty() {
                    e.session_display = disp.clone();
                }
                if e.nickname.trim().is_empty() {
                    e.nickname = disp;
                }
            } else if e.nickname.trim().is_empty() {
                e.nickname = disp;
            }
        }
        if !remark.trim().is_empty() {
            e.remark = remark;
        }
        if !alias.trim().is_empty() {
            e.alias = alias;
        }
    }
}

/// 在候选里按查询串匹配（忽略大小写，子串或全等 username）。
/// 选人排序规则（用户定的）：**先比匹配程度，同档再比字段**。
/// 程度：3=完全相等 2=前缀 1=包含；字段：备注 5 > 微信号/原始 id 4 > 昵称 3 > 会话显示名 2。
/// 返回 `(NameInfo, degree, field_rank, matched_by)`，按分数从高到低。
fn ranked_matches(map: &HashMap<String, NameInfo>, query: &str) -> Vec<(NameInfo, u8, u8, &'static str)> {
    const D_EXACT: u8 = 3;
    const D_PREFIX: u8 = 2;
    const D_CONTAINS: u8 = 1;
    const F_REMARK: u8 = 5;
    const F_ID: u8 = 4;
    const F_NICK: u8 = 3;
    const F_SESSION: u8 = 2;

    fn degree(q: &str, v: &str) -> u8 {
        let v = v.trim().to_lowercase();
        if v.is_empty() {
            return 0;
        }
        if v == q {
            D_EXACT
        } else if v.starts_with(q) || q.starts_with(&v) {
            D_PREFIX
        } else if v.contains(q) {
            D_CONTAINS
        } else {
            0
        }
    }

    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut best: HashMap<String, (NameInfo, u8, u8, &'static str)> = HashMap::new();
    for info in map.values() {
        for (value, rank, tag) in [
            (info.remark.as_str(), F_REMARK, "备注"),
            (info.alias.as_str(), F_ID, "微信号"),
            (info.username.as_str(), F_ID, "微信号/原始id"),
            (info.nickname.as_str(), F_NICK, "昵称"),
            (info.session_display.as_str(), F_SESSION, "会话显示名"),
        ] {
            let d = degree(&q, value);
            if d == 0 {
                continue;
            }
            let e = best.entry(info.username.clone()).or_insert_with(|| (info.clone(), 0, 0, "未匹配"));
            if d * 10 + rank > e.1 * 10 + e.2 {
                *e = (info.clone(), d, rank, tag);
            }
        }
    }
    let mut out: Vec<(NameInfo, u8, u8, &'static str)> = best.into_values().collect();
    out.sort_by(|a, b| (b.1 * 10 + b.2).cmp(&(a.1 * 10 + a.2)).then_with(|| a.0.best().cmp(b.0.best())));
    out
}

/// 从群消息正文里取 `@昵称` 的昵称部分。微信存的是可见文本「@昵称 」，
/// 到空白或常见标点为止。刻意排除：邮箱局部（前面紧跟字母/数字）、`@所有人`、纯数字。
/// 只做统计用，长度上限 24。
fn extract_at_tokens(text: &str) -> Vec<String> {
    let ch: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i] != '@' {
            i += 1;
            continue;
        }
        let prev_ok = i == 0 || !(ch[i - 1].is_ascii_alphanumeric() || ch[i - 1] == '.' || ch[i - 1] == '_');
        let mut j = i + 1;
        while j < ch.len()
            && !matches!(ch[j], ' ' | '\n' | '\r' | '\t' | '@' | '，' | '。' | '！' | '？'
                | '、' | '；' | ':' | '：' | ')' | '（' | '(' | '"' | '\'' | ']' | '】')
        {
            j += 1;
        }
        let tok: String = ch[i + 1..j].iter().collect();
        let tok = tok.trim();
        let n = tok.chars().count();
        if prev_ok && !tok.is_empty() && n <= 24 && tok != "所有人" && !tok.chars().all(|c| c.is_ascii_digit()) {
            out.push(tok.to_string());
        }
        i = j.max(i + 1);
    }
    out
}

fn is_group(username: &str) -> bool {
    username.ends_with("@chatroom") || username.contains("@chatroom")
}

/// 是否为「整串都是十六进制」的 BLOB 内容（weflow-core 把二进制列 hex 编码后的形态）。
fn is_hex_blob(s: &str) -> bool {
    s.len() >= 48 && s.len() % 2 == 0 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// local_type -> (标签, 可读文本)。type 1 直接给明文；其余给占位并尽量抽标题。
fn render_message(local_type: i64, content: &str) -> (String, String) {
    match local_type {
        1 => {
            // 微信 4.x 把卡片/转发/系统富消息的正文以二进制 BLOB 存储（被 weflow-core hex 化）。
            // 这类内容内嵌他人 wxid、外部链接、价格等，hex 解码后碎片仍会把标识符带到云端——
            // 因此一律降级为占位，绝不把解码碎片出境；真正人手敲的纯文本不是 hex，正常保留。
            if is_hex_blob(content) {
                ("rich".into(), "[卡片/链接消息（已省略原始内容）]".into())
            } else {
                ("text".into(), content.to_string())
            }
        }
        3 => ("image".into(), "[图片]".into()),
        34 => ("voice".into(), "[语音]".into()),
        43 => ("video".into(), "[视频]".into()),
        // 微信 4.x：47 = 动画表情（sticker），它的 XML 里带 cdnurl/thumburl（**普通 HTTP 地址**，
        // 与图片的 Mars filekey 不同）。以前这里误标成"短视频"，导致表情全被当成视频占位。
        47 => ("emoji".into(), "[动画表情]".into()),
        48 => ("location".into(), "[位置]".into()),
        50 => ("call".into(), "[语音/视频通话]".into()),
        // 244813135921 = 引用消息（WeFlow chatService.ts:8365 同一口径），以前落进"非文本"。
        244813135921 => ("quote".into(), extract_quote(content).unwrap_or_else(|| "[引用消息]".into())),
        10000 => ("system".into(), "[系统消息]".into()),
        244813109745 => ("emoji".into(), "[表情]".into()),
        17 => ("merged".into(), extract_title(content).unwrap_or_else(|| "[合并转发消息]".into())),
        49 => ("app".into(), extract_title(content).unwrap_or_else(|| "[链接/小程序/文件分享]".into())),
        // 未识别的类型：文本与标签都**不带原始编号**。那个 11~12 位数字是调试信息，
        // 塞进出境文本里既没信息量，又会被手机号/卡号类规则当成敏感数字（实测被出境自查拦过一次）。
        // 原始值仍随消息一起输出在 local_type 字段里，只留在本机。
        _ => ("other".into(), "[非文本消息]".into()),
    }
}

/// 取 `<tag>…</tag>` 的内文（支持标签带属性、CDATA 包裹）。
fn xml_tag_inner(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let after = start + open.len();
    let rest = &xml[after..];
    let gt = rest.find('>')?;
    if rest[..gt].ends_with('/') {
        return None; // 自闭合
    }
    let body_start = after + gt + 1;
    let close = format!("</{tag}>");
    let end = xml[body_start..].find(&close)? + body_start;
    let mut body = xml[body_start..end].to_string();
    if let Some(s) = body.strip_prefix("<![CDATA[").and_then(|b| b.strip_suffix("]]>")) {
        body = s.to_string();
    }
    Some(xml_unescape(body.trim()))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">")
        .replace("&quot;", "\"").replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn strip_xml_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 引用消息：把被引用的「谁说了什么」抽出来，模型才看得到上下文。
fn extract_quote(content: &str) -> Option<String> {
    let who = xml_tag_inner(content, "displayname").unwrap_or_default();
    let body = xml_tag_inner(content, "content")?;
    let body = strip_xml_tags(&xml_unescape(&body));
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let body: String = body.chars().take(200).collect();
    Some(if who.trim().is_empty() {
        format!("[引用：{body}]")
    } else {
        format!("[引用 {}：{}]", who.trim(), body)
    })
}

/// 从 XML/文本里朴素抽 title 或 des/摘要。
fn extract_title(content: &str) -> Option<String> {
    for key in ["title=\"", "<title>", "des=\"", "source-displayname=\""] {
        if let Some(start) = content.find(key) {
            let rest = &content[start + key.len()..];
            let end = rest.find('"').or_else(|| rest.find('<')).unwrap_or(rest.len());
            let val = rest[..end].trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

struct Args {
    data_root: Option<String>,
    name: Option<String>,
    sid: Option<String>,
    top: bool,
    nongroup: bool,
    n: usize,
    list_accounts: bool,
    dump_schema: bool,
    probe_media: bool,
    at_mentions: bool,
    sessions: bool,
    whoami: bool,
    derive_image_key: bool,
    img_out: Option<String>,
    voice_out: Option<String>,
    ffmpeg: Option<String>,
    pretty: bool,
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().collect();
    let mut a = Args { data_root: None, name: None, sid: None, top: false, nongroup: false, n: 30, list_accounts: false, dump_schema: false, probe_media: false, at_mentions: false, sessions: false, whoami: false, derive_image_key: false, img_out: None, voice_out: None, ffmpeg: None, pretty: false };
    let mut i = 1;
    let mut positional_done = false;
    while i < argv.len() {
        let s = argv[i].as_str();
        match s {
            "--name" => { i += 1; if i < argv.len() { a.name = Some(argv[i].clone()); } }
            "--sid" => { i += 1; if i < argv.len() { a.sid = Some(argv[i].clone()); } }
            "--n" => { i += 1; if i < argv.len() { a.n = argv[i].parse().unwrap_or(30); } }
            "--img-out" => { i += 1; if i < argv.len() { a.img_out = Some(argv[i].clone()); } }
            "--voice-out" => { i += 1; if i < argv.len() { a.voice_out = Some(argv[i].clone()); } }
            "--whoami" => a.whoami = true,
            "--derive-image-key" => a.derive_image_key = true,
            "--ffmpeg" => { i += 1; if i < argv.len() { a.ffmpeg = Some(argv[i].clone()); } }
            "--top" => a.top = true,
            "--nongroup" => a.nongroup = true,
            "--list-accounts" => a.list_accounts = true,
            "--dump-schema" => a.dump_schema = true,
            "--probe-media" => a.probe_media = true,
            "--at-mentions" => a.at_mentions = true,
            "--sessions" => a.sessions = true,
            "--pretty" => a.pretty = true,
            _ => {
                if !s.starts_with("--") && !positional_done && a.data_root.is_none() {
                    a.data_root = Some(s.to_string());
                    positional_done = true;
                }
            }
        }
        i += 1;
    }
    a
}

/// 列出 session 库里的 (username, display)，供 --top 选最近有消息的会话。
/// 微信 4.x 的 SessionTable 有 username 但没有昵称列，展示名一律靠下游 names 映射（contact.db）补，
/// 所以这里优先直接从 SessionTable 取 username。
fn list_session_pairs(account: &Account) -> Vec<(String, String)> {
    let db = &account.layout.session_db;
    if let Ok(tables) = account.with_conn(db, |c| list_tables(c)) {
        // 优先名为 SessionTable 的表（退化：任何含 username 列的会话表）
        let mut target: Option<(String, String)> = None;
        for t in &tables {
            let cols = match account.with_conn(db, |c| table_columns(c, t)) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let user = match find_candidate(&cols, USER_COLS) {
                Some(u) => u,
                None => continue,
            };
            if t.eq_ignore_ascii_case("SessionTable") {
                target = Some((t.clone(), user));
                break;
            }
            if target.is_none() {
                target = Some((t.clone(), user));
            }
        }
        if let Some((table, user)) = target {
            let (table_id, user_id) = (sanitize_ident(&table), sanitize_ident(&user));
            let (table_id, user_id) = match (table_id, user_id) {
                (Ok(t), Ok(u)) => (t, u),
                _ => (String::new(), String::new()),
            };
            if !table_id.is_empty() && !user_id.is_empty() {
                let sql = format!("SELECT \"{user_id}\" FROM \"{table_id}\"");
                if let Ok(rows) = account.with_conn(db, move |conn| {
                    let mut stmt = conn.prepare(&sql)?;
                    let out = stmt
                        .query_map([], |r| r.get::<_, Option<String>>(0))?
                        .filter_map(|x| x.ok().flatten())
                        .collect::<Vec<_>>();
                    Ok(out)
                }) {
                    let pairs: Vec<(String, String)> =
                        rows.into_iter().filter(|u| !u.trim().is_empty()).map(|u| (u, String::new())).collect();
                    if !pairs.is_empty() {
                        return pairs;
                    }
                }
            }
        }
    }
    // 兜底：老逻辑（会话表恰好同时带昵称列时）
    let nc = match pick_name_table(account, db) {
        Ok(Some(nc)) => nc,
        _ => return Vec::new(),
    };
    let rows = match query_rows(account, db, &nc.table, &nc.wanted) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    rows.iter()
        .map(|r| {
            let u = r.get(nc.i_user).and_then(|v| v.clone()).unwrap_or_default();
            let d = nc.i_disp.and_then(|i| r.get(i).and_then(|v| v.clone())).unwrap_or_default();
            (u, d)
        })
        .filter(|(u, _)| !u.trim().is_empty())
        .collect()
}

/// 选账号。store 里可能有**多个**账号的密钥（换过一次登录号就多一把），旧实现按
/// `BTreeMap` 字典序取第一个能打开的 ⇒ 两只都有密钥时可能拿错那只去查会话。
/// 现在：给了 `--sid` 就选「库里真有这个会话」的那只；否则选「session.db 最近还被写过」的
/// （只有当前登录的账号会被微信写），并把候选数量回传给调用方，让歧义可见。
fn account_mtime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

fn pick_account_dir(
    ks: &KeyStore,
    data_root: Option<&str>,
    wanted_sid: Option<&str>,
) -> Option<(PathBuf, String, usize)> {
    let mut accounts: Vec<PathBuf> = Vec::new();
    if let Some(root) = data_root {
        accounts = ks.accounts_under(&PathBuf::from(root));
    }
    if accounts.is_empty() {
        accounts = ks.all_accounts();
    }
    let mut openable: Vec<(PathBuf, String, std::time::SystemTime, bool)> = Vec::new();
    for dir in accounts {
        let session_db = dir.join("db_storage").join("session").join("session.db");
        if !session_db.is_file() {
            continue;
        }
        let key = match ks.session_key_for(&dir) {
            Some(k) => k,
            None => continue,
        };
        let account = match Account::open(session_db.to_string_lossy().as_ref(), &key) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let has_sid = match wanted_sid {
            Some(sid) => list_session_pairs(&account)
                .iter()
                .any(|(u, _)| u.eq_ignore_ascii_case(sid)),
            None => false,
        };
        openable.push((dir, key, account_mtime(&session_db), has_sid));
    }
    let candidates = openable.len();
    if wanted_sid.is_some() {
        if let Some((d, k, _, _)) = openable.iter().find(|x| x.3) {
            return Some((d.clone(), k.clone(), candidates));
        }
    }
    openable
        .into_iter()
        .max_by_key(|x| x.2)
        .map(|x| (x.0, x.1, candidates))
}

/// 本机「现在登的是哪只微信号」探针：只 stat 文件时间，不打开任何数据库、不需要密钥。
/// 判据是 `<root>/<账号>/db_storage/session/session.db` 的写入时间——微信只写当前登录的那只号；
/// `<root>/all_users/login/<wxid>/key_info.db` 作为次要信号（切号/续登时才写）。
/// 之所以必须单独有这个：`pick_account_dir` 只在「keystore 里有密钥」的账号里挑，
/// 换号后新号没密钥 ⇒ 它会安静地继续用旧号数据答话，用户看不出来。
fn login_probe(ks: &KeyStore, data_root: Option<&str>) -> Value {
    let root = match data_root {
        Some(r) => PathBuf::from(r),
        None => return json!({ "available": false, "note": "没传数据根目录" }),
    };
    let now = std::time::SystemTime::now();
    let age_s = |p: &Path| -> Option<i64> {
        let t = std::fs::metadata(p).ok()?.modified().ok()?;
        now.duration_since(t).ok().map(|d| d.as_secs() as i64)
    };
    let mut items: Vec<(i64, Value)> = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return json!({ "available": false, "note": format!("数据根读不到：{}", root.display()) }),
    };
    for ent in entries.flatten() {
        let dir = ent.path();
        let base = match dir.file_name().map(|s| s.to_string_lossy().to_string()) {
            Some(b) => b,
            None => continue,
        };
        let session_db = dir.join("db_storage").join("session").join("session.db");
        if !session_db.is_file() {
            continue;
        }
        let wxid = clean_account_dir_name(&base);
        let session_age = age_s(&session_db).unwrap_or(i64::MAX);
        let login_age = age_s(&root.join("all_users").join("login").join(&wxid).join("key_info.db"));
        let has_key = ks.session_key_for(&dir).is_some();
        // 有密钥 ≠ 密钥能用：重新登录后微信会换钥，store 里的旧钥会静默打不开库，
        // 于是导出器退回另一只有密钥的旧号。这里真的试打开一次，把可用性带回去。
        let key_works = has_key && ks
            .session_key_for(&dir)
            .map(|k| {
                Account::open(session_db.to_string_lossy().as_ref(), &k)
                    .map(|a| {
                        // 打开 sqlcipher 本身对错钥未必报错，真读一行才算数
                        a.with_conn(&session_db, |c| {
                            c.query_row("SELECT name FROM sqlite_master LIMIT 1", [], |r| {
                                r.get::<_, String>(0)
                            })
                            .map_err(anyhow::Error::from)
                        })
                        .is_ok()
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        let score = session_age.min(login_age.unwrap_or(i64::MAX));
        items.push((
            score,
            json!({
                "account_dir": base,
                "wxid": wxid,
                "tail": wxid.chars().rev().take(4).collect::<String>().chars().rev().collect::<String>(),
                "session_db_age_s": session_age,
                "key_info_age_s": login_age,
                "has_db_key": has_key,
                "key_works": key_works,
                "db_key_count": ks.candidate_db_keys(None, Some(&dir)).len(),
            }),
        ));
    }
    if items.is_empty() {
        return json!({ "available": false, "note": "数据根下没有账号目录" });
    }
    items.sort_by_key(|x| x.0);
    let accounts: Vec<Value> = items.iter().map(|x| x.1.clone()).collect();
    let live = &items[0].1;
    let live_dir = live["account_dir"].as_str().unwrap_or("").to_string();
    json!({
        "available": true,
        "accounts": accounts,
        "live_account": live_dir,
        "live_tail": live["tail"],
        "live_age_s": live["session_db_age_s"],
        "live_has_key": live["has_db_key"],
        // 「有密钥」和「密钥能打开当前号的库」是两件事：重登后旧钥失效，
        // 只查前者会让导出器静默退回另一只有效密钥的旧号。
        "live_key_valid": live["key_works"],
        "live_usable": live["key_works"],
        // 只有 1 只号且没密钥时不该吓用户说「登错号」，那是「还没取密钥」
        "other_accounts_without_key": items.iter().filter(|x| !x.1["has_db_key"].as_bool().unwrap_or(false)).count(),
    })
}

/// 把探针结果与「本次实际用了哪只号」对齐成一条可直接给用户看的告警。
fn login_guard(ks: &KeyStore, data_root: Option<&str>, used_dir: &Path) -> Value {
    let mut g = login_probe(ks, data_root);
    if g.get("available").and_then(|v| v.as_bool()) != Some(true) {
        return g;
    }
    let used_base = used_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let live = g["live_account"].as_str().unwrap_or("").to_string();
    let obj = match g.as_object_mut() {
        Some(o) => o,
        None => return g,
    };
    obj.insert("used_account".into(), json!(used_base));
    obj.insert("account_matches_login".into(), json!(used_base == live));
    let live_works = obj
        .get("accounts")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .find(|a| a["account_dir"].as_str() == Some(live.as_str()))
                .and_then(|a| a["key_works"].as_bool())
                .unwrap_or(false)
        })
        .unwrap_or(false);
    obj.insert("live_key_valid".into(), json!(live_works));
    obj.insert(
        "warning".into(),
        json!(if used_base == live {
            Value::Null
        } else if !live_works {
            Value::String(format!(
                "本机登录的是 {}（{} 秒前还在写库），但 store 里它的密钥打不开库（多半是重新登录换了钥）——本次读的是 {}，回「重扫」即可切回当前号",
                live, obj["live_age_s"], used_base
            ))
        } else {
            Value::String(format!(
                "本机登录的是 {}（{} 秒前还在写库），但本次读的是 {}——换号后没取新号密钥，答案来自旧号",
                live, obj["live_age_s"], used_base
            ))
        }),
    );
    g
}

fn emit(v: &Value, pretty: bool) {
    let s = if pretty { serde_json::to_string_pretty(v).unwrap() } else { serde_json::to_string(v).unwrap() };
    println!("{s}");
}

/// 账号目录名清洗 → 本人 wxid（对齐 WeFlow cleanAccountDirName）：
/// 去掉尾部 `_<4 位字母数字>` 哈希段（`wxid_xxx_1234` → `wxid_xxx`）。
fn clean_account_dir_name(base: &str) -> String {
    if let Some(pos) = base.rfind('_') {
        let head = &base[..pos];
        let suffix = &base[pos + 1..];
        if !head.is_empty()
            && suffix.len() == 4
            && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return head.to_string();
        }
    }
    base.to_string()
}

/// 各消息分库的 Name2Id：db 文件名（小写 stem）→ (rowid → user_name)，用于把
/// real_sender_id 还原成真实发送者（微信 4.x 消息表没有 sender_username 列）。
fn load_name2id_all(account: &Account) -> HashMap<String, HashMap<i64, String>> {
    const N2I_NAME: &[&str] = &["user_name", "username", "user_name_", "name"];
    let mut all: HashMap<String, HashMap<i64, String>> = HashMap::new();
    for db in &account.layout.message_dbs {
        let stem = db.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        if stem.is_empty() || all.contains_key(&stem) {
            continue;
        }
        let map = (|| -> Result<HashMap<i64, String>> {
            let tables = account.with_conn(db, |c| list_tables(c))?;
            let table = tables
                .iter()
                .find(|t| t.eq_ignore_ascii_case("Name2Id"))
                .or_else(|| tables.iter().find(|t| t.to_ascii_lowercase().contains("name2id")))
                .cloned()
                .ok_or_else(|| anyhow!("无 Name2Id 表"))?;
            let cols = account.with_conn(db, |c| table_columns(c, &table))?;
            let name_col = find_candidate(&cols, N2I_NAME).ok_or_else(|| anyhow!("Name2Id 无名称列"))?;
            let sql = format!(
                "SELECT rowid, \"{}\" FROM \"{}\"",
                sanitize_ident(&name_col)?,
                sanitize_ident(&table)?
            );
            account.with_conn(db, move |conn| {
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
                Ok(rows.flatten().collect::<HashMap<i64, String>>())
            })
        })()
        .unwrap_or_default();
        all.insert(stem, map);
    }
    all
}

/// 从一段文本里抽 XML 标签的「属性名 → 取值长度」（绝不外泄属性值本身，供摸清 img/voice 结构用）。
fn xml_attr_shape(s: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == '"' || bytes[i] == '\'' {
            let quote = bytes[i];
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != quote {
                j += 1;
            }
            // 往前找 attrname=
            let mut k = i;
            while k > 0 && bytes[k] != '<' && bytes[k] != ' ' && bytes[k] != '\n' && bytes[k] != '\t' {
                k -= 1;
            }
            let name: String = bytes[k..i].iter().collect::<String>().trim().trim_end_matches('=').trim().to_string();
            let value_len = j.saturating_sub(start);
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                out.push((name, value_len));
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 抽 XML「子元素标签名 → 内部文本长度」，覆盖微信 4.x 用 `<tag><![CDATA[值]]></tag>` 承载 md5 /
/// 语音长度等字段的形态。只报标签名与长度，绝不外泄文本值。
fn xml_element_shape(s: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == '<' && (bytes[i + 1].is_alphabetic() || bytes[i + 1] == '_') {
            // 读标签名
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_alphanumeric() || bytes[j] == '_' || bytes[j] == ':') {
                j += 1;
            }
            let tag: String = bytes[i + 1..j].iter().collect();
            // 跳到该标签的 '>'
            let mut gt = j;
            let mut self_closing = false;
            while gt < bytes.len() && bytes[gt] != '>' {
                if bytes[gt] == '/' && gt + 1 < bytes.len() && bytes[gt + 1] == '>' {
                    self_closing = true;
                }
                gt += 1;
            }
            if gt < bytes.len() && !self_closing {
                // 内容起点 → 找到同名闭合标签 </tag>
                let content_start = gt + 1;
                let mut k = content_start;
                while k + 1 < bytes.len() && !(bytes[k] == '<' && bytes[k + 1] == '/') {
                    k += 1;
                }
                // 去掉 CDATA 包裹标记，只数真正的值长度
                let raw: String = bytes[content_start..k.min(content_start)].iter().collect();
                let _ = raw;
                let inner: String = bytes[content_start..k].iter().collect();
                let val = inner
                    .trim()
                    .trim_start_matches("<![CDATA[")
                    .trim_end_matches("]]>")
                    .trim();
                if !val.is_empty() {
                    out.push((tag, val.chars().count()));
                }
            }
            i = gt + 1;
        } else {
            i += 1;
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 判断 hex 串解码后的前几字节「魔数」，只回吐结构信息（是否 zstd / protobuf / 明文 XML），
/// 绝不外泄内容。zstd 帧魔数 = 28 B5 2F FD；明文 XML 以 '<' 开头。
fn blob_magic(hexstr: &str) -> Value {
    if hexstr.len() < 8 || !hexstr.bytes().all(|b| b.is_ascii_hexdigit()) {
        // 非 hex：可能是纯文本，只看是否 '<' 开头
        let t = hexstr.trim_start();
        return json!({"kind": if t.starts_with('<') { "xml_text" } else { "text" }, "byte_len": t.len()});
    }
    let raw: Vec<u8> = (0..hexstr.len() / 2)
        .map(|i| u8::from_str_radix(&hexstr[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect();
    let head: Vec<String> = raw.iter().take(4).map(|b| format!("{b:02X}")).collect();
    let is_zstd = raw.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]);
    let starts_lt = raw.first() == Some(&b'<');
    let kind = if is_zstd { "zstd" } else if starts_lt { "xml" } else { "binary" };
    json!({"kind": kind, "magic": head.join(" "), "byte_len": raw.len()})
}

/// 解码 hex→bytes→zstd 帧为文本（供 probe 摸清解压后的 XML 结构）。失败返回 None。
fn zstd_decode_hex(hexstr: &str) -> Option<String> {
    if hexstr.is_empty() || hexstr.len() % 2 != 0 || !hexstr.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let raw: Vec<u8> = (0..hexstr.len() / 2)
        .map(|i| u8::from_str_radix(&hexstr[i * 2..i * 2 + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    if !raw.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        return None;
    }
    let mut dec = ruzstd::decoding::StreamingDecoder::new(&raw[..]).ok()?;
    let mut out = Vec::new();
    use std::io::Read;
    dec.read_to_end(&mut out).ok()?;
    Some(String::from_utf8_lossy(&out).to_string())
}

/// 从语音消息正文（zstd 压缩 XML）取 `voicelength="…"`（毫秒）。取不到返回 None。
fn voice_duration_ms(content: &str) -> Option<i64> {
    let xml = zstd_decode_hex(content).unwrap_or_else(|| content.to_string());
    let i = xml.find("voicelength=\"")? + "voicelength=\"".len();
    let rest = &xml[i..];
    let end = rest.find('"')?;
    rest[..end].trim().parse::<i64>().ok()
}

/// 扫 `%APPDATA%\Tencent\**\kvcomm\key_<code>_*.statistic` 取图片密钥的 code 候选（按 mtime 新→旧）。
/// 文件名里的 code 是明文，密钥本身要按 wxid 派生；绝不往微信数据目录写任何东西。
fn kvcomm_codes() -> Vec<(String, std::time::SystemTime)> {
    use std::time::UNIX_EPOCH;
    let appdata = match std::env::var("APPDATA") {
        Ok(v) => PathBuf::from(v),
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(String, std::time::SystemTime)> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(appdata.join("Tencent"), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 6 {
            continue;
        }
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                stack.push((p, depth + 1));
                continue;
            }
            if p.parent().and_then(|d| d.file_name()).map(|n| n.eq_ignore_ascii_case("kvcomm")) != Some(true) {
                continue;
            }
            let base = match p.file_name().map(|s| s.to_string_lossy().to_string()) {
                Some(b) => b,
                None => continue,
            };
            let rest = match base.strip_prefix("key_").and_then(|r| r.split_once('_')) {
                Some((code, tail)) if tail.ends_with(".statistic") => code,
                _ => continue,
            };
            if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let mt = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            out.push((rest.to_string(), mt));
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// 在账号目录里找几个本地 .dat 用于验证派生出来的图片密钥（最多 3 个、最多走 600 个文件）。
fn dat_samples(acct_dir: &Path) -> Vec<PathBuf> {
    let root = acct_dir.join("msg").join("attach");
    let mut out = Vec::new();
    let mut stack = vec![(root, 0usize)];
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if depth > 6 || out.len() >= 3 || seen >= 600 {
            continue;
        }
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                stack.push((p, depth + 1));
                continue;
            }
            seen += 1;
            if p.extension().map(|e| e.eq_ignore_ascii_case("dat")).unwrap_or(false) {
                out.push(p);
                if out.len() >= 3 {
                    break;
                }
            }
        }
    }
    out
}

/// 图片密钥自动派生（换号后必做）：code 来自 kvcomm 文件名，
/// `xor = code & 0xff`、`aes = md5(code + 清洗后wxid)[:16]`；用本机 .dat 试解来验证，
/// 不做"按最新文件猜"这种会在多账号机器上串号的假设。
fn derive_image_key(ks: &mut KeyStore, acct_dir: &Path, write: bool) -> Value {
    let base = acct_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let wxid = clean_account_dir_name(&base);
    let codes = kvcomm_codes();
    if codes.is_empty() {
        return json!({"ok": false, "reason": "没找到 kvcomm 下的 key_<code>_*.statistic"});
    }
    let samples = dat_samples(acct_dir);
    let mut tried = 0usize;
    let mut first_unverified: Option<(u64, String, String)> = None;
    for (code, _) in &codes {
        tried += 1;
        let xor = code.parse::<u64>().unwrap_or(0) & 0xff;
        let aes = format!("{:x}", md5::compute(format!("{code}{wxid}")))[..16].to_string();
        if first_unverified.is_none() {
            first_unverified = Some((xor, aes.clone(), code.clone()));
        }
        if samples.is_empty() {
            continue;
        }
        let mut hit = false;
        for p in &samples {
            let bytes = match std::fs::read(p) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if let Ok(d) = weflow_core::dat::decrypt_dat(&bytes, xor as u32, Some(&aes)) {
                if d.is_wxgf || weflow_core::dat::detect_ext(&d.data).is_some() {
                    hit = true;
                    break;
                }
            }
        }
        if hit {
            if write {
                ks.image_keys.insert(base.clone(), weflow_core::keystore::ImageKeyEntry {
                    xor_key: xor,
                    aes_key: Some(aes),
                });
                let _ = ks.save(&weflow_core::keystore::store_path());
            }
            return json!({
                "ok": true, "verified": true, "written": write,
                "xor_key": xor, "aes_len": 16, "code_len": code.len(),
                "tried_codes": tried, "samples": samples.len(),
            });
        }
    }
    // 没有本机 .dat 可验证时：给出最新 code 的派生结果，但如实标 unverified，
    // 由调用方决定要不要写（默认不写，宁可图片解不开也不串到别人账号的密钥）。
    match first_unverified {
        Some((xor, aes, code)) => json!({
            "ok": true, "verified": false, "written": false,
            "xor_key": xor, "aes_len": aes.len(), "code_len": code.len(),
            "tried_codes": tried, "samples": 0,
            "note": "本机没有该账号的 .dat 可试解，派生结果未经校验，未写入",
        }),
        None => json!({"ok": false, "reason": "没有可用 code"}),
    }
}

/// 布局探测报告进导出 JSON：微信改目录/文件名时，第一眼就能看到"哪个库没找到"。
fn layout_json(layout: &weflow_core::db::DbLayout) -> Value {
    let mut o = Map::new();
    for (name, ok, detail) in layout.summary() {
        o.insert(name, json!({ "found": ok, "detail": detail }));
    }
    Value::Object(o)
}

/// 媒体诊断落盘（路径由调用方用 REPLY_MEDIA_DIAG 指定；未设置就不写）。
/// 只收失败原因、命中路径、密钥指纹（不可逆 8 位），绝不写消息内容/明文密钥。
fn write_media_diag(lines: &[String]) {
    let path = match std::env::var("REPLY_MEDIA_DIAG") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => return,
    };
    let p = Path::new(&path);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 文件超过 2MB 只留尾部 1MB（按字符边界 + 行边界切）
    if let Ok(md) = std::fs::metadata(p) {
        if md.len() > 2 * 1024 * 1024 {
            if let Ok(s) = std::fs::read_to_string(p) {
                let mut cut = s.len().saturating_sub(1024 * 1024);
                while cut < s.len() && !s.is_char_boundary(cut) {
                    cut += 1;
                }
                let at = s[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(cut);
                let _ = std::fs::write(p, &s[at..]);
            }
        }
    }
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    // 跨运行去重：同一条"原因+路径"已经在最近 400 行里出现过就不再追加，
    // 否则每跑一次同样的命令都会把同一批行再写一遍，日志很快被噪声淹掉。
    let mut recent: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(s) = std::fs::read_to_string(p) {
        for l in s.lines().rev().take(400) {
            if let Some(i) = l.find("] ") {
                recent.insert(l[i + 2..].to_string());
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut body = String::new();
    for l in lines.iter().take(400) {
        if !seen.insert(l.as_str()) || recent.contains(l) {
            continue;
        }
        body.push_str(&format!("[{now}] {l}\n"));
    }
    if !body.is_empty() {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = f.write_all(body.as_bytes());
        }
    }
}

fn main() -> Result<()> {
    let args = parse_args();
    let mut ks = KeyStore::load_default();
    if args.list_accounts {
        let list: Vec<Value> = ks
            .all_accounts()
            .iter()
            .map(|d| {
                let session_db = d.join("db_storage").join("session").join("session.db");
                json!({
                    "account_dir": d.to_string_lossy(),
                    "has_session_key": ks.session_key_for(d).is_some(),
                    "db_count": ks.candidate_db_keys(None, Some(d)).len(),
                    "session_db_exists": session_db.is_file(),
                })
            })
            .collect();
        emit(&json!({ "accounts": list, "login": login_probe(&ks, args.data_root.as_deref()) }), args.pretty);
        return Ok(());
    }

    // --whoami：本机登录的是谁（昵称/微信号/wxid）。没有密钥也能回登录探针，
    // 只是 account 段为空 —— 入口启动横幅靠它，不能因为还没取密钥就整条失败。
    if args.whoami {
        let login = login_probe(&ks, args.data_root.as_deref());
        let mut acc = Value::Null;
        let mut note: Option<String> = None;
        match pick_account_dir(&ks, args.data_root.as_deref(), args.sid.as_deref()) {
            Some((dir, key, _)) => {
                let session_db = dir.join("db_storage").join("session").join("session.db");
                match Account::open(session_db.to_string_lossy().as_ref(), &key) {
                    Ok(account) => {
                        let my_id = std::env::var("WEFLOW_ACCOUNT_ID")
                            .ok()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .or_else(|| dir.file_name().map(|s| clean_account_dir_name(&s.to_string_lossy())))
                            .unwrap_or_default();
                        let mut nick = Value::Null;
                        let mut alias = Value::Null;
                        let mut remark = Value::Null;
                        if let Some(cdb) = &account.layout.contact_db {
                            if let Ok(Some(nc)) = pick_name_table(&account, cdb) {
                                // 列名也来自库里读到的 schema，和表名一样不能直接拼
                                // （以前只对表名和 WHERE 列做了 sanitize，SELECT 列表漏了）
                                let cols = nc
                                    .wanted
                                    .iter()
                                    .map(|c| sanitize_ident(c))
                                    .collect::<anyhow::Result<Vec<_>>>()?
                                    .join(", ");
                                let sql = format!(
                                    "SELECT {} FROM {} WHERE {} = ?1 LIMIT 1",
                                    cols,
                                    sanitize_ident(&nc.table)?,
                                    sanitize_ident(&nc.wanted[nc.i_user])?
                                );
                                let row = account
                                    .with_conn(cdb, |c| {
                                        let mut st = c.prepare(&sql)?;
                                        let mut rows = st.query([my_id.as_str()])?;
                                        let mut got = None;
                                        if let Some(r) = rows.next()? {
                                            let get = |i: Option<usize>| -> Option<String> {
                                                i.and_then(|i| r.get::<_, Option<String>>(i).ok().flatten())
                                                    .map(|s| s.trim().to_string())
                                                    .filter(|s| !s.is_empty())
                                            };
                                            got = Some((get(nc.i_disp), get(nc.i_alias), get(nc.i_remark)));
                                        }
                                        Ok(got)
                                    })
                                    .ok()
                                    .flatten();
                                if let Some((n, a, r)) = row {
                                    nick = json!(n);
                                    alias = json!(a);
                                    remark = json!(r);
                                }
                            } else {
                                note = Some("contact.db 里没找到带昵称列的联系人表".into());
                            }
                        }
                        acc = json!({
                            "wxid": my_id,
                            "account_dir": dir.to_string_lossy(),
                            "nick_name": nick,
                            "alias": alias,
                            "remark": remark,
                        });
                    }
                    Err(e) => note = Some(format!("打开账号库失败：{e}")),
                }
            }
            None => note = Some("keystore 里没有可打开的账号（还没取密钥）".into()),
        }
        emit(&json!({ "login": login, "account": acc, "note": note }), args.pretty);
        return Ok(());
    }

    // 图片密钥自动派生：换号后 store 里没有新号的 image_keys，图片会全部解不开。
    // 用本机 .dat 试解来验证候选 code，验证通过才写 store。
    if args.derive_image_key {
        match pick_account_dir(&ks, args.data_root.as_deref(), None) {
            Some((dir, _key, _)) => {
                let out = derive_image_key(&mut ks, &dir, true);
                emit(&json!({"account_dir": dir.to_string_lossy(), "result": out}), args.pretty);
            }
            None => {
                emit(&json!({"ok": false, "reason": "keystore 里没有可打开账号"}), args.pretty);
                std::process::exit(5);
            }
        }
        return Ok(());
    }

    let (acct_dir, session_key, acct_candidates) =
        match pick_account_dir(&ks, args.data_root.as_deref(), args.sid.as_deref()) {
            Some(x) => x,
            None => {
                emit(
                    &json!({
                        "error": "keystore 里没有可打开的账号（先运行 keyscan_readonly.py 取密钥）",
                        "login": login_probe(&ks, args.data_root.as_deref()),
                    }),
                    args.pretty,
                );
                std::process::exit(5);
            }
        };
    let session_db = acct_dir.join("db_storage").join("session").join("session.db");
    let account = Account::open(session_db.to_string_lossy().as_ref(), &session_key)?;

    if args.dump_schema {
        let one = |db: &Path| -> Value {
            let tables = account.with_conn(db, |c| list_tables(c)).unwrap_or_default();
            let mut per: Vec<Value> = Vec::new();
            for t in &tables {
                let cols = account.with_conn(db, |c| table_columns(c, t)).unwrap_or_default();
                per.push(json!({"table": t, "columns": cols}));
            }
            json!({"db": db.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                    "table_count": tables.len(), "tables": per})
        };
        let mut dbs = vec![one(&account.layout.session_db)];
        if let Some(c) = &account.layout.contact_db {
            dbs.push(one(c));
        }
        if let Some(first_msg) = account.layout.message_dbs.first() {
            // 消息库只看表名与首表列名，避免刷屏
            let tables = account.with_conn(first_msg, |c| list_tables(c)).unwrap_or_default();
            let sample_cols = tables.first().and_then(|t| account.with_conn(first_msg, |c| table_columns(c, t)).ok());
            let msg_tbl = tables.iter().find(|t| t.to_ascii_lowercase().starts_with("msg_"));
            let msg_cols = msg_tbl.and_then(|t| account.with_conn(first_msg, |c| table_columns(c, t)).ok());
            dbs.push(json!({"db": "message_dbs", "message_db_count": account.layout.message_dbs.len(),
                            "first_db_table_count": tables.len(), "first_db_tables": tables,
                            "first_table": tables.first(), "first_table_columns": sample_cols,
                            "sample_msg_table": msg_tbl, "sample_msg_columns": msg_cols}));
        }
        emit(&json!({"dbs": dbs}), args.pretty);
        return Ok(());
    }

    if args.probe_media {
        // 只读摸结构：hardlink.db 表/列（供以后 message→.dat 路径解析）+ 若干 type-3/34 消息
        // 的 XML「属性名 → 值长度」形状。绝不打印属性值本身（内含 md5/cdn url/他人标识）。
        let mut hardlink = Value::Null;
        if let Some(hd) = &account.layout.hardlink_db {
            let tables = account.with_conn(hd, |c| list_tables(c)).unwrap_or_default();
            let mut per: Vec<Value> = Vec::new();
            for t in &tables {
                let cols = account.with_conn(hd, |c| table_columns(c, t)).unwrap_or_default();
                per.push(json!({"table": t, "columns": cols}));
            }
            hardlink = json!({
                "db": hd.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                "exists": hd.is_file(),
                "table_count": tables.len(),
                "tables": per,
            });
        }

        // 扫描会话，收集少量图片(3)/语音(34)消息的 XML 属性形状 + content 长度分档
        let mut img_shapes: Vec<Value> = Vec::new();
        let mut voice_shapes: Vec<Value> = Vec::new();
        // 端到端验证：对前几张图片跑 md5→attach 路径→.dat→解密，只报结构，绝不吐字节内容。
        let acct_basename = acct_dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let ik = ks.image_key(Some(acct_basename.as_str())).or_else(|| ks.image_key(None));
        let xor_key = ik.map(|e| (e.xor_key & 0xff) as u32).unwrap_or(0);
        let aes_key: Option<String> = ik.and_then(|e| e.aes_key.clone());
        // 诊断模式故意把全量索引也建出来（下面 rows3 要拿它做对照），
        // 但定位本身一律走 Locator，这样 hits 就是「直拼成功 / 退化用索引 / 没定位到」的实测数。
        let attach_idx = media::attach_index(&account);
        let mut probe_loc = media::Locator::new();
        let mut st_checked = 0usize;
        let mut st_md5 = 0usize;
        let mut st_resolved = 0usize;
        let mut st_dat_exists = 0usize;
        let mut st_decrypted = 0usize;
        let mut ext_counts: HashMap<String, usize> = HashMap::new();
        // 载入 hardlink 三个哈希列，用于判断消息 img md5 到底落在哪一列（或都不落＝未下载）。
        let (hl_md5set, hl_hashset, hl_baseset): (
            std::collections::HashSet<String>,
            std::collections::HashSet<String>,
            std::collections::HashSet<String>,
        ) = if let Some(hd) = &account.layout.hardlink_db {
            account
                .with_conn(hd, |c| -> Result<(std::collections::HashSet<String>, std::collections::HashSet<String>, std::collections::HashSet<String>)> {
                    let mut a = std::collections::HashSet::new();
                    let mut b = std::collections::HashSet::new();
                    let mut d = std::collections::HashSet::new();
                    if let Ok(mut s) = c.prepare("SELECT \"md5\", \"file_name\" FROM \"image_hardlink_info_v4\"") {
                        if let Ok(rows) = s.query_map([], |r| Ok((r.get::<_, String>(0).unwrap_or_default(), r.get::<_, String>(1).unwrap_or_default()))) {
                            for (m, f) in rows.flatten() {
                                a.insert(m.to_ascii_lowercase());
                                d.insert(f.to_ascii_lowercase());
                            }
                        }
                    }
                    if let Ok(mut s2) = c.prepare("SELECT \"md5_hash\" FROM \"image_hardlink_info_v4\"") {
                        if let Ok(rows) = s2.query_map([], |r| r.get::<_, String>(0)) {
                            for v in rows.flatten() {
                                b.insert(v.to_ascii_lowercase());
                            }
                        }
                    }
                    Ok((a, b, d))
                })
                .unwrap_or_default()
        } else {
            Default::default()
        };
        let mut first_md5: Option<String> = None;
        let mut first_flags = Value::Null;
        let mut st_in_md5col = 0usize;
        let mut st_in_hashcol = 0usize;
        let mut st_in_base = 0usize;
        let mut recorded_sid = false;
        let mut st_voice_checked = 0usize;
        let mut st_voice_found = 0usize;
        let mut voice_recorded = false;
        let mut voice_first_info: Option<Value> = None;
        // 诊断：attach 根是否存在、索引了多少文件、hardlink 图片表行数、file_name 形态（只报长度/是否含斜杠）
        let attach_root = acct_dir.join("msg").join("attach");
        let mut hl_probe = Value::Null;
        if let Some(hd) = &account.layout.hardlink_db {
            let cols = account.columns(hd, "image_hardlink_info_v4").unwrap_or_default();
            if cols.iter().any(|c| c.eq_ignore_ascii_case("file_name")) {
                let rows3: Vec<Value> = account
                    .with_conn(hd, |c| -> Result<Vec<Value>> {
                        let mut s = c.prepare(
                            "SELECT \"md5\", \"file_name\", \"dir1\", \"dir2\" FROM \"image_hardlink_info_v4\" LIMIT 5",
                        )?;
                        let out = s
                            .query_map([], |r| {
                                let md5v: String = r.get(0).unwrap_or_default();
                                let fname: String = r.get(1).unwrap_or_default();
                                let d1: String = r.get(2).unwrap_or_default();
                                let d2: String = r.get(3).unwrap_or_default();
                                Ok((md5v, fname, d1, d2))
                            })?
                            .flatten()
                            .map(|(md5v, fname, d1, d2)| {
                                let base = fname.trim_end_matches(".dat").to_ascii_lowercase();
                                let shape = |v: &str| -> String {
                                    if v.is_empty() {
                                        "empty".to_string()
                                    } else if v.chars().all(|c| c.is_ascii_digit()) {
                                        format!("digits(len={})", v.chars().count())
                                    } else if v.len() == 32 && v.bytes().all(|c| c.is_ascii_hexdigit()) {
                                        "hex32".to_string()
                                    } else if v.contains('/') || v.contains('\\') {
                                        format!("pathish(len={})", v.chars().count())
                                    } else {
                                        format!("other(len={})", v.chars().count())
                                    }
                                };
                                json!({
                                    "md5_len": md5v.chars().count(),
                                    "fname_len": fname.chars().count(),
                                    "fname_eq_md5dat": base == md5v.to_ascii_lowercase(),
                                    "fname_in_attach_idx": attach_idx.contains_key(&fname.to_ascii_lowercase()),
                                    "md5_in_attach_idx": attach_idx.contains_key(&format!("{}.dat", md5v.to_ascii_lowercase())),
                                    "dir1_shape": shape(&d1),
                                    "dir2_shape": shape(&d2),
                                    "direct_join_exists": media::direct_path_probe(&attach_root, &fname, &d1, &d2),
                                })
                            })
                            .collect();
                        Ok(out)
                    })
                    .unwrap_or_default();
                hl_probe = json!({ "rows": rows3 });
            }
        }
        let _diag = json!({
            "attach_root_exists": attach_root.is_dir(),
            "attach_index_len": attach_idx.len(),
            "hardlink": hl_probe,
        });
        eprintln!("MEDIA_DIAG {}", serde_json::to_string(&_diag).unwrap());
        for (u, _d) in list_session_pairs(&account) {
            if st_checked >= 400 {
                break;
            }
            let rows = match session_message_rows_recent(&account, &u, 80) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for row in rows.iter().take(80) {
                let m = match row.as_object() {
                    Some(m) => m,
                    None => continue,
                };
                let lt = as_i64(ci_get(m, "local_type"));
                let content = as_str(ci_get(m, "message_content"));
                let compress = as_str(ci_get(m, "compress_content"));
                if content.is_empty() && compress.is_empty() {
                    continue;
                }
                let looks_xml = content.trim_start().starts_with('<') || content.contains("<msg");
                // 解压后的 XML（type 3/34 内容被 zstd 压缩）：只报结构，不外泄值。
                let decoded = zstd_decode_hex(&content).or_else(|| {
                    if looks_xml { Some(content.clone()) } else { None }
                });
                let mut dec_attrs: Vec<Value> = Vec::new();
                let mut dec_elems: Vec<Value> = Vec::new();
                let mut dec_head_is_xml = false;
                if let Some(dx) = &decoded {
                    dec_head_is_xml = dx.trim_start().starts_with('<');
                    dec_attrs = xml_attr_shape(dx)
                        .into_iter()
                        .map(|(n, l)| json!({"attr": n, "len": l}))
                        .collect();
                    dec_elems = xml_element_shape(dx)
                        .into_iter()
                        .map(|(n, l)| json!({"elem": n, "len": l}))
                        .collect();
                }
                let mut shape = json!({
                    "local_type": lt,
                    "content_len": content.len(),
                    "looks_xml": looks_xml,
                    "is_hex": is_hex_blob(&content),
                    "content_magic": blob_magic(&content),
                    "compress_present": !compress.is_empty(),
                    "compress_magic": blob_magic(&compress),
                    "decoded_is_xml": dec_head_is_xml,
                    "decoded_attrs": dec_attrs,
                    "decoded_elems": dec_elems,
                });
                if lt == 3 {
                    // 命中率统计：对所有图片消息尝试 md5→resolve→decrypt（限量），
                    // 前几张额外挂 pipeline 明细。只统计，绝不含字节内容。
                    let md5opt = decoded.as_deref().and_then(media::img_md5_from_xml);
                    if md5opt.is_some() {
                        st_md5 += 1;
                    }
                    if st_checked < 400 {
                        st_checked += 1;
                        if let Some(md5) = &md5opt {
                            let in_md5col = hl_md5set.contains(md5);
                            let in_hashcol = hl_hashset.contains(md5);
                            let in_base = hl_baseset.contains(&format!("{md5}.dat"));
                            if in_md5col {
                                st_in_md5col += 1;
                            }
                            if in_hashcol {
                                st_in_hashcol += 1;
                            }
                            if in_base {
                                st_in_base += 1;
                            }
                            if first_md5.is_none() {
                                first_md5 = Some(md5.clone());
                                first_flags = json!({ "in_md5col": in_md5col, "in_hashcol": in_hashcol, "in_base": in_base });
                            }
                            let probe_dat = media::dat_name_from_packed(
                                &as_str(ci_get(m, "packed_info_data")));
                            let resolved = probe_loc.resolve(&account, md5, probe_dat.as_deref(), &u);
                            let dat_exists = resolved.as_ref().map(|p| p.is_file()).unwrap_or(false);
                            let dec = resolved
                                .as_ref()
                                .filter(|p| p.is_file())
                                .and_then(|p| media::decrypt_dat_file(p, xor_key, aes_key.as_deref()).ok());
                            if resolved.is_some() {
                                st_resolved += 1;
                            }
                            if dat_exists {
                                st_dat_exists += 1;
                            }
                            if let Some(d) = &dec {
                                st_decrypted += 1;
                                if !recorded_sid {
                                    recorded_sid = true;
                                    let _ = std::fs::write(std::env::temp_dir().join("reply_probe_sid.txt"), u.as_bytes());
                                    let _ = std::fs::write(std::env::temp_dir().join("reply_probe_md5.txt"), md5.as_bytes());
                                }
                                let key = if d.is_wxgf { "wxgf".to_string() } else { d.ext.clone() };
                                *ext_counts.entry(key).or_insert(0) += 1;
                            }
                            if img_shapes.len() < 6 {
                                shape["pipeline"] = json!({
                                    "md5_len": md5.len(),
                                    "key_xor_low": format!("{:02X}", xor_key & 0xff),
                                    "has_aes": aes_key.is_some(),
                                    "dat_resolved": resolved.is_some(),
                                    "dat_exists": dat_exists,
                                    "decrypted": dec.is_some(),
                                    "decrypted_ext": dec.as_ref().map(|d| d.ext.clone()).unwrap_or_default(),
                                    "decrypted_version": dec.as_ref().map(|d| d.version).unwrap_or(0),
                                    "decrypted_bytes": dec.as_ref().map(|d| d.data.len()).unwrap_or(0),
                                });
                            }
                        }
                    }
                }
                if lt == 34 && st_voice_checked < 200 {
                    st_voice_checked += 1;
                    let ct = as_i64(ci_get(m, "create_time"));
                    let lid = as_i64(ci_get(m, "local_id").or_else(|| ci_get(m, "localId")));
                    let svr = as_i64(
                        ci_get(m, "server_id")
                            .or_else(|| ci_get(m, "svr_id"))
                            .or_else(|| ci_get(m, "svrid")),
                    );
                    let sender = as_str(ci_get(m, "sender_username")).to_string();
                    let mut vn: Vec<String> = vec![u.clone()];
                    if !sender.is_empty() {
                        vn.push(sender);
                    }
                    vn.sort();
                    vn.dedup();
                    let (probe_voice, _route) = media::voice_silk_bytes(&account, ct, lid, svr, &vn);
                    if let Some(b) = probe_voice {
                        st_voice_found += 1;
                        if !voice_recorded {
                            voice_recorded = true;
                            let head: String = b.iter().take(16).map(|x| format!("{:02x}", x)).collect();
                            let _ = std::fs::write(std::env::temp_dir().join("reply_probe_voice.silk"), &b);
                            let _ = std::fs::write(std::env::temp_dir().join("reply_probe_voice_sid.txt"), u.as_bytes());
                            voice_first_info = Some(json!({
                                "bytes": b.len(),
                                "head_hex": head,
                                "sid_prefix": u.chars().take(6).collect::<String>(),
                                "has_svr": svr != 0,
                                "has_local": lid != 0,
                            }));
                        }
                    }
                }
                let kind = match lt {
                    3 if img_shapes.len() < 6 => Some(&mut img_shapes),
                    34 if voice_shapes.len() < 4 => Some(&mut voice_shapes),
                    _ => None,
                };
                if let Some(dst) = kind {
                    dst.push(shape);
                }
            }
        }

        let img_stats = json!({
            "checked": st_checked,
            "md5_found": st_md5,
            "in_md5col": st_in_md5col,
            "in_hashcol": st_in_hashcol,
            "in_filebase": st_in_base,
            "dat_resolved": st_resolved,
            "dat_exists": st_dat_exists,
            "decrypted_ok": st_decrypted,
            "ext_counts": ext_counts,
            "first_md5_prefix": first_md5.as_ref().map(|m| m.chars().take(10).collect::<String>()),
            "first_flags": first_flags,
            "hl_md5set_len": hl_md5set.len(),
            "hl_hashset_len": hl_hashset.len(),
            "hl_baseset_len": hl_baseset.len(),
            // 定位途径实测数：scoped=只扫本会话子树就找到（快路），
            // via_full_index=退化去扫整棵 attach（本机 6.9 万文件），direct=hardlink 目录列直拼
            "locator_hits": {
                "direct": probe_loc.hits[0],
                "scoped": probe_loc.hits[1],
                "via_full_index": probe_loc.hits[2],
                "miss": probe_loc.hits[3],
                "full_walk_used": probe_loc.full_walk_used,
            },
        });
        eprintln!(
            "MEDIA_STATS {}",
            serde_json::to_string(&json!({ "images": img_stats })).unwrap()
        );
        let voice_stats = json!({
            "checked": st_voice_checked,
            "silk_found": st_voice_found,
            "first": voice_first_info,
        });
        eprintln!(
            "VOICE_STATS {}",
            serde_json::to_string(&voice_stats).unwrap()
        );
        emit(
            &json!({
                "hardlink": hardlink,
                "image_stats": img_stats,
                "image_samples": img_shapes,
                "voice_stats": voice_stats,
                "voice_samples": voice_shapes,
            }),
            args.pretty,
        );
        return Ok(());
    }

    if args.sessions {
        // 会话清单：每个会话最近 n 条里「判为我 / 判为对方 / 有几个不同发言人」。
        // 用途有二：① 核对群聊里的本人判定是否系统性失效（答错人的根源）；
        // ② 给「针对群内某几个成员回应」提供发言人候选池。只读本机库，不改任何东西。
        let my_id = std::env::var("WEFLOW_ACCOUNT_ID")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                clean_account_dir_name(
                    &acct_dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                )
            });
        let mut names_local: HashMap<String, NameInfo> = HashMap::new();
        load_names(&account, &account.layout.session_db, &mut names_local);
        if let Some(c) = &account.layout.contact_db {
            load_names(&account, c, &mut names_local);
        }
        let n2i = load_name2id_all(&account);
        let mut list: Vec<Value> = Vec::new();
        for (u, disp) in list_session_pairs(&account) {
            let rows = match session_message_rows_recent(&account, &u, args.n) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if rows.is_empty() {
                continue;
            }
            let group = is_group(&u);
            let mut mine = 0usize;
            let mut senders: HashMap<String, usize> = HashMap::new();
            for row in rows.iter() {
                let m = match row.as_object() {
                    Some(m) => m,
                    None => continue,
                };
                let mut sender = as_str(ci_get(m, "sender_username")).to_string();
                if sender.is_empty() {
                    let rid = as_i64(ci_get(m, "real_sender_id").or_else(|| ci_get(m, "realSenderId")));
                    let db_name = as_str(ci_get(m, "db_name")).to_lowercase();
                    if rid != 0 {
                        if let Some(uname) = n2i.get(&db_name).and_then(|mp| mp.get(&rid)) {
                            sender = uname.clone();
                        }
                    }
                }
                let is_me = if !my_id.is_empty() {
                    !sender.is_empty() && sender == my_id
                } else if !group {
                    !sender.is_empty() && sender != u
                } else {
                    false
                };
                if is_me {
                    mine += 1;
                } else {
                    let d = match names_local.get(&sender) {
                        Some(n) => n.best().to_string(),
                        None => sender.clone(),
                    };
                    *senders.entry(d).or_insert(0) += 1;
                }
            }
            let mut top: Vec<(String, usize)> = senders.into_iter().collect();
            top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            list.push(json!({
                "username": u,
                "display": disp,
                "is_group": group,
                "messages": rows.len(),
                "mine": mine,
                "distinct_others": top.len(),
                "others": top.iter().take(12).map(|(k, v)| json!({"display": k, "count": v})).collect::<Vec<Value>>(),
            }));
        }
        list.sort_by(|a, b| b["messages"].as_u64().unwrap_or(0).cmp(&a["messages"].as_u64().unwrap_or(0)));
        emit(&json!({ "account": my_id, "session_count": list.len(), "sessions": list }), args.pretty);
        return Ok(());
    }

    if args.at_mentions {
        // 群消息正文里的 @成员昵称：既不在人名词表、也不是本批消息的 sender_display，
        // 脱敏层的规则覆盖不到（需求 §5 缺口，实测原样出境过）。这里**只提取计数**，
        // 是否并入词表由调用方（Python 侧，先备份）决定；导出器绝不写任何规则文件。
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut groups = 0usize;
        let mut scanned = 0usize;
        for (u, _disp) in list_session_pairs(&account) {
            if !is_group(&u) {
                continue;
            }
            groups += 1;
            let rows = match session_message_rows_recent(&account, &u, args.n) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for row in rows.iter() {
                let m = match row.as_object() {
                    Some(m) => m,
                    None => continue,
                };
                let content = as_str(ci_get(m, "message_content"));
                if content.is_empty() {
                    continue;
                }
                let (_label, text) = render_message(as_i64(ci_get(m, "local_type")), &content);
                scanned += 1;
                for tok in extract_at_tokens(&text) {
                    *counts.entry(tok).or_insert(0) += 1;
                }
            }
        }
        let mut list: Vec<Value> = counts
            .into_iter()
            .map(|(name, count)| json!({"name": name, "count": count}))
            .collect();
        list.sort_by(|a, b| b["count"].as_u64().unwrap_or(0).cmp(&a["count"].as_u64().unwrap_or(0))
            .then_with(|| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))));
        emit(
            &json!({ "groups": groups, "messages_scanned": scanned,
                     "distinct_mentions": list.len(), "mentions": list }),
            args.pretty,
        );
        return Ok(());
    }

    // 汇总会话 + 联系人名表
    let mut names: HashMap<String, NameInfo> = HashMap::new();
    load_names(&account, &account.layout.session_db, &mut names);
    if let Some(contact_db) = &account.layout.contact_db {
        load_names(&account, contact_db, &mut names);
    }

    let sid = match &args.sid {
        Some(s) => s.clone(),
        None if args.top => {
            let mut picked: Option<String> = None;
            for (u, _d) in list_session_pairs(&account) {
                if args.nongroup && is_group(&u) {
                    continue;
                }
                if let Ok(rows) = session_message_rows_recent(&account, &u, 1) {
                    if !rows.is_empty() {
                        picked = Some(u);
                        break;
                    }
                }
            }
            match picked {
                Some(u) => u,
                None => {
                    emit(&json!({"error": "--top 未找到任何有消息的会话"}), args.pretty);
                    std::process::exit(4);
                }
            }
        }
        None => {
            let q = args.name.clone().ok_or_else(|| anyhow!("需 --name 或 --sid"))?;
            let ranked = ranked_matches(&names, &q);
            if ranked.is_empty() {
                emit(&json!({"ambiguous": false, "query": q, "matched": 0, "candidates": []}), args.pretty);
                std::process::exit(4)
            }
            // 顺序很重要：①先剔掉「本机根本没有聊天记录」的（同名旧号、只在通讯录里躺着的），
            // ②剩下的再按「先比匹配程度、同档比字段」选，唯一最优就直接锁定；
            // ③一个都没有聊天记录时，明确说"匹配到但本机无记录"，不能混成"没找到这个人"。
            let mut scored: Vec<(NameInfo, u8, u8, &'static str, i64)> = ranked
                .into_iter()
                .take(24)
                .map(|(n, d, f, t)| {
                    let rows = session_message_rows_recent(&account, &n.username, 1)
                        .unwrap_or_default();
                    let last = rows
                        .first()
                        .and_then(|r| r.as_object())
                        .map(|m| as_i64(ci_get(m, "create_time")))
                        .unwrap_or(0);
                    (n, d, f, t, last)
                })
                .collect();
            let alive_n = scored.iter().filter(|s| s.4 > 0).count();
            if alive_n == 0 {
                let list: Vec<Value> = scored
                    .iter()
                    .map(|(n, _, _, t, _)| json!({"username": n.username, "display": n.best(), "matched_by": t}))
                    .collect();
                emit(&json!({"matched_but_no_local_messages": true, "query": q, "candidates": list}), args.pretty);
                std::process::exit(4)
            }
            scored.retain(|s| s.4 > 0);
            scored.sort_by(|a, b| (b.1 * 10 + b.2).cmp(&(a.1 * 10 + a.2)).then_with(|| b.4.cmp(&a.4))
                .then_with(|| a.0.best().cmp(&b.0.best())));
            let top = scored[0].1 * 10 + scored[0].2;
            let tied: Vec<&(NameInfo, u8, u8, &'static str, i64)> =
                scored.iter().filter(|s| s.1 * 10 + s.2 == top).collect();
            if tied.len() == 1 {
                tied[0].0.username.clone()
            } else {
                let list: Vec<Value> = scored
                    .iter()
                    .map(|(n, d, f, t, last)| json!({
                        "username": n.username,
                        "display": n.best(),
                        "is_group": is_group(&n.username),
                        "last_time": last,
                        "has_messages": *last > 0,
                        "matched_by": t,
                        "degree": d,
                        "field_rank": f,
                    }))
                    .collect();
                emit(&json!({"ambiguous": true, "query": q, "candidates": list}), args.pretty);
                std::process::exit(2)
            }
        }
    };

    let info = names.get(&sid).cloned().unwrap_or_else(|| NameInfo { username: sid.clone(), ..Default::default() });
    // 媒体诊断（失败原因 + 命中路径 + 密钥指纹），由调用方用 REPLY_MEDIA_DIAG 指定落盘位置。
    // 这里只收集；写盘在最后统一做（去重、限行数）。
    let mut diag: Vec<String> = Vec::new();
    let mut diag_counts: HashMap<String, usize> = HashMap::new();
    let (rows, session_route, time_fallback) =
        session_message_rows_recent_routed(&account, &sid, args.n)?; // 新→旧，只取最近 args.n 条
    // 会话→消息表走的是哪一级路由（md5/mapping/shared/none）+ 是否因缺时间列退回全量读。
    // 微信下次改布局时，这条就是"从哪一级开始退化的"第一现场证据。
    diag.push(format!("session route={session_route} time_fallback={time_fallback} rows={}",
                      rows.len()));
    let n2i = load_name2id_all(&account);
    let my_id = std::env::var("WEFLOW_ACCOUNT_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| acct_dir.file_name().map(|s| clean_account_dir_name(&s.to_string_lossy())))
        .unwrap_or_default();
    let group = is_group(&sid);
    let take = args.n.min(rows.len());

    // 图片解密引擎（仅当 --img-out 指定输出目录时启用）：把可解密的图片写到本地目录，
    // stdout 只回相对文件名/扩展名/状态，绝不内联字节，也绝不上传网络。
    let mut img_engine = args.img_out.as_ref().map(|d| {
        let _ = std::fs::create_dir_all(d);
        let basename = acct_dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let ik = ks.image_key(Some(basename.as_str())).or_else(|| ks.image_key(None));
        let xor = ik.map(|e| (e.xor_key & 0xff) as u32).unwrap_or(0);
        let aes: Option<String> = ik.and_then(|e| e.aes_key.clone());
        // Locator 惰性建索引：绝大多数图片能靠 hardlink 的 dir1/dir2 直接拼出来，
        // 不再一上来就递归扫 6.9 万个 attach 文件。
        (PathBuf::from(d), xor, aes, media::Locator::new(), media::find_ffmpeg(args.ffmpeg.as_deref()))
    });
    let mut img_seq = 0usize;
    let mut img_written = 0usize;
    // 「这一窗里我说了几条」要随结果一起回传：万一本人身份判定静默失效（群聊里
    // 全部判成对方 = AI 会去答你自己说的话），这个数一眼就能看出来不对，不用靠肉眼读上下文。
    let mut mine_count = 0usize;
    // 语音导出引擎（仅当 --voice-out 指定目录时启用）：把本机 VoiceInfo 里的 silk blob 落盘。
    // 只取原始 silk，绝不在导出器里解码/转写；silk→wav→ASR 交下游 Python（音频不出本机）。
    let voice_dir = args.voice_out.as_ref().map(|d| {
        let _ = std::fs::create_dir_all(d);
        PathBuf::from(d)
    });
    let mut voice_seq = 0usize;
    let mut voice_written = 0usize;
    // 表情：XML/emoticon.db 里的 cdnurl 是普通 HTTP（与图片的 Mars filekey 不同）。
    // 只对表情联网，串行限速；用户可用 REPLY_EMOJI_FETCH=0 关掉。
    let emoji_enabled = std::env::var("REPLY_EMOJI_FETCH").map(|v| v.trim() != "0").unwrap_or(true);
    let mut emoji_fetcher = if emoji_enabled {
        args.img_out.as_ref().map(|d| cdn_emoji::Fetcher::new(PathBuf::from(d), 400))
    } else {
        None
    };
    let emoji_max: usize = std::env::var("REPLY_EMOJI_MAX").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(30);
    let mut emoji_seq = 0usize;
    let mut emoji_tried = 0usize;
    let mut emoji_fetched = 0usize;
    let img_key_fp = img_engine.as_ref().map(|(_, xor, aes, _, _)| {
        let h = format!("{:x}", md5::compute(format!("{}:{}", xor, aes.as_deref().unwrap_or(""))));
        h[..8].to_string()
    }).unwrap_or_default();

    let mut out: Vec<Value> = Vec::with_capacity(take);
    for row in rows.iter().take(take).rev() {
        // rev：把新→旧还原成时间正序输出（给 AI 读上下文更自然）
        let m = match row.as_object() {
            Some(m) => m,
            None => continue,
        };
        let local_type = as_i64(ci_get(m, "local_type"));
        let content = as_str(ci_get(m, "message_content"));
        let db_name = as_str(ci_get(m, "db_name")).to_lowercase();
        let mut sender = as_str(ci_get(m, "sender_username"));
        if sender.is_empty() {
            let rid = as_i64(ci_get(m, "real_sender_id").or_else(|| ci_get(m, "realSenderId")));
            if rid != 0 {
                if let Some(uname) = n2i.get(&db_name).and_then(|mp| mp.get(&rid)) {
                    sender = uname.clone();
                }
            }
        }
        let create_time = as_i64(ci_get(m, "create_time"));
        let is_me = if !my_id.is_empty() {
            !sender.is_empty() && sender == my_id
        } else if !group {
            !sender.is_empty() && sender != sid // 1:1：非对方即本人
        } else {
            false
        };
        let (label, text) = render_message(local_type, &content);
        if is_me {
            mine_count += 1;
        }
        let sender_display = if is_me {
            "我".to_string()
        } else if !sender.is_empty() {
            names.get(&sender).map(|n| n.best().to_string()).unwrap_or_else(|| sender.clone())
        } else {
            info.best().to_string()
        };
        // 图片：解析 md5 → .dat → 解密 → 写本地文件，记录状态。
        let mut image: Option<Value> = None;
        if local_type == 3 {
            if let Some((dir, xor, aes, loc, ff)) = img_engine.as_mut() {
                let xml = zstd_decode_hex(&content).unwrap_or_else(|| content.clone());
                match media::img_md5_from_xml(&xml) {
                    Some(md5) => {
                        // 定位与解密必须分开记：以前两者都 `.ok()` 折叠成 None，
                        // "密钥错/容器变了"和"本机没下载"在结果里长得一模一样。
                        // packed_info 里还有一棵独立的文件名树（WeFlow lateHit 的依据）；
                        // 只按消息 md5 查会漏掉"有 hardlink 行但名字对不上"的图。
                        let dat_name = media::dat_name_from_packed(
                            &as_str(ci_get(m, "packed_info_data")));
                        let located = loc.resolve(&account, &md5, dat_name.as_deref(), &sid);
                        let route = loc.last_route;
                        let md5_tag = &md5[..md5.len().min(8)];
                        let dec = located
                            .filter(|p| p.is_file())
                            .map(|p| media::decrypt_dat_file(&p, *xor, aes.as_deref()));
                        image = Some(match dec {
                            Some(Ok(d)) => {
                                diag.push(format!("img md5={md5_tag} route={route} stage=ok version={}",
                                                  d.version));
                                *diag_counts.entry("ok".into()).or_insert(0) += 1;
                                img_seq += 1;
                                // wxgf 容器：尝试转可读静态图（内嵌 JPEG/PNG 或 ffmpeg 解 HEVC）；
                                // 转不动就原样落 .wxgf，标 needs_convert 交下游。
                                let (bytes, ext, converted) = if d.is_wxgf {
                                    match media::wxgf_to_image(&d.data, ff.as_deref()) {
                                        Some((b, e)) => (b, e, true),
                                        None => {
                                            let why = media::last_convert_error();
                                            diag.push(format!(
                                                "img md5={md5_tag} stage=convert reason=wxgf_convert_failed err={}",
                                                if why.is_empty() { "-".into() } else { why }));
                                            (d.data.clone(), d.ext.clone(), false)
                                        }
                                    }
                                } else {
                                    (d.data.clone(), d.ext.clone(), false)
                                };
                                let fname = format!("img_{:04}.{}", img_seq, ext);
                                let wrote = std::fs::write(dir.join(&fname), &bytes).is_ok();
                                if wrote {
                                    img_written += 1;
                                }
                                if d.is_wxgf && !converted {
                                    *diag_counts.entry("raw_wxgf".into()).or_insert(0) += 1;
                                }
                                json!({
                                    "status": if !wrote { "write_failed" } else if converted { "converted" } else if d.is_wxgf { "raw_wxgf" } else { "decrypted" },
                                    "ext": ext,
                                    "version": d.version,
                                    "bytes": bytes.len(),
                                    "file": if wrote { fname } else { String::new() },
                                    "needs_convert": d.is_wxgf && !converted,
                                    "route": route,
                                })
                            }
                            Some(Err(e)) => {
                                let reason: String = format!("{e}").chars().take(160).collect();
                                diag.push(format!(
                                    "img md5={md5_tag} route={route} stage=decrypt reason={reason} keyfp={img_key_fp}"));
                                *diag_counts.entry("decrypt_failed".into()).or_insert(0) += 1;
                                json!({ "status": "decrypt_failed", "reason": reason, "route": route })
                            }
                            None => {
                                diag.push(format!("img md5={md5_tag} route={route} stage=locate reason=file_missing"));
                                *diag_counts.entry("not_local".into()).or_insert(0) += 1;
                                json!({ "status": "not_local", "route": route,
                                    "note": "图片仅存云端或未下载，本地无 .dat（4.x 云端媒体走 Mars 私有协议，需在微信里打开过才会落盘）" })
                            }
                        });
                    }
                    None => image = Some(json!({ "status": "no_md5" })),
                }
            }
        }
        // 表情（47）：本地文件是加密容器（实测常见密钥/算法都解不开），
        // 但 XML/emoticon.db 的 cdnurl 是普通 HTTP ⇒ 只对表情联网取回，落地后和图片一起送多模态。
        let mut emoji_info: Option<Value> = None;
        if local_type == 47 {
            if let Some((dir, _xor, _aes, _loc, ff)) = img_engine.as_mut() {
                if emoji_tried < emoji_max {
                    let xml = zstd_decode_hex(&content).unwrap_or_else(|| content.clone());
                    let md5 = media::elem_attr(&xml, "<emoji", "md5").unwrap_or_default();
                    let tag = &md5[..md5.len().min(8)];
                    let mut urls = cdn_emoji::candidate_urls(&xml);
                    let mut via = "xml";
                    if urls.is_empty() && !md5.is_empty() {
                        if let Some(u) = cdn_emoji::db_url(&account, &md5) {
                            urls.push(u);
                            via = "db";
                        }
                    }
                    match (urls.into_iter().next(), emoji_fetcher.as_mut()) {
                        (Some(first), Some(f)) => {
                            emoji_tried += 1;
                            // cdnurl 过期时（实测 403）继续试 thumburl / 库里地址
                            let mut candidates = vec![first];
                            candidates.extend(cdn_emoji::candidate_urls(&xml));
                            if !md5.is_empty() {
                                if let Some(u) = cdn_emoji::db_url(&account, &md5) {
                                    candidates.push(u);
                                }
                            }
                            candidates.dedup();
                            let mut got: Result<Vec<u8>, String> = Err("no_url".into());
                            for u in &candidates {
                                got = f.get(u);
                                if got.is_ok() {
                                    break;
                                }
                            }
                            match got {
                                Ok(bytes) => {
                                    let raw_ext = weflow_core::dat::detect_ext(&bytes)
                                        .unwrap_or("bin").to_string();
                                    let (data, out_ext, converted) = if raw_ext == "gif" || raw_ext == "webp" {
                                        match media::first_frame_jpg(&bytes, &raw_ext, ff.as_deref()) {
                                            Some(j) => (j, "jpg".to_string(), true),
                                            None => (bytes.clone(), raw_ext.clone(), false),
                                        }
                                    } else {
                                        (bytes.clone(), raw_ext.clone(), false)
                                    };
                                    emoji_seq += 1;
                                    let fname = format!("emoji_{:04}.{}", emoji_seq, out_ext);
                                    let wrote = std::fs::write(dir.join(&fname), &data).is_ok();
                                    if wrote && out_ext != "bin" {
                                        emoji_fetched += 1;
                                    }
                                    diag.push(format!(
                                        "emoji md5={tag} route=http via={via} stage=ok ext={out_ext} bytes={} converted={converted}",
                                        data.len()));
                                    *diag_counts.entry("emoji_ok".into()).or_insert(0) += 1;
                                    emoji_info = Some(json!({
                                        "status": if !wrote { "write_failed" } else if converted { "converted" } else { "decrypted" },
                                        "ext": out_ext, "bytes": data.len(),
                                        "file": if wrote { fname } else { String::new() },
                                        "kind": "emoji", "via": via,
                                    }));
                                }
                                Err(e) => {
                                    diag.push(format!("emoji md5={tag} route=http via={via} stage=fetch reason={e}"));
                                    *diag_counts.entry("emoji_failed".into()).or_insert(0) += 1;
                                    emoji_info = Some(json!({ "status": "fetch_failed", "reason": e, "kind": "emoji" }));
                                }
                            }
                        }
                        (None, _) => {
                            diag.push(format!("emoji md5={tag} stage=url reason=no_url"));
                            emoji_info = Some(json!({ "status": "no_url", "kind": "emoji" }));
                        }
                        (Some(_), None) => {
                            emoji_info = Some(json!({ "status": "fetch_disabled", "kind": "emoji" }));
                        }
                    }
                }
            }
        }
        // 语音：从 media_*.db VoiceInfo 取 silk blob 落 .silk（音频不出本机，转文字交下游 Python ASR）。
        let mut voice: Option<Value> = None;
        if local_type == 34 {
            if let Some(dir) = &voice_dir {
                let lid = as_i64(ci_get(m, "local_id").or_else(|| ci_get(m, "localId")));
                let svr = as_i64(
                    ci_get(m, "server_id")
                        .or_else(|| ci_get(m, "svr_id"))
                        .or_else(|| ci_get(m, "svrid")),
                );
                let mut vn: Vec<String> = vec![sid.clone()];
                if !sender.is_empty() {
                    vn.push(sender.clone());
                }
                vn.sort();
                vn.dedup();
                let (vsilk, vroute) = media::voice_silk_bytes(&account, create_time, lid, svr, &vn);
                match vsilk {
                    Some(b) => {
                        diag.push(format!("voice route={vroute} stage=ok bytes={}", b.len()));
                        *diag_counts.entry("voice_ok".into()).or_insert(0) += 1;
                        voice_seq += 1;
                        let fname = format!("voice_{:04}.silk", voice_seq);
                        let wrote = std::fs::write(dir.join(&fname), &b).is_ok();
                        if wrote {
                            voice_written += 1;
                        }
                        voice = Some(json!({
                            "status": if wrote { "extracted" } else { "write_failed" },
                            "bytes": b.len(),
                            "file": if wrote { fname } else { String::new() },
                            "duration_ms": voice_duration_ms(&content),
                            "route": vroute,
                        }));
                    }
                    None => {
                        // 全局兜底即使命中也不采纳（用户定的口径），这里把原因记清楚：
                        // global_rejected = 别的会话有同键语音；none = 本会话没有。
                        diag.push(format!("voice route={vroute} stage=lookup reason={vroute}"));
                        *diag_counts.entry(format!("voice_{vroute}")).or_insert(0) += 1;
                        voice = Some(json!({ "status": "not_local", "route": vroute,
                            "note": if vroute == "global_rejected" {
                                "本会话无这条语音；别处有同键记录但会话不符，未采纳"
                            } else {
                                "本机无该语音 silk blob（未播放/未下载）"
                            } }));
                    }
                }
            }
        }
        out.push(json!({
            "sender_username": sender,
            "sender_display": sender_display,
            "is_me": is_me,
            "create_time": create_time,
            "local_type": local_type,
            "type_label": label,
            "text": text,
            "raw_len": content.len(),
            "image": image,
            "voice": voice,
            "emoji": emoji_info,
        }));
    }

    write_media_diag(&diag);
    emit(
        &json!({
            "account_dir": acct_dir.to_string_lossy(),
            "account_candidates": acct_candidates,
            "login_guard": login_guard(&ks, args.data_root.as_deref(), &acct_dir),
            "session_id": sid,
            "session_display": info.best(),
            // 联系人档案的命名依据：微信号(alias)/备注/昵称（缺一个就少一段，由 Python 侧拼）
            "session_alias": info.alias,
            "session_remark": info.remark,
            "session_nickname": info.nickname,
            "is_group": is_group(&sid),
            "requested_n": args.n,
            "exported": out.len(),
            "session_route": session_route,
            "session_time_fallback": time_fallback,
            "my_account": my_id,
            "mine": mine_count,
            "others": out.len() - mine_count,
            "images_decrypted": img_written,
            "images_out_dir": args.img_out,
            // 定位途径与「慢路（整棵 attach 扫盘）有没有被走到」：P2 验收看 full_walk_used
            "image_lookup": img_engine.as_ref().map(|e| json!({
                "direct": e.3.hits[0],
                "scoped": e.3.hits[1],
                "via_full_index": e.3.hits[2],
                "miss": e.3.hits[3],
                "full_walk_used": e.3.full_walk_used,
            })),
            "voices_extracted": voice_written,
            "emojis_fetched": emoji_fetched,
            "voices_out_dir": args.voice_out,
            // 布局探测 + 媒体诊断汇总：让"下次微信改布局"留下可查的证据
            "layout": layout_json(&account.layout),
            "media_diag": diag_counts,
            "messages": out,
        }),
        args.pretty,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_extracts_displayname_and_cdata_content() {
        let xml = r#"<msg><appmsg><title>x</title><type>57</type><refermsg><type>1</type><displayname>张三</displayname><content><![CDATA[明天十点开会]]></content></refermsg></appmsg></msg>"#;
        assert_eq!(extract_quote(xml).as_deref(), Some("[引用 张三：明天十点开会]"));
    }

    #[test]
    fn quote_unescapes_and_strips_nested_xml() {
        let xml = r#"<refermsg><displayname>A&amp;B</displayname><content>&lt;msg&gt;&lt;appmsg&gt;&lt;title&gt;hello&lt;/title&gt;&lt;/appmsg&gt;&lt;/msg&gt;</content></refermsg>"#;
        let got = extract_quote(xml).unwrap_or_default();
        assert!(got.contains("A&B"), "{got}");
        assert!(got.contains("hello"), "{got}");
    }

    fn n(username: &str, nickname: &str, remark: &str, alias: &str) -> NameInfo {
        NameInfo {
            username: username.to_string(),
            nickname: nickname.to_string(),
            remark: remark.to_string(),
            alias: alias.to_string(),
            session_display: String::new(),
        }
    }

    fn map_of(items: Vec<NameInfo>) -> HashMap<String, NameInfo> {
        items.into_iter().map(|x| (x.username.clone(), x)).collect()
    }

    #[test]
    fn exact_beats_prefix_beats_contains() {
        let m = map_of(vec![
            n("wxid_aaa0001", "王小明", "王总", "wxwang"),
            n("wxid_bbb0002", "小王", "", ""),
            n("wxid_ccc0003", "张王麻子", "", ""),
        ]);
        let r = ranked_matches(&m, "王总");
        assert_eq!(r.len(), 1, "只有备注完全匹配该命中");
        assert_eq!(r[0].0.username, "wxid_aaa0001");
        assert_eq!(r[0].3, "备注");
        assert_eq!(r[0].1, 3, "degree=完全");

        let by_prefix = ranked_matches(&m, "王");
        // 备注「王总」前缀 > 昵称「小王」包含；同分时备注字段rank更高
        assert_eq!(by_prefix[0].0.username, "wxid_aaa0001");
        assert_eq!(by_prefix[0].3, "备注");

        let contains_only = ranked_matches(&m, "小明");
        assert_eq!(contains_only.len(), 1);
        assert_eq!(contains_only[0].3, "昵称");
    }

    #[test]
    fn wechat_id_matches_by_exact_input() {
        let m = map_of(vec![n("wxid_aaa0001", "王小明", "王总", "wxwang")]);
        let r = ranked_matches(&m, "wxwang");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].3, "微信号");
    }

    #[test]
    fn identical_remarks_tie_so_caller_must_ask() {
        let m = map_of(vec![
            n("wxid_zw0001", "张伟", "张伟", ""),
            n("wxid_zw0002", "张伟", "张伟", ""),
        ]);
        let r = ranked_matches(&m, "张伟");
        assert_eq!(r.len(), 2, "两个同名都要留下，让人来选");
        assert_eq!(r[0].1 * 10 + r[0].2, r[1].1 * 10 + r[1].2, "分数必须并列");
    }

    #[test]
    fn remark_wins_over_nickname_at_same_degree() {
        let m = map_of(vec![
            n("wxid_r1", "阿明某", "张阿明", ""),
            n("wxid_r2", "阿明", "", ""),
        ]);
        let r = ranked_matches(&m, "阿明");
        // 昵称完全(3*10+3=33) 比 备注包含(1*10+5=15) 优先：程度先于字段
        assert_eq!(r[0].0.username, "wxid_r2");
        assert_eq!(r[0].3, "昵称");
    }

    #[test]
    fn picks_visible_nicknames() {
        assert_eq!(extract_at_tokens("@欣萱米粒 大熊了"), vec!["欣萱米粒".to_string()]);
        assert_eq!(extract_at_tokens("请 @张三 和 @李四 确认"),
                   vec!["张三".to_string(), "李四".to_string()]);
        // 标点即止
        assert_eq!(extract_at_tokens("@王五，你来"), vec!["王五".to_string()]);
        assert_eq!(extract_at_tokens("@赵六（助理）在吗"), vec!["赵六".to_string()]);
    }

    #[test]
    fn skips_noise() {
        assert!(extract_at_tokens("@所有人 开会").is_empty());
        // 邮箱局部不该当成昵称
        assert!(extract_at_tokens("发到 someone@gmail.com 了").is_empty());
        assert!(extract_at_tokens("@12345").is_empty());
        assert!(extract_at_tokens("").is_empty());
    }

    #[test]
    fn caps_length_but_keeps_reasonable_names() {
        let long = format!("@{}", "名".repeat(30));
        assert!(extract_at_tokens(&long).is_empty());
        let ok = format!("@{}", "名".repeat(24));
        assert_eq!(extract_at_tokens(&ok).len(), 1);
    }
}
