//! 监控管道 / cloud 云控 / VerifyUser（docs/protocol/04-tables-misc.md 尾部）。
//!
//! 这三组都**不依赖 open_account/handle**，且都是可选符号（缺失只降级对应功能）。
//!
//! 现状：
//! - **监控管道**：真实的 Windows Named Pipe 服务端 + `ReadDirectoryChangesW` 目录监控
//!   需要 `windows` crate 与实机联调，暂返回非 0（TS 据此关闭监控功能，行为优雅降级）。
//!   后续可在 misc.rs 内补齐：NDJSON 单向推送 `{action,table,...}`（协议见 04 §监控管道线上协议）。
//! - **cloud 三件套**：无上报端点，`init` 起一个幂等后台线程、`report` 收下并记日志后返回 0
//!   （TS 据此认为整批已接收并 splice），`stop` 停线程并 join。
//! - **VerifyUser**：安全默认——始终写入 `{"success":false,...}`，绝不自动通过（避免绕过应用锁）。

use crate::ffi::*;
use serde_json::json;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

// ── 监控管道（暂禁用） ──

#[no_mangle]
pub extern "C" fn wcdb_start_monitor_pipe() -> i32 {
    guard(|| {
        crate::push_log("wcdb_start_monitor_pipe: not implemented, monitor disabled");
        // 返回非 0 → startMonitor() 返回 false，TS 整体关闭监控（优雅降级）
        ERR_GENERIC
    })
}

#[no_mangle]
pub extern "C" fn wcdb_stop_monitor_pipe() {
    // 幂等；未启动时安全
    let _ = guard(|| OK);
}

#[no_mangle]
pub extern "C" fn wcdb_get_monitor_pipe_name(out_name: *mut *mut c_void) -> i32 {
    guard(|| {
        // 未启动管道：返回非 0，TS 回退固定名（但监控已关，不会真正连接）
        let _ = out_name;
        ERR_GENERIC
    })
}

// ── cloud 云控（无端点，接受即丢弃 + 记日志） ──

struct CloudHandle {
    stop: Arc<AtomicBool>,
    joiner: Option<JoinHandle<()>>,
}

fn cloud_slot() -> &'static Mutex<Option<CloudHandle>> {
    static SLOT: OnceLock<Mutex<Option<CloudHandle>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

#[no_mangle]
pub extern "C" fn wcdb_cloud_init(interval_seconds: i32) -> i32 {
    guard(|| {
        let interval = if interval_seconds > 0 { interval_seconds as u64 } else { 300 };
        let mut slot = match cloud_slot().lock() {
            Ok(s) => s,
            Err(_) => return ERR_GENERIC,
        };
        if slot.is_some() {
            // 幂等：已初始化直接成功
            return OK;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let joiner = std::thread::spawn(move || {
            // 无云控端点：周期空转，仅维持线程语义（stop/shutdown 能干净停掉）
            while !stop_thread.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(interval.min(3600)));
            }
        });
        *slot = Some(CloudHandle { stop, joiner: Some(joiner) });
        OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_cloud_report(stats_json: *const c_char) -> i32 {
    guard(|| {
        // 字符串仅调用期有效：立即复制。收下即视为已接收（返回 0），TS 会 splice 该批。
        let payload = cstr(stats_json);
        let len = payload.len();
        crate::push_log(&format!("wcdb_cloud_report accepted {len} bytes (no endpoint, dropped)"));
        let _ = json!({}); // 保留 serde_json 依赖引用一致性
        OK
    })
}

#[no_mangle]
pub extern "C" fn wcdb_cloud_stop() {
    let _ = guard(|| {
        stop_cloud_locked();
        OK
    });
}

fn stop_cloud_locked() {
    if let Ok(mut slot) = cloud_slot().lock() {
        if let Some(mut handle) = slot.take() {
            handle.stop.store(true, Ordering::Relaxed);
            if let Some(j) = handle.joiner.take() {
                let _ = j.join();
            }
        }
    }
}

/// wcdb_shutdown 前调用，确保后台线程不会在资源释放后仍访问。
pub(crate) fn stop_cloud() {
    stop_cloud_locked();
}

// ── VerifyUser（PascalCase 导出名，调用方分配 1024B 缓冲区，不用 free_string） ──

#[no_mangle]
pub unsafe extern "C" fn VerifyUser(
    hwnd: i64,
    message: *const c_char,
    out_result: *mut c_char,
    max_len: i32,
) {
    let _ = hwnd;
    let _ = cstr(message);
    // 安全默认：绝不自动通过（未实现 Windows Hello / UserConsentVerifier）。
    let body = json!({ "success": false, "error": "当前数据服务未实现身份验证" }).to_string();
    write_caller_buffer(out_result, &body, max_len);
}

/// 向调用方分配的缓冲区写入 NUL 结尾字符串，严格不越过 max_len（含 NUL）。
unsafe fn write_caller_buffer(out: *mut c_char, s: &str, max_len: i32) {
    if out.is_null() || max_len <= 0 {
        return;
    }
    let cap = max_len as usize;
    let bytes = s.as_bytes();
    let n = bytes.len().min(cap.saturating_sub(1));
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast::<u8>(), n);
    *out.add(n) = 0;
}
