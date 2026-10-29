# AI 微信回复助手（本机只读版）

> 在手机上给飞书机器人发一句「回复 张三 30」，本机把指定微信会话最近 30 条导出、**脱敏**、交给云端模型，
> 三条回复建议（A/B/C）推到企业微信 —— **发不发、发什么，最终都由你自己在微信里决定**。

只处理**这台电脑上已登录的微信账号**的数据；只读数据库，不注入、不 hook、不需要管理员权限、不修改微信的任何文件。

## 三条红线

1. **AI 只出建议，永不代发送** —— 代码里不存在"调用微信发送"的能力，最后一步永远是人手动粘贴。
2. **数据出境先脱敏** —— 人名替换为 `[人物N]`，手机号 / wxid / 群 ID / 公众号 ID / 身份证按规则打码；
   出境前还会做一次自查，命中即**拒绝发送**。
3. **只读** —— 读库是只读连接；取密钥是**只读**扫描微信进程内存里的 `Config.Cipher`（不开写权限）。
   唯一会碰微信进程的是可选功能「补图」，默认关闭、一次只对一条命令生效、用完立即还原。

## 它不做什么

- 不代替你发送，不自动回复。
- 不联网拉微信的云端媒体：图片/语音若从未在本机落盘（`not_local`），只能靠你在微信里打开一次、
  或用可选的「补图」让微信自己去下载。
- 不抓取/破解他人数据：只读本机当前登录账号的库。

## 运行环境

| 依赖 | 说明 |
|---|---|
| Windows 10/11 x64 | 本机实测 Win11 |
| Python 3.10+ | **只用标准库**，无需 pip 安装 |
| Node.js 20+ | 飞书长连接入口 + 语音转写 |
| 微信 4.x（Windows） | 必须在本机**登录并运行**（数据库密钥只在运行中的进程内存里） |
| Rust 1.75+ / MSVC 生成工具 | 仅编译导出器时需要 |

## 仓库里**没有**、需要你自己准备的东西

仓库只放源码与文档，下面这些按体积/隐私原因不入库（都在 `.gitignore` 里）：

| 缺什么 | 怎么补 |
|---|---|
| `bin/reply-exporter.exe` | **去 [Releases](../../releases/latest) 下载 `reply-exporter.exe` 放进 `bin/`**（Windows x64，未签名；首次运行 Windows 可能提示“已保护你的电脑”→ 更多信息 → 仍要运行）。没有可用 Release 时再自行编译，见下文「构建」 |
| `bin/ffmpeg.exe` | 自行获取 ffmpeg 7.x **full** 构建（需 HEVC 解码，用于微信 wxgf 图转换） |
| `bin/img_helper.dll` | 可选，「补图」功能需要；**本仓库不含其源码**，不用该功能可忽略 |
| VC++ 运行库 | 安装 Microsoft Visual C++ 2015-2022 Redistributable（x64） |
| `vendor/`（语音 ASR） | `npm i silk-wasm sherpa-onnx-node` + SenseVoice 模型；不装则语音不转写（其余功能正常） |
| `node_modules/` | `npm install`（飞书 SDK） |
| `.env` | `cp config.example .env` 后填值；**永不入库** |
| 人名词表 | 放在项目**外**的私有目录（含真实姓名），用 `REPLY_WORDLIST_DIR` 指向 |

## 快速开始

```bash
git clone <本仓库> && cd Export_from_WeChat_v1
cp config.example .env         # 填最少 6 项：REPLY_DATA_ROOT、REPLY_WORDLIST_DIR、两套模型、企微 webhook
npm install                    # 飞书 SDK

# 1) 放好导出器：优先从 Releases 下载（文件就是 reply-exporter.exe）
#    放到 bin/ 即可；下载页：../../releases/latest
#    没有 Release 或想自己编（需要 Rust + MSVC，首次会联网拉依赖）：
#    cd native && source .tools/env.sh && cargo build --release -p reply-exporter
#    cp target/release/reply-exporter.exe ../bin/ && cd ..

# 2) 自检（零花费：不调用任何云端接口）
python pipeline.py --diagnose      # 或双击 diagnose.cmd

# 3) 启动飞书入口
start_feishu_entry.cmd             # 菜单 [1] 只读本机媒体（日常）
```

`python pipeline.py --name "张三" --n 30 --preview` 可以**零出境**预览：只导出 + 脱敏 + 拼 prompt + 跑自查。

## 飞书 / 企业微信怎么绑

见 [`使用说明书.md`](使用说明书.md) 第 2 章（含后台权限、长连接、`open_id` 白名单、以及三个实测踩过的坑）。
命令全集见第 5 章；想先看会花多少钱，看第 10 章。

## 目录结构

```
start_feishu_entry.cmd   启动入口（菜单：只读 / 一次性补图）
diagnose.cmd             一键自查
feishu_ws.cjs            飞书长连接入口（Node）
feishu_bridge.py         单条命令的 Python 桥
feishu_adapter.py        命令解析 / 状态文案 / 多轮状态
pipeline.py              编排：导出 → 脱敏 → 看图 → 模型 → 出境自查 → 推送
desensitize.py           脱敏（词表 + 正则 + 显示名登记）
profiles.py              联系人档案（15 项模板）
relationship.py          关系分析（1:1 + ≥200 条 + 无档案时触发）
voice2text.cjs           离线语音转写（silk → wav → SenseVoice）
keyscan_readonly.py      只读内存扫描取密钥
imghelper.py             可选：一次性「补图」
panel.py                 本地面板（127.0.0.1，备用入口）
config/脱敏规则.json      脱敏规则（不含真实人名）
native/                  Rust 工作区：reply-exporter / wcdb-native / weflow-core 等
```

## 隐私与安全

- 密钥取自**运行中的**微信进程内存（只读），存本机 `%APPDATA%\weflow-key-provider\store.json`，
  **不写入仓库、不写日志、不回显**（日志里只有 8 位指纹）。
- 会出境：脱敏后的聊天文本、联系人档案、关系分析输入；**图片字节**需要显式设置
  `REPLY_VLM_ALLOW_IMAGE_EXIT=1` 才发（像素无法脱敏，因此是硬开关）。
- 不会出境：语音原始音频（本机离线转写后只发文字）、密钥、`.env`。
- 运行日志 `logs/feishu_entry.log` 含命令原文 **与联系人名**，不要提交/外发。

## 已知边界

- 群聊在飞书里默认不响应（状态行会带联系人名），需要显式加 `REPLY_FEISHU_ALLOW_GROUP_CHAT_IDS`。
- 微信未落盘的媒体取不到（Mars 私有协议，消息里的地址不是 HTTP URL）。
- 「@群昵称」脱敏依赖人名词表快照，新成员需补表；漏掉时出境自查会拦下并提示。
- 未识别消息类型降级为 `[非文本消息]`。更多边界见 `使用说明书.md` 第 9 章。
- **导出器与微信版本绑定**：Release 里的 exe 只实测过某个微信版本（当前 4.1.15.x）。微信升级后取密钥的内存布局与库结构可能变化，先跑 `bin\reply-exporter.exe --version` 对照；不匹配时请回到 Releases 看是否有新构建。

## 免责声明

本项目仅用于处理**你自己账号**在本机的数据。微信客户端与其数据库格式属腾讯，可能随版本变化；
使用本工具产生的后果（包括但不限于账号风险）由使用者自负。请遵守当地法律与平台条款。

## 许可

[MIT](LICENSE) © 2026 mikoyan
