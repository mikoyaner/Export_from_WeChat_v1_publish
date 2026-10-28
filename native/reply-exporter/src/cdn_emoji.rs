//! 表情（local_type 47）的 CDN 获取。
//!
//! 与图片不同：表情 XML / `emoticon.db` 里的 `cdnurl`、`thumburl` 是**普通 HTTP 地址**，
//! 不需要 Mars、不需要注入（WeFlow 也是这样做的：`chatService.ts` 的 `doDownloadEmoji` 直接 http.get）。
//! 本模块只在用户显式允许（`REPLY_EMOJI_FETCH=1`，默认开）时联网；串行、限速、单文件上限。
//! URL 经 curl 的 stdin 配置传入，不出现在命令行/日志里。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36 \
MicroMessenger/7.0.20.1781(0x6700143B) NetType/WIFI WindowsWechat";
const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub struct Fetcher {
    dir: PathBuf,
    last: Option<Instant>,
    delay: Duration,
    pub fetched: usize,
    pub failed: usize,
    pub bytes: u64,
}

impl Fetcher {
    pub fn new(dir: PathBuf, delay_ms: u64) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self { dir, last: None, delay: Duration::from_millis(delay_ms), fetched: 0, failed: 0, bytes: 0 }
    }

    pub fn get(&mut self, url: &str) -> Result<Vec<u8>, String> {
        // URL 是从消息 XML 的属性里抠出来的（字符串解析，不是真 XML 解析器），
        // 属性值里的字面换行会原样留下。curl 的 --config 是一行一个选项，
        // 换行 = 注入第二个选项（cfg_escape 只转义 \ 和 "，挡不住）。
        // 合法 URL 不该含控制字符，直接拒，不回 URL 原文。
        if !url_ok(url) {
            self.failed += 1;
            return Err("bad_url".into());
        }
        self.throttle();
        let seq = self.fetched + self.failed + 1;
        let tmp = self.dir.join(format!("_emoji_{seq}.part"));
        let tmp_s = tmp.to_string_lossy().to_string();
        let mut cmd = Command::new(curl_bin());
        cmd.args([
            "-sS", "-L", "--connect-timeout", "8", "--max-time", "25",
            "--max-filesize", &MAX_BYTES.to_string(),
            "-A", UA, "-H", "Referer: https://servicewechat.com/",
            "-o", &tmp_s, "-w", "%{http_code}", "--config", "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.failed += 1;
                return Err(format!("curl_spawn_{}", e.kind()));
            }
        };
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(format!("url = \"{}\"\n", cfg_escape(url)).as_bytes());
        }
        let out = match child.wait_with_output() {
            Ok(o) => o,
            Err(e) => {
                self.failed += 1;
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("curl_wait_{}", e.kind()));
            }
        };
        let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let data = std::fs::read(&tmp).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp);
        if out.status.code() == Some(63) {
            self.failed += 1;
            return Err("too_big".into());
        }
        if code != "200" {
            self.failed += 1;
            return Err(if code.is_empty() { "curl_error".into() } else { format!("http_{code}") });
        }
        if data.is_empty() {
            self.failed += 1;
            return Err("empty".into());
        }
        self.fetched += 1;
        self.bytes += data.len() as u64;
        Ok(data)
    }

    fn throttle(&mut self) {
        if let Some(t) = self.last {
            let el = t.elapsed();
            if el < self.delay {
                std::thread::sleep(self.delay - el);
            }
        }
        self.last = Some(Instant::now());
    }
}

fn curl_bin() -> String {
    if let Ok(p) = std::env::var("REPLY_CURL_BIN") {
        let p = p.trim();
        if !p.is_empty() {
            return p.to_string();
        }
    }
    let sys = Path::new(r"C:\Windows\System32\curl.exe");
    if sys.is_file() {
        return sys.to_string_lossy().to_string();
    }
    "curl".to_string()
}

fn cfg_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// curl 配置只接受"一行一个选项"，所以含任何控制字符（含 \r \n）的 URL 一律拒绝：
/// 这类 URL 只可能来自被篡改的 XML，正常 CDN 地址不会有。
fn url_ok(url: &str) -> bool {
    !url.is_empty() && url.len() <= 4096 && !url.chars().any(|c| c.is_control())
}

/// `<emoji cdnurl="…" thumburl="…">`：按序给出候选（cdnurl 可能过期，thumburl 常常还能用）。
pub fn candidate_urls(xml: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in ["cdnurl", "thumburl"] {
        if let Some(u) = crate::media::elem_attr(xml, "<emoji", name) {
            let u = xml_unescape(u.trim());
            if (u.starts_with("http://") || u.starts_with("https://")) && !out.contains(&u) {
                out.push(u);
            }
        }
    }
    out
}

/// 兼容旧调用：取第一个候选。
pub fn pick_url(xml: &str) -> Option<String> {
    candidate_urls(xml).into_iter().next()
}

fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&quot;", "\"").replace("&apos;", "'")
}

/// 从 `emoticon.db/kNonStoreEmoticonTable` 按 md5 查 cdn_url（XML 里没有时的兜底）。
pub fn db_url(account: &weflow_core::db::Account, md5: &str) -> Option<String> {
    let db = account.layout.emoticon_db.as_ref()?;
    account
        .with_conn(db, |c| -> anyhow::Result<Option<String>> {
            let mut st = c.prepare("SELECT cdn_url FROM kNonStoreEmoticonTable WHERE md5 = ?1 LIMIT 1")?;
            let mut rows = st.query([md5])?;
            if let Some(r) = rows.next()? {
                let v: String = r.get(0).unwrap_or_default();
                if v.starts_with("http") {
                    return Ok(Some(v));
                }
            }
            Ok(None)
        })
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_cdn_before_thumb_and_unescapes() {
        let xml = r#"<emoji md5="x" cdnurl="http://a/b?x=1&amp;y=2" thumburl="http://t">"#;
        assert_eq!(pick_url(xml).as_deref(), Some("http://a/b?x=1&y=2"));
    }

    #[test]
    fn ignores_non_http() {
        assert_eq!(pick_url(r#"<emoji cdnurl="md5only">"#), None);
    }

    #[test]
    fn rejects_urls_with_control_chars() {
        assert!(url_ok("https://a/b?x=1&y=2"));
        assert!(!url_ok(""));
        assert!(!url_ok("https://a/b\noutput = \"C:/x\""));   // 换行注入第二个选项
        assert!(!url_ok("https://a/b\rurl = \"http://evil\""));
        assert!(!url_ok("https://a/b\u{0000}"));
        assert!(!url_ok(&format!("https://a/{}", "x".repeat(5000))));
    }
}
