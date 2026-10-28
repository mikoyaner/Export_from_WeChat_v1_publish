//! wcdb.dll —— WeFlow `wcdbLibPath` 组件（koffi C ABI）。
//!
//! 协议规格：本仓库 `docs/protocol/01~05-*.md`（逐函数对照 wcdbCore.ts 提取）。
//! 全局约定见 ffi.rs 头注释：状态码 0=成功、out 字符串唯一经 wcdb_free_string 归还、
//! rc==0 时 outJson 必非空、panic 不越 FFI 边界。

mod ffi;
mod contacts;
mod media;
mod messages;
mod misc;
mod query;
mod sessions;
mod stats;
mod tables;

use ffi::*;
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use weflow_core::db::Account;

// ── 全局账号注册表 ──
// WeFlow 同一时刻只维护一个活动 handle，但 testConnection 会产生临时 handle，
// 且 shutdown 要能释放全部资源，因此用注册表而非单例。
// 所有调用来自同一 Node worker 线程并串行化，全局 Mutex 足够。

static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

fn registry() -> &'static Mutex<HashMap<i64, Arc<Account>>> {
    static REG: OnceLock<Mutex<HashMap<i64, Arc<Account>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_account(handle: i64) -> Option<Arc<Account>> {
    registry().lock().ok()?.get(&handle).cloned()
}

/// 查询/写入类导出的公共入口：handle 路由 + panic 防护。
/// 后续 messages/sessions/contacts 等模块通过闭包实现各自逻辑。
pub(crate) fn with_account<F: FnOnce(&Account) -> i32>(handle: i64, f: F) -> i32 {
    guard(|| match get_account(handle) {
        Some(account) => f(&account),
        None => ERR_GENERIC,
    })
}

// ── 生命周期（docs/protocol/01 §1-§7） ──

#[no_mangle]
pub extern "C" fn wcdb_init() -> i32 {
    guard(|| {
        // 注册表惰性初始化，无全局引擎状态需要预热。
        // 必须支持与 shutdown 多次成对重入。
        let _ = registry();
        OK
    })
}

#[no_mangle]
pub extern "C" fn wcdb_shutdown() -> i32 {
    guard(|| {
        // 先停后台线程（cloud）与游标，再释放所有 handle、连接。
        misc::stop_cloud();
        stats::clear_cursors();
        if let Ok(mut reg) = registry().lock() {
            reg.clear();
        }
        OK
    })
}

#[no_mangle]
pub extern "C" fn wcdb_purge_memory() -> i32 {
    guard(|| {
        if let Ok(reg) = registry().lock() {
            for account in reg.values() {
                account.shrink_memory();
            }
        }
        OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_open_account(
    path: *const c_char,
    key: *const c_char,
    handle: *mut i64,
) -> i32 {
    guard(|| {
        if handle.is_null() {
            return ERR_GENERIC;
        }
        *handle = 0;
        let path = cstr(path);
        let key = cstr(key);
        if path.is_empty() || key.is_empty() {
            return ERR_GENERIC;
        }
        match Account::open(path, key) {
            Ok(account) => {
                let id = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
                match registry().lock() {
                    Ok(mut reg) => {
                        reg.insert(id, account);
                        *handle = id;
                        OK
                    }
                    Err(_) => ERR_GENERIC,
                }
            }
            Err(e) => {
                // 错误分类映射到 TS formatInitProtectionError 的状态码
                let msg = e.to_string();
                let code = if msg.contains("不存在") {
                    ERR_SESSION_DB_NOT_FOUND
                } else if msg.contains("密钥错误") || msg.contains("open ") {
                    ERR_DB_OPEN_FAILED
                } else {
                    ERR_DB_OPEN_FAILED
                };
                // 详细原因进内部日志，wcdb_get_logs 可取（TS 在 open 失败时会读日志）
                push_log(&format!("wcdb_open_account failed: {msg}"));
                code
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn wcdb_close_account(handle: i64) -> i32 {
    guard(|| {
        // WeFlow 从不调用（历史崩溃已弃用），但符号必须存在且行为健壮。
        match registry().lock() {
            Ok(mut reg) => {
                reg.remove(&handle);
                OK
            }
            Err(_) => ERR_GENERIC,
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_free_string(ptr: *mut c_void) {
    // 唯一的内存归还通道；void 返回，不允许 panic。
    let _ = guard(|| {
        free_out(ptr);
        OK
    });
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_set_my_account_id(handle: i64, account_id: *const c_char) -> i32 {
    with_account(handle, |account| {
        let id = cstr(account_id).trim().to_string();
        if let Ok(mut slot) = account.my_account_id.lock() {
            *slot = if id.is_empty() { None } else { Some(id) };
        }
        OK
    })
}

// ── 内部日志环形缓冲（wcdb_get_logs 数据源，open 失败时 TS 会读取） ──

const LOG_CAPACITY: usize = 256;

fn log_buffer() -> &'static Mutex<Vec<String>> {
    static LOGS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) fn push_log(msg: &str) {
    if let Ok(mut logs) = log_buffer().lock() {
        if logs.len() >= LOG_CAPACITY {
            logs.remove(0);
        }
        logs.push(format!(
            "[{}] {msg}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ));
    }
}

pub(crate) fn take_logs_json() -> String {
    match log_buffer().lock() {
        Ok(logs) => serde_json::to_string(&*logs).unwrap_or_else(|_| "[]".to_string()),
        Err(_) => "[]".to_string(),
    }
}
