# 02 — 统计 / 游标 / 搜索 / 报告类接口协议（18 个函数）

> **来源与边界**：本文是**接口行为描述**（函数名、字段名、返回形状、错误路径），源自对 WeFlow 项目（Electron/TypeScript）`wcdbCore.ts` 等文件的阅读笔记；**不含其源代码**，也不声称与上游实现完全一致。第三方权利与许可以 WeFlow 原项目为准。


> 本文档以 `WeFlow-main/electron/services/wcdbCore.ts`（5191 行）为唯一协议事实来源，
> 覆盖 wcdb.dll 中「会话统计、消息游标、搜索、聚合/年度/双人/群聊/足迹报告」共 18 个导出函数。
> 全局约定（koffi 加载方式、`wcdb_handle` 语义、状态码总表、`wcdb_free_string` 内存规则）见 **文档 01**，此处不重复。

## 0. 本组通用约定（引用文档 01，仅列本组特有事实）

1. **本组 18 个函数均无 `outError` 出参**。错误只通过返回状态码（`int32`）与 `outJson` 指针是否为 NULL 表达。
2. **outJson 内存规则**：所有 `outJson` 为 DLL 分配的 UTF-8 C 字符串（`char*`）。TS 侧统一走
   `decodeJsonPtr`（wcdbCore.ts L1547-1557）：`koffi.decode(ptr, 'char', -1)` 后立即用 `wcdb_free_string` 释放
   （即使解码抛异常也释放）。**Rust 实现必须用与 `wcdb_free_string` 配套的分配器分配 outJson**。
3. **时间戳单位**：本组所有 `begin/end/limit/offset` 时间参数均为 **int32 秒级 Unix 时间戳**。
   TS 侧入参前经 `normalizeTimestamp`（L1578-1586）：值 > 1e12 视为毫秒并转为秒；钳制到 `[0, 2147483647]`。
   `normalizeRange`（L1588-1598）：`end <= 0` → 取当前时间；`end < begin` → `end = begin`。
   注意：**游标三函数与 wcdb_search_messages 的时间戳 TS 不做 normalize，原样直传**（上层已保证秒级）。
   `0` 一律表示「不限」。
4. **状态码**：`result !== 0 || outJson == NULL` 均视为失败。本组出现的特殊码：
   - `-7`：message schema mismatch（消息表结构与程序要求不一致）。TS 对该码给出专门文案
     （getSessionMessageCounts L2268-2270、openMessageCursor L4054-4055）。
   - `-3`（仅 `wcdb_open_message_cursor`）：`WCDB_STATUS_NO_MESSAGE_DB`，消息库缓存为空。
     TS 触发 `forceReopen()` 并重试一次，重试受 15 秒冷却限制（`shouldRetryCursorAfterNoDb` L3992-3999）。
     **DLL 重开 handle 后旧游标全部失效，这是预期行为。**
5. **int64 大数**：消息行 JSON 中 `"server_id"` 可能是 ≥16 位十进制数。TS 的 `parseMessageJson`
   （L1559-1572）在 `JSON.parse` 之前用正则把 `"server_id": <16位以上数字>` 改写为字符串，防止 JS 精度丢失。
   Rust 侧可以直接输出数字（TS 会转换），但**建议输出为字符串**以避免其它消费者精度问题。
6. **符号必需性**：
   - 必需符号（绑定失败即抛异常，DLL 必须导出）：`wcdb_get_aggregate_stats`、`wcdb_open_message_cursor`、
     `wcdb_fetch_message_batch`、`wcdb_close_message_cursor`。
   - 可选符号（try/catch 绑定，缺失时置 null 并走降级路径）：本组其余 14 个函数。各函数的降级行为见其「备注」。
7. **调用链**：上层业务 → `wcdbService.ts`（薄代理）→ `wcdbWorker.ts`（worker 路由，L79-92 / L172-207 / L230-234）
   → `wcdbCore.ts`（真正 FFI 调用）。中间两层不改写 JSON 字段名。
8. **命名风格警示**：本组多数函数返回 **snake_case** 字段（type stats、footprint、搜索行），
   但 `wcdb_get_aggregate_stats` / `wcdb_get_annual_report_stats` / `wcdb_get_annual_report_extras` /
   `wcdb_get_dual_report_stats` 返回 **camelCase**（或混合）。逐函数以下文为准，勿统一风格。

---

### wcdb_get_session_message_counts

**签名**（绑定 wcdbCore.ts L1152，可选符号）
```c
int32_t wcdb_get_session_message_counts(wcdb_handle handle, const char* session_ids_json, char** out_json);
```

**调用点**
- wcdbCore.ts：`getSessionMessageCounts` L2261-2285，原生调用 L2266。
- 上游：chatService.ts L2368（`getSessionMessageCounts` 主路径）；main.ts L3621（IPC）；
  agentModelLedService.ts L2089、L2964；chatService.ts L9459。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| session_ids_json | `const char*` | JSON 字符串数组：`["wxid_xxx", "12345@chatroom", ...]`。TS 直接 `JSON.stringify(sessionIds || [])`，不去重不清洗（上层已处理）。 |

无时间范围参数：统计**全历史**消息总数。

**状态码与错误**
- `result !== 0 || out_json == NULL` → 失败。
- `-7` → TS 专门文案「message schema mismatch：当前账号消息表结构与程序要求不一致」。
- 其它非零 → `获取会话消息总数失败: ${result}`。

**返回 JSON 结构**
```jsonc
// Record<sessionId, number>：会话 ID → 消息总数（整数）
{
  "wxid_abc123": 1520,
  "12345678@chatroom": 48213
}
```
TS 消费方式（L2276-2280）：按**请求中的每个 sessionId** 读 `raw[sid] ?? 0`，`Math.floor` 并钳制 ≥0；
多余键被忽略、缺失键按 0 处理。因此 DLL 可以只返回有消息的会话，也可以返回全部。

**备注**
- 符号缺失时降级：逐会话调用 `wcdb_get_message_count`（`getMessageCounts`，L2263）。
- 值为「该会话消息行数」，不区分收发。

---

### wcdb_get_session_message_type_stats

**签名**（绑定 L1157，可选符号）
```c
int32_t wcdb_get_session_message_type_stats(wcdb_handle handle, const char* session_id,
    int32_t begin_timestamp, int32_t end_timestamp, char** out_json);
```

**调用点**
- wcdbCore.ts：`getSessionMessageTypeStats` L2287-2312，原生调用 L2296（begin/end 经 `normalizeTimestamp`）。
- 上游：chatService.ts L5763（会话画像/类型统计）。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| session_id | `const char*` | 单个会话 ID。 |
| begin_timestamp | `int32` | 秒；0 = 不限。 |
| end_timestamp | `int32` | 秒；0 = 不限（TS normalize 后 0 保持 0）。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `获取会话类型统计失败: ${result}`。
- 符号缺失 → `接口未就绪`（**无 JS 降级**，上层自行回退）。

**返回 JSON 结构**（wcdbCore 纯透传 `JSON.parse(jsonStr) || {}`；字段由 chatService.ts L5752-5867 消费确定，全 snake_case）
```jsonc
{
  "total_messages": 1234,          // number，范围内消息总数
  "voice_messages": 10,            // number，语音 (local_type 34)
  "image_messages": 200,           // number，图片 (local_type 3)
  "video_messages": 30,            // number，视频 (local_type 43)
  "emoji_messages": 400,           // number，表情 (local_type 47)
  "file_messages": 15,             // number，文件；chatService 用 hasOwnProperty 检查——旧版 DLL 可能缺此字段，缺失时上层走兼容回退
  "call_messages": 3,              // number，通话
  "transfer_messages": 1,          // number，转账
  "red_packet_messages": 2,        // number，红包
  "first_timestamp": 1600000000,   // number，范围内首条消息秒级时间戳
  "last_timestamp": 1700000000,    // number，范围内末条消息秒级时间戳
  "group_my_messages": 500,        // number，群聊中"我"发的消息数（私聊会话含义未确定）
  "group_sender_count": 42         // number，群内发送者人数（私聊会话含义未确定）
  // date_counts：单会话接口通常不含；chatService 在缺失时另行调用
  // wcdb_get_session_message_date_counts 回填（L5796）
}
```

**备注**
- 与 batch 版（下条）行结构完全一致；chatService 单会话路径也可能改用 batch 接口传单个 ID（L5753）。
- local_type 与消息类型映射（TS 消费端认知）：1/244813135921=文本，3=图片，34=语音，43=视频，47=表情，49=链接/文件。

---

### wcdb_get_session_message_type_stats_batch

**签名**（绑定 L1162，可选符号）
```c
int32_t wcdb_get_session_message_type_stats_batch(wcdb_handle handle, const char* session_ids_json,
    const char* options_json, char** out_json);
```

**调用点**
- wcdbCore.ts：`getSessionMessageTypeStatsBatch` L2314-2365，optionsJson 构造 L2345-2351，原生调用 L2352。
- 上游：chatService.ts L5753（单会话包成 batch）、L6128（真批量，`quickMode: !includeRelations && ids.length > 1`，
  `includeGroupSenderCount: true`，`includeDateCounts` 按需）；L6153 对缺 date_counts 的会话用
  `wcdb_get_session_message_date_counts_batch` 回填。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| session_ids_json | `const char*` | JSON 字符串数组。TS 已去重、trim、过滤空串；**空数组时 TS 直接返回 `{}`，不会调原生**。 |
| options_json | `const char*` | 选项对象，字段见下表。 |

options_json 字段（键名 snake_case）：
| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| begin | int（秒） | 0 | `normalizeTimestamp(beginTimestamp \|\| 0)`。 |
| end | int（秒） | 0 | 同上。 |
| quick_mode | bool | false | 仅当上层显式传 `true` 才为 true。精确语义（跳过哪些重统计）**未确定**；TS 只透传标志，不校验结果差异。 |
| include_group_sender_count | bool | **true** | TS 用 `!== false`，即未显式传 false 一律为 true。要求行内含 `group_sender_count`。 |
| include_date_counts | bool | **false** | TS 用 `=== true`。为 true 时行内应含 `date_counts`。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `批量获取会话类型统计失败: ${result}`。
- 符号缺失 → 降级为逐会话调用单会话版 `wcdb_get_session_message_type_stats`（L2328-2341）。

**返回 JSON 结构**（wcdbCore 纯透传）
```jsonc
// Record<sessionId, statsRow>；statsRow 与单会话版字段完全相同（snake_case），
// include_date_counts=true 时额外含 date_counts
{
  "wxid_abc123": {
    "total_messages": 1234,
    "voice_messages": 10,
    "image_messages": 200,
    "video_messages": 30,
    "emoji_messages": 400,
    "file_messages": 15,
    "call_messages": 3,
    "transfer_messages": 1,
    "red_packet_messages": 2,
    "first_timestamp": 1600000000,
    "last_timestamp": 1700000000,
    "group_my_messages": 500,
    "group_sender_count": 42,             // include_group_sender_count=true 时必须给出
    "date_counts": {                       // include_date_counts=true 时给出
      "2023-05-01": 12,                    // 键必须匹配 /^\d{4}-\d{2}-\d{2}$/，
      "2023-05-02": 7                      // 值经 normalizeMessageDateCounts（chatService L5203-5213）
                                           // 过滤：仅保留有限、>0，floor 取整
    }
  },
  "12345678@chatroom": { /* 同结构 */ }
}
```
TS 按 `nativeBatch.data?.[sessionId]` 逐会话读取；缺失的会话键 = 该会话统计不可用（上层回填/跳过）。

**备注**
- 请求去重后可能少于上层原始列表；返回对象建议覆盖所有请求 ID（含 0 值行），否则上层走 legacy 回填。

---

### wcdb_get_session_message_date_counts

**签名**（绑定 L1167，可选符号）
```c
int32_t wcdb_get_session_message_date_counts(wcdb_handle handle, const char* session_id, char** out_json);
```

**调用点**
- wcdbCore.ts：`getSessionMessageDateCounts` L2367-2387，原生调用 L2372。
- 上游：chatService.ts L5796（type stats 缺 date_counts 时回填）、L12116（按日消息分布）。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| session_id | `const char*` | 单个会话 ID。无时间范围参数（全历史）。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `获取会话日消息统计失败: ${result}`。
- 符号缺失 → `接口未就绪`（无降级）。

**返回 JSON 结构**
```jsonc
// Record<"YYYY-MM-DD", number>：日期（本地时区，键格式必须 %Y-%m-%d）→ 当日消息数
{
  "2023-05-01": 12,
  "2023-05-02": 7
}
```
TS 过滤（L2377-2382）：键非空、值有限且 > 0 才保留，`Math.floor` 取整。0 值日期可以省略。

**备注**
- 日期归属时区未在 TS 侧体现（由 DLL 决定）；上层将键直接当本地日期字符串消费。

---

### wcdb_get_session_message_date_counts_batch

**签名**（绑定 L1172，可选符号）
```c
int32_t wcdb_get_session_message_date_counts_batch(wcdb_handle handle, const char* session_ids_json, char** out_json);
```

**调用点**
- wcdbCore.ts：`getSessionMessageDateCountsBatch` L2389-2425，原生调用 L2405。
- 上游：chatService.ts L6153（批量 type stats 后对缺 date_counts 的会话补齐）。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| session_ids_json | `const char*` | JSON 字符串数组，TS 已去重/trim/过滤空；空数组不调原生，直接返回 `{}`。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `批量获取会话日消息统计失败: ${result}`。
- 符号缺失 → 降级为逐会话调用单会话版（L2394-2401）。

**返回 JSON 结构**
```jsonc
// Record<sessionId, Record<"YYYY-MM-DD", number>>
{
  "wxid_abc123": { "2023-05-01": 12, "2023-05-02": 7 },
  "12345678@chatroom": { "2024-01-01": 3 }
}
```
TS 过滤同单会话版（值有限且 > 0、floor）；缺失的会话键按 `{}` 处理（L2412）。

**备注**
- 无时间范围参数（全历史）。

---

### wcdb_open_message_cursor

**签名**（绑定 L1078，**必需符号**）
```c
// wcdb_cursor 为 int64
int32_t wcdb_open_message_cursor(wcdb_handle handle, const char* session_id, int32_t batch_size,
    int32_t ascending, int32_t begin_timestamp, int32_t end_timestamp, wcdb_cursor* out_cursor);
```

**调用点**
- wcdbCore.ts：`openMessageCursor` L4001-4074；原生调用 L4014，-3 重试路径 L4030。
- 上游（batchSize/方向实测值）：analyticsService.ts L222（500, 升序）；annualReportService.ts L209（**1**，边界探针，
  升/降序均有）、L544（2000, 升序）、L1037（1000, 升序）；chatService.ts L2968、L3282、L3411、L3546、
  L5506/L5566/L5630（500, **降序**）、L11139、L11225、L11667（'journey' 投影）、L11854（'journey'，800, 升序）、
  L12535（360）、L13607（120）、L13861、L14036；dualReportService.ts L386（500）、L482（`max(1, limit)`）；
  export/core/ExportContext.ts L4611；groupAnalyticsService.ts L879；groupSummaryService.ts L554；httpService.ts L710。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| session_id | `const char*` | 单个会话 ID。 |
| batch_size | `int32` | 每次 `wcdb_fetch_message_batch` 返回的最大行数。TS 不钳制、原样直传；实测调用值 1 / 120 / 360 / 500 / 800 / 1000 / 2000。**必须支持 batch_size=1**（边界探测用）。 |
| ascending | `int32` | 1 = 按 create_time（及排序键）升序，0 = 降序。TS 以 `ascending ? 1 : 0` 传。 |
| begin_timestamp | `int32` | 秒；0 = 下界不限。**本函数 TS 不做 normalize，原样直传**。 |
| end_timestamp | `int32` | 秒；0 = 上界不限。 |
| out_cursor | `int64*` | 出参：游标句柄。 |

**状态码与错误**
- 失败判定：`result !== 0 || out_cursor <= 0`（**游标必须 > 0**，0 视为无效/失败，L4044）。
- `-3` = `WCDB_STATUS_NO_MESSAGE_DB`（消息库缓存为空，常见于目标应用重装）：TS 执行 `forceReopen()`
  （重新 open 同一 path/key，得到**新 handle**）后携新 handle 重试一次；重试有 15 秒冷却
  （`cursorForceReopenCooldownMs`，`shouldRetryCursorAfterNoDb` L3992-3999）。冷却期内直接返回失败。
- `-7` = message schema mismatch，专门文案。
- 其它非零 → TS 调 `printLogs(true)` 拉取 DLL 日志并记录详细参数（L4045-4051），返回 `创建游标失败: ${result}，请查看日志`。

**返回**
- 成功：`out_cursor` 写入正整数游标 ID（int64；TS 经 koffi 读为 JS number，**取值须能被 double 精确表示**）。
- 随后 `projection === 'journey'` 且符号存在时，TS 紧接着调用
  `wcdb_set_message_cursor_projection(handle, cursor, 1)`（L4059-4067）——见该函数条目。

**备注**
- 生命周期：open → fetch*（任意次）→ close。上层普遍在 `finally` 中 close；DLL 应容忍「未 close 即进程退出」
  与「open 后一次都不 fetch 直接 close」。
- forceReopen 后旧 handle 及全部旧游标作废；TS 只在 -3 时触发该路径。
- 排序语义：升/降序按消息时间（create_time，同秒内按 sort_seq/rowid，具体次序键**未确定**——TS 消费端
  对批次内顺序宽容，但依赖跨批次的单调推进：降序扫描时上层用 `createTime` 做游标推进判断）。

---

### wcdb_fetch_message_batch

**签名**（绑定 L1089，**必需符号**）
```c
int32_t wcdb_fetch_message_batch(wcdb_handle handle, wcdb_cursor cursor, char** out_json, int32_t* out_has_more);
```

**调用点**
- wcdbCore.ts：`fetchMessageBatch` L4076-4094，原生调用 L4083。
- 上游：所有 open 调用点之后的循环体（analyticsService L229；annualReportService L212/559/1050；
  chatService L2994/3293/3555/3781/5514/5574/5638/11148/11690/11898/12583/13640/13917/14049；
  dualReportService L393/488；ExportContext L4634；groupAnalyticsService L909/994/1562；groupSummaryService L570；
  httpService L719 等）。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| cursor | `int64` | `wcdb_open_message_cursor` 返回的游标。 |
| out_json | `char**` | 出参：消息行 JSON 数组。 |
| out_has_more | `int32*` | 出参：是否还有后续批次。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `获取批次失败: ${result}`。
- **out_has_more 判定：TS 只认 `=== 1` 为 true**（L4090），其它任何值（含 0、2、-1）都当 false。
  Rust 侧必须严格写 1 / 0。
- 无效/已关闭游标的返回码未在 TS 侧特判（会走通用失败路径）；建议返回非零且**不写 out_json**。

**返回 JSON 结构**
`out_json` = 消息行对象的 JSON 数组，先经 `parseMessageJson`（server_id int64 → 字符串，见通用约定 5）。
行字段（TS 各消费端读取的字段并集；**主字段为 snake_case**，消费端同时容忍 camelCase 及 `WCDB_CT_` 前缀别名，
但新实现应输出 snake_case 主字段）：

```jsonc
[
  {
    "local_id": 123,                  // number，消息行主键（localId）
    "create_time": 1700000000,        // number，秒级（消费端兼容 createTime；create_time_ms 按毫秒处理）
    "sort_seq": 456,                  // number，同秒内排序序号（chatService L11723 读取）
    "computed_is_send": 1,            // number/bool，是否本人发送（消费端兼容 is_send / isSend / WCDB_CT_is_send）
    "sender_username": "wxid_xxx",    // string，群聊发送者（兼容 senderUsername / sender / WCDB_CT_sender_username）
    "local_type": 1,                  // number，消息类型（消费端兼容 type / localType / msg_type / msgType / WCDB_CT_local_type）
    "message_content": "...",         // string，明文内容（compress_content 为压缩/富媒体内容，二者消费端都读）
    "compress_content": "...",        // string，可缺省
    "server_id": "7123456789012345678", // ≥16 位时 TS 强制转字符串
    "source": "...",                  // string，系统/引用/@ 元数据 XML；足迹 mentions 依赖其中 @ 列表
    // 表情消息（local_type 47）相关，消费端多别名读取：
    "emoji_md5": "...",               // 兼容 emojiMd5 / md5
    "emoji_cdn_url": "...",           // 兼容 emojiCdnUrl / cdnurl / cdn_url / emoji_url / emojiUrl / url / thumburl / thumb_url
    "_session_id": "wxid_yyy"         // 仅 wcdb_search_messages 跨会话结果需要（见该函数）
  }
]
```
- **完整列集未确定**：TS 消费端全部是宽容读取（`row.a ?? row.b ?? ...`），上表是被实际读取的字段并集；
  其余列（如 talker、status、is_sender 等）是否输出由 DLL 决定，与 `wcdb_get_messages` 类接口共用行结构
  （见消息读取组文档 03）。
- 行数 ≤ batch_size。可以返回**空数组且 has_more=1**：chatService L11700-11707 显式容忍连续最多 2 个空批次
  （`emptyBatchStreak < 2`）后终止——即 DLL 允许「跳过一批无匹配行」但不应持续空转。
- 数据耗尽时：返回剩余行（可为空数组）且 `out_has_more = 0`。

**备注**
- 同一游标被顺序 fetch；TS 不做并发 fetch。游标状态（偏移）完全由 DLL 维护。

---

### wcdb_close_message_cursor

**签名**（绑定 L1092，**必需符号**）
```c
int32_t wcdb_close_message_cursor(wcdb_handle handle, wcdb_cursor cursor);
```

**调用点**
- wcdbCore.ts：`closeMessageCursor` L4096-4109，原生调用 L4101。
- 上游：所有游标使用点的 `finally`/清理路径（analyticsService L243；annualReportService L217/596/1170；
  chatService L888/937/1014/2970/3317/3437/3580/5545/5599/5717/9038/11154/11195/11250/11745/11871/12665/13676/14021/14114；
  dualReportService L431 等）。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| cursor | `int64` | 待关闭游标。 |

**状态码与错误**
- `result !== 0` → `关闭游标失败: ${result}`；**上层几乎全部 `.catch(() => {})` 吞掉**，不影响业务。
- 无出参、无内存分配。

**备注**
- 必须容忍：重复 close 同一 cursor（TS 多处有 `cursorClosed` 标志，但仍有并发清理路径可能重复调用）、
  close 无效/未知 cursor —— 建议对未知 cursor 返回 0 或专用非零码均可（TS 不区分），但**不得崩溃**。
- close 后必须释放游标占用的全部资源（Rust 侧 Box/句柄表）。

---

### wcdb_set_message_cursor_projection

**签名**（绑定 L1083，可选符号；绑定处注释 L1080-1081 明确协议）
```c
int32_t wcdb_set_message_cursor_projection(wcdb_handle handle, wcdb_cursor cursor, int32_t projection);
```

**调用点**
- wcdbCore.ts：仅 L4059-4067 —— `openMessageCursor(..., projection='journey')` 成功拿到 cursor 之后立即调用
  `wcdbSetMessageCursorProjection(handle, cursor, 1)`。
- 'journey' 的业务入口：chatService.ts L11673（关系旅程分析扫描）、L11860（旅程消息缓冲，batchSize 800 升序）；
  经 wcdbService.ts L639 / wcdbWorker.ts L193-207 透传。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| cursor | `int64` | **刚打开、尚未 fetch** 的游标。 |
| projection | `int32` | 0 = full（完整行投影，默认；TS 从显式传 0，因为不调用本函数即保持 full）；1 = journey（轻量投影）。其它取值未出现。 |

**状态码与错误**
- 返回非零 → 表示本 DLL 不支持/无法切换该投影。TS **仅记日志，静默保持 full 投影继续**（L4061-4066），
  open 仍算成功。因此 Rust 侧不支持时可返回非零，但**绝不能因此使游标失效**。
- 本函数**只改现有游标的投影，绝不分配新游标**（注释 L1080-1081）；出参只有状态码。

**返回 JSON 结构**
无（纯状态码函数）。

**备注**
- journey 投影的**确切列集未确定**：TS 消费端（chatService.mapRowsToRelationshipJourneyMessages，L6370 起）
  仍按完整行的宽容读取方式消费，实际用到的字段为
  `local_id`、`local_type`、`create_time`、`sort_seq`、`computed_is_send`/`is_send`、内容字段
  （`message_content`/`compress_content` → parsedContent/rawContent/content）及可见性过滤所需字段。
  journey 投影至少须包含这些列（或直接等价于 full），否则消费端得到空内容。

---

### wcdb_search_messages

**签名**（绑定 L1194，可选符号）
```c
int32_t wcdb_search_messages(wcdb_handle handle, const char* session_id, const char* keyword,
    int32_t limit, int32_t offset, int32_t begin_timestamp, int32_t end_timestamp, char** out_json);
```

**调用点**
- wcdbCore.ts：`searchMessages` L4707-4735，原生调用 L4715。
- 上游：chatService.ts L12731（`searchMessages`，行经 `parseMessage` 通用解析）；main.ts L3992（IPC）；
  agentModelLedService.ts L5152 / L5278（传 `scanLimit + 1` 作为 limit 以判断是否还有更多）；httpService.ts L1078。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| session_id | `const char*` | TS 传 `sessionId \|\| ''`。**空串 = 全局跨会话搜索**；非空 = 限定单会话。 |
| keyword | `const char*` | 搜索关键词。匹配语义（LIKE 子串 / FTS / 是否搜 compress_content）**未确定**——TS 完全不感知，仅透传。 |
| limit | `int32` | TS 传 `limit \|\| 50`（默认 50）。调用方可能传 `n+1` 探测下一页。 |
| offset | `int32` | TS 传 `offset \|\| 0`。 |
| begin_timestamp | `int32` | 秒；TS 传 `beginTimestamp \|\| 0`，**不 normalize**；0 = 不限。 |
| end_timestamp | `int32` | 同上。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `搜索消息失败: ${result}`。
- 符号缺失 → `当前数据服务版本不支持搜索消息`（无 JS 降级）。

**返回 JSON 结构**
`out_json` = 消息行 JSON 数组，行结构同 `wcdb_fetch_message_batch`（经 `parseMessageJson`），另加一个搜索特有约定：
- **跨会话搜索（session_id 为空）时，每行须带 `_session_id` 字段**（string，该行所属会话 ID）。
  消费点：chatService.ts L12744 `row._session_id` 用于在调用方未指定会话时标注归属。
  单会话搜索时该字段可有可无（上层用入参 sessionId 覆盖）。
- 群聊命中行可能缺 `sender_username` / 发送方标志：chatService L12745-12761 对群聊搜索结果做
  `needsDetailHydration`（缺 senderUsername 或 isSend 时用 `wcdb_get_message_by_id` 回取完整行）。
  这暗示搜索行允许是**轻投影**（不含完整发送者信息），但返回完整行同样兼容。
- 排序约定**未确定**（TS 不依赖搜索结果的顺序，仅按 limit/offset 分页）。

**备注**
- offset 分页语义由 DLL 定义（在同一查询条件下的稳定排序上跳过 offset 行）；排序不稳定会导致分页重复/丢失，
  建议按 create_time + local_id 排序（未强制）。

---

### wcdb_ai_query_session_candidates

**签名**（绑定 L1199，可选符号）
```c
int32_t wcdb_ai_query_session_candidates(wcdb_handle handle, const char* options_json, char** out_json);
```

**调用点**
- wcdbCore.ts：`querySessionCandidates` L4737-4767，optionsJson 构造与原生调用同在 L4752-4759。
- 管道：wcdbService.ts L718-726 → wcdbWorker.ts L233-234。
- **全仓库（electron/ 与 src/）没有任何业务消费方**——只有上述三层管道。

**入参**
options_json 字段（键名混合风格，`keyword` 是唯一的非 snake_case 键，照抄即可）：
| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| keyword | string | 必填 | 搜索关键词。 |
| session_id | string | `''` | 限定会话；空串 = 全局。 |
| limit | number | 30 | TS 传 `options.limit \|\| 30`。 |
| offset | number | 0 | TS 传 `options.offset \|\| 0`。 |
| begin_timestamp | number（秒） | 0 | TS 传 `options.beginTimestamp \|\| 0`，0 = 不限。 |
| end_timestamp | number（秒） | 0 | 同上。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `聚合搜索发送者失败: ${result}`。
- 符号缺失 → `当前数据服务版本不支持按发送者聚合搜索`。

**返回 JSON 结构**
- wcdbCore **纯透传**：`{ success: true, data: JSON.parse(jsonStr) }`（L4763），不做任何字段访问。
- 由于代码库中没有任何消费方读取 `data` 的字段，**输出结构完全未确定**。
  仅从包装函数命名（「按发送者聚合搜索」）与入参推测其意图为「按关键词聚合出候选会话/发送者列表」，
  字段名、嵌套、数组元素结构均**未确定**，不做猜测。Rust 侧首期可返回空数组/空对象并置状态码 0
  （无消费者会因此出错），或与维护者确认后再实现。

**备注**
- 该函数是 18 个中唯一「输出协议无代码证据」的函数，实现优先级最低。

---

### wcdb_get_group_stats

**签名**（绑定 L1058，可选符号）
```c
int32_t wcdb_get_group_stats(wcdb_handle handle, const char* chatroom_id,
    int32_t begin_timestamp, int32_t end_timestamp, char** out_json);
```

**调用点**
- wcdbCore.ts：`getGroupStats` L3890-3911，原生调用 L3900（begin/end 经 `normalizeRange`）。
- 上游：groupAnalyticsService.ts L651（群成员发言数面板）、L1392（发言排行）、L1442（活跃小时）、
  L1461（媒体类型分布）；export/formatters/JsonFormatter.ts L630 → ExportContext.`extractGroupSenderCountMap`
  L5156-5183。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| chatroom_id | `const char*` | 单个群会话 ID（`xxx@chatroom`）。 |
| begin_timestamp | `int32` | 秒；`normalizeRange`：end<=0→now、end<begin→end=begin。 |
| end_timestamp | `int32` | 同上。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `获取群聊统计失败: ${result}`。
- 符号缺失 → **降级为 `wcdb_get_aggregate_stats([chatroomId], begin, end)`**（L3894-3896）。
  因此输出结构与聚合统计高度重叠（但 aggregate 无 `senders`，降级时群成员发言数功能得到空表——
  消费端对 `senders` 缺失均安全返回空）。

**返回 JSON 结构**（透传；字段由消费端确定，camelCase 为主，与 aggregate stats 同族）
```jsonc
{
  // —— 与 wcdb_get_aggregate_stats 相同的公共字段（消费端有读取的）——
  "hourly": [0, 0, /* ... 共 24 项 ... */ 12],   // number[24]，下标 = 小时（groupAnalyticsService L1446-1448 逐项读取）
  "typeCounts": { "1": 500, "3": 120, "47": 80 }, // Record<string(localType), number>（L1464-1480 遍历）
  "idMap": { "123456": "wxid_abc" },              // Record<数字内部ID, username>（顶层；ExportContext 兼容 sessionStats.idMap）

  "sessions": {
    "12345678@chatroom": {
      "senders": {                                 // Record<senderKey, number>：每个发送者的消息数
        "123456": 300,                             // senderKey 可能是数字内部 ID（经 idMap 反查 username，
        "789012": 150                              //   groupAnalyticsService L657-670 / L1399-1408）
      }
      // ExportContext L5163-5166 还兼容 sessionStats 内层 idMap；
      // 以及顶层 senders 兜底（L5165-5166，groupStats.senders）
    }
  }
  // total/sent/received/firstTime/lastTime/daily/monthly 等 aggregate 同名字段是否输出：未确定
  //（本函数的消费端不读取，但降级路径 getAggregateStats 会输出它们）
}
```

**备注**
- `sessions` 的键应等于入参 chatroom_id；`senders` 的键空间（数字 ID vs username）由 DLL 决定，
  TS 两种都兼容（`idMap[id] || id`）。
- `extractGroupSenderCountMap` 对 count 要求：`Number(raw)` 有限且 > 0，floor 后累加——0/负值/非数值安全忽略。

---

### wcdb_get_aggregate_stats

**签名**（绑定 L1019，**必需符号**）
```c
int32_t wcdb_get_aggregate_stats(wcdb_handle handle, const char* session_ids_json,
    int32_t begin_timestamp, int32_t end_timestamp, char** out_json);
```

**调用点**
- wcdbCore.ts：`getAggregateStats` L3748-3804，原生调用 L3767（内部闭包 `callAggregate`，含数字 ID 重试）。
- 上游：analyticsService.ts L513（数据分析页）；annualReportService.ts L1480/1482/1513/1514（上下半年切分）；
  并作为 `getAnnualReportStats`（wcdbCore L3834）与 `getGroupStats`（L3895）缺符号时的降级实现。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| session_ids_json | `const char*` | **双形态**：若数组元素全部为纯数字字符串（`/^\d+$/`），TS 将其转换为 JSON **数字数组**发送（如 `[123456, 789012]`，L3762-3764）；否则为字符串数组（username）。DLL 必须两种都接受。 |
| begin_timestamp | `int32` | 秒，`normalizeTimestamp`；0 = 不限。 |
| end_timestamp | `int32` | 秒；`end <= 0` → 现在；`begin > 0 && end < begin` → `end = begin`（L3753-3760，等效 normalizeRange）。 |

**数字 ID 重试协议**（L3781-3798）：首次调用成功但 `data.total === 0 && data.idMap` 存在时，
TS 用 idMap 反查（username → 数字 ID）构造**纯数字数组**重试一次并采用重试结果。
含义：当以 username 查询命中 0 条时，DLL 应通过 `idMap` 暴露 username → 内部数字 ID 的映射，
使 TS 能以数字 ID 重试（微信数据库以数字 ID 关联会话时该路径必须工作）。

**状态码与错误**
- `result !== 0 || out_json == NULL` → `获取聚合统计失败: ${result}`。无符号缺失降级（必需符号）。

**返回 JSON 结构**（透传；字段全部 **camelCase**，由 analyticsService L717-747 / L776 / L800-803 / L836-853
消费，并与 JS 降级实现 `computeAggregateByCursor` L349-423 的输出逐字段对齐——降级实现即协议镜像）：
```jsonc
{
  "total": 1000,            // number，范围内消息总数
  "sent": 400,              // number，本人发送数
  "received": 600,          // number，接收数
  "firstTime": 1600000000,  // number，首条消息秒级时间戳
  "lastTime": 1700000000,   // number，末条消息秒级时间戳

  "typeCounts": {           // Record<string, number>：local_type（十进制字符串）→ 条数
    "1": 500,               //   文本
    "244813135921": 20,     //   新版文本类型，TS 与 "1" 同等计入文本（analyticsService L723）
    "3": 120,               //   图片
    "34": 30,               //   语音
    "43": 10,               //   视频
    "47": 80                //   表情
  },

  "hourly": [/* number[24]，下标 = 小时 0-23 */],

  "weekday": {              // Record<string, number>：键为 SQLite strftime('%w')，"0"=周日 … "6"=周六
    "0": 100, "1": 200      //   TS 将 "0" 映射为 7（周一=1 … 周日=7，analyticsService L836-843）
  },

  "daily":     { "2023-05-01": 12 },   // Record<"YYYY-MM-DD", number>
  "sentDaily": { "2023-05-01": 5 },    // Record<"YYYY-MM-DD", number>，仅本人发送
  "monthly":   { "2023-05": 300 },     // Record<"YYYY-MM", number>

  "sessions": {                        // Record<sessionId, SessionStat>：逐会话拆分
    "wxid_abc": {
      "total": 500,                    // number
      "sent": 200,                     // number
      "received": 300,                 // number
      "lastTime": 1700000000           // number，秒
      // 注意：不含 monthly（annual report 才要求逐会话 monthly）
    }
  },

  "idMap": { "123456": "wxid_abc" }    // Record<数字内部ID字符串, username>：
                                       //   sessions 键为数字时，TS 用它反查 username
                                       //   （normalizeAggregateSessions，analyticsService L604-620：
                                       //    仅当 sessions 键"全部"为纯数字时才做重映射）
}
```

**备注**
- `sessions` 键空间：可以是 username 或数字内部 ID；若用数字 ID，**必须**同时给出可反查的 `idMap`，
  且当以 username 查询得到 total=0 时 idMap 是重试协议的载体（见上）。
- `monthly` 键数（`Object.keys(d.monthly).length`）被 analyticsService L730 用作「活跃月份数」，
  必须只含有消息的月份。
- 所有字段在 JS 降级实现中均无条件输出（含空对象/0 值）；Rust 实现应同样**始终输出全部字段**，
  消费端对缺失字段虽有个别 `|| 0` 容错，但 `d.sessions`、`d.typeCounts` 缺失会导致上层逻辑退化。

---

### wcdb_get_available_years

**签名**（绑定 L1023，可选符号）
```c
int32_t wcdb_get_available_years(wcdb_handle handle, const char* session_ids_json, char** out_json);
```

**调用点**
- wcdbCore.ts：`getAvailableYears` L3806-3827，原生调用 L3816。
- 上游：annualReportService.ts L726（与 **5 秒超时** race，超时/失败降级为表扫描 + 边界游标探测，L628-849）；
  main.ts L5024 / L5119（IPC）。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| session_ids_json | `const char*` | JSON 字符串数组（username）。**空数组时 TS 直接返回 `[]`，不调原生**（L3813）。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `获取年度列表失败: ${result}`。
- 符号缺失 → `未支持获取年度列表` → 上层自行扫描（因此本函数是纯性能优化项，但**必须能在 5 秒内返回**，
  否则等同失败）。

**返回 JSON 结构**
```jsonc
[2021, 2022, 2023, 2024]   // number[]：这些会话中存在消息的年份（本地时区），升序与否未强制
                           //（TS 纯透传 JSON.parse；annualReportService 期望 Array<number> 并自行排序/去重）
```

**备注**
- 语义 = 「所选会话集合的消息 create_time 覆盖到的年份并集」。

---

### wcdb_get_annual_report_stats

**签名**（绑定 L1030，可选符号）
```c
int32_t wcdb_get_annual_report_stats(wcdb_handle handle, const char* session_ids_json,
    int32_t begin_timestamp, int32_t end_timestamp, char** out_json);
```

**调用点**
- wcdbCore.ts：`getAnnualReportStats` L3829-3850，原生调用 L3839（`normalizeRange`）。
- 上游：annualReportService.ts L903（年度报告主数据），消费 L903-939、L1480-1544。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| session_ids_json | `const char*` | JSON 字符串数组（username；本函数 TS **不做**数字 ID 转换/重试）。 |
| begin_timestamp | `int32` | 秒，normalizeRange（通常为某年 1/1 00:00:00）。 |
| end_timestamp | `int32` | 秒（通常为某年 12/31 23:59:59）。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `获取年度统计失败: ${result}`。
- 符号缺失 → **降级为 `wcdb_get_aggregate_stats(sessionIds, begin, end)`**（L3833-3835）。
  降级结果的 `sessions[sid]` 无 `monthly`，上层对两种形态分别兼容（L1538-1544 对 monthly 键做 parseInt 兼容）。

**返回 JSON 结构**（透传；camelCase；消费端 annualReportService L903-939）
```jsonc
{
  "total": 5000,                 // number，年度消息总数
  "sessions": {                  // Record<sessionId, AnnualSessionStat>
    "wxid_abc": {
      "sent": 2000,              // number，本人对该会话发送数
      "received": 3000,          // number，接收数
      "monthly": {               // Record<string, number>：月份 → 条数
        "1": 120,                //   键为不带前导零的月份数字字符串 "1".."12"
        "12": 300                //   TS 用 parseInt 读取（L928-935）
      }
    }
  },
  "daily": {                     // Record<"YYYY-MM-DD", number>：全年逐日消息数
    "2023-05-01": 120            //   TS 取最大值日为 peakDay：
                                 //   peakDayBegin = 该日 00:00:00 秒级时间戳（本地时区，
                                 //   由 new Date(`${key}T00:00:00`) 构造），peakDayEnd = begin + 86399，
                                 //   随后作为 wcdb_get_annual_report_extras 的入参
  }
}
```
- 降级路径（aggregate stats）额外含 firstTime/lastTime/typeCounts/hourly/weekday/sentDaily/monthly/idMap，
  上层兼容读取；原生实现**只需保证上表字段**。

**备注**
- `sessions` 键应为请求中的 username（上层直接用请求 ID 索引 `data.sessions[sid]`，L1480-1514 切分统计；
  未见对本函数结果的 idMap 数字重映射——若 DLL 返回数字键将导致逐会话数据丢失，**务必用 username 作键**）。

---

### wcdb_get_annual_report_extras

**签名**（绑定 L1037，可选符号）
```c
int32_t wcdb_get_annual_report_extras(wcdb_handle handle, const char* session_ids_json,
    int32_t begin_timestamp, int32_t end_timestamp, int32_t peak_day_begin, int32_t peak_day_end,
    char** out_json);
```

**调用点**
- wcdbCore.ts：`getAnnualReportExtras` L3852-3888，原生调用 L3869（begin/end 经 `normalizeRange`，
  peakBegin/peakEnd 经 `normalizeTimestamp`）。
- 上游：annualReportService.ts L957（消费 L942 类型声明、L957-1022、response 循环 L1401-1428）。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| session_ids_json | `const char*` | JSON 字符串数组。**空数组时 TS 直接返回 `{}`，不调原生**（L3865）。 |
| begin_timestamp / end_timestamp | `int32` | 秒，年度范围（与 annual stats 相同）。 |
| peak_day_begin | `int32` | 秒，峰值日窗口起点（由 annual stats 的 daily 推导出，当日 00:00:00）。 |
| peak_day_end | `int32` | 秒，峰值日窗口终点（= peak_day_begin + 86399）。窗口内统计逐会话消息数（peakDay 字段）。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `获取年度扩展统计失败: ${result}`。
- 符号缺失 → `未支持年度扩展统计`，上层用游标全量扫描自算（L1037-1170，batchSize 1000）。

**返回 JSON 结构**（透传；camelCase；消费端 annualReportService）
```jsonc
{
  "heatmap": [                    // number[7][24]，必须恰好 7 行（TS 校验 length === 7，否则整体弃用）
    [0, 1, /* ...24 项... */],    //   行 = 星期，**周一 = 下标 0** … 周日 = 下标 6
    /* ...共 7 行... */            //   列 = 小时 0-23；值 = 消息数
                                  //   注意与 aggregate 的 weekday（%w，周日=0）不同！
  ],
  "midnight": {                   // Record<sessionId, number>：深夜时段消息数（时段定义未确定，DLL 自定）
    "wxid_abc": 15
  },
  "conversation": {               // Record<sessionId, {initiated, received}>：对话发起统计
    "wxid_abc": {
      "initiated": 30,            // number，本人发起的对话轮次数
      "received": 20              // number，对方发起的轮次数
    }
  },
  "response": {                   // Record<sessionId, ResponseStat>：回复速度（单位：秒）
    "wxid_abc": {
      "avg": 123.4,               // number，平均回复时长；TS 循环 L1407-1417 读取（<=0 跳过）
      "count": 50,                // number，参与统计的回复对数；avg*count 加权汇总
      "fastest": 3                // number（可选），最快回复；TS 主循环未读取，仅类型声明含（L942），
                                  //   "最快好友"由最小 avg 推导（L1413-1416）
    }
  },
  "peakDay": {                    // Record<sessionId, number>：峰值日窗口内各会话消息数
    "wxid_abc": 200
  },
  "topPhrases": [                 // Array<{phrase, count}>：年度高频短语
    { "phrase": "哈哈哈", "count": 300 }
  ],
  "streak": {                     // 最长连续聊天纪录（对象可整体缺省；字段皆可选）
    "sessionId": "wxid_abc",      // string
    "days": 120,                  // number，连续天数
    "startDate": "2023-03-01",    // string，"YYYY-MM-DD"
    "endDate": "2023-06-28"       // string，"YYYY-MM-DD"
  }
}
```

**备注**
- heatmap 的星期索引（周一=0）与 midnight 的「深夜」定义、conversation 的「发起」判定阈值均由 DLL 决定，
  TS 无从校验（未确定项，但消费安全）。
- 所有 Record 键均应为请求中的 username（上层按会话 ID 直接索引并经 contactInfoMap 解析显示名）。

---

### wcdb_get_dual_report_stats

**签名**（绑定 L1044，可选符号）
```c
int32_t wcdb_get_dual_report_stats(wcdb_handle handle, const char* session_id,
    int32_t begin_timestamp, int32_t end_timestamp, char** out_json);
```

**调用点**
- wcdbCore.ts：`getDualReportStats` L5064-5085，原生调用 L5074（`normalizeRange`）。
- 上游：dualReportService.ts L650（消费 L655-794）；IPC 类型 `src/types/electron.d.ts` L1703-1716。

**入参**
| 参数 | 类型 | 说明 |
|---|---|---|
| session_id | `const char*` | **单个**会话 ID（双人报告 = 我 + 一个好友；TS 传 friendUsername）。 |
| begin_timestamp / end_timestamp | `int32` | 秒，normalizeRange。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `获取双人报告统计失败: ${result}`。
- 符号缺失 → `未支持双人报告统计`，上层游标降级自算（dualReportService L386-432，batchSize 500 升序）。

**返回 JSON 结构**（透传；camelCase 为主；消费端 dualReportService L655-794）
```jsonc
{
  "counts": {
    "total": 1000,     // number，总消息数
    "words": 50000,    // number，总字数（文本字符数）
    "image": 100,      // number，图片数
    "voice": 50,       // number，语音数
    "emoji": 300       // number，表情数
  },

  "emojis": [          // Array<object>：表情候选，TS 用 parseEmojiCandidate（L310-351）极宽容解析，
                       //   推荐直接输出规范键：{ count, md5, cdnUrl, isMe }
    {
      "count": 12,                    // number；TS 依次尝试键 ['count','cnt','times','total','num']
      "md5": "abc123...",             // string；TS 依次尝试 ['md5','emojiMd5','emoji_md5','emd5']，
                                      //   都缺省时从 content 中提取
      "cdnUrl": "https://...",        // string；TS 依次尝试 ['cdnUrl','cdnurl','emojiUrl','emoji_url',
                                      //   'url','thumbUrl','thumburl']
      "isMe": true                    // 归属标志；TS resolveEmojiOwner（L281-303）依次尝试布尔键
                                      //   ['isMe','is_me','isSent','is_sent','isSend','is_send','fromMe','from_me']，
                                      //   再尝试字符串键 ['side','sender','from','owner','role','direction']
                                      //   （值含 me/self/mine/out/outgoing/sent → 我方；
                                      //     friend/peer/other/in/incoming/received/recv → 对方），
                                      //   最后兜底解析 content 前缀 "1:"（我方）/ "0:"（对方）
    }
  ],

  "phrases":       [ { "phrase": "哈哈哈", "count": 100 } ],  // Array<{phrase: string, count: number}>：双方合计高频短语
  "myPhrases":     [ { "phrase": "好的",   "count": 60 } ],   // 我方高频短语
  "friendPhrases": [ { "phrase": "在吗",   "count": 40 } ],   // 对方高频短语

  "heatmap": [ [0, /* ...24 项... */] /* ...行数未强制（TS 直接透传给前端 number[][]）... */ ],
                       // number[][]：小时×星期活跃热力图（与 annual extras 的 7×24 同族；行列序未确定，透传）

  "initiative": { "initiated": 30, "received": 20 },   // {initiated: number, received: number}：对话发起
  "response":   { "avg": 100.5, "fastest": 2, "slowest": 3600, "count": 80 },
                       // 回复时长（秒）：avg 平均、fastest 最快、slowest 最慢（可选）、count 样本数
  "monthly":    { "2023-01": 200, "2023-02": 180 },    // Record<"YYYY-MM", number>：逐月消息数
  "streak":     { "days": 50, "startDate": "2023-03-01", "endDate": "2023-04-19" }
                       // 最长连续聊天：days number；日期 "YYYY-MM-DD"
}
```
（heatmap/initiative/response/monthly/streak 五者在 dualReportService L790-794 原样透传进
`DualReportData`，最终形态由 `src/types/electron.d.ts` L1703-1716 佐证。）

**备注**
- emojis 数组元素建议采用规范键 `{count, md5, cdnUrl, isMe}`（TS 首选键即这四个的 camelCase 形态）；
  其余别名仅为兼容旧 DLL。
- 各字段缺失时 TS 用 0/空数组兜底（counts 逐字段 `|| 0`），但 counts 与 phrases 族是报告主体，应完整输出。

---

### wcdb_get_my_footprint_stats

**签名**（绑定 L1065，可选符号）
```c
int32_t wcdb_get_my_footprint_stats(wcdb_handle handle, const char* options_json, char** out_json);
```

**调用点**
- wcdbCore.ts：`getMyFootprintStats` L3913-3970，payload 构造 L3946-3955，原生调用 L3958。
- 上游：chatService.ts L12251（「我的足迹」主路径）、L12485、L13786（零提及调试探针）；main.ts L4003（IPC）；
  归一化消费 `normalizeMyFootprintData` chatService.ts L13333-13414，接口声明 L298-389。

**入参**
options_json 字段（键名 snake_case，**但 `my_accountId` 是混合命名特例，必须原样接受**）：
| 字段 | 类型 | 说明 |
|---|---|---|
| begin | int32（秒） | `normalizeTimestamp(beginTimestamp \|\| 0)`；0 = 不限。 |
| end | int32（秒） | 同上。 |
| my_accountId | string | **本人账号 ID**（wxid），已 trim；用于判定收发方向与 @ 我。注意键名大小写：`my_accountId`。 |
| private_session_ids | string[] | 私聊会话 ID 列表，TS 已去重/trim/过滤空。chatService 侧对**群列表**分块：每次调用保证 options JSON ≤ ~900 字节，多次调用后合并（L12130-12458）；私聊列表同理会话数可能很大，DLL 应能处理数百个 ID。 |
| group_session_ids | string[] | 群聊会话 ID 列表（同上约束）。 |
| mention_limit | int ≥ 0 | @我 的候选条数上限；**0 = 不截断候选**（chatService 主路径传 0）。TS 已 floor 且钳制 ≥0。 |
| private_limit | int ≥ 0 | 私聊候选上限；0 语义同上（不截断）。 |
| mention_mode | string | 当前唯一取值 `'text_at_me'`（默认值，TS `options?.mentionMode \|\| 'text_at_me'`）。其它取值未出现；未知取值的行为未确定（建议按默认处理）。 |

**状态码与错误**
- `result !== 0 || out_json == NULL` → `获取我的足迹统计失败: ${result}`。
- 符号缺失 → `接口未就绪`，上层整体走 JS 游标重建（chatService L12511-12676，batchSize 360）。

**返回 JSON 结构**（透传；**全 snake_case**；字段名逐一由 chatService `normalizeMyFootprintData`
L13333-13414 与 `MyFootprint*` 接口 L298-389 确定）：
```jsonc
{
  "summary": {
    "private_inbound_people": 10,    // number，私聊中来找过我的人数
    "private_replied_people": 6,     // number，我回复过的人数
    "private_outbound_people": 8,    // number，我主动私聊的人数
    "private_reply_rate": 0.6,       // number，回复率（0-1 小数）
    "mention_count": 25,             // number，@我 的消息总数
    "mention_group_count": 5         // number，@我 出现过的群数
  },

  "private_sessions": [              // 每私聊会话一条
    {
      "session_id": "wxid_abc",      // string
      "incoming_count": 30,          // number，对方发我条数
      "outgoing_count": 20,          // number，我发对方条数
      "replied": true,               // bool/0|1，我是否回复过（TS Boolean 化）
      "first_incoming_ts": 1690000000, // number，秒，首条来消息时间
      "first_reply_ts": 1690000100,    // number，秒，我的首次回复时间（未回复为 0/缺省）
      "latest_ts": 1700000000,         // number，秒，最近一条消息时间
      "anchor_local_id": 123,          // number，锚点消息 local_id（跳转定位用）
      "anchor_create_time": 1690000000 // number，秒，锚点消息时间
    }
  ],

  "private_segments": [              // 私聊「对话段」列表
    {
      "session_id": "wxid_abc",      // string，必须非空（TS 过滤条件之一）
      "segment_index": 0,            // number，段序号
      "start_ts": 1690000000,        // number，秒，必须 > 0（TS 过滤：session_id 非空 && start_ts>0 才保留）
      "end_ts": 1690003600,          // number，秒
      "duration_sec": 3600,          // number，秒
      "incoming_count": 5,           // number
      "outgoing_count": 3,           // number
      "message_count": 8,            // number
      "replied": true,               // bool
      "first_incoming_ts": 1690000000,
      "first_reply_ts": 1690000100,
      "latest_ts": 1690003600,
      "anchor_local_id": 123,
      "anchor_create_time": 1690000000
      // displayName / avatarUrl 由 TS 侧补充，DLL 不需要输出
    }
  ],

  "mentions": [                      // @我 的消息列表
    {
      "session_id": "12345@chatroom",// string，所属群
      "local_id": 456,               // number，消息 local_id
      "create_time": 1690000000,     // number，秒
      "sender_username": "wxid_xyz", // string，@我的人
      "message_content": "@我 在吗", // string，消息内容（展示用，可为摘要）
      "source": "<msgsource>...</msgsource>"
                                     // string，消息行 source 字段原文（内含 @ 用户名列表 XML）。
                                     // TS 用 filterMyFootprintMentionsBySource（L13416 起）按 source
                                     // 二次过滤「确实 @ 到我」的条目——**必须输出 source**，否则上层过滤退化
    }
  ],

  "mention_groups": [                // 按群聚合的 @我 统计
    { "session_id": "12345@chatroom", "count": 5, "latest_ts": 1700000000 }
  ],

  "diagnostics": {
    "truncated": false,              // bool，结果整体是否被截断
    "scanned_dbs": 3,                // number，扫描的数据库数
    "elapsed_ms": 120,               // number，DLL 内部耗时
    "mention_truncated": false,      // bool，mention_limit 是否生效截断
    "private_truncated": false       // bool，private_limit 是否生效截断
  }
}
```

**备注**
- 多次分块调用的合并由 TS 负责；单次调用只需针对本次传入的会话列表统计。
- `private_segments`：TS 主路径经常忽略原生结果并用游标重建（L12511-12676），但接口契约上仍应输出
  （探针与其它路径会接受原生值）。段切分阈值（间隔多久算新段）**未确定**，DLL 自定。
- `summary.private_reply_rate` 为 0-1 小数（TS 展示层再乘 100）；分母为 0 时应输出 0。
- 全部时间戳为**秒级**（与消息行 create_time 一致）。

---

## 附：本组函数 × TS 包装器 × 降级行为速查

| DLL 函数 | 必需性 | wcdbCore 包装器（行号） | 符号缺失时行为 |
|---|---|---|---|
| wcdb_get_session_message_counts | 可选 | getSessionMessageCounts L2261 | 降级：逐会话 wcdb_get_message_count |
| wcdb_get_session_message_type_stats | 可选 | getSessionMessageTypeStats L2287 | 报错「接口未就绪」 |
| wcdb_get_session_message_type_stats_batch | 可选 | getSessionMessageTypeStatsBatch L2314 | 降级：逐会话单版 |
| wcdb_get_session_message_date_counts | 可选 | getSessionMessageDateCounts L2367 | 报错「接口未就绪」 |
| wcdb_get_session_message_date_counts_batch | 可选 | getSessionMessageDateCountsBatch L2389 | 降级：逐会话单版 |
| wcdb_open_message_cursor | **必需** | openMessageCursor L4001 | —（绑定失败即 DLL 不可用） |
| wcdb_fetch_message_batch | **必需** | fetchMessageBatch L4076 | — |
| wcdb_close_message_cursor | **必需** | closeMessageCursor L4096 | — |
| wcdb_set_message_cursor_projection | 可选 | openMessageCursor 内 L4059 | 保持 full 投影（静默） |
| wcdb_search_messages | 可选 | searchMessages L4707 | 报错「不支持搜索消息」 |
| wcdb_ai_query_session_candidates | 可选 | querySessionCandidates L4737 | 报错「不支持按发送者聚合搜索」 |
| wcdb_get_group_stats | 可选 | getGroupStats L3890 | 降级：getAggregateStats([chatroomId]) |
| wcdb_get_aggregate_stats | **必需** | getAggregateStats L3748 | — |
| wcdb_get_available_years | 可选 | getAvailableYears L3806 | 报错→上层自扫描（5s 超时同样降级） |
| wcdb_get_annual_report_stats | 可选 | getAnnualReportStats L3829 | 降级：getAggregateStats |
| wcdb_get_annual_report_extras | 可选 | getAnnualReportExtras L3852 | 报错→上层游标自算 |
| wcdb_get_dual_report_stats | 可选 | getDualReportStats L5064 | 报错→上层游标自算 |
| wcdb_get_my_footprint_stats | 可选 | getMyFootprintStats L3913 | 报错→上层游标重建 |
