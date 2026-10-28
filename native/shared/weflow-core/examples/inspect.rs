//! 只读调试：用给定密钥打开一个 .db，打印表清单/列/或执行一条 SELECT。
//! 仅用于本机自有的微信数据核验（任务 #7）。用法：
//!   cargo run -p weflow-core --example inspect -- <db_path> <64hex_key> [sql]

use rusqlite::Connection;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: inspect <db_path> <64hex_key> [sql]");
        std::process::exit(2);
    }
    let path = &args[0];
    let key_arg = args[1].trim();
    let key = if key_arg.is_empty() || key_arg.eq_ignore_ascii_case("store") {
        // 从共享密钥库按路径解析，调用者无需在命令行给出明文密钥。
        let db = std::path::Path::new(path);
        let adir = weflow_core::db::derive_account_dir(db);
        weflow_core::keystore::KeyStore::load_default().resolve_key(db, adir.as_deref(), "")
    } else {
        key_arg.to_string()
    };
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        eprintln!("key must be 64 hex (or pass 'store' to resolve from keystore)");
        std::process::exit(2);
    }
    let conn = Connection::open(path).expect("open");
    conn.execute_batch(&format!("PRAGMA key = \"x'{key}'\";")).expect("pragma");

    if let Some(sql) = args.get(2) {
        let mut stmt = conn.prepare(sql).expect("prepare");
        let ncols = stmt.column_count();
        let names: Vec<String> = (0..ncols).map(|i| stmt.column_name(i).unwrap_or("?").to_string()).collect();
        println!("columns: {names:?}");
        let mut rows = stmt.query([]).expect("query");
        let mut n = 0usize;
        while let Some(r) = rows.next().expect("row") {
            let vals: Vec<String> = (0..ncols)
                .map(|i| {
                    r.get_ref(i)
                        .map(|v| match v {
                            rusqlite::types::ValueRef::Null => "NULL".into(),
                            rusqlite::types::ValueRef::Integer(x) => x.to_string(),
                            rusqlite::types::ValueRef::Real(x) => x.to_string(),
                            rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                            rusqlite::types::ValueRef::Blob(b) => format!("<blob {}B>", b.len()),
                        })
                        .unwrap_or_else(|_| "?".into())
                })
                .collect();
            println!("[{n}] {vals:?}");
            n += 1;
            if n >= 200 {
                println!("... (truncated at 200 rows)");
                break;
            }
        }
        println!("total printed: {n}");
        return;
    }

    // 默认：列全部表 + 每张表列名
    let mut stmt = conn
        .prepare("SELECT name, type FROM sqlite_master WHERE type IN ('table','view') ORDER BY name")
        .expect("master");
    let objs: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("q")
        .collect::<Result<_, _>>()
        .expect("collect");
    println!("== {} objects in {}", objs.len(), path);
    for (name, ty) in &objs {
        let mut ps = conn.prepare(&format!("PRAGMA table_info(\"{name}\")")).expect("ti");
        let cols: Vec<String> = ps
            .query_map([], |r| r.get::<_, String>(1))
            .expect("cq")
            .collect::<Result<_, _>>()
            .expect("cc");
        println!("[{ty}] {name}: {cols:?}");
    }
}
