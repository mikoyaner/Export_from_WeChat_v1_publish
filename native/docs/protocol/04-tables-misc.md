# 04 - 表 / 快照 / 运维 / 杂项 函数组协议规格

> **来源与边界**：本文是**接口行为描述**（函数名、字段名、返回形状、错误路径），源自对 WeFlow 项目（Electron/TypeScript）`wcdbCore.ts` 等文件的阅读笔记；**不含其源代码**，也不声称与上游实现完全一致。第三方权利与许可以 WeFlow 原项目为准。


适用范围：`wcdb_get_message_tables`、`wcdb_get_message_table_stats`、`wcdb_get_message_dates`、
`wcdb_get_message_meta`、`wcdb_get_message_table_columns`、`wcdb_get_message_table_time_range`、
`wcdb_list_tables`、`wcdb_get_table_schema`、`wcdb_export_table_snapshot`、`wcdb_import_table_snapshot`、
`wcdb_import_table_snapshot_with_schema`、`wcdb_list_message_dbs`、`wcdb_list_media_dbs`、
`wcdb_get_media_schema_summary`、`wcdb_exec_query`、`wcdb_get_db_status`、`wcdb_get_logs`、
`wcdb_start_monitor_pipe`、`wcdb_stop_monitor_pipe`、`wcdb_get_monitor_pipe_name`、
`wcdb_cloud_init`、`wcdb_cloud_report`、`wcdb_cloud_stop`、`VerifyUser`。

协议唯一来源：`WeFlow-main/electron/services/wcdbCore.ts`（下文行号均指该文件，除非另注）。
全局约定（koffi 加载方式、int32 状态码 `0=成功`、`_Out_ void**` 出参字符串必须能用
`wcdb_free_string` 释放、UTF-8 编码等）见 `01` 号文档，此处不重复。

本组内所有带 `int64 handle` 的函数，调用前 TS 侧都执行 `ensureReady()`（L1574-1576，
`initialized && handle !== null`），即 handle 一定是 `wcdb_open_account` 成功返回的正整数。
例外：`wcdb_get_logs`、monitor pipe 三个函数、`wcdb_cloud_*`、`VerifyUser` 不要求已 open
（详见各节）。

线程模型（全局要点）：所有 DLL 调用都发生在**单一** Electron worker 线程
（`electron/wcdbWorker.ts` L329-338 用 promise 链把全部请求串行化），因此同步 API 不会被
并发调用；但 DLL 内部自起的线程（monitor pipe 服务线程、cloud 心跳线程）必须自行与
API 调用线程做好同步。

绑定必选性（全局要点）：`initialize()` 中**不在 try/catch 内**的 `lib.func(...)` 若符号缺失
会抛异常并导致整个 DLL 初始化失败。本组中属"必须导出"的符号：
`wcdb_get_message_tables`(L974)、`wcdb_get_message_meta`(L977)、
`wcdb_get_message_table_stats`(L1016)、`wcdb_get_logs`(L1095，L1051 另有一次容错绑定但被覆盖)、
`wcdb_exec_query`(L1098)、`wcdb_list_message_dbs`(L1120)、`wcdb_list_media_dbs`(L1123)。
其余本组符号均为可选（缺失时对应功能返回"接口未就绪"，不影响其它功能）。

---

### wcdb_get_message_tables

- 签名：`int32 wcdb_get_message_tables(int64 handle, const char* sessionId, _Out_ void** outJson)`（绑定 L974，必选）
- 调用点：`getMessageTables()` L3353-3370，native 调用在 L3359。上层链路 `wcdbWorker.ts` L118-119 → `wcdbService.ts` L491-492；**当前 main/渲染层没有任何消费方**（仅管道透传）。
- 入参：`sessionId` — 会话用户名（如 `wxid_xxx` 或 `xxx@chatroom`）。
- 状态码与错误：`result !== 0 || !outPtr[0]` → `{success:false, error:"获取消息表失败: <code>"}`（L3360-3362）。无 outError 出参，错误细节只能靠状态码 + `wcdb_get_logs`。
- 返回 JSON 结构：`JSON.parse(jsonStr)` 后直接作为 `tables: any[]` 透传（L3365-3366），TS 不读任何字段。顶层必须是**数组**。元素结构：**未确定**（无消费方；从同族的 `wcdb_get_message_table_stats` 推断元素应含 `db_path`/`table_name`，但这是推断而非协议事实）。
- 备注：符号必须导出（否则整个 initialize 失败），但功能上目前无人消费；实现时建议与 `wcdb_get_message_table_stats` 返回同样的行对象（去掉统计字段亦可）。

### wcdb_get_message_table_stats

- 签名：`int32 wcdb_get_message_table_stats(int64 handle, const char* sessionId, _Out_ void** outJson)`（绑定 L1016，必选）
- 调用点：`getMessageTableStats()` L3395-3412，native 调用 L3401。消费方（决定字段名，全部经 `wcdbService.getMessageTableStats`）：
  - `chatService.ts` L1585-1594（防撤回：读 `table_name`/`tableName`）；
  - `chatService.ts` L5418-5427（会话消息表缓存：读 `t.table_name || t.name`、`t.db_path`，**db_path 无 camelCase 回退，必须 snake_case**）；
  - `chatService.ts` L9570-9600（会话统计快照：读 `count ?? message_count ?? messageCount`；`last_timestamp ?? lastTimestamp ?? last_time ?? lastTime ?? max_create_time ?? maxCreateTime`）；
  - `chatService.ts` L12044-12070（关系旅程指纹：读 `db_path`（无回退）、`table_name || name`、`Number(row.count)`（无回退）、`first_timestamp/firstTimestamp/first_time/firstTime/min_create_time/minCreateTime`、`last_timestamp/lastTimestamp/last_time/lastTime/max_create_time/maxCreateTime`）；
  - `messagePushService.ts` L680-692（防撤回推送候选表：读 `db_path || dbPath`、`table_name || tableName`、**`last_time || lastTime`（不读 last_timestamp！）**，并用 `last_time >= since` 过滤）；
  - `annualReportService.ts` L366-375（读 `table_name || name`、`db_path || dbPath`）。
- 入参：`sessionId` — 会话用户名。
- 状态码与错误：`result !== 0 || !outPtr[0]` → 失败 `获取表统计失败: <code>`（L3402-3404）。
- 返回 JSON 结构（顶层为数组，每个元素对应一张承载该会话消息的分表）：

```jsonc
[
  {
    "db_path": "C:/.../db_storage/message/message_0.db", // string，绝对路径。必须叫 db_path（chatService L5425/L12049 无回退）
    "table_name": "Msg_xxxxxxxx",                        // string，消息分表名。必须叫 table_name（多个消费方首选）
    "count": 1234,                                       // number（或数字字符串，chatService L12051 用 parseInt），该表内该会话消息条数
    "first_timestamp": 1500000000,                       // number，秒级 Unix 时间；别名 first_time/min_create_time 也被接受
    "last_timestamp": 1700000000,                        // number，秒级 Unix 时间；别名 last_time/max_create_time 也被接受
    "last_time": 1700000000                              // 【强烈建议同时输出】messagePushService L688 只读 last_time/lastTime，
                                                          // 缺失会导致防撤回候选表全部被过滤（lastTime=0 < since）
  }
]
```

- 备注：为同时满足所有消费方，每个元素建议输出字段集合：`db_path`、`table_name`、`count`、`first_timestamp`、`last_timestamp`、`last_time`（=last_timestamp）。时间戳经 `toUnixTimestamp`/`Number()` 处理，秒或毫秒都能被 annualReport 侧容忍，但 chatService/messagePushService 直接与秒级 `create_time` 比较，**必须为秒**。

### wcdb_get_message_dates

- 签名：`int32 wcdb_get_message_dates(int64 handle, const char* sessionId, _Out_ void** outJson)`（绑定 L1071-1075，可选）
- 调用点：`getMessageDates()` L3372-3393，native 调用 L3381。链路：`wcdbWorker.ts` L124-125 → `wcdbService.ts` L502-503 → `chatService.ts` L11532-11552 → IPC `chat:getMessageDates`（main.ts L3939）→ `preload.ts` L296 → `src/pages/ChatPage.tsx` L2106-2113 → `JumpToDateDialog.tsx` L67-73 / `JumpToDatePopover.tsx` L67-76。
- 入参：`sessionId` — 会话用户名。
- 状态码与错误：**特殊**：`result !== 0 || !outPtr[0]` 时不报错，而是按"无消息"处理，返回 `{success:true, dates:[]}`（L3382-3385）。仅 decode 失败（返回 NULL 字符串）才报 `解析日期列表失败`。
- 返回 JSON 结构：顶层为**字符串数组**，每个元素是 `YYYY-MM-DD`（本地时区日期键）。前端用它做日历打点：`messageDates.has(\`${year}-${MM}-${DD}\`)`（JumpToDateDialog.tsx L70-72）。

```jsonc
["2023-01-05", "2023-01-06", "2024-12-31"]
```

- 备注：日期必须按本地日历（与消息 `create_time` 秒级时间戳换算的本地日期一致）生成，补零两位。

### wcdb_get_message_meta

- 签名：`int32 wcdb_get_message_meta(int64 handle, const char* dbPath, const char* tableName, int32 limit, int32 offset, _Out_ void** outJson)`（绑定 L977，必选）
- 调用点：`getMessageMeta()` L3414-3431，native 调用 L3420。链路 `wcdbWorker.ts` L127-128 → `wcdbService.ts` L509-510；**当前无更上层消费方**。
- 入参：`dbPath` — 消息库文件绝对路径（来自 `wcdb_get_message_table_stats` 的 `db_path`）；`tableName` — 消息分表名；`limit`/`offset` — 分页参数（TS 原样透传，不做裁剪）。
- 状态码与错误：`result !== 0 || !outPtr[0]` → `获取消息元数据失败: <code>`（L3421-3423）。
- 返回 JSON 结构：顶层为**数组**（`rows: any[]` 透传，L3426-3427），元素为消息行的"元数据"对象；字段名**未确定**（无解析点）。建议实现为与 `wcdb_exec_query` 的行对象一致的原始表列（`local_id`、`server_id`、`create_time`、`local_type`、`message_content` 等）。
- 备注：符号必须导出（绑定不在 try/catch 内），否则整个 DLL 初始化失败。

### wcdb_get_message_table_columns

- 签名：`int32 wcdb_get_message_table_columns(int64 handle, const char* dbPath, const char* tableName, _Out_ void** outJson)`（绑定 L1227-1231，可选）
- 调用点：`getMessageTableColumns()` L3515-3529，native 调用 L3520。消费方：`annualReportService.ts` L291-312（detectTimeColumn：在列名集合里找 `create_time`/`createtime`/`msg_create_time`/`msg_time`/`msgtime`/`time`）；`chatService.ts` L5439-5460（列名缓存，转小写匹配）、L10435-10441（找 `local_id`/`id`、`create_time`/`createtime`、`local_type`/`type`）。
- 入参：`dbPath` — 消息库绝对路径；`tableName` — 表名。
- 状态码与错误：`result !== 0 || !outPtr[0]` → `获取消息表列失败: <code>`；符号缺失 → `接口未就绪`。
- 返回 JSON 结构：顶层为**字符串数组**（列名，保持数据库中的原始大小写；TS 会对每个元素做 `String(c || '')`，L3524-3525）：

```jsonc
["local_id", "server_id", "create_time", "local_type", "message_content", "compress_content", "..."]
```

- 备注：消费方均按小写归一后匹配，因此大小写不敏感，但必须是 PRAGMA table_info 意义上的**完整列名清单**。

### wcdb_get_message_table_time_range

- 签名：`int32 wcdb_get_message_table_time_range(int64 handle, const char* dbPath, const char* tableName, _Out_ void** outJson)`（绑定 L1257-1261，可选）
- 调用点：`getMessageTableTimeRange()` L3621-3635，native 调用 L3626。消费方：`annualReportService.ts` L316-346（getTableTimeRange）。
- 入参：`dbPath`、`tableName` 同上。
- 状态码与错误：`result !== 0 || !outPtr[0]` → `获取消息表时间范围失败: <code>`。
- 返回 JSON 结构（顶层为**对象**，annualReportService L325-330 逐字段读取）：

```jsonc
{
  "column": "create_time", // string，本次实际用于求 MIN/MAX 的时间列名；annualReport 用它校验列是否符合预期（大小写不敏感比较）
  "first_ts": 1500000000,  // number，最早消息时间；别名 firstTs/min_ts/minTs 亦被接受；经 toUnixTimestamp 归一（秒或毫秒均可）
  "last_ts": 1700000000    // number，最晚消息时间；别名 lastTs/max_ts/maxTs 亦被接受
}
```

- 备注：首选字段名 `column` / `first_ts` / `last_ts`（snake_case）。空表时可返回 `first_ts=0,last_ts=0` 或非 0 状态码（消费方把两种都当 null 处理）。

### wcdb_list_tables

- 签名：`int32 wcdb_list_tables(int64 handle, const char* kind, const char* dbPath, _Out_ void** outJson)`（绑定 L1232-1236，可选）
- 调用点：`listTables()` L3531-3545，native 调用 L3536。消费方：`backupService.ts` L775-779（备份时枚举每个库的表）。
- 入参：
  - `kind` — 库类型字符串。已知合法取值（backupService L590-624 构造）：`'session'`、`'contact'`、`'emoticon'`、`'sns'`、`'hardlink'`、`'message'`、`'media'`。
  - `dbPath` — 库文件绝对路径；对 `message`/`media` 来自 `wcdb_list_message_dbs`/`wcdb_list_media_dbs` 的返回值；对前五类固定库，TS 传入自行解析出的路径，**找不到文件时会传空字符串 `''`**（backupService L592-599，`resolveKnownDbPath` 可返回 `''`），此时 DLL 应基于 handle 对应账号目录自行解析该 kind 的默认库（约定路径见 backupService L523-586：`session/session.db`、`Contact/contact.db`、`emoticon/emoticon.db`、`sns/sns.db`、`hardlink/hardlink.db`）。TS 侧永不传 NULL（`dbPath || ''`，L3536）。
- 状态码与错误：`result !== 0 || !outPtr[0]` → `获取表列表失败: <code>`。
- 返回 JSON 结构：顶层为**字符串数组**（表名；TS 对每项 `String(c||'')` 并过滤空串，L3540-3541）：

```jsonc
["Session", "SessionUnread", "..."]
```

- 备注：备份流程对返回空数组的库直接跳过（不算失败），因此 kind 不适用/库不存在时可返回 0 + `[]`，也可返回非 0；两者表现一致。

### wcdb_get_table_schema

- 签名：`int32 wcdb_get_table_schema(int64 handle, const char* kind, const char* dbPath, const char* tableName, _Out_ void** outJson)`（绑定 L1237-1241，可选）
- 调用点：`getTableSchema()` L3547-3560，native 调用 L3552。消费方：`backupService.ts` L782-786（取建表 SQL 写入备份 manifest 的 `schemaSql`，恢复时回传给 `wcdb_import_table_snapshot_with_schema`）。
- 入参：`kind`/`dbPath` 语义同 `wcdb_list_tables`（TS 传 `dbPath || ''`）；`tableName` — 表名。
- 状态码与错误：**双通道**：`result !== 0` 或 `data.success === false` 均判失败，错误文案取 `data.error`（若有 outJson 仍会被解析，L3553-3555）。即失败时也建议写 outJson：`{"success":false,"error":"..."}`。
- 返回 JSON 结构（顶层对象）：

```jsonc
{
  "success": true,                    // 可省略（仅 === false 时触发失败分支）
  "schema": "CREATE TABLE Msg_xx(...)", // string，完整 CREATE TABLE SQL（sqlite_master.sql）
  "error": "..."                      // string，仅失败时
}
```

- 备注：`schema` 为空字符串时备份侧跳过该表（backupService L783）。

### wcdb_export_table_snapshot

- 签名：`int32 wcdb_export_table_snapshot(int64 handle, const char* kind, const char* dbPath, const char* tableName, const char* outputPath, _Out_ void** outJson)`（绑定 L1242-1246，可选）
- 调用点：`exportTableSnapshot()` L3562-3575，native 调用 L3567。消费方：`backupService.ts` L805-817（导出到备份暂存目录 `<staging>/snapshots/<dbId>/<table>.wfsnap`，并把 `rows`/`columns` 写入 manifest）。
- 入参：`kind`/`dbPath`/`tableName` 同上；`outputPath` — 快照输出文件的**绝对路径**（目录已由 TS 创建；文件格式 `.wfsnap` 对 TS 完全不透明，由 DLL 自定义、由 `wcdb_import_table_snapshot*` 读回）。
- 状态码与错误：双通道同 `wcdb_get_table_schema`：`result !== 0 || data.success === false` → 失败，错误取 `data.error`；备份流程失败会直接抛错中止（backupService L806-808）。
- 返回 JSON 结构（顶层对象）：

```jsonc
{
  "success": true,   // 可省略；=== false 触发失败分支
  "rows": 1234,      // number，导出的行数（TS Number(data.rows||0)）
  "columns": 25,     // number，列数
  "error": "..."     // 仅失败时
}
```

- 备注：快照文件必须自包含（恢复时配合外部传入的 createTableSql 可重建表），建议内含列名/类型与全部行数据。

### wcdb_import_table_snapshot

- 签名：`int32 wcdb_import_table_snapshot(int64 handle, const char* kind, const char* dbPath, const char* tableName, const char* inputPath, _Out_ void** outJson)`（绑定 L1247-1251，可选）
- 调用点：`importTableSnapshot()` L3577-3597，native 调用 L3582。链路到 `wcdbService.ts` L529-530；**当前备份恢复流程实际使用的是 with_schema 变体**（backupService L1113-1119），本函数无直接上层调用，但仍需按同构协议实现。
- 入参：`kind`/`dbPath`（恢复目标库绝对路径，TS 传 `dbPath || ''`）/`tableName`；`inputPath` — 快照文件绝对路径。目标库文件可能尚不存在（backupService L1112 先 `mkdirSync(dirname(targetDbPath))`），DLL 应能创建加密库并按已存在的表结构导入。
- 状态码与错误：双通道：`result !== 0 || data.success === false` → 失败取 `data.error`。
- 返回 JSON 结构（顶层对象；L3586-3593 逐字段读取）：

```jsonc
{
  "success": true,  // 可省略；=== false 触发失败分支
  "rows": 1234,     // number，快照中的总行数
  "inserted": 1200, // number，实际插入行数（备份恢复页汇总展示）
  "ignored": 34,    // number，因已存在/冲突被忽略的行数
  "malformed": 0,   // number，格式损坏无法解析的行数
  "columns": 25,    // number，列数
  "error": "..."    // 仅失败时
}
```

### wcdb_import_table_snapshot_with_schema

- 签名：`int32 wcdb_import_table_snapshot_with_schema(int64 handle, const char* kind, const char* dbPath, const char* tableName, const char* inputPath, const char* createTableSql, _Out_ void** outJson)`（绑定 L1252-1256，可选）
- 调用点：`importTableSnapshotWithSchema()` L3599-3619，native 调用 L3604。消费方：`backupService.ts` L1113-1127（恢复流程：目标库路径按 manifest 的 relativePath 解析；`createTableSql` 即导出时 `wcdb_get_table_schema` 返回的 `schema` 原文）。
- 入参：在 `wcdb_import_table_snapshot` 基础上多一个 `createTableSql` — 完整 CREATE TABLE 语句（TS 传 `createTableSql || ''`，可能为空串，此时行为应退化为不带 schema 的导入）。
- 状态码与错误：双通道，同上。失败时恢复侧仅计 `skipped`，不中止。
- 返回 JSON 结构：与 `wcdb_import_table_snapshot` 完全一致（`rows`/`inserted`/`ignored`/`malformed`/`columns`/`error`，L3608-3615）。
- 备注：语义应为"表不存在则先执行 createTableSql 建表，再幂等导入快照行"；`inserted`/`ignored` 用于恢复结果汇总（backupService L1121-1123）。

### wcdb_list_message_dbs

- 签名：`int32 wcdb_list_message_dbs(int64 handle, _Out_ void** outJson)`（绑定 L1120，必选）
- 调用点：`listMessageDbs()` L4248-4261，native 调用 L4252。消费方：`backupService.ts` L601-610（把每个路径作为 kind='message' 的备份目标）；`chatService.ts` L2259 附近（消息库计数快照 `dbPaths`）。
- 入参：仅 handle。
- 状态码与错误：`result !== 0 || !outPtr[0]` → `获取消息库列表失败: <code>`。
- 返回 JSON 结构：顶层为**字符串数组**，每个元素是 message_*.db 的**绝对路径**（消费方直接 `String(dbPath)`、`basename()`、参与文件存在性检查）：

```jsonc
["C:/Users/x/AppData/.../db_storage/message/message_0.db",
 "C:/Users/x/AppData/.../db_storage/message/message_1.db"]
```

- 备注：无消息库时返回 `[]`（chatService 以 `Array.isArray(data)` 判定，空数组合法）。

### wcdb_list_media_dbs

- 签名：`int32 wcdb_list_media_dbs(int64 handle, _Out_ void** outJson)`（绑定 L1123，必选）
- 调用点：`listMediaDbs()` L4263-4275，native 调用 L4267。消费方：`chatService.ts` L797-802（预热缓存 `mediaDbsCache: string[]`）、L822、L10384-10394（语音 fallback 直查 media_*.db）；`backupService.ts` L613-622。
- 入参：仅 handle。
- 状态码与错误：`result !== 0 || !outPtr[0]` → `获取媒体库列表失败: <code>`。TS 侧失败时有手动扫描 `media_*.db` 的 fallback（chatService L10396-10403、L8099 注释），但 native 实现仍应正确返回。
- 返回 JSON 结构：同 `wcdb_list_message_dbs`，**绝对路径字符串数组**（media_*.db / 相关媒体库文件）。

### wcdb_get_media_schema_summary

- 签名：`int32 wcdb_get_media_schema_summary(int64 handle, const char* dbPath, _Out_ void** outJson)`（绑定 L1146-1150，可选）
- 调用点：`getMediaSchemaSummary()` L4333-4347，native 调用 L4338。链路 `wcdbWorker.ts` L245-246 → `wcdbService.ts` L742-743；**当前无更上层消费方**。
- 入参：`dbPath` — 媒体库文件绝对路径。
- 状态码与错误：`result !== 0 || !outPtr[0]` → `获取媒体表结构摘要失败: <code>`。
- 返回 JSON 结构：`JSON.parse(jsonStr) || {}` 后整体作为 `data: any` 透传（L4342-4343），**字段未确定**（无解析点）。顶层必须是对象（TS 用 `|| {}` 兜底 null）。建议实现为 `{ "<表名>": ["<列名>", ...], ... }` 或类似摘要，但此为建议非协议。
- 备注：符号缺失只影响该 API（返回"接口未就绪"）。

### wcdb_exec_query

- 签名：`int32 wcdb_exec_query(int64 handle, const char* kind, const char* path, const char* sql, _Out_ void** outJson)`（绑定 L1098，必选）
- 调用点：`execQuery()` L4128-4186，native 调用 L4159。这是全应用的**通用 SQL 逃生门**，调用点极多：wcdbCore 内部诊断（L551-565）、macOS 联系人/头像路径（L3091、L3196）、getContact fallback（L3441、L3462）、getContactsCompact fallback（L3675）；`bizService.ts` L203/L240/L247；`chatService.ts` L4047/L4081/L4130/L4161/L4183/L4194/L8228/L8260/L10448/L10495-10590/L13172/L13229/L13244/L13257/L13293/L13767；`messagePushService.ts` L607/L631/L667；`export/core/ExportContext.ts` L1225。
- 入参：
  - `kind` — 库类型。实际出现的取值：`'contact'`、`'message'`、`'media'`（大小写原样传入，TS 内部仅自己比较时才 toLowerCase）。语义：告诉 DLL 用哪套密钥/连接池打开目标库。
  - `path` — 目标库文件绝对路径；**可能为空串**（TS 传 `effectivePath = path || ''`，L4147-4159；koffi 形参为 `const char*`，本调用路径不会传 NULL，但 `wcdbCore.execQuery` 的 TS 签名允许 `path: string | null`）。`kind='contact'` 且 path 为空时，TS 会先尝试自行解析 `db_storage/Contact/contact.db` 并覆盖 path（L4148-4156）；解析不到仍以空串调用，DLL 需能基于 handle 自行定位 contact 库。`kind='message'/'media'` 时 path 总是显式的（来自 stats/list 接口）。
  - `sql` — 完整 SQL 文本。只读查询为主（SELECT / PRAGMA / sqlite_master），TS 层无参数绑定（`params` 是占位符，**不会传给 DLL**，L4138-4143）。SQL 内可能包含中文、单引号转义值、`hex(col) AS alias` 等表达式。
- 状态码与错误：`result !== 0 || !outPtr[0]` → `执行查询失败: <code>`；contact 相关失败会额外触发 `wcdb_get_db_status` 与 `wcdb_get_logs` 诊断转储（L4160-4166）。
- 返回 JSON 结构：**通用结果集 = 行对象数组**（不是"列名数组+行数组"两段式）：

```jsonc
[
  // 每个元素是一行；key = SQL 输出列名/别名（原样，含 AS 别名与大小写）
  { "name": "Msg_abc123", "type": "table" },          // sqlite_master 查询示例（消费方读 row.name）
  { "local_id": 123, "create_time": 1700000000 },      // 数值列 → JSON number
  { "username": "wxid_x", "remark": null }              // NULL → JSON null
]
```

  值类型规则（由消费代码反推）：
  - 文本列 → JSON string；整数/浮点列 → JSON number（消费方大量 `Number(...)`/`parseInt`；`rowid`、`local_id` 等都按 number 读）。int64 大整数（如 16+ 位 `server_id`）经 `JSON.parse` 会丢精度——exec_query 的消费方都用 `String()`/`toSafeInt` 容忍，但建议超过 2^53 的整数值以字符串输出或确保 SQL 侧已 `hex()`/`CAST`。
  - BLOB 列 → 无直接 BLOB 消费；代码总是写 `hex(col) AS data` 再按 **hex 字符串**读取（chatService L10556、L8194-8214 `decodeVoiceBlob` 同时接受 hex 字符串、base64 字符串、字节数组）。若直接 SELECT BLOB 列，建议输出 hex 或 base64 字符串（`decodeVoiceBlob` 两者都能解）。
  - 空结果集 → `[]`（消费方以 `rows.length === 0` 判断无命中）。
- 备注：写 SQL 的注入风险由 TS 侧自负；DLL 只需原样执行。PRAGMA 语句（`PRAGMA table_info(x)`）也必须支持，返回行对象含 `cid`/`name`/`type`/`notnull`/`dflt_value`/`pk` 等标准列（消费方读 `row.name`、`row.type`）。

### wcdb_get_db_status

- 签名：`int32 wcdb_get_db_status(int64 handle, _Out_ void** outJson)`（绑定 L1129-1133，可选）
- 调用点：`dumpDbStatus()` L510-535（native 调用 L521）。触发时机：open 成功后（L1923）、getContact 主接口失败（L3456）、exec_query contact 失败/异常（L4163、L4182）。
- 入参：仅 handle。
- 状态码与错误：`rc !== 0 || !outPtr[0]` → 仅写日志跳过，不影响主流程。
- 返回 JSON 结构：**未确定** —— JSON 字符串被原样写进诊断日志（`[diag:<tag>] db_status=<json>`，L531），TS 不解析任何字段。顶层建议为对象，内容自定（如各库连接数、页缓存、最近错误等诊断信息）。
- 备注：符号缺失时仅记录 `api not supported`。此函数是排障链的一部分，建议实现为返回当前账号各数据库的可打开性/连接状态摘要。

### wcdb_get_logs

- 签名：`int32 wcdb_get_logs(_Out_ void** outJson)`（绑定 L1049-1054 容错 + L1095 **必选**重绑定；无 handle 参数）
- 调用点：
  - `printLogs()` L1484-1502（native 调用 L1488）：open/testConnection 失败、游标失败、contact 查询失败时把原始 JSON 写入本地日志文件；
  - `pollLogs()` L1519-1545（native 调用 L1523）：日志开关开启时每 2 秒轮询一次，`JSON.parse(jsonStr) as string[]`，按"上次尾行"增量转发到 wcdb.log；
  - `getLogs()` L4111-4126（native 调用 L4116）：经 `wcdbService.getLogs()`（L841-842）→ `cloudControlService.getLogs()` → IPC `cloud:getLogs`（main.ts L3191）暴露给设置页；另在 `chatService.ts` L13705、`ExportContext.ts` L4950 用于诊断。
- 入参：无。**不要求已 open_account**（printLogs 在 open 失败路径上调用）。
- 状态码与错误：`result !== 0 || !outPtr[0]` → 调用方静默跳过或返回 `获取日志失败: <code>`。
- 返回 JSON 结构：顶层为**字符串数组**（DLL 内部日志行，按时间序；pollLogs 依赖"数组尾部是最新行"且用 `lastIndexOf(上次尾行)` 做增量，L1531-1541，因此数组应保持追加序、内容稳定）：

```jsonc
["[info] opened message_0.db", "[warn] schema mismatch in Msg_xxx", "..."]
```

- 备注：无日志时应返回 `0` + `[]`（`Array.isArray && length===0` 直接跳过）。内存仍由 `wcdb_free_string` 释放。

### wcdb_start_monitor_pipe

- 签名：`int32 wcdb_start_monitor_pipe()`（绑定 L1334，可选；三个 pipe 函数一起 try/catch，任一缺失则整组置 null）
- 调用点：`startMonitor()` L224-266，native 调用 L244。触发链：`chatService.setupDbMonitor()`（chatService.ts L762-790）→ `wcdbService.setMonitor()`（wcdbService.ts L173-175）→ `wcdbWorker.ts` L27-37（`core.setMonitor(cb)`，事件经 `parentPort.postMessage({id:-1,type:'monitor',payload:{type,json}})` 转回主线程）。
- 入参：无（无 handle；但实际调用发生在已连接后）。
- 状态码与错误：返回 `!== 0` → `startMonitor` 返回 false，监控功能整体关闭。
- 返回 JSON 结构：无 outJson。
- 备注（native 侧职责）：启动**命名管道服务端**（Windows Named Pipe；macOS 为 Unix socket）+ 目录监控（chatService L765 注释指明用 `ReadDirectoryChangesW` 监听 session/message 目录变更），每当数据库文件变化时向已连接客户端推送事件。**可重入**：TS 在每次 startMonitor 前先调 `wcdb_stop_monitor_pipe()` 再调本函数（L240、L244），native 必须容忍"未启动就 stop"与"重复 start"。

### wcdb_get_monitor_pipe_name

- 签名：`int32 wcdb_get_monitor_pipe_name(_Out_ void** outName)`（绑定 L1336，可选）
- 调用点：`startMonitor()` L251-259（native 调用 L254）。
- 入参：无。
- 状态码与错误：返回 0 且 `*outName != NULL` 时，TS 用 `koffi.decode(ptr,'char',-1)` 读取 C 字符串，随后**必须**能用 `wcdb_free_string` 释放（L255-256）。失败/缺失时 TS 回退到固定名 `\\.\pipe\weflow_monitor`（L250）。
- 返回内容：不是 JSON，是**管道路径字符串**，注释说明为"动态管道名（含 PID）"，Windows 形如 `\\.\pipe\weflow_monitor_<pid>`；macOS 侧应为 socket 文件路径（TS 用 `net.createConnection(pipePath)` 连接，两种平台 Node 都支持）。
- 备注：必须在 `wcdb_start_monitor_pipe()` 成功后调用（TS 顺序即如此）。

### wcdb_stop_monitor_pipe

- 签名：`void wcdb_stop_monitor_pipe()`（绑定 L1335，可选；无返回值）
- 调用点：`startMonitor()` L240（重启前清场）、`stopMonitor()` L376-387（native 调用 L385；由 `close()` L1965 在 `wcdb_shutdown()` 之前调用）。
- 入参/返回：无。
- 备注：必须幂等、未启动时调用安全；调用后应关闭管道服务端并停止监控线程（close() 的注释明确要求：先停监控，避免 shutdown 后 native 回调访问已释放资源）。

**监控管道线上协议**（由 wcdbCore.ts L280-355 客户端实现 + 各消费方反推）：

1. 传输：DLL 为服务端，Electron（worker 线程内 `net.createConnection(pipePath)`，L292）为客户端；客户端在连接断开后每 3 秒重连（L358-372），服务端需接受重连。首次连接延迟约 100ms（L288-352）。
2. 分帧：UTF-8 文本，**每个事件一个 JSON 对象**，以换行 `\n` 分隔（NDJSON）。TS 客户端做了强归一化（L303-309）：`\0` → `\n`；相邻 `}{` → `}\n{`；即使没有任何分隔符、缓冲区自成完整 `{...}` 也会被解析（L327-338）。因此 `\n`、`\0` 或纯拼接三种写法都能工作，推荐 `\n`。
3. 事件对象字段（消费方实际读取的）：

```jsonc
{
  "action": "update",      // string，可选。作为回调 type 透传（L316：parsed.action || 'update'）。
                            // 消费方对 type 只做小写包含匹配（'message'/'session'/'db'/'group'/'member'/'contact'，chatService L5308-5320），
                            // 因此 'update' 是安全默认值
  "table": "Session"       // string，强烈建议。变更的表名。消费点：
                            //  - GlobalSessionMonitor.tsx L58-74：payload.table === 'Session'|'session' → 刷新会话列表
                            //  - BizPage.tsx L90-102：table 小写后为 'session'、含 'message'、以 'msg_' 开头 → 刷新公众号
                            //  - messagePushService.ts L95-125：table==='session' → 触发推送同步；table 为消息表（'message'/'msg' 前缀/含 'message'，L1241-1249）→ 防撤回扫描
  // 其余字段可选但有用：
  //  - 任何 key 含 'table' 的字符串字段、或任何值匹配 /\b(msg|message)_[a-z0-9_]+/i 的字符串字段，
  //    会被 messagePushService.collectMessageTableNamesFromPayload（L1204-1235）提取为受影响消息表名
  //  - 任何 key 含 'session'/'talker'/'username'/'chatroom' 的字符串字段（或值含 '@chatroom' 的字符串），
  //    会被 chatService.collectSessionIdsFromPayload（L5271-5301）提取为受影响会话 ID，用于精准失效缓存
  //  - 'db'/'path' 等字段无固定读取，但整个 JSON 文本参与 chatService L5312 的关键字包含判断
}
```

4. 无法解析的行：按 `('update', 原始行文本)` 透传（L318-322），不会崩溃，但建议始终输出合法 JSON。

### wcdb_cloud_init

- 签名：`int32 wcdb_cloud_init(int32 intervalSeconds)`（绑定 L1353-1357，可选）
- 调用点：`cloudInit()` L4628-4645，native 调用 L4637。上层：`cloudControlService.init()`（cloudControlService.ts L34-43）传入 **300**（秒）；`wcdbCore.cloudInit` 的默认参数为 600，但实际链路只用 300。
- 入参：`intervalSeconds` — DLL 内部云控/心跳线程的周期（秒）。
- 状态码与错误：`!== 0` → `{success:false, error:"Cloud init failed: <code>"}`。TS 侧保证在调用前 `wcdb_init()` 已成功（L4629-4632），但**不要求 open_account/handle**（无 ensureReady 检查）——云控功能独立于账号连接。
- 返回 JSON 结构：无 outJson。
- 备注（线程语义）：应启动一个后台线程按 intervalSeconds 周期执行云控任务（与 `wcdb_cloud_report` 上报的数据配合，如在线心跳/离线校验）。JS 侧 `initialized` 标志保证通常只调一次，但 native 仍应容忍重复调用（幂等）。后续 `wcdb_cloud_stop`/`wcdb_shutdown` 时必须能干净地停掉该线程。

### wcdb_cloud_report

- 签名：`int32 wcdb_cloud_report(const char* statsJson)`（绑定 L1360-1364，可选）
- 调用点：`cloudReport()` L4647-4664，native 调用 L4656（TS 传 `statsJson || ''`）。上层：`cloudControlService.flushQueue()`（cloudControlService.ts L93-122），每 ~5 分钟（300s 基准 + 0-30s 抖动，L26-27、L77-91）把积压报告按**每批最多 20 条**分批调用。
- 入参：`statsJson` — JSON **数组**字符串，元素为 UsageStats（cloudControlService.ts L4-11、L52-61）：

```jsonc
[
  {
    "appVersion": "1.2.3",     // string，WeFlow 版本（app.getVersion()）
    "platform": "Windows 11",  // string，OS 描述（'Windows 10/11'、'macOS <ver>'、Linux PRETTY_NAME 等）
    "deviceId": "9a1b...",     // string，md5(hostname+platform+arch) 的 hex，设备指纹
    "timestamp": 1730000000000,// number，毫秒级 Unix 时间（Date.now()）
    "online": true,            // boolean，恒为 true（在线上报）
    "pages": ["chat", "export"]// string[]，上个周期内访问过的页面名集合（recordPage 收集，上报后清空）
  }
]
```

- 状态码与错误：`!== 0`（或 JS 异常）→ TS 记一次连续失败：重试延迟 5s 起指数翻倍封顶 120s，连续 5 次失败后熔断 120s（cloudControlService.ts L20-32、L105-117）。**批次失败不会重发同一批以外的数据**——pendingReports 只在成功后 splice，因此 native 返回非 0 时应视为"整批未接收"。
- 返回 JSON 结构：无 outJson。
- 备注（线程语义）：字符串指针仅在调用期间有效（koffi 栈上编组），native 若要异步处理必须先复制。与 `wcdb_cloud_init` 一样不要求 handle。

### wcdb_cloud_stop

- 签名：`void wcdb_cloud_stop()`（绑定 L1367-1371，可选；无返回值）
- 调用点：`cloudStop()` L4666-4676（native 调用 L4671）；`close()` L1966 在 `wcdb_shutdown()` **之前**调用（注释：避免 shutdown 后仍有 native 回调访问已释放资源）；`cloudControlService.stop()` L233-239（应用退出/重置时）。
- 入参/返回：无。
- 备注：必须停止 cloud_init 启动的后台线程并 join，幂等、未 init 时调用安全。stop 之后再次 `wcdb_cloud_init` 应能重新启动（cloudControlService.initialized 复位后会再走 init，L232、L34-38）。

### VerifyUser

- 签名：`void VerifyUser(int64 hwnd, const char* message, _Out_ char* outResult, int maxLen)`（绑定 L1346-1350，可选；注意导出名是 **PascalCase `VerifyUser`**，无 `wcdb_` 前缀，无状态码返回值）
- 调用点：`verifyUser()` L4677-4705，native 调用 L4695。上层：`wcdbService.verifyUser`（wcdbService.ts L848-849，经 wcdbWorker L299-300）→ `windowsHelloService.verify()`（windowsHelloService.ts L11-29）——用于 Windows Hello 身份验证（应用锁解锁）。
- 入参：
  - `hwnd` — 父窗口句柄（int64）。来源：`BrowserWindow.getNativeWindowHandle()` 的 Buffer 按小端整数转十进制字符串，再 `BigInt(hwnd)`（windowsHelloService.ts L19-21、wcdbCore L4694）。**可能为 0**（无窗口时 `BigInt(0)`），native 需容忍（用作 WinRT/UserConsentVerifier 对话框的 owner HWND，0 表示无 owner）。
  - `message` — UTF-8 提示文案（如默认值 `请验证您的身份以解锁 WeFlow`），传给系统验证对话框。
  - `outResult` — **调用方分配**的缓冲区（`Buffer.alloc(1024)`，L4690-4691），native 向其中写入 NUL 结尾的 JSON 字符串；**不使用 `wcdb_free_string`**（这是全库唯一"调用方出缓冲区"的函数）。
  - `maxLen` — 缓冲区容量，固定 1024；写入（含 NUL）不得超过此长度。
- 状态码与错误：无返回码。TS 直接 `koffi.decode(outBuf,'char',-1)` + `JSON.parse` 并把解析结果**原样 resolve** 给调用方（L4698-4700）；解析抛异常时返回 `{success:false, error:String(e)}`。上层只检查 `result.success` 与 `result.error`（windowsHelloService 返回类型 `{success:boolean; error?:string}`）。
- 返回 JSON 结构（写入 outResult 的内容）：

```jsonc
{ "success": true }                       // 验证通过
{ "success": false, "error": "用户取消" }  // 失败/取消；error 为可选的中文说明，直接展示给上层
```

- 备注：调用是同步阻塞的（在 worker 线程内等待用户完成验证），实现时可阻塞直至验证流程结束。若符号缺失，上层得到 `Binding not found: VerifyUser`。outResult 必须始终写入合法 JSON（哪怕验证异常），否则 TS 侧 JSON.parse 抛错被 catch 成 `{success:false,error:"<异常文本>"}`，行为仍可接受但文案不可控。

---

## 本组全局性协议要点汇总

1. **snake_case 是事实标准**：所有被解析的字段（`db_path`、`table_name`、`count`、`first_timestamp`、`last_timestamp`、`first_ts`、`last_ts`、`column`、`rows`、`inserted`、`ignored`、`malformed`、`columns`、`schema`、`error`、`success`）均为小写 snake_case；TS 中出现的 camelCase 只是防御性回退，实现只需输出 snake_case——**唯一例外**：`messagePushService.getCandidateMessageTables` 只认 `last_time`/`lastTime`，因此 `wcdb_get_message_table_stats` 的行需同时携带 `last_time` 与 `last_timestamp`。
2. **快照/表操作类接口的双通道错误**：`wcdb_get_table_schema`、`wcdb_export_table_snapshot`、`wcdb_import_table_snapshot(_with_schema)` 即使在 `rc==0` 时也会检查 `data.success === false`，失败原因取 `data.error`；建议失败时同时返回非 0 状态码与 `{"success":false,"error":"..."}`。
3. **kind 取值集合**：`'session' | 'contact' | 'emoticon' | 'sns' | 'hardlink' | 'message' | 'media'`（`wcdb_list_tables`/`wcdb_get_table_schema`/快照三件套）；`wcdb_exec_query` 实际只用 `'contact' | 'message' | 'media'`。`dbPath/path` 为空串时 DLL 需基于 handle 账号目录自解析（contact 最常见）。
4. **exec_query 结果集**是"行对象数组"（key=SQL 列名/别名），不是列名+行两段式；BLOB 建议 hex 字符串。
5. **无消费方的透传接口**（`wcdb_get_message_tables`、`wcdb_get_message_meta`、`wcdb_get_media_schema_summary`、`wcdb_get_db_status`）字段结构在 TS 侧未确定；其中 `wcdb_get_message_tables`、`wcdb_get_message_meta` 因绑定不在 try/catch 内而**必须导出**，否则整个 DLL 无法初始化。
6. **管道监控**是 NDJSON 单向推送（DLL=服务端），关键字段 `action`（默认 'update'）与 `table`；会话/表名可放在任意含 `session`/`talker`/`username`/`chatroom`/`table` 关键字的字段中被自动提取。
7. **cloud 三件套与 VerifyUser、get_logs、monitor pipe 均不依赖 open_account**；cloud 上报负载是 UsageStats 数组（`appVersion/platform/deviceId/timestamp(ms)/online/pages[]`），失败会触发 JS 侧指数退避与熔断。
