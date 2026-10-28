//! welive-export.exe ↔ weliveBridge.ts 进程协议实测（合成夹具，不触碰真实微信数据目录）。
//!
//! 校验点全部来自 docs/protocol/06-welive-raw-jsonl.md：
//! - stdin 收一份 camelCase ExportRequest，argv 容忍任意前缀；
//! - stdout 每行一个 JSON 事件，顺序 ready → progress…→ complete → result；
//! - result 必须带 raw_session_output_paths + raw_export_manifests，且 manifest.rows/bytes
//!   与落盘文件逐字节一致（WeFlow 读前读后各校验一次，末字节必须是 \n）；
//! - 每会话一个 JSONL，流式写出（分库内按 sort_seq 升序，跨分库不做全局排序——
//!   WeFlow 读 raw 后总会按 create_time→local_id 重排），行内含 WeFlow 认的 snake_case 键。

use rusqlite::Connection;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const KEY_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const SESSION_ID: &str = "1234567890@chatroom";

fn msg_table_name() -> String {
    format!("Msg_{:x}", md5::compute(SESSION_ID.as_bytes()))
}

fn fixture_root() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("welive-export-protocol-{}-{}", std::process::id(), nanos))
}

fn open_encrypted(path: &Path, key_hex: &str) -> Connection {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(&format!("PRAGMA key = \"x'{key_hex}'\";"))
        .unwrap();
    conn
}

fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `.../<account>/db_storage/{session/session.db, message/message_N.db}`
fn build_fixture(root: &Path) {
    let storage = root.join("wxid_fixture").join("db_storage");
    let table = msg_table_name();

    let session = open_encrypted(&storage.join("session").join("session.db"), KEY_HEX);
    session
        .execute_batch("CREATE TABLE Name2Desc (username TEXT PRIMARY KEY, lastMsgs TEXT);")
        .unwrap();
    session
        .execute(
            "INSERT INTO Name2Desc (username, lastMsgs) VALUES (?, '{}')",
            rusqlite::params![SESSION_ID],
        )
        .unwrap();

    // 分库 0：snake_case 列名
    let db0 = open_encrypted(&storage.join("message").join("message_0.db"), KEY_HEX);
    db0.execute_batch(&format!(
        "CREATE TABLE {table} (local_id INTEGER PRIMARY KEY, server_id INTEGER, local_type INTEGER, \
         create_time INTEGER, sort_seq INTEGER, message_content TEXT, sender_username TEXT, \
         is_send INTEGER, img_buf BLOB);"
    ))
    .unwrap();
    let canonical: Vec<(i64, i64, i64, i64, i64, &str, &str, i64, Option<Vec<u8>>)> = vec![
        (11, 7312345678901234567, 1, 1700000002, 1002, "你好，世界 🌍", "wxid_alice", 1, None),
        (
            12,
            111,
            3,
            1700000001,
            1001,
            "<msg><img md5=\"0123456789abcdef0123456789abcdef\"/></msg>",
            "wxid_bob",
            0,
            None,
        ),
        (
            13,
            112,
            49,
            1700000003,
            1004,
            "<appmsg><title>报告.pdf</title></appmsg>",
            "wxid_alice",
            1,
            Some(vec![0xde, 0xad]),
        ),
    ];
    for (local_id, server_id, local_type, ct, seq, content, sender, is_send, blob) in canonical {
        db0.execute(
            &format!(
                "INSERT INTO {table} (local_id, server_id, local_type, create_time, sort_seq, \
                 message_content, sender_username, is_send, img_buf) VALUES (?,?,?,?,?,?,?,?,?);"
            ),
            rusqlite::params![local_id, server_id, local_type, ct, seq, content, sender, is_send, blob],
        )
        .unwrap();
    }

    // 分库 1：camelCase 列名，验证归一层补齐 snake_case 原型键
    let db1 = open_encrypted(&storage.join("message").join("message_1.db"), KEY_HEX);
    db1.execute_batch(&format!(
        "CREATE TABLE {table} (localId INTEGER, MsgSvrId INTEGER, localType INTEGER, createTime INTEGER, \
         sortSeq INTEGER, messageContent TEXT, sender TEXT, isSend INTEGER);"
    ))
    .unwrap();
    let mixed: Vec<(i64, i64, i64, i64, i64, &str)> = vec![
        (21, 222, 1, 1700000000, 1000, "最早的一条"),
        (22, 223, 34, 1700000004, 1003, "<msg><voicelength>3</voicelength></msg>"),
    ];
    for (local_id, server_id, local_type, ct, seq, content) in mixed {
        db1.execute(
            &format!(
                "INSERT INTO {table} (localId, MsgSvrId, localType, createTime, sortSeq, messageContent, sender, isSend) \
                 VALUES (?,?,?,?,?,?,?,?);"
            ),
            rusqlite::params![local_id, server_id, local_type, ct, seq, content, SESSION_ID, 0],
        )
        .unwrap();
    }
}

fn request_json(root: &Path, exports_dir: &Path) -> Value {
    json!({
        "account": {
            "sessionDb": fwd(&root.join("wxid_fixture").join("db_storage").join("session").join("session.db")),
            "dbKey": KEY_HEX,
            "myAccountId": "wxid_alice",
            "accountDir": fwd(&root.join("wxid_fixture")),
            "imageXorKey": "0x5a",
            "imageAesKey": ""
        },
        "sessionIds": [SESSION_ID],
        "outputDir": fwd(&root.join("out")),
        "exportsDir": fwd(exports_dir),
        "mediaTypes": [],
        "format": "raw-jsonl",
        "parseContent": true,
        "preserveMessageContent": true,
        "compactRaw": true,
        "sanitize": false,
        "batchSize": 20000,
        "ascending": true,
        "options": { "format": "chatlab", "contentType": "all" }
    })
}

struct Run {
    code: i32,
    events: Vec<Value>,
    stderr: String,
}

/// 拉起 exe：带一个前缀参数以验证 argv 容忍；stdout 必须逐行是 JSON。
fn run_engine(request: &Value) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_welive-export"))
        .args(["--config", "unused.yaml", "weflow-export"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to launch welive-export");

    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(request.to_string().as_bytes())
        .expect("write request");

    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let mut events = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        events.push(
            serde_json::from_str(trimmed)
                .unwrap_or_else(|e| panic!("stdout 行不是合法 JSON: {line} ({e})")),
        );
    }
    Run {
        code: output.status.code().unwrap_or(-1),
        events,
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

#[test]
fn protocol_round_trip() {
    let root = fixture_root();
    let exports_dir = root.join("raw").join("exports");
    std::fs::create_dir_all(&exports_dir).unwrap();
    build_fixture(&root);

    let run = run_engine(&request_json(&root, &exports_dir));
    assert_eq!(run.code, 0, "退出码非 0，stderr:\n{}", run.stderr);

    let types: Vec<&str> = run
        .events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(types.first(), Some(&"ready"), "首个事件必须是 ready，实际 {types:?}");
    assert_eq!(types.last(), Some(&"result"), "末个事件必须是 result，实际 {types:?}");
    let phases: Vec<&str> = run
        .events
        .iter()
        .filter(|e| e["type"] == "progress")
        .map(|e| e["phase"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        phases.last(),
        Some(&"complete"),
        "progress 序列必须以 complete 收尾，实际 {phases:?}"
    );
    assert!(
        run.events
            .iter()
            .filter(|e| e["type"] == "progress")
            .all(|e| e["session_id"] == json!(SESSION_ID)),
        "每条 progress 都要带 session_id，WeFlow 用它定位当前会话"
    );

    let result = run.events.last().unwrap();
    assert_eq!(result["success"], json!(true), "result.success 必须为 true");
    assert_eq!(result["success_count"], json!(1));
    assert_eq!(result["fail_count"], json!(0));
    assert!(result.get("failed_sessions").is_none(), "不该有 failed_sessions");

    let raw_paths = result["raw_session_output_paths"].as_object().expect("raw_session_output_paths");
    let jsonl_path = raw_paths
        .get(SESSION_ID)
        .and_then(|v| v.as_str())
        .expect("每会话必须回传 raw JSONL 路径");
    let manifests = result["raw_export_manifests"].as_object().expect("raw_export_manifests");
    let manifest = manifests.get(SESSION_ID).expect("每会话必须回传完整性清单");
    assert_eq!(manifest["path"].as_str().unwrap().replace('\\', "/"), jsonl_path.replace('\\', "/"), "manifest.path 必须与 raw 路径一致");

    let file = PathBuf::from(jsonl_path);
    let bytes = std::fs::read(&file).unwrap_or_else(|e| panic!("raw JSONL 不存在: {jsonl_path} ({e})"));
    assert_eq!(bytes.last(), Some(&b'\n'), "rows>0 时末字节必须是 LF");
    assert_eq!(manifest["bytes"].as_u64().unwrap(), bytes.len() as u64, "manifest.bytes 必须等于文件字节数");

    let lines: Vec<Value> = String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("每行必须是合法 JSON 对象"))
        .collect();
    assert_eq!(manifest["rows"].as_u64().unwrap(), lines.len() as u64, "manifest.rows 必须等于非空行数");
    assert_eq!(lines.len(), 5);

    // 流式写出的顺序保证：同一分库的行连续成段，且段内 sort_seq 严格升序。
    // 跨分库不再全局合并排序（内存与整会话大小解耦），最终顺序由 WeFlow 的重排决定。
    let mut seen_shards: Vec<&str> = Vec::new();
    let mut prev: Option<(&str, i64)> = None;
    for row in &lines {
        let shard = row["db_name"].as_str().expect("行内需保留 db_name");
        let seq = row["sort_seq"].as_i64().expect("行内必须有 sort_seq");
        if let Some((p_shard, p_seq)) = prev {
            if p_shard == shard {
                assert!(seq > p_seq, "同分库 {shard} 内必须按 sort_seq 升序：{p_seq} → {seq}");
            } else {
                assert!(!seen_shards.contains(&shard), "分库 {shard} 的输出被打散，流式写必须逐段完成");
            }
        }
        if !seen_shards.contains(&shard) {
            seen_shards.push(shard);
        }
        prev = Some((shard, seq));
    }
    assert_eq!(seen_shards.len(), 2, "夹具应覆盖两个分库，实际 {seen_shards:?}");

    // 按 WeFlow 的比较器（create_time，再 local_id）重排 = 用户在导出结果里看到的顺序
    let mut ordered = lines.clone();
    ordered.sort_by(|a, b| {
        let ct = a["create_time"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&b["create_time"].as_i64().unwrap_or(0));
        if ct != std::cmp::Ordering::Equal {
            return ct;
        }
        a["local_id"].as_i64().unwrap_or(0).cmp(&b["local_id"].as_i64().unwrap_or(0))
    });

    for row in &lines {
        for key in ["create_time", "local_type", "message_content", "computed_is_send", "sender_username", "local_id"] {
            assert!(row.get(key).is_some(), "行内缺少 WeFlow 认的 snake_case 键 {key}: {row}");
        }
        assert!(row.get("db_path").is_some() && row.get("table_name").is_some(), "行内需保留来源列");
    }

    // camelCase 分库经归一层补齐 + 大数 server_id + BLOB→hex
    assert_eq!(ordered[0]["message_content"].as_str(), Some("最早的一条"));
    assert_eq!(ordered[0]["sender_username"].as_str(), Some(SESSION_ID));
    assert!(ordered[1]["message_content"].as_str().unwrap().contains("<img md5="), "content 必须是完整明文，不得裁剪 XML");
    assert_eq!(ordered[2]["message_content"].as_str(), Some("你好，世界 🌍"));
    assert_eq!(ordered[2]["server_id"].as_i64(), Some(7312345678901234567));
    assert_eq!(ordered[2]["serverIdRaw"].as_str(), Some("7312345678901234567"), "大数 server_id 必须附字符串");
    assert_eq!(ordered[3]["computed_is_send"], json!(1));
    assert_eq!(ordered[3]["img_buf"].as_str(), Some("dead"), "BLOB 必须以 hex 字符串输出");
    assert!(!ordered[1].get("media_path").is_some(), "文本引擎不应伪造 media_path");

    // created_file 与 raw 路径一致（bridge 用它兜底）
    let created = run
        .events
        .iter()
        .find(|e| e["type"] == "created_file")
        .expect("必须发 created_file");
    assert_eq!(created["session_id"].as_str(), Some(SESSION_ID));
    assert_eq!(created["path"].as_str().unwrap().replace('\\', "/"), jsonl_path.replace('\\', "/"));

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn wrong_key_reports_failure_not_silence() {
    let root = fixture_root();
    let exports_dir = root.join("raw").join("exports");
    std::fs::create_dir_all(&exports_dir).unwrap();
    build_fixture(&root);

    let mut request = request_json(&root, &exports_dir);
    request["account"]["dbKey"] = json!("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");

    let run = run_engine(&request);
    assert_ne!(run.code, 0, "密钥错误必须非零退出");
    let result = run.events.last().expect("失败时也要发一个 result");
    assert_eq!(result["type"].as_str(), Some("result"));
    assert_eq!(result["success"], json!(false));
    assert!(!run.stderr.is_empty(), "错误原因要写进 stderr，WeFlow 会展示它");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn missing_session_db_exits_nonzero() {
    let root = fixture_root();
    let exports_dir = root.join("raw").join("exports");
    std::fs::create_dir_all(&exports_dir).unwrap();
    let mut request = request_json(&root, &exports_dir);
    request["account"]["sessionDb"] = json!(fwd(&root.join("nope").join("session.db")));

    let run = run_engine(&request);
    assert_ne!(run.code, 0);
    assert_eq!(run.events.last().map(|e| e["success"].clone()), Some(json!(false)));

    std::fs::remove_dir_all(&root).ok();
}
