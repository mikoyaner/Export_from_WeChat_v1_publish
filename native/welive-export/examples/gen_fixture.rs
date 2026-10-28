//! 合成大夹具生成器（内存基准用）：只写临时目录，不读取也不写入任何真实微信数据。
//!
//! 产物落在 cargo workspace 根的 target/（cargo 默认行为），别在命令里写死 --target-dir。
//! cargo run --offline --release \
//!   -p welive-export --example gen_fixture -- <根目录> <会话数> <每会话消息数>

use rusqlite::Connection;
use std::path::{Path, PathBuf};

const KEY_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(args.get(0).map(|s| s.as_str()).unwrap_or("bench-fixture"));
    let per_session: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    std::fs::create_dir_all(&root)?;

    let sid = "bench0000000000000000000@chatroom";
    let table = format!("Msg_{:x}", md5::compute(sid.as_bytes()));
    let storage = root.join("wxid_bench").join("db_storage");

    let session = open(&storage.join("session").join("session.db"))?;
    session.execute_batch("CREATE TABLE Name2Desc (username TEXT PRIMARY KEY, lastMsgs TEXT);")?;
    session.execute("INSERT INTO Name2Desc (username, lastMsgs) VALUES (?, '{}')", [sid])?;
    drop(session);

    // 4 个分库，消息均分，制造跨分库场景
    let shards = 4i64;
    for shard in 0..shards {
        let db = open(&storage.join("message").join(format!("message_{shard}.db")))?;
        db.execute_batch(&format!(
            "CREATE TABLE {table} (local_id INTEGER PRIMARY KEY, server_id INTEGER, local_type INTEGER, \
             create_time INTEGER, sort_seq INTEGER, message_content TEXT, compress_content BLOB, \
             sender_username TEXT, is_send INTEGER);"
        ))?;
        let n = per_session / shards;
        db.execute_batch("BEGIN;")?;
        for i in 0..n {
            let global = shard * n + i;
            let content = format!(
                "<msg><appmsg><title>消息 {}</title><descpayload>{}</descpayload></appmsg></msg>",
                global,
                "填充内容用于基准测试。".repeat(6)
            );
            db.execute(
                &format!(
                    "INSERT INTO {table} (local_id, server_id, local_type, create_time, sort_seq, \
                     message_content, compress_content, sender_username, is_send) VALUES (?,?,?,?,?,?,?,?,?);"
                ),
                rusqlite::params![
                    global,
                    7_000_000_000_000_000_000i64.wrapping_add(global),
                    if global % 17 == 0 { 3i64 } else { 1i64 },
                    1_700_000_000 + global,
                    global,
                    content,
                    [0x28u8, 0xb5, 0x2f, 0xfd].repeat(64),
                    format!("wxid_sender{}", global % 9),
                    (global % 3 == 0) as i64,
                ],
            )?;
        }
        db.execute_batch("COMMIT;")?;
        println!("shard {shard}: {n} rows");
    }
    println!("fixture: {}", root.display());
    println!("session: {sid}");
    Ok(())
}

fn open(path: &Path) -> Result<Connection, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let conn = Connection::open(path)?;
    conn.execute_batch(&format!("PRAGMA key = \"x'{KEY_HEX}'\";"))?;
    Ok(conn)
}
