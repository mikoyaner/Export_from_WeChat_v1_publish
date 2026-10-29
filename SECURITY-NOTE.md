# 安全提醒：`.env` 里的四组真实凭据需要轮换

> 写于 2026-10-24。原因见下；**轮换完成后请更新本文件勾选状态**。
> 本文不记录任何 key 片段，只写凭据种类与轮换入口。

`.env`（已 gitignore，但以**明文**躺在项目目录里）当前持有四组真实凭据。
它们曾在明文状态下随项目目录存在、被备份/同步盘/编辑器历史接触过，按"曾经落盘即视为
可能泄露"的口径，**建议全部轮换**：

| # | 凭据 | 泄露后果 | 去哪里换 |
|---|---|---|---|
| 1 | DashScope API key（`REPLY_LLM_API_KEY`） | 他人用你的计费账户调模型 | 阿里云控制台 → DashScope → API-KEY 管理：作废旧 key、新建后填回 `.env` |
| 2 | 第三方聚合站 VLM key（`REPLY_VLM_API_KEY`，站点 `apihub.agnes-ai.com`） | 同上（第三方转售商，风险更高） | 该站控制台重置 key；同时建议评估是否把图片出境改回 `REPLY_VLM_ALLOW_IMAGE_EXIT=0` |
| 3 | 企业微信群机器人 webhook（`REPLY_WECOM_WEBHOOK`） | **任何人可冒充机器人向该公司群发任意消息** | 群设置 → 群机器人 → 删除旧的、新建一个，替换 `.env` 里的 webhook |
| 4 | 飞书自建应用 App Secret（`REPLY_FEISHU_APP_SECRET`） | 可完全冒用该应用（收发消息、读通讯录） | 飞书开放平台 → 开发者后台 → 该应用 → 凭证与基础信息 → 重置 App Secret |

轮换后的操作要点：

1. 只改 `.env` 对应的值，程序无其他落点（密钥不写日志、`--diagnose` 只显示长度）。
2. 轮换后跑一次 `python pipeline.py --diagnose` 与一条真实飞书命令，确认新凭据生效。
3. `logs/feishu_entry.log` 记录了完整命令原文（含联系人名），`.env`、`联系人档案/`
   都在 gitignore 里——**拷贝项目到别的机器/介质时不要带上这两样**（见 DEPENDENCIES.md §4）。
   `native/.scratch/scan.txt`（当年扫描出的明文密钥）**已于 2026-10-24 删除**：实测其 37 个密钥
   全部存在于 `store.json`，是纯冗余副本；它唯一的价值只在「微信未运行且 store 丢失」时可离线导入。
4. 若曾把项目目录打进压缩包/网盘同步，轮换上述四组凭据是唯一可靠的止损动作。

相关边界：这些凭据只被 `pipeline.py` / `feishu_bridge.py` / `feishu_ws.cjs` 在进程内
读取，不写日志、不回显（`config_status` 只报长度）。残余风险来自"明文落盘"本身——
有条件的话迁移到系统凭据管理器（Windows 凭据管理器 / DPAPI 加密文件）是更彻底的修法，
需要时单独立项。
