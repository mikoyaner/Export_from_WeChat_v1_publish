# wcdb.dll 协议规格 01 —— 核心 / 会话 / 消息

> **来源与边界**：本文是**接口行为描述**（函数名、字段名、返回形状、错误路径），源自对 WeFlow 项目（Electron/TypeScript）`wcdbCore.ts` 等文件的阅读笔记；**不含其源代码**，也不声称与上游实现完全一致。第三方权利与许可以 WeFlow 原项目为准。


> 协议唯一来源：`WeFlow-main/electron/services/wcdbCore.ts`（5191 行，下文行号均指该文件，除非另注）。
> 上层消费方：`electron/wcdbWorker.ts`（worker 分发）、`electron/services/wcdbService.ts`（主线程代理）、
> `electron/services/chatService.ts`（会话/消息行字段的最终解析者）、`electron/main.ts`（IPC 入口）。
> 本文档覆盖函数组：核心生命周期 + 会话 + 消息读写 + 防撤回触发器。
> 字段名**逐字母**对照 TS 解析代码提取；确实无法从代码确定的以「未确定」标注。

---

## 0. 全局约定（其他函数组文档引用本节）

### 0.1 加载与符号绑定

- WeFlow 用 `koffi.load(libPath)` 加载动态库（wcdbCore.ts:865），随后用
  `lib.func('<C 原型字符串>')` 逐个绑定导出符号（wcdbCore.ts:870–1371）。
- 绑定分两类：
  - **必需符号**：直接 `this.lib.func(...)`，不在 try/catch 内。**任何一个缺失都会让整个
    `initialize()` 抛异常失败**（错误码 -2302），全部功能不可用。本函数组的必需符号：
    `wcdb_init`、`wcdb_shutdown`、`wcdb_open_account`、`wcdb_close_account`（绑定必需但
    从未被调用，见 3.2）、`wcdb_free_string`、`wcdb_get_sessions`、`wcdb_get_messages`、
    `wcdb_get_message_count`、`wcdb_get_message_by_svrid`、`wcdb_get_message_by_id`。
    （其他组的必需符号见各自文档；完整清单可从 wcdbCore.ts:870–1350 中不在 try/catch 内的绑定推出。）
  - **可选符号**：包在 `try { ... } catch { fn = null }` 中（如 wcdbCore.ts:875–880 的
    `wcdb_purge_memory`）。缺失时对应 TS 方法返回
    `{ success: false, error: '当前数据服务版本不支持…' / '接口未就绪' }`，或退化到备用实现
    （例如 `getSessionMessageCounts` 缺失时逐个调 `wcdb_get_message_count`，wcdbCore.ts:2263），
    **不影响其余功能**。
- 加载前 WeFlow 会尝试预加载同目录的 `WCDB.dll`、`SDL2.dll`（Windows）/`libWCDB.dylib`（macOS），
  失败不致命（wcdbCore.ts:827–862）。Rust 实现若为单文件 dll 则无需理会。
- 所有调用发生在 Node **worker_threads** 内（`electron/wcdbWorker.ts`），worker 内用 promise 队列
  串行化请求；同步阻塞调用是允许的（TS 侧自行用 `setImmediate` 让出事件循环）。

### 0.2 状态码约定

- 所有 `int32` 返回值均为状态码，**0 = 成功**。
- 特例：`wcdb_install_message_anti_revoke_trigger` 返回 **1 = 已安装（也算成功）**（wcdbCore.ts:4874）。
- 非 0 时 TS 的一般处理：包装成 `{ success: false, error: '<中文提示>: <code>' }`；
  初始化/打开阶段的错误码会经 `formatInitProtectionError()`（wcdbCore.ts:412–446）映射为用户可读文案。
  DLL 可能返回并被映射的错误码：

| 错误码 | 含义（TS 侧文案） |
|---|---|
| -1 | 未配置 WCDB 实现（TS 侧生成，DLL 不必返回） |
| -3 | 未找到 session.db 文件 |
| -4 | 已找到 session.db，但数据库无法打开（密钥/权限/损坏） |
| -5 | 数据库已打开，但无法读取 schema |
| -6 | schema 可读，但没有兼容的会话表 |
| -7 | 会话表缺排序时间字段；**在消息接口中 = message schema mismatch**（wcdbCore.ts:2189、2246、4054） |
| -8 | 会话表缺最后消息时间字段 |
| -9 | 排序时间字段与最后消息时间字段配置冲突 |
| -10 | 读取会话记录数量失败 |
| -11 | 回写会话排序时间失败，事务已回滚 |
| -101 ~ -105 | 环境/防调试/防回拨/完整性校验类错误 |
| -1000 ~ -1010 | 电脑环境异常类错误 |
| -2301/-2302/-2303、-3001~-3004 | TS 侧自行生成（DLL 不必返回） |

- `wcdb_open_message_cursor` 场景下 **-3 = WCDB_STATUS_NO_MESSAGE_DB**（消息库缓存为空，
  TS 会 forceReopen 后重试一次，wcdbCore.ts:4023–4043）——该约定属于游标组，但错误码语义全局一致。

### 0.3 out 参数与内存约定

- **指针出参** `_Out_ void** outJson / outError / outUrl / outHex / outName`：
  - DLL 侧分配一段 **NUL 结尾的 UTF-8 C 字符串**，把指针写入 `*outJson`。
  - TS 用 `koffi.decode(ptr, 'char', -1)` 读取（wcdbCore.ts:1550），随后**必须**由
    `wcdb_free_string(ptr)` 释放（统一封装在 `decodeJsonPtr()`，wcdbCore.ts:1547–1557：
    decode → free → 返回字符串；异常时也 free 并返回 null）。
  - **失败路径同样遵守**：rc != 0 时若写了 outError，TS 也会 decode + free
    （如 wcdbCore.ts:2045–2048、4870–4873）。
  - **rc == 0 时不允许留空指针**：TS 普遍用 `result !== 0 || !outPtr[0]` 判失败
    （wcdbCore.ts:2017、2138、2447 等）。即使结果集为空，也必须返回指向 `"[]"` 或 `"{}"` 的有效指针。
- **标量出参** `_Out_ int64* handle / _Out_ int32* outCount / outInstalled / outHasMore / _Out_ int64* outCursor`：
  TS 传一个单元素 JS 数组（如 `const outCount = [0]`），koffi 原地写回；`int64` 出参在 JS 侧为 number。
- `outError` 的内容在**本函数组内一律按纯文本**处理（TS 只 decode 成字符串直接当 error 消息用，
  不做 JSON.parse）；`outJson` 一律是合法 JSON 文本。唯一混合场景是 `wcdb_insert_text_message`
  失败时 outJson 的原文被直接当错误消息展示（见 3.14）。

### 0.4 handle 生命周期

1. `initialize()`：`wcdb_init()`（一次），成功后 `initialized = true`。
2. `open(accountDir, hexKey)`：定位 session.db → `wcdb_open_account(path, key, &handle)` →
   handle 必须 > 0 → `wcdb_set_my_account_id(handle, accountId)`（可选符号，异常静默）。
3. 运行期：所有查询/写入调用都携带该 handle。**TS 同一时刻只维护一个活动 handle**。
4. `close()`：**不调用 `wcdb_close_account`**（注释明言其"可能导致崩溃"，wcdbCore.ts:1936），
   而是直接 `wcdb_shutdown()` 释放全部资源，然后清空 handle 并置 `initialized = false`。
5. 再次 `open()` 会重新走 `wcdb_init()` + `wcdb_open_account()`。`testConnection()` 与
   `forceReopen()`（wcdbCore.ts:3976–3990）同样以 shutdown→init→open 循环运作。
   **因此 DLL 必须支持 init/shutdown 在同进程内多次成对重入**，shutdown 需释放所有
   handle、游标、监控管道与后台线程资源。
6. 周期性维护：open 成功后每 60s 调一次 `wcdb_purge_memory()`（若存在，wcdbCore.ts:1942–1953），
   返回值被忽略。

### 0.5 JSON 文本编码与大整数

- 所有 JSON 出参为 **UTF-8 编码、NUL 结尾**的字符串。
- `parseMessageJson()`（wcdbCore.ts:1559–1572）在 JSON.parse 之前，对**原始文本**做正则替换：
  `"server_id": <16 位及以上十进制整数>` → 加引号变字符串。因此：
  - `server_id` 可以输出 JSON number（即使 > 2^53 也不丢精度，TS 会转成字符串）；
    输出字符串同样被接受（消费方 `normalizeUnsignedIntegerToken(row.server_id)` 两种都处理）。
  - **该保护只针对字段名 `server_id`**。其他 int64 字段若以 number 输出且 > 2^53 会丢精度；
    实际用到的其他数值（如 `local_type` 最大 8589934592049、时间戳秒值）都在安全整数范围内。
- 数值字段输出 number 或数字字符串均可：消费方大量使用 `parseInt(row.x || '0', 10)` /
  `Number(...)`（如 chatService.ts:1295–1313、1349）。**推荐统一输出 JSON number**，
  但字段名必须与本文档一致。

### 0.6 会话 ID / 用户名语义

- `username`、`sessionId` 参数同义：单聊为 wxid（如 `wxid_xxx`），群聊为 `xxx@chatroom`，
  公众号为 `gh_xxx`，另有企业号 openim 等。TS 原样透传，不做转义（DLL 侧自行参数化查询）。
- `wcdb_set_my_account_id` 传入的 accountId 是**账号目录名清洗后**的结果
  （`cleanAccountDirName`，wcdbCore.ts:15–26：`accountId_wxid_xxx_1234` → `accountId_wxid_xxx`；
  `wxid_xxx_1234`（4 位字母数字后缀）→ `wxid_xxx`）。DLL 用它判定消息 is_send / 本人身份。

---

## 1. `wcdb_init`

- **签名**：`int32 wcdb_init()`（必需，绑定于 wcdbCore.ts:870）
- **调用点**：wcdbCore.ts:1375（`initialize()` 内，绑定完所有符号后调用一次）。
- **入参**：无。
- **状态码与错误**：`!== 0` → `lastDllInitError = formatInitProtectionError(code)`，
  `initialize()` 返回 false，整个数据服务不可用（wcdbCore.ts:1376–1380）。可能返回
  -101~-105、-1000~-1010 等环境/防护错误码。
- **返回 JSON**：无。
- **备注**：全局引擎初始化（SQLite/SQLCipher、日志缓冲等）。必须可与 `wcdb_shutdown`
  成对多次调用（见 0.4）。TS 在 init 失败后不会调用 shutdown。

## 2. `wcdb_shutdown`

- **签名**：`int32 wcdb_shutdown()`（必需，绑定于 wcdbCore.ts:873）
- **调用点**：wcdbCore.ts:1454（`testConnection` 清理测试句柄）、1970（`close()`）、
  3982（`forceReopen()`）。
- **入参**：无。
- **状态码与错误**：**返回值被完全忽略**；抛异常才会被 TS 捕获打日志。
- **返回 JSON**：无。
- **备注**：语义 = 释放**所有**账号 handle、游标、监控管道与内部资源，回到未初始化状态；
  之后 TS 会重新 `wcdb_init()`。这是 WeFlow 实际使用的唯一"断开"手段（不用 close_account）。
  shutdown 后 TS 侧 `initialized=false`、`handle=null`，所有 `ensureReady()` 守卫会拒绝后续查询。

## 3. `wcdb_purge_memory`

- **签名**：`int32 wcdb_purge_memory()`（**可选**，绑定于 wcdbCore.ts:875–880，缺失置 null）
- **调用点**：wcdbCore.ts:1947（`startPeriodicPurge()`：open 成功后每 60 秒一次；
  close/shutdown 时停止定时器）。
- **入参**：无。
- **状态码与错误**：返回值被忽略，异常被吞（"purge 失败不影响功能"）。
- **返回 JSON**：无。
- **备注**：语义 = 回收空闲数据库连接与页缓存（SQLite `release_memory`/关闭闲置句柄），
  **不得影响**活动 handle 和正在使用的游标。缺失时 TS 不启动定时器，无降级路径。

## 4. `wcdb_open_account`

- **签名**：`int32 wcdb_open_account(const char* path, const char* key, _Out_ int64* handle)`
  （必需，绑定于 wcdbCore.ts:884）
- **调用点**：wcdbCore.ts:1438（`testConnection`）、1882（`open`）。
- **入参**：
  - `path`：**session.db 的完整文件路径**。TS 在 `<accountDir>/db_storage` 下递归查找
    （深度 ≤ 5）名为 `session.db` 的文件（大小写不敏感，`findSessionDb`，wcdbCore.ts:577–606），
    找不到则根本不会调用本函数。DLL 应从 session.db 出发自行发现同目录树下的
    message/contact/media/emoticon/SNS 等分库。
  - `key`：**十六进制字符串形式的 SQLCipher 密钥**（由用户配置的外部 key provider 产生，
    TS 原样透传，不校验长度/格式；典型为 64 个 hex 字符）。
  - `handle` 出参：TS 预置 `[0]`；成功时 DLL 写入 **> 0** 的 int64 句柄。
- **状态码与错误**：
  - `rc !== 0` → TS 打印 DLL 日志（`wcdb_get_logs`）并把 rc 交给 `formatInitProtectionError`
    映射（-3/-4/-5/-6/-7/-8/-9/-10/-11、-101 系列等，见 0.2）。
  - `rc === 0` 但 `handle <= 0` → TS 报 -3003「数据库句柄无效」。
- **返回 JSON**：无（纯句柄出参）。
- **备注**：
  - 成功后 TS 立即（若符号存在）调 `wcdb_set_my_account_id(handle, accountId)`。
  - TS 单活动连接：参数相同的重复 open 直接短路返回；参数不同则先 shutdown 再 init+open。
  - handle 值仅要求 > 0 且能在后续调用中路由到正确的账号上下文；testConnection 得到的
    临时 handle 不经 close_account，直接由 shutdown 回收。

## 5. `wcdb_close_account`

- **签名**：`int32 wcdb_close_account(int64 handle)`（**必需绑定**，wcdbCore.ts:888 —— 不在
  try/catch 内，符号缺失会导致整个 initialize 失败）
- **调用点**：**全文件无任何调用**。wcdbCore.ts:1934–1937 注释：
  「wcdb_close_account 可能导致崩溃，使用 shutdown 代替」。
- **入参**：`handle` = `wcdb_open_account` 返回的句柄。
- **状态码与错误 / 返回 JSON**：不会被 TS 消费。
- **备注**：实现为"关闭单个 handle 并释放其资源"即可，但**必须导出该符号**；
  健壮性要求高（WeFlow 因历史崩溃已弃用之，其他宿主可能调用）。

## 6. `wcdb_free_string`

- **签名**：`void wcdb_free_string(void* ptr)`（必需，绑定于 wcdbCore.ts:919）
- **调用点**（均紧随一次 out 指针 decode 之后）：wcdbCore.ts:256（monitor pipe name）、
  1493/1529（logs）、1551/1554（`decodeJsonPtr` 统一出口，**所有 outJson/outError 都经此释放**）、
  2047（mark_all_sessions_read outError）、2092（reorder outJson）、
  4872/4897（anti-revoke install/uninstall outError）、4988/5015/5053（SNS 组 outError）。
- **入参**：`ptr` = 此前通过任意 `void**` 出参返回的指针。TS 只在指针非空时调用，
  但实现仍建议容忍 NULL。
- **返回**：无。
- **备注**：释放必须与分配器配对（Rust 侧建议 `CString::into_raw` / 自定义 alloc 后在此
  `from_raw` 回收）。这是协议中唯一的内存归还通道——**每一个** out 字符串指针最终都会且只会
  被交给本函数一次。

## 7. `wcdb_set_my_account_id`

- **签名**：`int32 wcdb_set_my_account_id(int64 handle, const char* accountId)`
  （**可选**，绑定于 wcdbCore.ts:891–895）
- **调用点**：wcdbCore.ts:1913（`open()` 成功保存 handle 之后立即调用；异常被静默吞掉，
  **返回值被忽略**）。
- **入参**：
  - `handle`：刚打开的句柄。
  - `accountId`：当前登录账号 ID（清洗后的账号目录名，见 0.6；如 `wxid_xxx`）。
- **状态码与错误 / 返回 JSON**：TS 不消费。
- **备注**：DLL 应把它用于：消息行 `computed_is_send`/`sender_username` 判定、
  群成员"我"的识别、`wcdb_insert_text_message` 的发送者写入、SNS 导出统计等。
  符号缺失时相关推断功能退化（TS 侧会用 `sender_username` 与配置的 myAccountId 兜底比较）。

## 8. `wcdb_get_sessions`

- **签名**：`int32 wcdb_get_sessions(int64 handle, _Out_ void** outJson)`
  （必需，绑定于 wcdbCore.ts:922）
- **调用点**：wcdbCore.ts:2012（`getSessions()`）；其结果还被
  `getMediaStream` 的会话名映射复用（wcdbCore.ts:2829–2855）。
- **入参**：`handle`。
- **状态码与错误**：`rc !== 0 || outJson 为空` → `{ success:false, error:'获取会话失败: <rc>' }`；
  decode 失败 → '解析会话失败'。JSON.parse 异常按 exception 处理。
- **返回 JSON 结构**：顶层是**会话行对象数组**，`JSON.parse` 后原样透传给 chatService。
  消费方按下列**候选字段名顺序**取值（第一个非空即生效；推荐实现输出加粗的首选名）：

```jsonc
[
  {
    "username": "wxid_xxx | xxx@chatroom",   // 必需。兼容名: user_name, userName, usrName,
                                             // UsrName, talker, talker_id, talkerId
                                             // (chatService.ts:1277-1286, 1530-1542;
                                             //  wcdbCore.ts:2836-2844 不含 talker_id/talkerId)
    "type": 0,                     // 会话类型, parseInt(row.type || '0') (chatService.ts:1348)
    "local_type": 1,               // 联系人 localType, 用于会话过滤/分类
                                   // 兼容: localType, WCDB_CT_local_type
                                   // (chatService.ts:8620-8623 getSessionLocalType)
    "unread_count": 0,             // 未读数。兼容: unreadCount, unreadcount (chatService.ts:1349)
    "sort_timestamp": 1712345678,  // 排序时间戳(Unix 秒)。兼容: sortTimestamp, sort_time, sortTime
                                   // (chatService.ts:1295-1302; wcdbCore.ts:2846-2851 只认
                                   //  sort_timestamp/sortTimestamp/last_timestamp/lastTimestamp)
    "last_timestamp": 1712345678,  // 最后消息时间(Unix 秒)。兼容: lastTimestamp, last_msg_time,
                                   // lastMsgTime; 缺失时回退 sort_timestamp (chatService.ts:1303-1310)
    "summary": "最后一条消息摘要",   // 兼容: digest, last_msg, lastMsg (chatService.ts:1312)
    "last_msg_type": 1,            // 最后消息 localType, 摘要为空时转类型标签
                                   // 兼容: lastMsgType (chatService.ts:1313)
    "message_count": 123,          // 可选的消息数提示。兼容: messageCount, msg_count, msgCount,
                                   // total_count, totalCount, n_msg, nMsg, message_num, messageNum
                                   // (chatService.ts:1314-1324; 缺失时走缓存/统计接口)
    "last_msg_sender": "wxid_yyy",       // 可选 (chatService.ts:1357)
    "last_sender_display_name": "昵称",  // 可选 (chatService.ts:1358)
    "display_name": "备注/昵称"           // 可选; wcdbCore.ts:2845 按 displayName, display_name,
                                          // remark 顺序取用(媒体流会话名)
  }
]
```

- **备注**：
  - 空会话列表也必须返回 `"[]"` + rc=0（outJson 为空指针会被判失败）。
  - `username` 为空的行会被消费方丢弃；TS 侧还会按 local_type/username 规则过滤
    （`shouldKeepSession`），并按 sort_timestamp 降序重排（chatService.ts:1389）。
  - 数值字段用 number 或数字字符串都能被 parseInt 接受（见 0.5）。

## 9. `wcdb_mark_all_sessions_read`

- **签名**：`int32 wcdb_mark_all_sessions_read(int64 handle, _Out_ void** outError)`
  （**可选**，绑定于 wcdbCore.ts:925–929）
- **调用点**：wcdbCore.ts:2043（`markAllSessionsRead()`）。
- **入参**：`handle`。
- **状态码与错误**：outError **无论成败**只要非空就被 decode 成字符串并 free
  （wcdbCore.ts:2044–2048）；`rc !== 0` → `{ success:false, error: <outError 文本> || '一键已读失败: <rc>' }`。
  outError 是**纯文本**错误描述，不是 JSON。
- **返回 JSON**：无（outError 仅错误文本）。
- **备注**：语义 = 把 session 表所有会话未读数清零（回写数据库）。成功后 TS 清空媒体流
  会话缓存与分页缓存（wcdbCore.ts:2056–2057），前端随后重新拉取会话列表。
  符号缺失 → '当前数据服务版本不支持一键已读'。

## 10. `wcdb_reorder_sessions_by_time`

- **签名**：`int32 wcdb_reorder_sessions_by_time(int64 handle, _Out_ void** outJson)`
  （**可选**，绑定于 wcdbCore.ts:932–936）
- **调用点**：wcdbCore.ts:2088（`reorderSessionsByTime()`）。
- **入参**：`handle`。
- **状态码与错误**：outJson 无论成败都会被 decode + free。
  - `rc !== 0`：尝试把 outJson 当 JSON 解析，读 `payload.error`（string）、
    `payload.code`（number）、`payload.stage`（string）；解析失败则把**原始文本**当 error；
    再兜底 `会话按时间回写失败: <rc>`（wcdbCore.ts:2101–2106）。
  - `rc === 0` 但 `payload.success !== true` → 失败，error = `payload.error`（wcdbCore.ts:2108–2110）。
- **返回 JSON 结构**（成功，字段为**精确 snake_case**，wcdbCore.ts:2114–2122）：

```jsonc
{
  "success": true,               // 必须为字面量 true, 否则视为失败
  "table": "Session",            // 被回写的会话表名
  "sort_column": "sortTimeStamp",// 排序时间列名
  "source_column": "createTime", // 最后消息时间来源列名
  "row_count": 123,              // 会话总行数
  "updated_rows": 45,            // 实际回写行数
  "already_normalized": false    // true = 无需回写(已归一化)
}
```

  失败时可选结构：`{ "error": "...", "code": <int>, "stage": "..." }`。
- **备注**：语义 = 用最后消息时间回写会话排序时间字段，使会话列表按真实时间排序。
  成功后 TS 清空媒体流缓存。符号缺失 → '当前数据服务版本不支持会话按时间回写'。
  上层（main.ts:3604 → chatService.reorderSessionsByTime, chatService.ts:1523）原样透传结果对象。

## 11. `wcdb_get_messages`

- **签名**：`int32 wcdb_get_messages(int64 handle, const char* username, int32 limit, int32 offset, _Out_ void** outJson)`
  （必需，绑定于 wcdbCore.ts:939）
- **调用点**：wcdbCore.ts:2137（`getMessages(sessionId, limit, offset)`）。
  上层链路：chatService.getMessagesByOffsetStable（chatService.ts:3211–3259，
  `probeLimit = min(500, pageLimit+1)`）、getLatestMessages（offset=0）。
- **入参**：
  - `username`：会话 ID（见 0.6）。
  - `limit`：最多返回行数。TS 依赖「返回行数 > 请求页大小 ⇒ hasMore」的探测法，
    所以**必须严格截断到 limit 行**。
  - `offset`：跳过的行数（从 0 开始）。offset=0 必须返回**最新**的消息 →
    **排序为时间降序（新→旧）**；TS 之后会按 sortSeq/createTime/localId 自行重排为升序展示
    （normalizeMessageOrder，chatService.ts:3624–3643），但分页语义要求降序稳定。
- **状态码与错误**：`rc !== 0 || outJson 空` → `获取消息失败: <rc>`；decode 失败 → '解析消息失败'。
- **返回 JSON 结构**：顶层为**消息行对象数组**，经 `parseMessageJson`（server_id 大整数保护，
  见 0.5）后透传。消费方 chatService.mapRowsToMessages（chatService.ts:6424+）、
  mapRowsToMessagesLite（6315–6368）读取的字段（**主字段名为 snake_case**）：

```jsonc
[
  {
    "local_id": 123,               // 消息表本地自增 ID (chatService.ts:6324/6642)
    "server_id": 7123456789012345678, // int64 服务器 ID; number(≥16位会被TS转字符串)或
                                   // 十进制字符串均可 (chatService.ts:6325-6326)
    "local_type": 1,               // 消息类型; 注意可为超 int32 的复合值如 8589934592049(转账)、
                                   // 8594229559345(红包)、34359738417(文件变体)
                                   // (chatService.ts:5518-5531, 6320); 兼容 localType, WCDB_CT_local_type
    "status": 2,                   // 消息状态, 可为 null (chatService.ts:6321 getRowNullableInt)
    "create_time": 1712345678,     // Unix 秒。兼容: createTime, msg_time, msgTime, time
                                   // (chatService.ts:6322/6442)
    "sort_seq": 1712345678000,     // 排序序列(毫秒级); 缺失时 TS 回退 create_time*1000
                                   // (chatService.ts:6323/6645)
    "message_content": "...",      // 消息内容: 纯文本或 XML; 也可能是 hex/base64 编码的
                                   // zstd 压缩数据 —— TS 侧 decodeMessageContent 会尝试
                                   // 解压(hex/base64 → zstd magic 0xFD2FB528 → 解压为 UTF-8)
                                   // (chatService.ts:6327/8500; wcdbCore.ts:2558-2588)
    "compress_content": "...",     // 压缩内容列(同上编码规则); 解码时优先于 message_content
                                   // (chatService.ts:6327, 8500-8501)
    "is_send": 0,                  // 0=接收 1=发送, 可为 null
    "computed_is_send": 0,         // DLL 预计算的 is_send; **优先于 is_send** 被读取
                                   // (chatService.ts:6329/6388/6436: row.computed_is_send ?? row.is_send)
    "sender_username": "wxid_yyy", // 真实发送者(群聊必需; 单聊可空)。为空时 TS 尝试从
                                   // content XML 提取 (chatService.ts:6336/6438)
    "real_sender_id": "wxid_yyy",  // 备用发送者字段 (chatService.ts:14083)
    "db_path": "…/message_0.db",   // 消息所在 db 文件路径; 也接受 _db_path
    "db_name": "message_0",        // 可选; 缺失时 TS 从 db_path 取 basename
    "table_name": "MSG_x"          // 消息所在表名 (chatService.ts:3650-3660 getMessageSourceInfo;
                                   // 三者参与 messageKey 构造与删除定位)
  }
]
```

  媒体类消息可选扩展字段（getMediaStream / 类型 3、43 解析时按候选名读取，
  wcdbCore.ts:2693–2755、chatService 图片/视频解密链路）：
  `image_md5`/`imageMd5`、`video_md5`/`videoMd5`/`raw_md5`/`rawMd5`、
  `image_path`/`imagePath`/`image_dat_name`/`imageDatName`/`img_path`/`img_name` 等、
  `packed_info_data`/`packed_info_blob`/`packed_info`/`BytesExtra`/`bytes_extra`/`reserved0`
  （hex/base64/字节数组均可，TS 会归一化）。这些字段主要服务于 `wcdb_scan_media_stream`
  的行结构；`wcdb_get_messages` 的行若不含它们，TS 会从 content XML 提取 md5。
- **备注**：空结果必须返回 `"[]"`（outJson 空指针 = 失败）。

## 12. `wcdb_get_messages_by_type`

- **签名**：`int32 wcdb_get_messages_by_type(int64 handle, const char* sessionId, int64 localType, int32 ascending, int32 limit, int32 offset, _Out_ void** outJson)`
  （**可选**，绑定于 wcdbCore.ts:1176–1180）
- **调用点**：wcdbCore.ts:2438（`getMessagesByType(sessionId, localType, ascending=false, limit=0, offset=0)`）。
  上层：chatService.getAllVoiceMessages（localType=34，chatService.ts:11310）、
  getAllImageMessages（localType=3，chatService.ts:11353）、
  按类型分页拉取（chatService.ts:11476）。
- **入参**：
  - `sessionId`：会话 ID。
  - `localType`：**int64**，TS 以 `BigInt(localType)` 传递（wcdbCore.ts:2441）——
    必须支持 49 以上的复合类型值（如 8589934592049）。匹配语义由 DLL 决定
    （调用方传 34/3 等基础类型期望精确匹配该类消息）。
  - `ascending`：1=时间升序，0=降序（TS 默认传 0）。
  - `limit`：`max(0, floor(limit))`；**调用点传 0 表示不限量（返回全部匹配行）**。
  - `offset`：`max(0, floor(offset))`，跳过行数。
- **状态码与错误**：`rc !== 0 || outJson 空` → `按类型读取消息失败: <rc>`；
  解析结果非数组时 TS 置为空数组（wcdbCore.ts:2451）。
- **返回 JSON 结构**：消息行对象**数组**，行结构与 `wcdb_get_messages` 完全一致
  （结果直接进 `mapRowsToMessages`，chatService.ts:11315/11358）。
- **备注**：空结果返回 `"[]"`。符号缺失 → '接口未就绪'。

## 13. `wcdb_get_message_count`

- **签名**：`int32 wcdb_get_message_count(int64 handle, const char* username, _Out_ int32* outCount)`
  （必需，绑定于 wcdbCore.ts:942）
- **调用点**：wcdbCore.ts:2187（`getMessageCount`）、2245（`getMessageCounts` 批量循环，
  每 160 个会话让出一次事件循环；也是 `wcdb_get_session_message_counts` 缺失时的降级路径）。
- **入参**：`username` = 会话 ID。
- **状态码与错误**：
  - `rc === -7` → 'message schema mismatch：当前账号消息表结构与程序要求不一致'
    （wcdbCore.ts:2189–2191、2246–2248）——**必须用 -7 表达消息表结构不兼容**。
  - 其他非 0 → `获取消息总数失败: <rc>`；批量场景该会话计数记 0。
  - 成功时 TS 对 outCount 做 `max(0, floor())` 钳制。
- **返回 JSON**：无（int32 出参）。注意计数超 int32 上限的行为未定义（现实数据不会超）。
- **备注**：应为该会话所有消息分表的总行数（含被防撤回触发器保留的行）。

## 14. `wcdb_get_message_by_id`

- **签名**：`int32 wcdb_get_message_by_id(int64 handle, const char* sessionId, int32 localId, _Out_ void** outJson)`
  （**必需**，绑定于 wcdbCore.ts:1126，不在 try/catch 内）
- **调用点**：wcdbCore.ts:4280（`getMessageById(sessionId, localId)`）。
  上层：chatService.ts:10900/12715/14235、httpService.ts:919、export/core/ExportContext.ts:5007。
- **入参**：`sessionId`；`localId` = 消息的 `local_id`（**int32**）。
- **状态码与错误**：`rc !== 0 || outJson 空` → `查询消息失败: <rc>`；
  rc=0 且返回**空对象 `{}`** → TS 判为 `{ success:false, error:'未找到消息' }`（wcdbCore.ts:4286）。
  即"不存在"既可以用非 0 状态码表达，也可以用 `{}` 表达，两者 TS 都能处理。
- **返回 JSON 结构**：**单个消息行对象**（非数组），经 parseMessageJson；
  字段与 `wcdb_get_messages` 的行结构一致（消费方直接走同一套 mapRowsToMessages /
  媒体解析逻辑）。
- **备注**：用于引用消息解析、导出补详、HTTP API 单条查询。

## 15. `wcdb_get_message_by_svrid`

- **签名**：`int32 wcdb_get_message_by_svrid(int64 handle, const char* sessionId, const char* svrid, _Out_ void** outJson)`
  （必需，绑定于 wcdbCore.ts:945）
- **调用点**：wcdbCore.ts:2206（`getMessageByServerId(sessionId, svrid)`）。
  上层：chatService.resolveQuotedMessages（chatService.ts:7418，按 8 个一批并发查询引用消息）、
  ExportContext.ts:2372。
- **入参**：
  - `sessionId`：会话 ID。
  - `svrid`：**十进制字符串形式的 int64 服务器消息 ID**（来自引用消息 XML 的 svrid 属性；
    TS 侧全程 string，不转 number）。
- **状态码与错误**：
  - `rc !== 0` → `查询消息失败: <rc>`。
  - `rc === 0` 且（outJson 空 / decode 为空 / 解析结果为空对象 `{}`）→
    **`{ success:true, row:null }`**（wcdbCore.ts:2210–2217）：查不到不算错误。
- **返回 JSON 结构**：**单个消息行对象**，字段同 `wcdb_get_messages` 行；
  引用解析实际读取 `local_type`、`message_content`、`compress_content`（chatService.ts:7435–7439）。
- **备注**：svrid=0 或空串的行应返回 `{}`/null 而非报错。

## 16. `wcdb_update_message`

- **签名**：`int32 wcdb_update_message(int64 handle, const char* sessionId, int64 localId, int32 createTime, const char* newContent, _Out_ void** outError)`
  （**可选**，绑定于 wcdbCore.ts:898–902）
- **调用点**：wcdbCore.ts:5096（`updateMessage(sessionId, localId, createTime, newContent)`）。
  上层：main.ts:3662（IPC `chat:updateMessage`）→ chatService.updateMessage（chatService.ts:1040）。
- **入参**：
  - `sessionId`：会话 ID。
  - `localId`：**int64**（TS number 直传）——目标消息 local_id。
  - `createTime`：int32，消息 create_time（Unix 秒），用于定位分库/分表。
  - `newContent`：替换后的**纯文本**内容（用户在 UI 编辑的消息文本）。
- **状态码与错误**：`rc !== 0` → error = outError 解码文本（decodeJsonPtr：decode+free），
  空则 'Unknown Error (Decode Failed)'/'Unknown Error'（wcdbCore.ts:5098–5104）。
  outError 为**纯文本**错误描述。`rc === 0` → `{ success:true }`，无 JSON 出参。
- **返回 JSON**：无。
- **备注**：写入 message_content（并同步压缩列/摘要等由 DLL 决定）。成功后 TS 使
  该会话所有消息缓存失效（invalidateSuccessfulMessageMutation）。符号缺失 →
  'WCDB Not Initialized or Method Missing'。

## 17. `wcdb_insert_text_message`

- **签名**：`int32 wcdb_insert_text_message(int64 handle, const char* sessionId, const char* content, int64 status, int64 createTime, _Out_ void** outJson)`
  （**可选**，绑定于 wcdbCore.ts:904–909）
- **调用点**：wcdbCore.ts:5130（`insertTextMessage(sessionId, content, status, createTime?)`）。
  上层：main.ts:3667 → chatService.insertTextMessage（chatService.ts:1059–1125，校验
  status 为安全整数、createTime 为正 Unix 秒或 undefined）→ 前端 ChatPage.tsx:5950
  （status 由用户输入，createTime 由本地时间换算为**秒**；未填时间则传 undefined → TS 传 0）。
- **入参**：
  - `sessionId`：目标会话。
  - `content`：纯文本消息内容（非空，TS 已 trim 校验）。
  - `status`：int64，消息状态值（写入 message 表 status 列；由调用方决定，如 2=已发送；
    TS 不解释语义）。
  - `createTime`：int64 Unix 秒；**0 = 由 DLL 取当前时间**。允许早于会话最后一条消息
    （此时不应更新会话摘要，见 session_updated）。
  - 消息由**当前账号发出**（发送者 = wcdb_set_my_account_id 提供的 accountId）。
- **状态码与错误**：
  - `rc !== 0` → `{ success:false, error: <outJson 原始文本> || '新增消息失败（<rc>）' }`
    ——失败时 outJson 的**原文**（可为纯文本或 JSON 串）直接展示给用户（wcdbCore.ts:5139–5141）。
  - `rc === 0` 但 outJson 空 → '新增消息成功，但返回结果为空'（视为失败）。
- **返回 JSON 结构**（rc=0 时必须是该 JSON，字段为**精确 snake_case**，wcdbCore.ts:5144–5156）：

```jsonc
{
  "success": true,          // 必须为字面量 true, 否则 TS 返回 success:false
  "local_id": 123,          // 新消息 local_id (0/缺失 → TS 置 undefined)
  "create_time": 1712345678,// 实际写入的 Unix 秒 (0/缺失 → undefined)
  "sort_seq": 1712345678000,// 实际写入的排序序列 (0/缺失 → undefined)
  "status": 2,              // 实际写入的状态值 (Number(parsed.status))
  "sender_row_id": 45,      // 发送者在会话成员/联系人表中的 rowid (0/缺失 → undefined)
  "session_updated": true,  // 是否同步更新了会话摘要/排序时间;
                            // false = 插入时间早于当前最后一条消息, 未动会话
                            // (UI 文案区分, ChatPage.tsx:5969-5973)
  "db_path": "…/message_0.db", // 写入的 db 文件路径 (string 才接受)
  "table_name": "MSG_x",       // 写入的表名 (string 才接受)
  "error": "..."               // 可选; success=true 时一般为空 (string 才接受)
}
```

- **备注**：TS 侧结果类型 `InsertTextMessageResult`（wcdbCore.ts:37–48）。
  成功后同样触发会话缓存失效。符号缺失 → '当前数据服务版本不支持新增消息'。

## 18. `wcdb_delete_message`

- **签名**：`int32 wcdb_delete_message(int64 handle, const char* sessionId, int64 localId, int32 createTime, const char* dbPathHint, _Out_ void** outError)`
  （**可选**，绑定于 wcdbCore.ts:911–916；注意绑定注释里的旧 3 参原型已过时，
  以 5 参绑定为准）
- **调用点**：wcdbCore.ts:5172（`deleteMessage(sessionId, localId, createTime, dbPathHint?)`）。
  上层：main.ts:3681 → chatService.deleteMessage（chatService.ts:1130–1156）。
- **入参**：
  - `sessionId`：会话 ID。
  - `localId`：int64，目标消息 local_id。
  - `createTime`：int32 Unix 秒；TS 传 `createTime || 0`（0 = 未知，DLL 需自行定位）。
  - `dbPathHint`：**可能为空字符串 `''`**（TS 传 `dbPathHint || ''`，绝不传 NULL）。
    非空时为消息行携带的 `db_path`（见 §11），用于直接定位分库，避免全库扫描。
- **状态码与错误**：`rc !== 0` → error = outError 解码文本（decode+free），空则
  'Unknown Error'；并 console.error 记录（wcdbCore.ts:5174–5181）。outError 为纯文本。
- **返回 JSON**：无。
- **备注**：若目标会话装有防撤回触发器（拦截 DELETE），DLL 需自行决定物理删除如何绕过
  自身触发器（协议未约定，未确定）。成功后 TS 失效会话缓存。

## 19. `wcdb_check_message_anti_revoke_trigger`

- **签名**：`int32 wcdb_check_message_anti_revoke_trigger(int64 handle, const char* sessionId, _Out_ int32* outInstalled)`
  （**可选**，绑定于 wcdbCore.ts:1297–1302）
- **调用点**：wcdbCore.ts:4915（`checkMessageAntiRevokeTrigger`）；批量包装
  `checkMessageAntiRevokeTriggers`（wcdbCore.ts:4925–4940，纯 TS 循环去重逐个调用，无额外协议）。
  上层：chatService.checkAntiRevokeTriggers（chatService.ts:1158–1170）、main.ts:3686。
- **入参**：`sessionId`（TS 已 trim 且拒绝空串）。
- **状态码与错误**：`rc !== 0` → `{ success:false, error:'DLL error <rc>' }`
  （**没有** rc=1 特殊分支，与 install 不同）。
- **返回**：`outInstalled === 1` → installed=true；其他值（含 0）→ false。无 JSON。
- **备注**：语义 = 查询该会话的消息表上是否已安装"防撤回"触发器
  （拦截微信客户端 DELETE 消息行的 trigger）。

## 20. `wcdb_install_message_anti_revoke_trigger`

- **签名**：`int32 wcdb_install_message_anti_revoke_trigger(int64 handle, const char* sessionId, _Out_ void** outError)`
  （**可选**，绑定于 wcdbCore.ts:1283–1288）
- **调用点**：wcdbCore.ts:4868（`installMessageAntiRevokeTrigger`）；批量包装
  `installMessageAntiRevokeTriggers`（wcdbCore.ts:4942–4957）。
  上层：chatService.installAntiRevokeTriggers（chatService.ts:1187，先用
  filterAntiRevokeSessionIds 过滤：仅联系人/群聊且存在消息表，chatService.ts:1583–1699）、
  main.ts:3690。
- **入参**：`sessionId`（trim 后非空）。
- **状态码与错误**（wcdbCore.ts:4869–4880）：
  - `rc === 0` → `{ success:true, alreadyInstalled:false }`（本次新装）。
  - `rc === 1` → `{ success:true, alreadyInstalled:true }`（**1 = 已安装，是成功态**）。
  - 其他 → `{ success:false, error: <outError 文本> || 'DLL error <rc>' }`。
  - outError 无论成败只要非空即 decode + free；纯文本。
- **返回 JSON**：无。
- **备注**：在该会话全部消息分表上创建拦截 DELETE 的触发器，使被撤回消息保留在库中
  （后续通过 status/local_type 或上层逻辑标记撤回态——标记方式属 DLL 内部实现，协议未约定）。

## 21. `wcdb_uninstall_message_anti_revoke_trigger`

- **签名**：`int32 wcdb_uninstall_message_anti_revoke_trigger(int64 handle, const char* sessionId, _Out_ void** outError)`
  （**可选**，绑定于 wcdbCore.ts:1290–1295）
- **调用点**：wcdbCore.ts:4893（`uninstallMessageAntiRevokeTrigger`）；批量包装
  `uninstallMessageAntiRevokeTriggers`（wcdbCore.ts:4959–4974）。
  上层：chatService.uninstallAntiRevokeTriggers（chatService.ts:1206）、main.ts:3694。
- **入参**：`sessionId`（trim 后非空）。
- **状态码与错误**：`rc !== 0` → error = outError 文本 || `DLL error <rc>`；
  `rc === 0` → success。**没有 rc=1 分支**（幂等卸载应返回 0）。
  outError 无论成败非空即 decode + free；纯文本。
- **返回 JSON**：无。
- **备注**：移除 §20 安装的触发器，恢复微信原生撤回删除行为。

---

## 附录 A：本组函数绑定/调用点速查

| 函数 | 绑定行 | 必需? | TS 调用行（方法） |
|---|---|---|---|
| wcdb_init | 870 | 必需 | 1375（initialize） |
| wcdb_shutdown | 873 | 必需 | 1454（testConnection）、1970（close）、3982（forceReopen） |
| wcdb_purge_memory | 877 | 可选 | 1947（startPeriodicPurge，60s 周期） |
| wcdb_open_account | 884 | 必需 | 1438（testConnection）、1882（open） |
| wcdb_close_account | 888 | 必需(仅绑定) | 无调用点 |
| wcdb_set_my_account_id | 892 | 可选 | 1913（open） |
| wcdb_free_string | 919 | 必需 | 256、1493、1529、1551、2047、2092、4872、4897、4988、5015、5053 |
| wcdb_get_sessions | 922 | 必需 | 2012（getSessions）、2829（getMediaStream 会话名） |
| wcdb_mark_all_sessions_read | 926 | 可选 | 2043（markAllSessionsRead） |
| wcdb_reorder_sessions_by_time | 933 | 可选 | 2088（reorderSessionsByTime） |
| wcdb_get_messages | 939 | 必需 | 2137（getMessages） |
| wcdb_get_message_count | 942 | 必需 | 2187（getMessageCount）、2245（getMessageCounts） |
| wcdb_get_message_by_svrid | 945 | 必需 | 2206（getMessageByServerId） |
| wcdb_get_message_by_id | 1126 | 必需 | 4280（getMessageById） |
| wcdb_get_messages_by_type | 1177 | 可选 | 2438（getMessagesByType） |
| wcdb_update_message | 899 | 可选 | 5096（updateMessage） |
| wcdb_insert_text_message | 906 | 可选 | 5130（insertTextMessage） |
| wcdb_delete_message | 913 | 可选 | 5172（deleteMessage） |
| wcdb_install_message_anti_revoke_trigger | 1285 | 可选 | 4868 |
| wcdb_uninstall_message_anti_revoke_trigger | 1292 | 可选 | 4893 |
| wcdb_check_message_anti_revoke_trigger | 1299 | 可选 | 4915 |

## 附录 B：消息行 / 会话行字段候选名总表（消费方按顺序取第一个非空）

- **消息行**（wcdb_get_messages / by_type / by_id / by_svrid / fetch_message_batch 共用）：
  - 身份：`local_id`；`server_id`（number≥16位或 string）；`create_time|createTime|msg_time|msgTime|time`；`sort_seq`
  - 类型/状态：`local_type|localType|WCDB_CT_local_type`（可超 int32）；`status`（可 null）
  - 内容：`message_content`、`compress_content`（均可为原文 / hex / base64，可能 zstd 压缩，TS 自解压；compress_content 优先）
  - 方向/发送者：`computed_is_send` ?? `is_send`（0/1/null）；`sender_username`；`real_sender_id`
  - 来源：`db_path|_db_path`、`db_name`、`table_name`
  - 媒体扩展（可选）：`image_md5|imageMd5`、`video_md5|videoMd5|raw_md5|rawMd5`、
    `image_path|imagePath|image_dat_name|imageDatName|img_path|imgPath|img_name|imgName`、
    `packed_info_data|packedInfoData|packed_info_blob|packed_info|BytesExtra|bytes_extra|reserved0|Reserved0|WCDB_CT_*`
- **会话行**（wcdb_get_sessions）：
  `username|user_name|userName|usrName|UsrName|talker|talker_id|talkerId`、`type`、
  `local_type|localType|WCDB_CT_local_type`、`unread_count|unreadCount|unreadcount`、
  `sort_timestamp|sortTimestamp|sort_time|sortTime`、`last_timestamp|lastTimestamp|last_msg_time|lastMsgTime`、
  `summary|digest|last_msg|lastMsg`、`last_msg_type|lastMsgType`、
  `message_count|messageCount|msg_count|msgCount|total_count|totalCount|n_msg|nMsg|message_num|messageNum`、
  `last_msg_sender`、`last_sender_display_name`、`displayName|display_name|remark`。
