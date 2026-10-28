//! 只读端到端核验：从共享密钥库解析各库密钥，打开账号并验证 message/contact 等
//! 分库都可读、能取到消息行。**密钥来自密钥库，不从命令行传入**，故调用者无需接触明文密钥。
//!
//! 用法（需先 `key-provider bulk` 导入密钥库）：
//!   cargo run -q -p weflow-core --example verify -- <session.db 绝对路径>

use anyhow::Result;
use std::path::PathBuf;
use weflow_core::db::{derive_account_dir, Account};
use weflow_core::keystore::KeyStore;
use weflow_core::messages::session_message_rows;
use serde_json::Value;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: verify <session.db 绝对路径>");
        std::process::exit(2);
    }
    let session_db = PathBuf::from(&args[0]);
    if !session_db.is_file() {
        eprintln!("session.db 不存在: {}", session_db.display());
        std::process::exit(1);
    }

    // 从密钥库解析 session key（不接触明文：仅程序内部使用）。
    let store = KeyStore::load_default();
    let adir = derive_account_dir(&session_db);
    let session_key = store.resolve_key(&session_db, adir.as_deref(), "");
    if session_key.is_empty() {
        eprintln!("密钥库中没有该账号 session.db 的密钥；请先 `key-provider bulk --account-dir <dir> --file <scan.txt>`");
        std::process::exit(1);
    }

    println!("── 打开账号（session 主 key 由密钥库解析）──");
    let account = Account::open(&session_db.to_string_lossy(), &session_key)?;

    // 1) 逐个打开所有已知分库：能开即证明每库密钥解析正确。
    let mut opened = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let try_open = |label: &str, p: &PathBuf, opened: &mut usize, failed: &mut Vec<String>, account: &Account| {
        match account.conn(p) {
            Ok(_) => *opened += 1,
            Err(e) => failed.push(format!("{label}: {e}")),
        }
    };
    for (i, db) in account.layout.message_dbs.iter().enumerate() {
        try_open(&format!("message_{i}"), db, &mut opened, &mut failed, &account);
    }
    for (i, db) in account.layout.media_dbs.iter().enumerate() {
        try_open(&format!("media_{i}"), db, &mut opened, &mut failed, &account);
    }
    for (label, opt) in [
        ("contact", &account.layout.contact_db),
        ("sns", &account.layout.sns_db),
        ("emoticon", &account.layout.emoticon_db),
        ("hardlink", &account.layout.hardlink_db),
    ] {
        if let Some(p) = opt {
            try_open(label, p, &mut opened, &mut failed, &account);
        }
    }
    println!("分库打开成功: {opened} 个；失败: {} 个", failed.len());
    for f in &failed {
        println!("  ✗ {f}");
    }

    // 2) 会话数（session.db 可读的旁证）。
    let session_count: i64 = account.with_conn(&account.layout.session_db, |c| {
        Ok(c.query_row("SELECT count(*) FROM SessionTable", [], |r| r.get::<_, i64>(0))?)
    })?;
    println!("SessionTable 会话数: {session_count}");

    // 3) 取最近会话，读取其消息行（证明按每库密钥路由到 message_N.db 并解密成功）。
    let recent: Vec<String> = account.with_conn(&account.layout.session_db, |c| {
        let mut stmt = c.prepare("SELECT username FROM SessionTable ORDER BY sort_timestamp DESC LIMIT 25")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r?);
        }
        Ok(v)
    })?;

    println!("── 最近会话消息读取（不打印聊天内容，仅统计）──");
    let mut sessions_with_msgs = 0usize;
    let mut total_msgs = 0usize;
    let mut reported = 0usize;
    for sid in &recent {
        match session_message_rows(&account, sid, 0, i64::MAX, true) {
            Ok(rows) => {
                if rows.is_empty() {
                    continue;
                }
                sessions_with_msgs += 1;
                total_msgs += rows.len();
                if reported < 8 {
                    reported += 1;
                    let mut type_count: std::collections::BTreeMap<i64, usize> = Default::default();
                    for row in &rows {
                        if let Value::Object(m) = row {
                            if let Some(v) = m.get("local_type").and_then(|x| x.as_i64()) {
                                *type_count.entry(v).or_default() += 1;
                            }
                        }
                    }
                    println!("  会话[{} 字] 消息 {} 条，type 分布 {:?}", sid.len(), rows.len(), type_count);
                }
            }
            Err(e) => println!("  会话读取失败: {e}"),
        }
    }
    println!("取样 {}/{} 个最近会话，其中 {} 个含消息，共 {} 条", recent.len(), recent.len(), sessions_with_msgs, total_msgs);

    if failed.is_empty() && opened > 0 && sessions_with_msgs > 0 {
        println!("\n✓ 端到端通过：每库独立密钥解析 + 分库可读 + 真实消息行可读。");
        Ok(())
    } else {
        println!("\n结果存在失败项或未能读出任何消息，见上。");
        std::process::exit(1);
    }
}
