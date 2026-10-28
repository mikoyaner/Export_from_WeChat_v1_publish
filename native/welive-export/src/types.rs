//! WeLive 导出引擎与 WeFlow 之间的进程协议类型。
//!
//! 严格对应 WeFlow `electron/services/weliveBridge.ts`：
//! - stdin：一次性读入 `ExportRequest` JSON；
//! - stdout：按行输出 `Event` 的 NDJSON（snake_case 字段，`type` 为鉴别器）。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufWriter, Write};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    /// 会话库路径（db_storage/session/session.db 或其他分库），SQLCipher 加密。
    pub session_db: String,
    /// SQLCipher 密钥（hex 字符串）。
    pub db_key: String,
    pub my_account_id: Option<String>,
    /// 账号根目录，如 <微信数据根>\wxid_xxx。
    pub account_dir: Option<String>,
    /// dat 解密 XOR key：可能是数字或字符串（十六进制/十进制），见 normalize_image_xor_key。
    pub image_xor_key: Option<Value>,
    pub image_aes_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    pub account: AccountInfo,
    pub session_ids: Vec<String>,
    pub output_dir: String,
    pub exports_dir: Option<String>,
    /// 仅单会话 chunk 时提供（媒体导出模式）。
    pub media_dir: Option<String>,
    /// 'image' | 'voice' | 'video' | 'emoji' | 'file'
    pub media_types: Option<Vec<String>>,
    pub emoji_cache_dir: Option<String>,
    pub formatted_dir: Option<String>,
    /// WeFlow 固定传 'raw-jsonl'。
    pub format: Option<String>,
    pub preferred_output_path: Option<String>,
    pub parse_content: Option<bool>,
    pub compact_raw: Option<bool>,
    pub preserve_message_content: Option<bool>,
    pub sanitize: Option<bool>,
    pub batch_size: Option<u32>,
    pub ascending: Option<bool>,
    pub options: Option<Value>,
}

/// 对应 exportWorker.ts `normalizeImageXorKey` 语义：number 直接用；
/// 字符串按 0x 前缀十六进制或十进制解析。
pub fn normalize_image_xor_key(value: &Option<Value>) -> Option<u32> {
    match value {
        Some(Value::Number(n)) => n.as_u64().map(|v| v as u32),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                return None;
            }
            if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                u32::from_str_radix(hex, 16).ok()
            } else {
                t.parse::<u32>().ok().or_else(|| u32::from_str_radix(t, 16).ok())
            }
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FailedSession {
    pub session_id: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RawExportManifest {
    pub path: String,
    pub rows: u64,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images_failed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voices_failed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub videos_failed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emojis_failed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_failed: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Ready {
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output_dir: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        exports_dir: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        media_dir: Option<String>,
    },
    Progress {
        #[serde(skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        current: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        exported_messages: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        estimated_total_messages: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        phase_progress: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        phase_total: Option<u64>,
    },
    CreatedFile {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    CreatedDir {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    SessionError {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Result {
        #[serde(skip_serializing_if = "Option::is_none")]
        success: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        success_count: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        fail_count: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        failed_sessions: Option<Vec<FailedSession>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_output_paths: Option<BTreeMap<String, String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_session_output_paths: Option<BTreeMap<String, String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_export_manifests: Option<BTreeMap<String, RawExportManifest>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
    },
}

/// 按行写 stdout 并立即 flush（WeFlow 端按 '\n' 增量解析）。
pub struct EventSink {
    writer: BufWriter<std::io::Stdout>,
}

impl EventSink {
    pub fn new() -> Self {
        Self { writer: BufWriter::new(std::io::stdout()) }
    }

    pub fn emit(&mut self, event: &Event) -> anyhow::Result<()> {
        let line = serde_json::to_string(event)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn flush(&mut self) -> anyhow::Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}

pub fn read_request_from_stdin() -> anyhow::Result<ExportRequest> {
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
    let request: ExportRequest = serde_json::from_str(buf.trim())?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serialization_matches_bridge() {
        let event = Event::Result {
            success: Some(true),
            success_count: Some(2),
            fail_count: Some(0),
            failed_sessions: None,
            session_output_paths: None,
            raw_session_output_paths: Some(BTreeMap::from([("sid".to_string(), "p.jsonl".to_string())])),
            raw_export_manifests: Some(BTreeMap::from([(
                "sid".to_string(),
                RawExportManifest { path: "p.jsonl".into(), rows: 10, bytes: 4096, images_failed: Some(1), voices_failed: None, videos_failed: None, emojis_failed: None, files_failed: None },
            )])),
            elapsed_ms: Some(123),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"result\""));
        assert!(json.contains("\"success_count\":2"));
        assert!(json.contains("\"raw_session_output_paths\""));
        assert!(json.contains("\"images_failed\":1"));
        assert!(!json.contains("voices_failed"));
    }

    #[test]
    fn request_deserialization() {
        let raw = r#"{
            "account": {"sessionDb": "E:/x/session.db", "dbKey": "ab", "imageXorKey": "0x5a", "imageAesKey": "0123456789abcdef"},
            "sessionIds": ["a@chatroom"],
            "outputDir": "D:/out",
            "format": "raw-jsonl",
            "batchSize": 20000,
            "ascending": true,
            "options": {"format": "chatlab"}
        }"#;
        let req: ExportRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.session_ids, vec!["a@chatroom"]);
        assert_eq!(normalize_image_xor_key(&req.account.image_xor_key), Some(0x5a));
        assert_eq!(req.batch_size, Some(20000));
    }
}
