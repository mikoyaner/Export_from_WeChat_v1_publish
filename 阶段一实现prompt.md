# 阶段一实现 Prompt · 自研只读导出器最小链路

> 版本：v1（2026-10-18）。基于《AI微信回复助手需求文档 V3.5》与《需求文档V3.4-修订建议 v4（决策定稿）》。
> 用途：把本文件整段交给一个实现会话（Qoder / 其它 agent），它据此在本机跑通「飞书命令 → 自研导出器一次性导出 → 脱敏 → 云端 AI 出 A/B/C → 企业微信 webhook 推回手机」的最小闭环。
> 交付目标不是"写完所有代码"，而是**阶段一 7 个门槛全部实测通过**（见 §7）。任何一步卡住，如实报告卡点，不要伪造成功。
> **2026-10-20 起路径变了**：本文里写的 Rust 底座 `common/key-provider/` 已整体移入项目内 `Export_from_WeChat_v1/native/`，运行时二进制在 `Export_from_WeChat_v1/bin/`。构建/运行命令以 `worklog.md §0` 与 `DEPENDENCIES.md` 为准。

---

## 0. 角色与总目标

你在 `<项目目录>` 工作。要为「AI 微信回复助手」实现**阶段一：自研只读导出器最小链路**。

一句话架构：

```
手机 → 飞书命令「回复 <联系人/群名> [范围]」
     → [本机自研 Rust 导出器：一次性导出该会话最近 N 条]
     → 脱敏（人名过词表 + 正则）
     → 云端 AI 生成 A/B/C 回复建议
     → 企业微信 webhook 推回手机
                        ↘ 飞书回一条调试/状态信息
本地面板（备用命令入口，共用同一后端）
```

全链路本机运行，**不开任何公网入站端口**（企微 webhook 是出站 HTTP，飞书走 SDK 长连接出站）。**命令到达才导出一次**——不轮询、不订阅推送、不维护未回复列表、不做群聊 @ 识别。

---

## 1. 红线（不可协商，写代码前先读）

1. **只读**：只读本机 WCDB 数据库文件 + 读活进程内存取密钥；**不注入、不 hook、不改微信、不要 admin**。
2. **只处理本机登录账号自己的会话**，绝不读取/分析他人设备数据。
3. **AI 只出建议，发送动作永远由人**：无任何自动回复 / 自动发送到微信的代码路径。
4. **数据出境先脱敏**：发往云端 AI 的内容必须先过本机脱敏层。
5. **密钥与云端 API key 只存本机**（配置文件 / 环境变量），**绝不写入代码仓库、绝不打印到日志明文、绝不出境**。
6. **IP 合规 / 不抄码**：底座用我们自己的 Rust `common/key-provider`。`pc_wechat_exp`（sunhanaix，无 LICENSE、上游已撤）与 `wechat_exp`（Ray0612，GPL v3）**只借鉴技术思路，禁止复制其代码**（避免许可传染与侵权）。WeFlow_v2（Electron/TS）仅作消息类型映射的参考。
7. **WeFlow 5031 / 解密后的聊天库绝不可裸暴露公网**。

---

## 2. 已有底座（"自研"≠ 从零，先复用再动手）

Rust cargo workspace：`common/key-provider/`（`cargo build --release`，rustup + MSVC 已就绪，见其 README；crates 镜像 rsproxy.cn）。成员：

| crate | 作用 | 阶段一怎么用 |
|---|---|---|
| `shared/weflow-core/` | SQLCipher 打开微信 4.x 库、表结构、dat 解密、hardlink、**密钥仓库** | 直接复用。`db.rs::Account::open(session_db, db_key)` 打开库；`messages.rs::stream_session_message_rows` 流式读会话消息行；`keystore.rs` = **密钥持久化仓库**（从 env `WEFLOW_KEY_STORE` / 同目录 `key-store.json` / `%APPDATA%\weflow-key-provider\store.json` 读，非扫描器） |
| `wcdb-native/` | C ABI `wcdb.dll`，`sessions.rs`/`messages.rs`/`contacts.rs` 读会话·消息·联系人 | 联系人解析（命令→id）与消息读取可复用其逻辑 |
| `welive-export/` | **stdin JSON → stdout NDJSON 的 CLI 导出器**（正是命令触发式导出形态） | 阶段一导出器的骨架。注意：它的 `ExportRequest.account.db_key` 是**把密钥当输入**——取密钥不在这个 crate 里 |
| `docs/protocol/` | 从 WeFlow `wcdbCore.ts` 逐函数提取的协议规格（错误码 -3 NO_MESSAGE_DB 等） | 实现 wcdb 交互时对照 |

**参考实现（只看思路，不抄码）：**
- `pc_wechat_exp-master/src/key_scan.py`（41KB）= 本机**唯一实测通过**的只读取密钥路线：Config.Cipher 对象 + 固定 XOR 掩码扫描活进程内存，**37/37 salts / 4.3s，不注入·不 admin**（要求微信在运行）。Rust 重写时把它当**技术规格**读，理解算法后自己写，不要逐行翻译复制。
- `WeFlow_v2/WeFlow-main/electron/services/httpService.ts`：`mapMessageType`(:2240)、`getMessageContent`(:2389) = localType→可读文本的映射表。自研导出器把原始消息行转可读文本时照此思路实现。
- 脱敏交付件（4 份，随需求文档）：`<你的词表目录>\脱敏规则.json` + `脱敏词表-人名与备注名.txt` / `-组织与项目代号.txt` / `-地名与地址.txt`。

---

## 3. 语言 / 形态分工（已锁定）

- **Rust 导出器**（复用 key-provider）：取密钥（见 §4）→ 打开 WCDB → 按命令解析联系人 → 一次性导出指定会话最近 N 条 → 原始行转可读文本。**输入**：JSON（会话/联系人、范围 N、账号数据目录）。**输出**：结构化 JSON（消息数组：发送者显示名、时间、类型、可读文本）。
- **Python 胶水**（我们自己的实现）：飞书命令接收/解析 → 调 Rust 导出器 → 脱敏 → 拼 prompt 调云端 AI → 企微 webhook 推送 → 飞书回调试信息。

---

## 4. 门槛一（最关键）：只读内存扫描取密钥 · Rust 重写 + 本机重测

**背景**：已验证的只读扫描当前是 Python（`key_scan.py`，37/37）。选了 Rust 底座 + 不抄码 ⇒ 这套算法要在 **Rust 侧按技术规格重写**，且**必须在本机重新实测命中率**（对齐 37/37 salt 命中）才算通过。这是唯一一处"已验证能力需要重新验证"的地方。

要求：
1. 在 workspace 内新建/复用一个 crate（如 `key-provider` 成员或新 `wcdb-keyscan`）实现只读内存扫描：枚举微信进程内存、按 Config.Cipher 布局 + 固定 XOR 掩码定位 salt/key。**只读**（`ReadProcessMemory` 级别，不写、不注入、不申请 admin）。
2. 扫描结果写入 `weflow-core::keystore`（`key-store.json`），供 `Account::open` 使用。密钥**只在内存/本机密钥文件**，不入 git、不打印明文。
3. **验收**：在本机（微信 4.1.15.13，数据目录 `<微信数据根>\<wxid>_<suffix>`，用 `<wxid>` 占位、真实路径不入仓库）运行扫描，打印命中的 salt/key 数量，**对齐 37/37**。达不到就是没通过，报告实际命中数与失败点。

**Plan B（允许，且建议先跑通链路时用）**：若 Rust 重写扫描短期搞不定，**密钥扫描这一步暂留 Python 胶水**（我们自己的实现，参照 key_scan.py 思路重写、不复制其代码），扫描后把密钥写入同一个 `key-store.json`；Rust 只负责拿到密钥后的解密 + 读库 + 导出。这不违反 IP 边界，能先跑通链路。**在报告里明确说明用了 A 还是 B。**

---

## 5. 门槛二～五：导出 → 脱敏 → AI → 回程

**门槛二 · 命令→联系人解析**
- 命令语法：`回复 <联系人名/群名> [范围]`，范围缺省 = 最近 **N=30** 条。
- 读 contact / session 表把显示名解析成会话 id；**同名歧义**时回候选清单（备注名 + id 尾号 + 最近聊天时间），让用户回编号确认。
- 群聊：**直接导出**，不做 @ 识别。

**门槛三 · 一次性导出 + 类型映射**
- 打开 WCDB，导出该会话最近 N 条消息（`stream_session_message_rows`）。
- 原始行转可读文本：参照 WeFlow `mapMessageType`/`getMessageContent` 思路，把 localType 映射为文本（文本直出；图片/语音/表情等非文本给占位描述，如 `[图片]`）。
- 导出前做**健康检查**：微信是否在运行、密钥是否就绪、目标库是否存在（错误码对照 `docs/protocol/`）。

**门槛四 · 脱敏（硬要求）**
> 导出的整块聊天内容直接发云端 AI，其中**发送者显示名（备注名/昵称/群昵称）必须先过人名词表**；未命中默认打码 `[人物?]` 并记脱敏日志，**不得原样出境**。正则类（手机号/身份证/银行卡/车牌/邮箱/验证码等）沿用 `脱敏规则.json`。
- 执行顺序：先词表（消实体，最长匹配优先）→ 再正则（兜号码硬格式）。
- 占位符**本轮上下文内稳定映射**：同一联系人始终映射同一占位符（如 `[人物1]`），让 AI 能区分是否同一人。
- 脱敏日志只记"哪些条目被替换"，**原文不出机**。

**门槛五 · AI 生成 + 企微回程**
- Python 胶水拼 prompt（固定段：关系卡设定〔可选，阶段一可空〕→ 脱敏后最近 N 条 → 当前待回复消息 → 用户意图〔推进/婉拒/维持/敷衍，可缺省〕→ 输出格式约束 A/B/C + 适用说明 + 风险提示 + 后续建议）。
- 云端 API：OpenAI 兼容格式（DeepSeek / 通义千问 / GPT 任选其一先跑通）；**API key 只存本机配置/env，不入仓库**。
- 结果（A/B/C 建议）**推企业微信群机器人 webhook**（出站 HTTP，Markdown 卡片）到手机。
- 飞书侧**回一条调试/状态信息**（收到命令、导出条数、脱敏命中数、AI 调用耗时、成功/失败）——飞书是**正式命令入口 + 调试回显**，AI 最终答案走企微 webhook。

---

## 6. 门槛六：飞书命令入口 + 本地面板备用

- **飞书**（用户已实现飞书侧基建）：接收命令文本 → 解析 → 触发导出链路；回调试信息。若飞书 SDK 长连接接入细节缺失，先在本文件报告里列出"飞书侧需要你确认的对接点"，不要臆造。
- **本地面板**（备用入口，在电脑前时用）：localhost 页面，输入同一命令语法，共用同一后端；**仅监听 localhost**，不开公网。阶段一可先做最简（一个输入框 + 结果展示 + 状态），重点是后端闭环。

---

## 7. 阶段一验收清单（7 个门槛，逐项实测）

| # | 门槛 | 通过判据 |
|---|---|---|
| 1 | 只读内存扫描取密钥 | 本机实测命中对齐 **37/37 salt**；标明走 A(Rust) 还是 B(Python 胶水) |
| 2 | 命令→联系人解析 | `回复 <名>` 能定位会话；同名回候选编号 |
| 3 | 一次性导出 + 类型映射 | 导出指定会话最近 N=30 条为可读文本 JSON |
| 4 | 脱敏 | 人名过词表、未命中打码 `[人物?]`；正则命中号码类；脱敏日志不含原文 |
| 5 | AI + 企微回程 | 云端出 A/B/C；企微 webhook 推到手机；飞书回状态 |
| 6 | 飞书入口 + 面板备用 | 飞书发命令触发闭环；localhost 面板可触发同一后端 |
| 7 | 红线自检 | 全程只读·不注入·不 admin；无自动发送；密钥/云端 key 未入仓库（`git status`/`git log` 自证） |

**前置**：git 已装（`C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Git`）；Rust 工具链已在（见 key-provider README）；Python 环境按 key-provider 之外另行准备（脱敏/胶水用）。

---

## 8. 实现会话的工作方式要求

- **先探明现状再动手**：读 `common/key-provider` 各 crate 的现有 API（`Account::open`、`stream_session_message_rows`、`keystore`、`welive-export` 的 `ExportRequest`/NDJSON 事件），确认可复用边界，不要重复造轮子。
- **每过一个门槛做一次实测并记录**（命令 + 关键输出数字），不写"应该能跑"。
- **占位符化**：文档/示例里的账号目录、wxid、密钥、API key 一律用 `<wxid>` / `<DATA_DIR>` / `<CLOUD_API_KEY>` 占位，真实值不入仓库、不进日志明文。
- **卡住就报告**：命中数对不齐、飞书对接点不明、库结构不符——如实报告卡点与已排除项，不要伪造成功或跳过门槛。
- 参考 `readonly-probe-before-editing-wechat-keys` / `probe-wechat-keys-readonly` 技能做只读探针验证（改动取密钥链路前先跑）。
