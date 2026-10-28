//! welive-export.exe —— WeFlow `welivePath` 组件（raw-jsonl 导出引擎）。
//!
//! 协议见 docs/protocol/06-welive-raw-jsonl.md（来源 weliveBridge.ts / exportWorker.ts /
//! ExportContext.ts）。要点：
//! - 子命令 `weflow-export`（容忍任意前缀参数）；
//! - stdin 一次性读入 ExportRequest JSON；
//! - stdout 仅输出 NDJSON 事件（任何调试信息一律走 stderr，绝不污染协议）；
//! - 每会话写一个 raw JSONL 到 exportsDir，行内保留完整明文字段；
//! - 成功判定：退出码 0 + 发出 result 事件 + result.success != false。

mod types;

use anyhow::Result;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use weflow_core::db::Account;
use weflow_core::messages::stream_session_message_rows;

use types::{Event, EventSink, ExportRequest, FailedSession, RawExportManifest};

fn main() {
    let mut sink = EventSink::new();
    match run(&mut sink) {
        Ok(()) => {
            let _ = sink.flush();
            std::process::exit(0);
        }
        Err(e) => {
            // 失败：stderr 记录，尽力发一个 result.success=false，非零退出。
            eprintln!("welive-export error: {e:#}");
            let result = Event::Result {
                success: Some(false),
                success_count: None,
                fail_count: None,
                failed_sessions: None,
                session_output_paths: None,
                raw_session_output_paths: None,
                raw_export_manifests: None,
                elapsed_ms: None,
            };
            let _ = sink.emit(&result);
            let _ = sink.flush();
            std::process::exit(1);
        }
    }
}

fn run(sink: &mut EventSink) -> Result<()> {
    let started = std::time::Instant::now();
    let req: ExportRequest = types::read_request_from_stdin()?;

    eprintln!(
        "welive-export: sessions={} format={:?} exportsDir={:?} mediaDir={:?}",
        req.session_ids.len(),
        req.format,
        req.exports_dir,
        req.media_dir
    );

    // 1) 打开账号（session.db + SQLCipher 密钥）
    let account = Account::open(&req.account.session_db, req.account.db_key.trim())?;
    if let Some(me) = req.account.my_account_id.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if let Ok(mut slot) = account.my_account_id.lock() {
            *slot = Some(me.to_string());
        }
    }

    // 2) 准备 raw JSONL 输出目录
    let exports_dir = req
        .exports_dir
        .clone()
        .or_else(|| Some(req.output_dir.clone()))
        .unwrap_or_else(|| ".".to_string());
    std::fs::create_dir_all(&exports_dir)?;

    sink.emit(&Event::Ready {
        total: Some(req.session_ids.len()),
        output_dir: Some(req.output_dir.clone()),
        exports_dir: Some(exports_dir.clone()),
        media_dir: req.media_dir.clone(),
    })?;

    let ascending = req.ascending.unwrap_or(true);

    let mut raw_paths: BTreeMap<String, String> = BTreeMap::new();
    let mut manifests: BTreeMap<String, RawExportManifest> = BTreeMap::new();
    let mut failed: Vec<FailedSession> = Vec::new();
    let mut success_count = 0u64;

    // 3) 逐会话导出
    for (idx, sid) in req.session_ids.iter().enumerate() {
        sink.emit(&Event::Progress {
            phase: Some("parsing".to_string()),
            current: Some(idx as u64),
            total: Some(req.session_ids.len() as u64),
            session_id: Some(sid.clone()),
            label: Some(format!("导出会话 {}", idx + 1)),
            exported_messages: None,
            estimated_total_messages: None,
            phase_progress: None,
            phase_total: None,
        })?;

        match export_one_session(&account, sid, &exports_dir, idx, ascending) {
            Ok((path, rows, bytes)) => {
                raw_paths.insert(sid.clone(), path.clone());
                manifests.insert(
                    sid.clone(),
                    RawExportManifest {
                        path: path.clone(),
                        rows,
                        bytes,
                        images_failed: None,
                        voices_failed: None,
                        videos_failed: None,
                        emojis_failed: None,
                        files_failed: None,
                    },
                );
                success_count += 1;
                sink.emit(&Event::CreatedFile {
                    path: Some(path),
                    session_id: Some(sid.clone()),
                })?;
            }
            Err(e) => {
                eprintln!("welive-export: session {sid} failed: {e:#}");
                failed.push(FailedSession {
                    session_id: sid.clone(),
                    error: e.to_string(),
                });
                sink.emit(&Event::SessionError {
                    session_id: Some(sid.clone()),
                    error: Some(e.to_string()),
                })?;
            }
        }
    }

    sink.emit(&Event::Progress {
        phase: Some("complete".to_string()),
        current: Some(req.session_ids.len() as u64),
        total: Some(req.session_ids.len() as u64),
        session_id: req.session_ids.last().cloned(),
        label: Some("完成".to_string()),
        exported_messages: None,
        estimated_total_messages: None,
        phase_progress: None,
        phase_total: None,
    })?;

    let fail_count = failed.len() as u64;
    sink.emit(&Event::Result {
        // 只要有成功会话就 success=true；全失败则 false。
        success: Some(success_count > 0),
        success_count: Some(success_count),
        fail_count: Some(fail_count),
        failed_sessions: if failed.is_empty() { None } else { Some(failed) },
        session_output_paths: None,
        raw_session_output_paths: Some(raw_paths),
        raw_export_manifests: Some(manifests),
        elapsed_ms: Some(started.elapsed().as_millis() as u64),
    })?;

    Ok(())
}

/// 导出单个会话：流式读取消息 → 边读边写 raw JSONL → 返回 (绝对路径, 行数, 字节数)。
/// 内存只与「当前一行」有关，与整会话大小无关；整会话（百万级）不再先攒 Vec<Value>。
fn export_one_session(
    account: &Account,
    sid: &str,
    exports_dir: &str,
    idx: usize,
    ascending: bool,
) -> Result<(String, u64, u64)> {
    let file_path = PathBuf::from(exports_dir).join(format!("session_{idx}.jsonl"));
    let mut file = std::io::BufWriter::with_capacity(1 << 18, std::fs::File::create(&file_path)?);
    let mut byte_count = 0u64;

    let streamed = stream_session_message_rows(account, sid, 0, 0, ascending, |row: Value| -> Result<()> {
        let mut obj: Map<String, Value> = match row {
            Value::Object(m) => m,
            other => {
                let mut m = Map::new();
                m.insert("value".to_string(), other);
                m
            }
        };
        // server_id 大数保真：附 serverIdRaw 字符串（WeFlow ctx 读取该别名）
        if let Some(Value::Number(n)) = obj.get("server_id") {
            obj.insert("serverIdRaw".to_string(), Value::String(n.to_string()));
        }
        let mut line = serde_json::to_string(&Value::Object(obj))?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        byte_count += line.len() as u64;
        Ok(())
    });

    let flushed = match streamed {
        Ok(rows) => file.flush().map(|_| rows).map_err(anyhow::Error::from),
        Err(e) => Err(e),
    };
    drop(file);

    // 半截 JSONL 会被 WeFlow 当成完整数据读，失败时一律删掉
    let rows = match flushed {
        Ok(rows) => rows,
        Err(e) => {
            let _ = std::fs::remove_file(&file_path);
            return Err(e);
        }
    };

    // 以文件系统真实字节数为准（与 manifest.bytes 校验对齐）
    let actual_bytes = std::fs::metadata(&file_path)?.len();
    debug_assert_eq!(byte_count, actual_bytes);
    Ok((
        weflow_core::messages::path_str(&file_path),
        rows,
        actual_bytes,
    ))
}
