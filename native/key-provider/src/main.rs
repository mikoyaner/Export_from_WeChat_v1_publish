//! key-provider —— WeFlow `keyProviderPath` 可执行组件。
//!
//! 职责：以 WeFlow 约定的 NDJSON 协议，把用户事先以已授权方式取得、并存入本地库的
//! SQLCipher DB 密钥 / .dat 图片密钥，**经真实库文件校验后**分发给 WeFlow。
//!
//! 本组件**不实现也不调用**任何从微信进程内存提取密钥的功能（该步骤属凭证提取）。
//! 它只负责：保管、校验、按协议分发。
//!
//! 拉起方式（见 keyProviderService.ts）：`spawn(exe, [], { stdio:['pipe','pipe','pipe'] })`，
//! 即无额外参数 + stdin 管道输入 → 自动进入协议模式；否则按子命令走管理模式。

mod cli;
mod protocol;
mod validate;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::run_cli(&args) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            // 协议模式下 WeFlow 以是否有 result 行判成败；此处兜底：若为管道模式仍尽量不发裸文本，
            // 但错误信息写 stderr 无害（WeFlow 的 result 解析只看 stdout 行）。
            eprintln!("key-provider: {e}");
            std::process::exit(1);
        }
    }
}
