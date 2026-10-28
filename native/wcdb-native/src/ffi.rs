//! C ABI 公共设施：状态码、出参内存约定、panic 防护。
//!
//! 协议依据 `docs/protocol/01-core-sessions-messages.md` §0：
//! - 所有 out 字符串 = DLL 分配的 NUL 结尾 UTF-8，唯一归还通道 `wcdb_free_string`；
//! - rc==0 时 outJson 不允许空指针（空结果集也要给 `"[]"`/`"{}"`）；
//! - rc!=0 时 TS 不读输出内容，错误文案可写入 outError（纯文本）。
//!
//! 这里的辅助函数都声明为 **safe fn**（内部用 unsafe 块），因为它们要在
//! `with_account`/`guard` 传入的闭包里调用——闭包体不会继承外层 `unsafe extern fn`
//! 的 unsafe 上下文。契约：out 指针由 TS 保证有效或为 NULL；入参 C 字符串由 TS 保证
//! NUL 结尾或为 NULL。

use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

// ── 状态码（wcdbCore.ts formatInitProtectionError 映射表） ──
pub const OK: i32 = 0;
pub const ERR_GENERIC: i32 = -1;
pub const ERR_SESSION_DB_NOT_FOUND: i32 = -3;
pub const ERR_DB_OPEN_FAILED: i32 = -4;
pub const ERR_SCHEMA_UNREADABLE: i32 = -5;
pub const ERR_NO_SESSION_TABLE: i32 = -6;
/// 会话表缺排序时间字段；在消息接口中 = message schema mismatch。
pub const ERR_SCHEMA_MISMATCH: i32 = -7;
pub const ERR_NO_MESSAGE_DB: i32 = -3;
pub const ERR_PANIC: i32 = -100;

/// panic 不得越过 FFI 边界：所有导出函数体都包在 guard 内。
pub fn guard<F: FnOnce() -> i32>(f: F) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(rc) => rc,
        Err(_) => ERR_PANIC,
    }
}

/// 读取可空 C 字符串；NULL 按空串处理（协议允许部分入参为 NULL，如
/// contacts_compact 的空列表、delete_message 的 dbPathHint 空串）。
pub fn cstr<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
}

/// 分配 out 字符串：返回给 TS 的指针，之后必经 wcdb_free_string 归还。
pub fn alloc_out(s: &str) -> *mut c_void {
    // 内嵌 NUL 会截断 C 字符串语义，替换为空格保证 JSON/文本完整性
    let sanitized = if s.contains('\0') { s.replace('\0', " ") } else { s.to_string() };
    CString::new(sanitized)
        .unwrap_or_else(|_| CString::new("").unwrap())
        .into_raw()
        .cast::<c_void>()
}

/// 把字符串写入 `_Out_ void**` 出参；out 为 NULL 时静默跳过（TS 不会这么调）。
pub fn write_out(out: *mut *mut c_void, s: &str) {
    if !out.is_null() {
        unsafe { *out = alloc_out(s) };
    }
}

/// 释放 alloc_out 分配的指针；容忍 NULL。
pub fn free_out(ptr: *mut c_void) {
    if !ptr.is_null() {
        unsafe { drop(CString::from_raw(ptr.cast::<c_char>())) };
    }
}

/// 公共出参收敛：Ok(json) → 写 outJson 返回 OK；Err → 记日志返回指定错误码。
/// 覆盖 tables/query 组绝大多数导出（rc!=0 时 TS 只把状态码拼进中文提示）。
pub fn emit_json(out: *mut *mut c_void, result: anyhow::Result<String>, err_code: i32) -> i32 {
    match result {
        Ok(json) => {
            write_out(out, &json);
            OK
        }
        Err(e) => {
            crate::push_log(&format!("wcdb error: {e}"));
            err_code
        }
    }
}
