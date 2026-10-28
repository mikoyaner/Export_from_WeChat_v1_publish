# Export_from_WeChat_v1 的原生组件（Rust cargo workspace）

> 2026-10-20 从 `common/key-provider` 移入本目录。WeFlow 应用侧的原生组件在
> `WeFlow/native`；这里主要维护助手的 `reply-exporter` 和共享的 `weflow-core`。

| 目录 | 产物 | 用途 |
|---|---|---|
| `reply-exporter/` | `reply-exporter.exe`（stdin/stdout JSON） | 助手的本机只读导出器（`REPLY_EXPORTER_BIN`） |
| `wcdb-native/` | `wcdb.dll`（C ABI，koffi 加载） | 四件之一（WeFlow 现已改用 `WeFlow/native`） |
| `image-decrypt-addon/` | `image_decrypt_addon.node`（napi-rs） | 四件之一（同上） |
| `welive-export/` | `welive-export.exe`（stdin JSON → stdout NDJSON） | 四件之一（同上） |
| `key-provider/` | `key-provider.exe` | 四件之一（同上） |
| `shared/weflow-core/` | 内部共享库：SQLCipher 打开、微信 4.x 表结构、dat 解密、hardlink 解析 | — |

`docs/protocol/` 下是从 WeFlow `electron/services/wcdbCore.ts` 逐函数提取的 JSON 协议规格，
为 `wcdb-native` 的实现依据。

## 构建前提

1. **rustup + MSVC 工具链**
   - 下载 https://mirrors.tuna.tsinghua.edu.cn/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe
   - PowerShell 设置镜像后运行：
     ```powershell
     $env:RUSTUP_DIST_SERVER="https://mirrors.tuna.tsinghua.edu.cn/rustup"
     $env:RUSTUP_UPDATE_ROOT="https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup"
     .\rustup-init.exe   # 选 1) Proceed with standard installation
     ```
   - 本机 MSVC 14.36 + Windows SDK 10.0.x（装 VS 生成工具时勾选「使用 C++ 的桌面开发」）。
     rustup 通过 vswhere 自动检测；若链接失败，从 "x64 Native Tools Command Prompt for VS 2022"
     里执行 cargo build，或设置 `LIB`/`INCLUDE` 环境变量。
2. **Perl + NASM。** `rusqlite` 开了 `bundled-sqlcipher-vendored-openssl`，构建脚本要现编 OpenSSL：
   Configure 依赖 Perl，汇编阶段依赖 NASM。公开环境装 **Strawberry Perl + NASM** 并加入 `PATH`
   （Git 自带的 Perl 缺 `Locale::Maketext::Simple`，不能用；Git Bash 里需把 `/c/Strawberry/perl/bin`
   排到 `PATH` 前面，否则 `/usr/bin/perl` 抢先生效）。`native/.tools/env.sh`（便携 Perl + nasm，
   自动设 `CARGO_TARGET_DIR`）只在作者本机的完整交付目录里，公开仓库不含。
3. crates.io 镜像已配置在 `.cargo/config.toml`（rsproxy.cn）。

## 构建

```bash
cargo build --release         # 需 Strawberry Perl + NASM 已在 PATH（见「构建前提」）
```

只编助手导出器，并更新项目的运行时副本：

```bash
cargo build --release -p reply-exporter
cp target/release/reply-exporter.exe ../bin/reply-exporter.exe
```

产物：

- `target/release/reply-exporter.exe` → 助手的 `../bin/reply-exporter.exe`
- `target/release/wcdb.dll`
- `target/release/image_decrypt_addon.dll`（WeFlow 要求改名为 `.node`）
- `target/release/welive-export.exe`
- `target/release/key-provider.exe`

## 验证

```bash
../bin/reply-exporter.exe <微信数据根> --list-accounts >/dev/null; echo $?
```

输出 `0` 表示导出器能正常运行。
