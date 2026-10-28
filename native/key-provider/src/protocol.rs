//! WeFlow `keyProviderPath` NDJSON 协议类型。
//!
//! 契约来自 WeFlow `electron/services/keyProviderService.ts`：
//! - stdin：一行 JSON 请求（`child.stdin.end(JSON.stringify(request) + "\n")`）；
//! - stdout：0..n 行 `{"type":"progress","message","level"}`，随后**恰好一行**
//!   `{"type":"result", ...}`。result 去掉 `type` 字段后整体即为结果对象。
//!   - DbKeyResult:  { success, key?, accountId?, error?, logs? }
//!   - ImageKeyResult: { success, xorKey?, aesKey?, verified?, error? }
//! - 未输出 result 行 → WeFlow 报"未返回结果"；故任何分支都必须发 result 行。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 请求鉴别字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    GetDbKey,
    GetImageKey,
    ScanImageKeyMemory,
}

impl Action {
    fn from_str(s: &str) -> Option<Action> {
        match s {
            "get_db_key" => Some(Action::GetDbKey),
            "get_image_key" => Some(Action::GetImageKey),
            "scan_image_key_memory" => Some(Action::ScanImageKeyMemory),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawRequest {
    pub action: String,
    #[serde(default)]
    pub db_path: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub internal_db_key_hex: Option<String>,
    #[serde(default)]
    pub account_dir: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub action: Action,
    pub db_path: Option<String>,
    pub account_id: Option<String>,
    pub internal_db_key_hex: Option<String>,
    pub account_dir: Option<String>,
}

impl Request {
    /// 解析一行 stdin。返回 Err 表示非法/未知请求（调用方应发 success=false 的 result）。
    pub fn parse(line: &str) -> anyhow::Result<Request> {
        let raw: RawRequest = serde_json::from_str(line)?;
        let action = Action::from_str(&raw.action)
            .ok_or_else(|| anyhow::anyhow!("未知 action: {}", raw.action))?;
        Ok(Request {
            action,
            db_path: raw.db_path,
            account_id: raw.account_id,
            internal_db_key_hex: raw.internal_db_key_hex,
            account_dir: raw.account_dir,
        })
    }
}

/// progress 事件行：`{"type":"progress","message":..,"level":..}`。
#[derive(Debug, Serialize)]
pub struct ProgressEvent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub message: String,
    pub level: u8,
}

/// result 事件行：`{"type":"result", ...}`，字段 camelCase（对齐 DbKeyResult/ImageKeyResult）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultEvent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xor_key: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aes_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
}

/// stdout 事件：progress 或 result。untagged 包装，`type` 字段在各自结构体内。
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Event {
    Progress(ProgressEvent),
    Result(ResultEvent),
}

impl Event {
    pub fn progress(message: impl Into<String>, level: u8) -> Event {
        Event::Progress(ProgressEvent { kind: "progress", message: message.into(), level })
    }

    pub fn db_ok(key: String, account_id: Option<String>, logs: Vec<String>) -> Event {
        Event::Result(ResultEvent {
            kind: "result",
            success: true,
            key: Some(key),
            account_id,
            error: None,
            logs: Some(logs),
            xor_key: None,
            aes_key: None,
            verified: None,
        })
    }

    pub fn image_ok(xor_key: Value, aes_key: Option<String>, verified: bool) -> Event {
        Event::Result(ResultEvent {
            kind: "result",
            success: true,
            key: None,
            account_id: None,
            error: None,
            logs: None,
            xor_key: Some(xor_key),
            aes_key,
            verified: Some(verified),
        })
    }

    pub fn err(error: impl Into<String>) -> Event {
        Event::Result(ResultEvent {
            kind: "result",
            success: false,
            key: None,
            account_id: None,
            error: Some(error.into()),
            logs: None,
            xor_key: None,
            aes_key: None,
            verified: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_db_key_request() {
        let line = r#"{"action":"get_db_key","dbPath":"E:/x/db_storage/session/session.db","accountId":"wxid_1","internalDbKeyHex":"ab"}"#;
        let req = Request::parse(line).unwrap();
        assert_eq!(req.action, Action::GetDbKey);
        assert_eq!(req.account_id.as_deref(), Some("wxid_1"));
        assert_eq!(req.internal_db_key_hex.as_deref(), Some("ab"));
    }

    #[test]
    fn unknown_action_is_error() {
        assert!(Request::parse(r#"{"action":"nope"}"#).is_err());
    }

    #[test]
    fn result_event_shape() {
        let json = serde_json::to_string(&Event::db_ok("aa".into(), Some("wx".into()), vec![])).unwrap();
        assert!(json.contains("\"type\":\"result\""));
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"key\":\"aa\""));
        assert!(json.contains("\"accountId\":\"wx\""));
        assert!(!json.contains("xorKey"));
    }

    #[test]
    fn progress_event_shape() {
        let json = serde_json::to_string(&Event::progress("hi", 1)).unwrap();
        assert_eq!(json, r#"{"type":"progress","message":"hi","level":1}"#);
    }
}
