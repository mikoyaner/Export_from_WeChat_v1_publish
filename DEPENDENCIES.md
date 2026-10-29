# Export_from_WeChat_v1 依赖清单

> 本项目清单（实测 2026-10-24）。“独立”= 不依赖其他项目文件夹；不是裸机便携包。
> ⚠ **公开仓库（GitHub）不含 `bin/`、`vendor/`、`node_modules/`、`native/target`、`native/.tools`**：
> 体积原因不入库，按 `使用说明书.md §1.2.1` 自行编译/下载/`npm install`。
> 下表的“随项目”指**作者本机的完整目录**。

## 1. 运行时依赖

| 依赖 | 版本/位置 | 必需 |
|---|---|---|
| Windows | Windows 10/11 x64 | 是 |
| Python | 3.13.9；**只用标准库，无 pip 依赖**；必须能从 `PATH` 调 `python`，或设 `REPLY_PY_BIN` | 是 |
| Node.js | 24.21.0；必须能从 `PATH` 调 `node`，或设 `REPLY_NODE_BIN` | 是（飞书入口、语音 ASR） |
| 微信 | 4.x 本机登录；本机实测 4.1.15.13，其他 4.x 版本未验证 | 是 |
| 微信数据根 | `<微信数据根>`（`REPLY_DATA_ROOT`） | 是 |
| 导出器 | `bin\reply-exporter.exe` | 是 |
| ffmpeg | `bin\ffmpeg.exe`（7.0 full，含 HEVC） | 是（wxgf/表情转图） |
| 图片助手 | `bin\img_helper.dll` | 可选（补图模式） |
| VC++ 运行库 | `bin\msvcp140.dll`、`msvcp140_1.dll`、`vcruntime140.dll`、`vcruntime140_1.dll` | 是（`reply-exporter` 依赖 VCRUNTIME140；`img_helper` 依赖 MSVCP140） |
| ASR 依赖/模型 | `vendor\`（约 251 MB） | 是（语音转写） |
| 飞书 SDK | `node_modules\`（约 40 MB） | 是 |
| 环境变量 | `.env`（含真实凭据，gitignored） | 是 |
| 脱敏规则 | 项目内 `config\脱敏规则.json`；`REPLY_CONFIG_DIR` 默认 `config`（相对项目根） | 是 |
| 人名词表 | **项目外** `REPLY_WORDLIST_DIR`（`<你的词表目录>`）；含真实姓名，不进项目/git | 是（缺了人名不会被替换） |
| 密钥库 | `%APPDATA%\weflow-key-provider\store.json` | 是（逐机重扫） |
| 运行状态 | `%APPDATA%\reply-assistant\` | 是（可重建） |
| 网络 | 飞书长连接、LLM/VLM、企微 webhook | 是 |

## 2. 编译 Rust

| 依赖 | 版本/位置 |
|---|---|
| Rust | 1.99.0 `stable-x86_64-pc-windows-msvc` |
| MSVC | 14.36（含 Windows SDK 10.0.x） |
| Windows SDK | 10.0.22000 |
| Perl | `native\.tools\perl-portable`（不要用 Git 自带 Perl） |
| 网络 | 首次 `cargo build` 需要联网拉 crate（本机离线缓存不完整） |

```bash
cd /d/Export_from_WeChat/Export_from_WeChat_v1/native
source .tools/env.sh
cargo build --release -p reply-exporter
cp target/release/reply-exporter.exe ../bin/reply-exporter.exe
```

只改 Python 时不需要编译 Rust。

## 3. 验证

```bash
python -c "import pipeline; print(pipeline.EXPORTER_BIN)"
bin/reply-exporter.exe <微信数据根> --list-accounts >/dev/null; echo $?
bin/ffmpeg.exe -version
python -c "import imghelper; print(imghelper.dll_path(), imghelper.available())"
```

2026-10-21 本机实测：

| 检查 | 结果 |
|---|---|
| `python -m py_compile *.py` | 通过 |
| `node --check feishu_ws.cjs` / `voice2text.cjs` | 通过 |
| `require('@larksuiteoapi/node-sdk')` | 通过 |
| `voice2text.cjs` 无参启动 | 依赖/模型均识别为“随包 vendor” |
| `Desensitizer(config, REPLY_WORDLIST_DIR)` | 通过，加载 403 个人名词条 |
| 从非项目 cwd 导入 `pipeline` | 相对 `config`/`bin` 仍解析到项目根；`path_problems()` 为 `([], [])` |
| `%USERPROFILE%` / 相对路径覆盖 | `%USERPROFILE%/xwechat_files` 正确展开；相对 `bin/reply-exporter.exe` 正确落到项目 `bin/` |
| `python pipeline.py --diagnose` | 7 个路径全部显示 `✅` 及解析后的绝对路径 |
| `feishu_ws.selfCheck()`（从非项目 cwd） | 导出器/数据根/Python 桥均显示 `✓` |
| 缺数据根 / 缺词表 | 分别给出 fatal 与 warning；`check_paths(strict=True)` 抛出明确错误 |
| `imghelper._load()` | DLL 加载通过（VC 运行库已满足） |
| `bin/reply-exporter.exe --list-accounts` | exit 0 |
| `cargo build --release -p reply-exporter`（复用 target） | exit 0，0.21 s |
| 隔离目录全量构建（全新 target + 便携 Perl/NASM） | exit 0，10m30s；产物 md5 `61f3b600…`，`--list-accounts` exit 0 |

未验证：飞书/LLM/企微完整链路。

## 4. 拷贝/迁移

- **整目录拷贝**（资源管理器或 `robocopy /E /COPY:DAT`），不要 `git clone`：`.env`、`bin/`、`vendor/`、`联系人档案/` 都被 gitignore。
- 项目内四件（`keyscan_readonly.py`、`bin\reply-exporter.exe`、`bin\ffmpeg.exe`、`bin\img_helper.dll`）默认按项目根自动解析，**不需要写进 `.env`**。
- `.env` 只需要关心外部路径：`REPLY_DATA_ROOT`、`REPLY_WORDLIST_DIR`；`REPLY_CONFIG_DIR=config` 是相对项目根。三者都支持绝对路径、`%VAR%`、`~` 和相对路径；如设置了 `REPLY_PY_BIN`/`REPLY_NODE_BIN` 也要按目标机改。
- 目标机需另装 Python 3 与 Node.js；微信数据、人名词表、密钥库不在项目内。
- `.env` 含真实 API key；`联系人档案/` 含隐私；两者都不要拷到不可控介质。
  （`native\.scratch\` 里原有一份明文扫描密钥 `scan.txt`，**已于 2026-10-24 删除**——核对过 37/37 全部存在于 `%APPDATA%\weflow-key-provider\store.json`，属纯冗余副本；日后若再生成同类扫描输出，同样不要随项目拷出）
- 🔴 **`.env` 里四组真实凭据（DashScope / VLM 聚合站 / 企微 webhook / 飞书 App Secret）应轮换**——明文落盘过即视为可能泄露，逐项后果与轮换入口见 [SECURITY-NOTE.md](SECURITY-NOTE.md)。

## 5. 已知非独立点

- `voice2text.cjs` 在 `vendor\` 缺失时才退回 `WeFlow_v2`；正常交付 `vendor\` 已包含。
- `reply-exporter` 代码里仍有硬编码的 `WeFlow_v2\...\ffmpeg.exe` 兜底；`.env` 的 `WEFLOW_FFMPEG` 优先，运行时不受影响。直接运行 `native\target\release\reply-exporter.exe` 时同目录没有 ffmpeg，需设 `WEFLOW_FFMPEG` 或把 `ffmpeg.exe` 放到同目录；正常入口用 `bin\` 副本，不受影响。
- Python、Node、微信数据、人名词表（`REPLY_WORDLIST_DIR`）、`%APPDATA%` 仍是宿主/本机依赖；规则已随项目放 `config\`。
- 路径解析规则：绝对路径原样；相对路径按项目根（不是当前工作目录）；支持 `%VAR%` 和 `~`。启动/`--diagnose` 会列出解析结果与存在性。
- 微信版本耦合：`keyscan_readonly.py` 的 Config.Cipher 偏移、`img_helper.dll` 的进程补丁偏移都是按 4.1.15.13 实测的；微信升级后可能失效，需要重新验证。
- 干净环境全量 Rust 编译需要联网拉 crate，并用 `native\.tools\perl-portable`（含 NASM）编 vendored OpenSSL；已在隔离目录实测成功（10m30s，产物 md5 与现有 `bin` 不同，构建非确定）。只改 Python 不需要。
