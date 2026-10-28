//! CLI 与协议分派。
//!
//! 两种运行形态：
//! - **协议模式**（WeFlow 拉起）：无子命令、stdin 被管道输入一行 JSON 请求，
//!   按 NDJSON 回 progress + 恰好一条 result；
//! - **管理模式**（用户操作）：子命令 `bulk` / `import` / `image` / `doctor` / `list` / `help`。
//!
//! 密钥统一存放在 weflow-core 的共享 KeyStore（wcdb.dll / welive 也读同一份）。
//! 本组件不做进程内存扫描；密钥由用户以已授权方式取得后导入。

use crate::protocol::{Action, Event, Request};
use crate::validate as vd;
use anyhow::{bail, Result};
use serde_json::json;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use weflow_core::keystore::{KeyStore, DEFAULT_ACCOUNT};

// ------------------------------------------------------------------ 协议模式

/// 处理一行 stdin 请求，返回待写出的事件序列（0..n progress + 恰好 1 result）。
pub fn handle_request(req: &Request) -> Vec<Event> {
    match req.action {
        Action::GetDbKey => handle_db_key(req),
        Action::GetImageKey => handle_image_key(req),
        Action::ScanImageKeyMemory => vec![Event::err(
            "本组件不执行进程内存扫描；请以已授权方式获取图片密钥后用 `key-provider image --xor .. --aes ..` 导入",
        )],
    }
}

fn handle_db_key(req: &Request) -> Vec<Event> {
    let mut events = Vec::new();
    let store = KeyStore::load_default();

    // WeFlow 语义：给出"数据库目录"（常为父级，如 <微信数据根>）+ 可选 accountId，
    // 期望组件据本地密钥库识别账号并返回其 session 主密钥 + accountId。
    // 因此先按密钥库解析候选账号目录，逐个用"该账号自己的密钥开该账号自己的 session.db"校验。
    let mut dirs: Vec<PathBuf> = Vec::new();
    let push_dir = |d: PathBuf, dirs: &mut Vec<PathBuf>| {
        let key = weflow_core::keystore::norm_path(&d);
        if !dirs.iter().any(|x| weflow_core::keystore::norm_path(x) == key) {
            dirs.push(d);
        }
    };

    if let Some(raw) = req.db_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let p = PathBuf::from(raw);
        if let Some(adir) = weflow_core::db::derive_account_dir(&p) {
            if store.has_db_key_map_for(&adir) {
                push_dir(adir, &mut dirs);
            }
        }
        if p.is_dir() {
            for a in store.accounts_under(&p) {
                push_dir(a, &mut dirs);
            }
            if let Some(acc) = req.account_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let joined = p.join(acc);
                if joined.is_dir() {
                    push_dir(joined, &mut dirs);
                }
            }
        }
    }
    if let Some(acc) = req.account_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let want = acc.to_ascii_lowercase();
        for existing in store.all_accounts() {
            if dir_basename(&existing) == Some(want.clone()) {
                push_dir(existing, &mut dirs);
            }
        }
    }

    let mut logs: Vec<String> = Vec::new();
    for dir in &dirs {
        let session_db = dir.join("db_storage").join("session").join("session.db");
        if !session_db.is_file() {
            logs.push(format!("跳过 {}：无 session.db", dir.display()));
            continue;
        }
        // 校验目标就是 session.db，故把该账号的 session 密钥放首位，命中最快。
        let mut cands: Vec<String> = Vec::new();
        if let Some(sk) = store.session_key_for(dir) {
            cands.push(sk);
        }
        for k in store.candidate_db_keys(dir_basename(dir).as_deref(), Some(dir)) {
            if !cands.contains(&k) {
                cands.push(k);
            }
        }
        if cands.is_empty() {
            continue;
        }
        events.push(Event::progress(format!("候选账号: {}", dir.display()), 0));
        let (hit, l) = vd::pick_valid_db_key(&session_db, &cands);
        logs.extend(l);
        if let Some(key) = hit {
            let acct = dir_basename(dir);
            for line in &logs {
                events.push(Event::progress(line.clone(), 0));
            }
            events.push(Event::db_ok(key, acct, logs));
            return events;
        }
    }

    // 兜底：对 resolve_db_file 得到的具体库校验（含内联 internalDbKeyHex）。
    let db_file = match vd::resolve_db_file(req.db_path.as_deref()) {
        Some(f) => f,
        None => {
            for line in &logs {
                events.push(Event::progress(line.clone(), 0));
            }
            events.push(Event::err(if dirs.is_empty() {
                "本地密钥库未包含该账号的密钥：请先用已授权方式取得密钥并 `key-provider bulk` 或 `import` 导入"
                    .to_string()
            } else {
                "已找到候选账号目录，但其 session.db 校验失败（见日志）".to_string()
            }));
            return events;
        }
    };
    let account_dir = weflow_core::db::derive_account_dir(&db_file);
    let mut candidates: Vec<String> = Vec::new();
    if let Some(hex) = req.internal_db_key_hex.as_deref().and_then(vd::normalize_hex_key) {
        candidates.push(hex);
    }
    for k in store.candidate_db_keys(req.account_id.as_deref(), account_dir.as_deref()) {
        if !candidates.contains(&k) {
            candidates.push(k);
        }
    }
    if candidates.is_empty() {
        for line in &logs {
            events.push(Event::progress(line.clone(), 0));
        }
        events.push(Event::err(
            "本地密钥库为空：请先用已授权方式取得密钥并 `key-provider bulk` 或 `import` 导入",
        ));
        return events;
    }

    events.push(Event::progress(format!("目标库: {}", db_file.display()), 0));
    let (hit, l) = vd::pick_valid_db_key(&db_file, &candidates);
    for line in &l {
        events.push(Event::progress(line.clone(), 0));
    }
    match hit {
        Some(key) => events.push(Event::db_ok(key, req.account_id.clone(), l)),
        None => events.push(Event::err(format!(
            "无候选密钥能通过 {} 的 SQLCipher 校验",
            db_file.display()
        ))),
    }
    events
}

/// 目录路径的最后一段（小写），用于取 accountId。
fn dir_basename(p: &Path) -> Option<String> {
    p.components()
        .next_back()
        .and_then(|c| c.as_os_str().to_str())
        .map(|s| s.to_ascii_lowercase())
}

fn handle_image_key(req: &Request) -> Vec<Event> {
    let store = KeyStore::load_default();
    match store.image_key(req.account_id.as_deref()) {
        Some(k) => {
            let xor = json!(k.xor_key);
            vec![
                Event::progress(
                    format!(
                        "从本地密钥库返回图片密钥（accountDir={}；如需校验请对具体 .dat 运行 `key-provider image --dat <样例.dat>`）",
                        req.account_dir.as_deref().unwrap_or("-")
                    ),
                    0,
                ),
                Event::image_ok(xor, k.aes_key.clone(), false),
            ]
        }
        None => vec![Event::err(
            "本地密钥库无图片密钥：请用 `key-provider image --xor <..> --aes <..>` 导入",
        )],
    }
}

/// 协议主循环：读 stdin 一行，写事件后退出。WeFlow 每次调用只发一个请求。
pub fn run_protocol() -> Result<()> {
    let mut buf = String::new();
    std::io::stdin().lock().read_to_string(&mut buf)?;
    let line = buf
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");

    dump_request_for_diag(line);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let events = match Request::parse(line) {
        Ok(req) => handle_request(&req),
        Err(e) => vec![Event::err(format!("请求解析失败: {e}"))],
    };
    for ev in &events {
        let text = serde_json::to_string(ev)?;
        writeln!(out, "{text}")?;
        out.flush()?;
    }
    Ok(())
}

/// 诊断：把 WeFlow 实际发来的原始请求行 + 关键解析结果追加到密钥库同目录的 requests.log。
fn dump_request_for_diag(line: &str) {
    use std::io::Write;
    let dir = weflow_core::keystore::store_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(std::env::temp_dir);
    let req: serde_json::Value = serde_json::from_str(line).unwrap_or(serde_json::Value::Null);
    let db_path = req.get("dbPath").and_then(|v| v.as_str()).unwrap_or("");
    let account_id = req.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let has_internal = req
        .get("internalDbKeyHex")
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let store = KeyStore::load_default();
    let summary = if db_path.is_empty() {
        format!("dbPath=<empty> accountId={account_id} hasInternal={has_internal}")
    } else {
        let p = Path::new(db_path);
        let adir = weflow_core::db::derive_account_dir(p);
        let adir_norm = adir.as_deref().map(weflow_core::keystore::norm_path).unwrap_or_default();
        let map_hit = adir
            .as_deref()
            .map(|d| store.has_db_key_map_for(d))
            .unwrap_or(false);
        let cands = store
            .candidate_db_keys(if account_id.is_empty() { None } else { Some(account_id) }, adir.as_deref())
            .len();
        format!("dbPath={db_path} accountId={account_id} hasInternal={has_internal} account_dir_norm={adir_norm} map_hit={map_hit} candidates={cands}")
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("requests.log")) {
        // 不记录原始行（可能内联密钥），仅记录脱敏后的解析摘要。
        let _ = writeln!(f, "{summary}");
    }
}

// ------------------------------------------------------------------ 管理模式

struct Flags {
    positional: Vec<String>,
    kv: std::collections::HashMap<String, String>,
}

impl Flags {
    fn get(&self, key: &str) -> Option<&str> {
        self.kv.get(key).map(|s| s.as_str())
    }
}

/// 解析 `--key <v>` / `--flag` / 位置参数。
fn parse_args(args: &[String]) -> Flags {
    let mut positional = Vec::new();
    let mut kv = std::collections::HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(key) = a.strip_prefix("--") {
            let next = args.get(i + 1);
            if let Some(n) = next {
                if !n.starts_with("--") {
                    kv.insert(key.to_string(), n.clone());
                    i += 2;
                    continue;
                }
            }
            kv.insert(key.to_string(), "true".to_string());
            i += 1;
        } else {
            positional.push(a.clone());
            i += 1;
        }
    }
    Flags { positional, kv }
}

pub fn run_cli(args: &[String]) -> Result<i32> {
    if args.is_empty() {
        if !std::io::stdin().is_terminal() {
            run_protocol()?;
            return Ok(0);
        }
        print_help();
        return Ok(0);
    }
    let cmd = args[0].as_str();
    let flags = parse_args(&args[1..]);
    match cmd {
        "help" | "--help" | "-h" => {
            print_help();
            Ok(0)
        }
        "bulk" => cmd_bulk(&flags),
        "import" => cmd_import(&flags),
        "image" => cmd_image(&flags),
        "doctor" => cmd_doctor(&flags),
        "list" => cmd_list(),
        other => {
            eprintln!("未知子命令: {other}");
            print_help();
            Ok(2)
        }
    }
}

/// 批量导入取钥工具的输出（每库一把）。
/// 用法：key-provider bulk --account-dir <dir> [--file <scanner.txt>] [--verify]
/// 或：  直接把扫描输出粘到 stdin（不带 --file 且 stdin 非 tty）。
/// 把扫描输出里的 .db 路径统一成 `db_storage/` 之后的相对段（小写正斜杠）。
/// 兼容扫描器给出绝对路径或相对路径两种格式；不含 `db_storage/` 时按已是相对段处理。
fn storage_rel(rel: &str) -> String {
    const MARK: &str = "db_storage/";
    match rel.rfind(MARK) {
        Some(i) => rel[i + MARK.len()..].to_string(),
        None => rel.trim_start_matches('/').to_string(),
    }
}

fn cmd_bulk(flags: &Flags) -> Result<i32> {
    let account_dir = resolve_account_dir(flags)?;
    let text = match flags.get("file") {
        Some(f) => std::fs::read_to_string(f)?,
        None => {
            if std::io::stdin().is_terminal() {
                bail!("请提供 --file <扫描输出文件>，或把扫描结果直接管道到 stdin（key-provider bulk --account-dir <dir> < scan.txt）");
            }
            let mut s = String::new();
            std::io::stdin().lock().read_to_string(&mut s)?;
            s
        }
    };

    // 统一裁成 `db_storage/` 之后的相对段，保证与打开时 rel_under_db_storage 的 key 一致。
    let mut map: std::collections::BTreeMap<String, String> = Default::default();
    let mut entries: Vec<(String, String)> = Vec::new();
    for (rel, key) in KeyStore::parse_scanner_lines(&text) {
        let rel = storage_rel(&rel);
        entries.push((rel.clone(), key.clone()));
        map.insert(rel, key);
    }
    if entries.is_empty() {
        bail!("未从输入解析到任何 `相对.db -> 64hex` 记录（检查扫描输出格式）");
    }

    let path = weflow_core::keystore::store_path();
    let mut store = KeyStore::load(&path);
    let n = store.put_db_key_map(&account_dir, map);

    // 顺带把 session.db 的 key 作为该账号主 key（若存在），供 WeFlow 单 key 打开账号用。
    let acct = account_id_from_dir(&account_dir);
    if let Some(session_key) = entries.iter().find(|(rel, _)| rel == "session/session.db").map(|(_, k)| k.clone()) {
        store.db_keys.insert(acct.clone(), session_key);
    }
    store.save(&path)?;

    println!("✓ 解析 {n} 条每库密钥，写入 {}", path.display());
    println!("  account_dir = {}", account_dir.display());
    println!("  accountId(主key桶) = {acct}");

    if flags.get("verify").is_some() {
        println!("── 逐库 SQLCipher 校验 ──");
        let storage = account_dir.join("db_storage");
        let (mut ok, mut miss, mut bad) = (0usize, 0usize, 0usize);
        for (rel, key) in &entries {
            let f = storage.join(rel);
            if !f.is_file() {
                miss += 1;
                continue;
            }
            if vd::check_db_key(&f, key).0 {
                ok += 1;
            } else {
                bad += 1;
                println!("  ✗ {rel}: 校验失败 ({})", vd::mask(key));
            }
        }
        println!("校验通过 {ok}，文件缺失 {miss}，密钥不符 {bad}");
    }
    println!("现在把 WeFlow 的 keyProviderPath 指向本 exe 即可；wcdb.dll/welive 会自动读取同一密钥库。");
    Ok(0)
}

fn cmd_import(flags: &Flags) -> Result<i32> {
    let key = flags
        .get("key")
        .and_then(vd::normalize_hex_key)
        .or_else(|| flags.positional.first().and_then(|p| vd::normalize_hex_key(p)));
    let key = match key {
        Some(k) => k,
        None => bail!("需提供合法的 64-hex 密钥：key-provider import --key <64hex> [--account <wxid>] [--db-path <.db|dir>]"),
    };
    let account = flags.get("account").unwrap_or(DEFAULT_ACCOUNT).to_string();
    let path = weflow_core::keystore::store_path();
    let mut store = KeyStore::load(&path);

    if let Some(db) = flags.get("db-path").or(flags.get("db")) {
        match vd::resolve_db_file(Some(db)) {
            Some(f) => {
                let (ok, logs) = vd::check_db_key(&f, &key);
                for l in &logs {
                    println!("{l}");
                }
                if !ok {
                    bail!("密钥无法打开 {}，未导入", f.display());
                }
                println!("✓ 密钥经 SQLCipher 校验通过，可安全导入");
            }
            None => eprintln!("提示: --db-path 未解析到 .db 文件，跳过校验直接导入"),
        }
    } else {
        eprintln!("提示: 未提供 --db-path，跳过校验直接导入（建议带上以确认无误；微信4.x 每库一钥，单 key 仅能开 session.db）");
    }

    store.db_keys.insert(account.clone(), key.clone());
    store.save(&path)?;
    println!("✓ 已写入主密钥 {}  账号={}", path.display(), account);
    println!("  密钥={}", vd::mask(&key));
    Ok(0)
}

fn cmd_image(flags: &Flags) -> Result<i32> {
    let xor = flags
        .get("xor")
        .and_then(vd::parse_xor_key)
        .or_else(|| flags.positional.first().and_then(|p| vd::parse_xor_key(p)));
    let xor = match xor {
        Some(x) => x,
        None => bail!("需提供 xor key：key-provider image --xor <十进制|0x十六进制> [--aes <hex>] [--dat <样例.dat>] [--account <wxid>]"),
    };
    let aes = flags.get("aes").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let account = flags.get("account").unwrap_or(DEFAULT_ACCOUNT).to_string();
    let path = weflow_core::keystore::store_path();
    let mut store = KeyStore::load(&path);

    let mut verified = false;
    if let Some(dat) = flags.get("dat") {
        let p = Path::new(dat);
        if p.is_file() {
            let (ok, msg) = vd::verify_dat(p, xor, aes.as_deref());
            println!("{msg}");
            verified = ok;
        } else {
            eprintln!("提示: --dat 指定的样例文件不存在，跳过校验");
        }
    }

    store.image_keys.insert(account.clone(), weflow_core::keystore::ImageKeyEntry { xor_key: xor, aes_key: aes.clone() });
    store.save(&path)?;
    println!("✓ 图片密钥已写入 {}  账号={} xor={} aes={:?}", path.display(), account, xor, aes.as_deref().map(vd::mask));
    if verified {
        println!("  已通过样例 .dat 校验。");
    }
    Ok(0)
}

fn cmd_doctor(flags: &Flags) -> Result<i32> {
    let key = flags
        .get("key")
        .and_then(vd::normalize_hex_key)
        .or_else(|| flags.positional.first().and_then(|p| vd::normalize_hex_key(p)));
    let db = flags.get("db-path").or(flags.get("db"));
    let file = match vd::resolve_db_file(db) {
        Some(f) => f,
        None => bail!("doctor 需要 --db-path <.db|dir>，可选 --key <64hex>（缺省则测试密钥库中的候选）"),
    };
    let candidates: Vec<String> = match key {
        Some(k) => vec![k],
        None => {
            let store = KeyStore::load_default();
            let acc = flags.get("account");
            let adir = weflow_core::db::derive_account_dir(&file);
            let c = store.candidate_db_keys(acc, adir.as_deref());
            if c.is_empty() {
                bail!("既未给 --key，密钥库也空；请先 bulk/import 或直接 --key <64hex>");
            }
            c
        }
    };
    println!("目标库: {}", file.display());
    let (hit, logs) = vd::pick_valid_db_key(&file, &candidates);
    for l in &logs {
        println!("{l}");
    }
    match hit {
        Some(k) => {
            println!("✓ 可用密钥: {}", vd::mask(&k));
            Ok(0)
        }
        None => {
            println!("✗ 没有候选密钥能打开该库");
            Ok(1)
        }
    }
}

fn cmd_list() -> Result<i32> {
    let path = weflow_core::keystore::store_path();
    let store = KeyStore::load(&path);
    println!("密钥库: {}", path.display());
    if store.db_keys.is_empty() {
        println!("  (无主 DB 密钥)");
    } else {
        println!("  主 DB 密钥:");
        for (acc, k) in &store.db_keys {
            println!("    {acc}: {}", vd::mask(k));
        }
    }
    if store.db_key_maps.is_empty() {
        println!("  (无每库密钥表)");
    } else {
        println!("  每库密钥表 (account_dir -> rel.db):");
        for (acct, map) in &store.db_key_maps {
            println!("    {acct}: {} 条", map.len());
            for (rel, k) in map {
                println!("      {rel}: {}", vd::mask(k));
            }
        }
    }
    if store.image_keys.is_empty() {
        println!("  (无图片密钥)");
    } else {
        println!("  图片密钥:");
        for (acc, ik) in &store.image_keys {
            println!("    {acc}: xor={} aes={}", ik.xor_key, ik.aes_key.as_deref().map(vd::mask).unwrap_or_else(|| "-".into()));
        }
    }
    Ok(0)
}

/// 从 flags 解析 account_dir：优先 --account-dir；否则由 --db-path 反推。
fn resolve_account_dir(flags: &Flags) -> Result<PathBuf> {
    if let Some(d) = flags.get("account-dir").or(flags.get("dir")) {
        let p = PathBuf::from(d);
        if p.is_dir() {
            return Ok(p);
        }
        bail!("--account-dir 不是有效目录: {d}");
    }
    if let Some(db) = flags.get("db-path").or(flags.get("db")) {
        if let Some(f) = vd::resolve_db_file(Some(db)) {
            if let Some(dir) = weflow_core::db::derive_account_dir(&f) {
                return Ok(dir);
            }
        }
    }
    bail!("需要 --account-dir <账号目录>（db_storage 的上级），或 --db-path 供反推")
}

/// 从 account_dir 目录名推 accountId（去掉末尾 _hash 段做兜底，保留原始目录名亦可）。
fn account_id_from_dir(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string())
}

fn print_help() {
    println!(
        r#"key-provider —— WeFlow keyProviderPath 兼容的密钥保管/校验组件（不扫描进程内存）

微信 4.x 每个 .db 各有一把 SQLCipher key（实测：session key 打不开 message db），
且 session.db 内无 keyring。故本组件维护"每库密钥表"，供 wcdb.dll / welive 共享读取。

WeFlow 协议模式：
  （无子命令且 stdin 为管道时自动进入）读取一行 {{"action":"get_db_key"|...}}，
  按 NDJSON 输出 progress，最后输出一条 result。get_db_key 会针对 WeFlow 传入的具体
  dbPath，从每库表挑出能打开它的密钥返回（因此对任意分库都能给出正确 key）。

管理命令：
  key-provider bulk --account-dir <账号目录> [--file <扫描输出.txt>|stdin] [--verify]
      批量导入取钥工具输出（每行 `<相对>.db salt=.. -> <64hex>`）为每库密钥表。
      --verify 时对每个真实 .db 逐库 SQLCipher 校验并统计。
  key-provider import --key <64hex> [--account <wxid>] [--db-path <.db|目录>]
      导入单把主密钥（一般就是 session.db 的 key）。带 --db-path 时先校验再落盘。
  key-provider image  --xor <十进制|0x..> [--aes <hex>] [--dat <样例.dat>] [--account <wxid>]
      导入 .dat 图片密钥；带 --dat 时解密校验。
  key-provider doctor --db-path <.db|目录> [--key <64hex>] [--account <wxid>]
      诊断：用指定或库内候选逐个尝试打开目标库，脱敏打印。
  key-provider list
      列出本地密钥库（主 key / 每库表 / 图片 key，密钥脱敏）。

环境变量：
  WEFLOW_KEY_STORE   覆盖密钥库文件路径（默认 %APPDATA%\weflow-key-provider\store.json）。

安全说明：本组件仅保管/校验由用户在自有设备、自有账号上以已授权方式取得的密钥，
不实现、也不调用任何从微信进程内存提取密钥的功能。"#,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Action;

    fn req(action: Action, db: Option<&str>, internal: Option<&str>) -> Request {
        Request {
            action,
            db_path: db.map(|s| s.to_string()),
            account_id: None,
            internal_db_key_hex: internal.map(|s| s.to_string()),
            account_dir: None,
        }
    }

    #[test]
    fn scan_memory_action_is_rejected() {
        let evs = handle_request(&req(Action::ScanImageKeyMemory, None, None));
        match evs.last().unwrap() {
            Event::Result(r) => {
                assert!(!r.success);
                assert!(r.error.as_deref().unwrap().contains("不执行进程内存扫描"));
            }
            _ => panic!("expected result"),
        }
    }

    #[test]
    fn db_key_without_dbpath_errors() {
        let evs = handle_request(&req(Action::GetDbKey, None, None));
        assert!(matches!(evs.last(), Some(Event::Result(r)) if !r.success));
    }

    #[test]
    fn flags_parse_kv_and_positional() {
        let args: Vec<String> = ["import", "--key", "aa", "--account", "wx", "extra"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let flags = parse_args(&args[1..]);
        assert_eq!(flags.get("key"), Some("aa"));
        assert_eq!(flags.get("account"), Some("wx"));
        assert_eq!(flags.positional, vec!["extra".to_string()]);
    }
}
