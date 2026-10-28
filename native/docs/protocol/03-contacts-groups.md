# 03 - 联系人 / 群聊 / 头像 协议规格

> **来源与边界**：本文是**接口行为描述**（函数名、字段名、返回形状、错误路径），源自对 WeFlow 项目（Electron/TypeScript）`wcdbCore.ts` 等文件的阅读笔记；**不含其源代码**，也不声称与上游实现完全一致。第三方权利与许可以 WeFlow 原项目为准。


适用范围：`wcdb.dll` 中联系人、群聊、头像相关的 14 个导出函数。
唯一协议来源：`WeFlow-main/electron/services/wcdbCore.ts`（下文行号均指该文件，除非另注）。
全局约定（koffi 加载、`int32` 状态码 0=成功、`outJson` 为 DLL 分配的 NUL 结尾 UTF-8 C 字符串、
用后由调用方经 `wcdb_free_string` 释放，见 `decodeJsonPtr()`，wcdbCore.ts:1547-1557）
请参照 `01` 号文档，此处不再重复。

本组函数的补充约定（与 01 文档一致，但对实现细节有影响，特别强调）：

1. **本组所有函数均无 `outError` 出参**，失败时 TS 只把状态码拼进错误文本（如 `获取昵称失败: ${result}`）。
2. **成功时 `outJson` 必须写入非 NULL 指针**。TS 侧统一用 `result !== 0 || !outPtr[0]` 判定失败，
   即使结果为空也必须返回合法的 `"{}"` / `"[]"` 字符串，否则按失败处理（会触发降级路径）。
3. 绑定失败（符号缺失）时功能整体跳过并返回"接口未就绪"或走降级；
   `wcdb_get_contact` / `wcdb_get_display_names` / `wcdb_get_avatar_urls` /
   `wcdb_get_group_member_count` / `wcdb_get_group_members` 为**非可选绑定**（不在 try/catch 中，
   符号缺失会导致整个 `initialize()` 抛出），其余为可选绑定（try/catch 包裹，置 null）。
4. macOS（darwin）下 `getContact` / `getDisplayNames` / `getAvatarUrls` 不走 DLL，而是直接
   `execQuery('contact', ...)` SQL 查询；DLL 协议实际生效平台为 Windows/Linux。
   但 darwin SQL 路径揭示了字段语义优先级，本文在各函数"备注"中引用。
5. 所有 `usernamesJson` / `chatroomIdsJson` 入参均由 `JSON.stringify(string[])` 生成，
   即**纯字符串数组** `["wxid_a","wxid_b"]`，无对象元素。个别函数可能传 `NULL`（见
   `wcdb_get_contacts_compact`）或 `"[]"`（空数组，见 alias_map / friend_flags / contact_status）。

---

### wcdb_get_contact

**签名**

```c
int32 wcdb_get_contact(int64 handle, const char* username, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:980（非可选）。
- 调用：wcdbCore.ts:3453，包装方法 `getContact()`（3433-3482）。
- 失败降级：`code != 0 || outJson == NULL` 时记日志、`dumpDbStatus`，然后用
  `execQuery('contact', null, "SELECT * FROM contact WHERE username='...' LIMIT 1")` 兜底
  （3459-3473）；兜底也无行则报 `联系人不存在: ${username}`。

**入参**

- `username`：单个联系人 ID（个人 wxid、`gh_` 公众号、`xxx@chatroom` 群 ID 均可）。非 JSON。

**状态码与错误**

- `0` 且 `out_json` 非空 → 成功；其余 → 降级 SQL；两条路都失败才向上返回 error。

**返回 JSON 结构**（outJson = 单个联系人对象）

TS 侧 `getContact()` 将对象**原样透传**（`contact: any`），字段名由各消费方决定。
汇总所有消费方实际读取的键（同一语义的多个别名都会被接受，但**推荐键**是消费方第一优先读取的那个）：

```ts
{
  // —— 身份 ——
  "username": "wxid_xxx",        // 推荐。别名: user_name / userName（chatService.ts:8715, groupAnalyticsService.ts:1323-1324）
  "userName": "wxid_xxx",        // 可选别名（groupAnalyticsService.ts:1324 读 contact.userName || contact.user_name）
  "encryptUsername": "...",      // 可选。别名: encrypt_username（groupAnalyticsService.ts:1325, contactResolver.ts:43）
  "encryptUserName": "...",      // 可选（groupAnalyticsService.ts:1326, contactResolver.ts:44）
  "alias": "weixin_id",          // 推荐。别名: Alias（chatService.ts:8702, snsService.ts:454）
  "remark": "备注名",             // 推荐。别名: Remark（chatService.ts:8717, snsService.ts:455）
  "nickName": "昵称",             // 推荐 camelCase！导出格式化器只读 nickName：
                                 //   TxtFormatter.ts:347 / ExcelFormatter.ts:418 / JsonFormatter.ts:297 /
                                 //   WeCloneFormatter.ts:275 / MarkdownFormatter.ts:258
                                 // 别名: nick_name / nickname / NickName（chatService.ts:8719, snsService.ts:456,
                                 //   contactResolver.ts:34, groupAnalyticsService.ts:1321 作为兜底）

  // —— 分类判定（ExportContext.ts:1456-1462 resolveSessionFilePrefix 读取）——
  "local_type": 1,               // 整数。别名: localType / WCDB_CT_local_type
  "flag": 0,                     // 整数。别名: contact_flag / contactFlag / WCDB_CT_flag
  "quan_pin": "QuanPin",         // 字符串。别名: quanPin / WCDB_CT_quan_pin

  // —— 群主探测（可选，见备注）——
  // 任意键名包含 owner / host / creator（不区分大小写）的字段会被
  // groupAnalyticsService.extractOwnerUsername（groupAnalyticsService.ts:162-204）扫描：
  //   - boolean true  → 用同对象里的 username 当群主
  //   - string        → 直接与群成员 username 匹配（或按 [,\s;|] 切分后匹配，或 <4096 长度时按 JSON 解析递归）
}
```

**备注**

- 消费方汇总：chatService.ts:8695-8724（`getContact` 归一化为 `{username, alias, remark, nickName}`）、
  snsService.ts:451-457、groupAnalyticsService.ts:231/1316/1842/1875、
  export/core/ExportContext.ts:1480（会话文件名前缀判定）、export/contacts/contactResolver.ts:34-44、
  export/formatters/*（直接读 `contact.nickName` / `.remark` / `.alias`）。
- **`nickName` 用 camelCase 是硬要求**（格式化器无 snake_case 兜底）；`local_type`/`flag`/`quan_pin`
  用 snake_case 是首选（三个别名都被接受，但与 `wcdb_get_contacts_compact` 保持一致最稳妥）。
- 对 `xxx@chatroom` 传入时返回群"联系人"行；若含 owner 类字段可帮助群主识别（未确定 DLL 是否必须提供，
  TS 有 ext_buffer 与成员列表两条兜底路径）。

---

### wcdb_get_contact_status

**签名**

```c
int32 wcdb_get_contact_status(int64 handle, const char* usernames_json, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:984（可选，try/catch）。
- 调用：wcdbCore.ts:3493，包装方法 `getContactStatus()`（3484-3513）。符号缺失 → `{success:false, error:'接口未就绪'}`。
- 消费方：chatService.ts:1982（`getSessionStatuses`，写入 sessionStatusCache）。

**入参**

- `usernames_json`：`JSON.stringify(usernames || [])`，字符串数组，可能为 `"[]"`。
  元素为会话/联系人 ID（含 `@chatroom`、折叠群等）。

**状态码与错误**

- `code !== 0 || !outPtr[0]` → `获取会话状态失败: ${code}`；`decodeJsonPtr` 返回空 → `解析会话状态失败`。

**返回 JSON 结构**

```jsonc
// 顶层：对象，键 = 入参 username，值 = 状态对象
{
  "wxid_a":        { "isFolded": false, "isMuted": true  },
  "12345@chatroom":{ "isFolded": true,  "isMuted": false }
}
```

- 字段名**只接受 camelCase** `isFolded` / `isMuted`（wcdbCore.ts:3504-3507 直接
  `Boolean(state.isFolded)` / `Boolean(state.isMuted)`，无 snake_case 兜底）。
- 某个 username 缺失或值为 null → TS 按 `{}` 处理，两标志均落为 `false`，不算错误。
- 语义：`isFolded` = 会话被折叠（如折叠的群聊）；`isMuted` = 消息免打扰。

**备注**

- 数据源对应微信 contact/session 表中的折叠与免打扰标志位；具体位运算在 DLL 内完成，TS 只要布尔。

---

### wcdb_get_contact_type_counts

**签名**

```c
int32 wcdb_get_contact_type_counts(int64 handle, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:990（可选）。
- 调用：wcdbCore.ts:3652，包装方法 `getContactTypeCounts()`（3637-3670）。
- 降级：符号缺失 / `code!=0` / 解码为空 / 异常时，调用 `getContactsCompact()` 全量拉取并用
  `deriveContactTypeCounts()`（wcdbCore.ts:762-803）在 TS 侧本地分类。
- 消费方：chatService.ts:2223-2248 → 前端 `contactTypeCountsStore.ts:126`。

**入参**

- 无（仅 handle）。

**状态码与错误**

- 非 0 不直接报错，而是走上述降级路径；降级也失败才返回 error。

**返回 JSON 结构**

```jsonc
{
  "private": 123,        // 私聊好友数
  "group": 45,           // 群聊数（username 以 @chatroom 结尾）
  "official": 67,        // 公众号数（gh_ 前缀且 local_type==1，排除内置助手 gh_f0a92aa7146c）
  "former_friend": 8,    // 曾经的好友（注意：snake_case 键！）
  "blocked": 9           // 黑名单（非 gh_ 且 flag 位 0x8 置位）；缺失时 TS 记 0（类型上为可选字段）
}
```

- 五个键在 wcdbCore.ts:3660-3664 逐一以 `Number(raw.<key> || 0)` 读取，**键名必须精确匹配**
  （`private`/`group`/`official`/`former_friend`/`blocked`），值为 JSON number。

**备注（TS 降级分类规则，Rust 实现应与之对齐，wcdbCore.ts:773-799）**

按顺序判定，每行只计一类：
1. `username` 以 `@chatroom` 结尾 → `group`；
2. `gh_` 前缀且 `local_type==1` 且不在内置助手表 `{gh_f0a92aa7146c}` → `official`；
3. 非 `gh_` 且 `(flag & 8) == 8` → `blocked`；
4. 非 `gh_` 且（`local_type==0 && quan_pin非空`）或（`local_type==3 && flag!=4 && (quan_pin||alias||remark 任一非空)`）→ `former_friend`；
5. `local_type==1` 且 `!(flag & 8)` 且不在排除表 `{medianote, floatbottle, qmessage, qqmail, fmessage}` → `private`。
字段别名接受：`local_type|localType|WCDB_CT_local_type`、`flag|contact_flag|contactFlag|WCDB_CT_flag`、
`quan_pin|quanPin|WCDB_CT_quan_pin`、`alias|WCDB_CT_alias`、`remark|WCDB_CT_remark`。

---

### wcdb_get_contacts_compact

**签名**

```c
int32 wcdb_get_contacts_compact(int64 handle, const char* usernames_json /* 可为 NULL */, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:995（可选）。
- 调用：wcdbCore.ts:3688，包装方法 `getContactsCompact(usernames = [])`（3672-3697）。
- 降级：符号缺失 / `code!=0` / 解码空 / 异常 → `execQuery('contact', null, buildContactSelectSql(usernames))`
  即 `SELECT * FROM contact [WHERE username IN (...)]`（wcdbCore.ts:755-760）。
- 消费方：chatService.ts:1565/1710/2730/4953/8636、groupAnalyticsService.ts:630、
  wcdbCore.ts:3640（type_counts 降级）。

**入参**

- `usernames_json`：**两种取值**（wcdbCore.ts:3687）：
  - 非空数组 → `JSON.stringify(usernames)`，字符串数组；
  - 空数组/未传 → **`NULL` 指针**，语义为"返回全部联系人"。Rust 侧必须接受 NULL。

**状态码与错误**

- 非 0 → 走 SQL 降级（不直接失败）。

**返回 JSON 结构**（outJson = 联系人行对象数组）

TS 原样透传（`contacts: any[]`，仅要求是数组，非数组按 `[]` 处理）。消费方读取的字段：

```ts
[
  {
    // —— 必需 ——
    "username": "wxid_xxx",     // chatService.ts:2786/1569/4960 直接读 row.username；
                                // wcdbCore.ts:774 / chatService.ts:4096 兜底 user_name / userName
    "nick_name": "昵称",         // chatService.ts:2826/2837 读 row.nick_name（snake_case 优先！）；
                                // chatService.ts:1571 亦接受 nickName
    "local_type": 1,            // 整数，别名 localType / WCDB_CT_local_type（chatService.ts:2791, wcdbCore.ts:777）
    "flag": 0,                  // 整数，别名 contact_flag / contactFlag / WCDB_CT_flag（chatService.ts:2792, wcdbCore.ts:779）
    "quan_pin": "QuanPin",      // 别名 quanPin / WCDB_CT_quan_pin（chatService.ts:2793, wcdbCore.ts:781）
    "alias": "weixin_id",       // 别名 WCDB_CT_alias（chatService.ts:2794）
    "remark": "备注",            // 别名 WCDB_CT_remark（chatService.ts:2795）

    // —— 可选扩展字段（chatService.ts:526-532 contactExtendedFieldCandidates，缺失时 TS 会
    //    用 PRAGMA table_info(contact) + SELECT 补偿查询合并，见 chatService.ts:4028-4109）——
    "label_list": "1,2",        // 或 labelList/labels/label_names/labelNames/tags/tag_list/tagList；
                                // 数组或分隔字符串均可（chatService.ts:4333-4367）
    "description": "...",       // 或 desc/contact_description/contactDescription/detail_description/detailDescription
    "signature": "...",         // 或 sign/personal_signature/personalSignature；缺失时从 extra_buffer protobuf field 4 提取
    "country": "CN", "province": "广东", "city": "深圳",   // 或 region/location/area；
                                // 缺失时从 extra_buffer field 5/6/7 提取（chatService.ts:4597-4650）
    "extra_buffer": "0a12..."   // 或 extraBuffer。联系人扩展 protobuf 的 hex 字符串（推荐）；
                                // chatService.ts:4221-4243 也接受字节数组；field 30=标签ID、4=签名、5/6/7=国省市
  }
]
```

**备注**

- 与 `wcdb_get_contact` 的键风格差异是**实测行为**：compact 的主消费路径（chatService.getContacts）
  读 `row.nick_name`（snake），而 get_contact 的格式化器消费路径读 `contact.nickName`（camel）。
  实现时建议两个键都输出，或分别遵循上述推荐键。
- `hydrateContactsWithExtendedFields`（chatService.ts:4028）注释明确：
  "wcdb_get_contacts_compact 可能只给部分联系人返回 extra_buffer"——若任何一行缺可解析的
  `extra_buffer`，TS 会发起补偿 SQL 查询，性能变差但功能不坏。**建议每行都带 extra_buffer（hex）**。

---

### wcdb_get_contact_alias_map

**签名**

```c
int32 wcdb_get_contact_alias_map(int64 handle, const char* usernames_json, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:1000（可选）。
- 调用：wcdbCore.ts:3704，包装方法 `getContactAliasMap()`（3699-3713）。符号缺失 → `接口未就绪`。
- 消费方：analyticsService.ts:86-97（过滤空值）、chatService.ts:8706（get_contact 未返回 alias 时兜底）。

**入参**

- `usernames_json`：`JSON.stringify(usernames || [])`，字符串数组（可能为 `"[]"`）。

**状态码与错误**

- `code !== 0 || !outPtr[0]` → `获取联系人 alias 失败: ${code}`；解码空 → `解析联系人 alias 失败`。

**返回 JSON 结构**

```jsonc
// 顶层：扁平对象，键 = username，值 = 微信号 alias 字符串
{
  "wxid_a": "my_weixin_id",
  "wxid_b": ""                 // 空串合法，消费方会过滤
}
```

**备注**

- 值必须是 JSON string；chatService.ts:8707-8708 以 `aliasResult.map[username]` 直读并 `String()`。

---

### wcdb_get_contact_friend_flags

**签名**

```c
int32 wcdb_get_contact_friend_flags(int64 handle, const char* usernames_json, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:1005（可选）。
- 调用：wcdbCore.ts:3720，包装方法 `getContactFriendFlags()`（3715-3729）。符号缺失 → `接口未就绪`。
- 消费方：export/core/ExportContext.ts:5198-5219（`queryFriendFlagMap`）。

**入参**

- `usernames_json`：`JSON.stringify(usernames || [])`，字符串数组（已去重去空）。

**状态码与错误**

- `code !== 0 || !outPtr[0]` → `获取联系人好友标记失败: ${code}`；解码空 → `解析联系人好友标记失败`。

**返回 JSON 结构**

```jsonc
// 顶层：扁平对象，键 = username，值 = 是否好友（boolean）
{
  "wxid_a": true,
  "wxid_b": false
}
```

**备注**

- 消费方 `Boolean(isFriend)` 强转，truthy/falsy 值也可，但**应输出 JSON true/false**。
- 结果中缺失的 username，消费方默认置 `false`（ExportContext.ts:5213-5217）。

---

### wcdb_get_chat_room_ext_buffer

**签名**

```c
int32 wcdb_get_chat_room_ext_buffer(int64 handle, const char* chatroom_id, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:1010（可选）。
- 调用：wcdbCore.ts:3736，包装方法 `getChatRoomExtBuffer()`（3731-3746）。符号缺失 → `接口未就绪`。
- 消费方：groupAnalyticsService.ts:240-248（`detectGroupOwnerUsername` 的最后一条兜底路径）。

**入参**

- `chatroom_id`：单个群 ID（`xxx@chatroom`）。非 JSON。

**状态码与错误**

- `code !== 0 || !outPtr[0]` → `获取群聊 ext_buffer 失败: ${code}`；解码空 → `解析群聊 ext_buffer 失败`。

**返回 JSON 结构**

```jsonc
{
  "ext_buffer": "..."   // snake_case 键，必须精确为 ext_buffer（wcdbCore.ts:3741 String(data.ext_buffer || '').trim()）
}
```

- 空串/缺失 → 包装方法返回 `extBuffer: undefined`（success 仍为 true）。

**备注**

- `ext_buffer` 的**值编码未在 TS 中确定**（未确定：hex 的 protobuf 原文还是可读文本）。
  TS 只做 `String().trim()` 后交给 `extractOwnerUsername({ ext_buffer })`
  （groupAnalyticsService.ts:132-204）做群主启发式识别：
  - 若字符串以 `{` 或 `[` 开头且长度 < 4096 → 按 JSON 解析，递归找键名含 `owner`/`host`/`creator` 的项；
  - 否则整串或按 `[,\s;|]+` 切分后的片段，与群成员 username（含去后缀变体）精确匹配。
- 因此对 WeFlow 最有用的输出是：**能从中定位群主 username 的字符串**（例如 JSON
  `{"owner":"wxid_xxx"}`，或含群主 username 的分隔文本）。若返回 hex 的 protobuf 原文，
  此路径基本不会命中（仅当 hex 串恰好等于某 username，实际不可能），但不会导致错误。

---

### wcdb_get_display_names

**签名**

```c
int32 wcdb_get_display_names(int64 handle, const char* usernames_json, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:948（非可选）。
- 调用：wcdbCore.ts:3123，包装方法 `getDisplayNames()`（3063-3164）；调用前已去重、trim、
  并剔除本地 TTL 缓存命中的项（只把缓存未命中的 `toFetch` 发给 DLL）。
- 消费方（均为 `map[username]` 直读）：chatService.ts:1433/2078、httpService.ts:1124/1238/1975/2131、
  groupAnalyticsService.ts:1091/1146/1303/1416/1680/1837/1862、analyticsService.ts:667/779、
  annualReportService.ts:1215/1250/1564、dualReportService.ts:450、export/core/ExportContext.ts:1431/4916/5099、
  wcdbCore.ts:3022（会话列表内部补名）。

**入参**

- `usernames_json`：`JSON.stringify(toFetch)`，字符串数组，保证非空（空数组在调用前已短路返回）。

**状态码与错误**

- `result !== 0 || !outPtr[0]`：若已有部分缓存结果则 `success:true` + error 文本
  `获取昵称失败: ${result}`；否则 `success:false`。解码空 → `解析昵称失败`。
- 失败/缺失项一律回退为 username 本身（3155-3159），不会抛错。

**返回 JSON 结构**

```jsonc
// 顶层：扁平对象，键 = 请求中的 username（须原样回显，区分大小写），值 = 显示名字符串
{
  "wxid_a": "备注或昵称",
  "12345@chatroom": "群名称"
}
```

- TS 读取逻辑（3143-3153）：`map[username]` 为 string → 直接使用；null/undefined → 跳过（回退 username）；
  其他类型 → `String(v).trim()`；trim 后为空 → 回退 username。

**备注**

- 显示名语义优先级（darwin SQL 路径 wcdbCore.ts:3101-3105 揭示）：
  `remark` > `nick_name` > `alias` > `username`。Windows DLL 应实现同样优先级。
- 命中的结果会写入 TS 本地 TTL 缓存（displayNameCache），DLL 无需缓存。

---

### wcdb_get_avatar_urls

**签名**

```c
int32 wcdb_get_avatar_urls(int64 handle, const char* usernames_json, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:951（非可选）。
- 调用：wcdbCore.ts:3234，包装方法 `getAvatarUrls()`（3166-3264）；入参同样先经去重 + 本地 TTL 缓存过滤。
- 消费方：chatService.ts:2082/8759/8902、httpService.ts:1999/2168、groupAnalyticsService.ts:1092/1417、
  analyticsService.ts:668/780、annualReportService.ts:1216/1251/1262/1565、dualReportService.ts:538、
  export/core/ExportContext.ts:1432/4917/5100。

**入参**

- `usernames_json`：`JSON.stringify(toFetch)`，字符串数组，非空。

**状态码与错误**

- `result !== 0 || !outPtr[0]`：有部分缓存则 success+error `获取头像失败: ${result}`，否则失败。
- 解码空 → `解析头像失败`（直接失败）。

**返回 JSON 结构**

```jsonc
// 顶层：扁平对象，键 = username，值 = 头像 URL 字符串
{
  "wxid_a": "https://wx.qlogo.cn/mmhead/xxxx/132",
  "wxid_b": ""                 // 空串合法：TS 跳过且不缓存，下次重试（3250-3258）
}
```

- TS 以 `map[username]` 直读，`url.trim()` 非空才采用/缓存。

**备注**

- 值应为 **http(s) URL**。chatService.isValidAvatarUrl（chatService.ts:609-616）会拒收
  以 `ffd8`（JPEG hex 魔数）开头或含 `base64,ffd8` 的值——**不要把图片字节/hex/base64 塞进本接口**，
  二进制头像走 `wcdb_get_head_image_buffers`。
- URL 优先级（darwin SQL 路径 wcdbCore.ts:3207-3212 揭示）：
  `big_head_img_url` > `small_head_img_url` > `head_img_url` > `avatar_url`（及各 camel 变体），即优先大图。

---

### wcdb_get_head_image_buffers

**签名**

```c
int32 wcdb_get_head_image_buffers(int64 handle, const char* usernames_json, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:1187（可选）。
- 调用：wcdbCore.ts:4354，包装方法 `getHeadImageBuffers()`（4349-4363）。符号缺失 → `接口未就绪`。
- 消费方：chatService.ts:2156-2196（`getAvatarsFromHeadImageDb`），每批最多 **320** 个 username。

**入参**

- `usernames_json`：`JSON.stringify(usernames || [])`，字符串数组（已去重去空）。

**状态码与错误**

- `result !== 0 || !outPtr[0]` → `获取头像二进制失败: ${result}`；解码空 → `解析头像二进制失败`。
- 单个 username 解码失败仅被静默跳过（chatService.ts:2186-2188），不影响整批。

**返回 JSON 结构**

```jsonc
// 顶层：扁平对象，键 = username，值 = 头像图片原始字节的 hex 编码字符串（小写/大写均可）
{
  "wxid_a": "ffd8ffe000104a46...",   // JPEG 字节流的 hex
  "wxid_b": ""                        // 空串 → 跳过
}
```

- **二进制编码方式：hex（不是 base64）**。证据：chatService.ts:2178-2184
  `Buffer.from(hex, 'hex').toString('base64')` 再拼成 `data:image/jpeg;base64,...`。
- 数据来源为微信 `head_image.db` 中的头像 blob（注释 chatService.ts:2154）。

---

### wcdb_get_group_member_count

**签名**

```c
int32 wcdb_get_group_member_count(int64 handle, const char* chatroom_id, int32_t* out_count);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:954（非可选）。
- 调用：wcdbCore.ts:3272，包装方法 `getGroupMemberCount()`（3266-3280）。

**入参**

- `chatroom_id`：单个群 ID。非 JSON。

**状态码与错误**

- `result !== 0` → `获取群成员数量失败: ${result}`；成功时读 `out_count[0]`。

**返回 JSON 结构**

- 无 JSON 输出。成功时向 `out_count` 写入群成员数（int32，含自己；应 ≥ 0）。

**备注**

- 本函数是 `wcdb_get_group_member_counts` 缺失时的逐个降级路径（wcdbCore.ts:3287-3296），必须实现。

---

### wcdb_get_group_member_counts

**签名**

```c
int32 wcdb_get_group_member_counts(int64 handle, const char* chatroom_ids_json, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:958（可选）。
- 调用：wcdbCore.ts:3299，包装方法 `getGroupMemberCounts()`（3282-3310）；符号缺失 → 循环调用单个版。
- 消费方：chatService.ts:6029/6088（导出统计）、groupAnalyticsService.ts:1083/1110（群排行）。

**入参**

- `chatroom_ids_json`：`JSON.stringify(chatroomIds)`，字符串数组（`@chatroom` 结尾），非空
  （空数组在调用前短路）。

**状态码与错误**

- `result !== 0 || !outPtr[0]` → `获取群成员数量失败: ${result}`；解码空 → `解析群成员数量失败`。

**返回 JSON 结构**

```jsonc
// 顶层：扁平对象，键 = chatroomId（与入参一致），值 = 成员数（必须是 JSON number）
{
  "11111@chatroom": 385,
  "22222@chatroom": 12
}
```

- 消费方以 `typeof map[id] === 'number'` 校验（groupAnalyticsService.ts:1110、chatService.ts:6031），
  **字符串数字会被当作 0**。缺失键按 0 处理，不算错误。

---

### wcdb_get_group_members

**签名**

```c
int32 wcdb_get_group_members(int64 handle, const char* chatroom_id, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:964（非可选）。
- 调用：wcdbCore.ts:3318，包装方法 `getGroupMembers()`（3312-3329），数组原样透传。
- 消费方：groupAnalyticsService.ts:1128/1290/1847（群成员面板、导出）、chatService.ts:5941（共同好友统计）、
  export/core/ExportContext.ts:5075（ChatLab 成员合并）、main.ts:4912（IPC 透出）。

**入参**

- `chatroom_id`：单个群 ID。非 JSON。

**状态码与错误**

- `result !== 0 || !outPtr[0]` → `获取群成员失败: ${result}`；解码空 → `解析群成员失败`。
- 注意：空群返回 `"[]"` 是合法的（消费方按 `members.length === 0` 处理）。

**返回 JSON 结构**（outJson = 成员对象数组）

```ts
[
  {
    "username": "wxid_member1",   // 必需。chatService.ts:4936-4949 extractGroupMemberUsername 亦接受
                                  //   userName / user_name / encryptUsername / encryptUserName /
                                  //   encrypt_username / originalName 作为兜底；
                                  //   元素甚至允许是纯字符串（typeof member === 'string'）
    "avatarUrl": "https://...",   // 可选，原样透传到 UI（groupAnalyticsService.ts:1205/1376,
                                  //   ExportContext.ts:5112 作为最低优先级头像来源）
    "originalName": "wxid_old",   // 可选，历史/原始 ID，用于跨表匹配与群昵称候选
                                  //   （groupAnalyticsService.ts:1162/1178, ExportContext.ts:5110）
    "nickname": "群内昵称",        // 可选。ExportContext.ts:5110 按
    "displayName": "...",         //   nickname || displayName || remark || originalName
    "remark": "..."               //   的顺序合成 groupNickname
    // 可选：键名含 owner / host / creator 的字段（boolean true 或字符串）会被
    //   groupAnalyticsService.detectGroupOwnerUsername（206-251 → 162-204）用于识别群主
  }
]
```

**备注**

- 成员的 remark/nickName/alias 等详细资料不由本接口提供——消费方拿 `username` 再批量调
  `wcdb_get_contacts_compact` / `wcdb_get_contact` / `wcdb_get_display_names` 补齐。
- 群主识别共三条路径：成员对象内 owner 类字段 → `wcdb_get_contact(chatroomId)` 返回对象内 owner 类字段 →
  `wcdb_get_chat_room_ext_buffer`。三者在 DLL 侧至少实现其一体验最佳（均非硬性）。

---

### wcdb_get_group_nicknames

**签名**

```c
int32 wcdb_get_group_nicknames(int64 handle, const char* chatroom_id, char** out_json);
```

**绑定 / 调用点**

- 绑定：wcdbCore.ts:968（可选）。
- 调用：wcdbCore.ts:3340，包装方法 `getGroupNicknames()`（3331-3351）。
  符号缺失 → `{success:false, error:'当前数据服务版本不支持获取群昵称接口'}`。
- 消费方：chatService.ts:4838（群名片缓存）、messagePushService.ts:1384、httpService.ts:2137、
  groupAnalyticsService.ts:274、export/core/ExportContext.ts:1521。

**入参**

- `chatroom_id`：单个群 ID。非 JSON。

**状态码与错误**

- `result !== 0 || !outPtr[0]` → `获取群昵称失败: ${result}`；解码空 → `解析群昵称失败`。

**返回 JSON 结构**

```jsonc
// 顶层：扁平对象，键 = 群成员 ID（username，或群内显示的 memberId），值 = 群昵称字符串
{
  "wxid_member1": "群内昵称A",
  "wxid_member2": "群内昵称B"
}
```

- 消费方对键做 `trim().toLowerCase()` 归一（chatService.ts:8813-8816、messagePushService.ts:1394-1397、
  groupAnalyticsService.ts:302-307），因此键大小写不敏感；但建议直接输出与
  `wcdb_get_group_members` 一致的 `username` 原样。
- 值为空串/纯 `"'@` 符号的条目会被丢弃（groupAnalyticsService.ts:493-498）。
- 消费方带"唯一性净化"：同一 ID 映射到多个不同昵称时整个 ID 被丢弃（防串号），
  所以**每个成员 ID 在对象中只应出现一次**（JSON 对象天然满足，勿用数组）。

---

## 附：调用点速查表

| 导出函数 | 绑定行 | 调用行 | 包装方法（行） | 绑定可选性 |
|---|---|---|---|---|
| wcdb_get_contact | 980 | 3453 | getContact (3433) | 必需 |
| wcdb_get_contact_status | 984 | 3493 | getContactStatus (3484) | 可选 |
| wcdb_get_contact_type_counts | 990 | 3652 | getContactTypeCounts (3637) | 可选 |
| wcdb_get_contacts_compact | 995 | 3688 | getContactsCompact (3672) | 可选 |
| wcdb_get_contact_alias_map | 1000 | 3704 | getContactAliasMap (3699) | 可选 |
| wcdb_get_contact_friend_flags | 1005 | 3720 | getContactFriendFlags (3715) | 可选 |
| wcdb_get_chat_room_ext_buffer | 1010 | 3736 | getChatRoomExtBuffer (3731) | 可选 |
| wcdb_get_display_names | 948 | 3123 | getDisplayNames (3063) | 必需 |
| wcdb_get_avatar_urls | 951 | 3234 | getAvatarUrls (3166) | 必需 |
| wcdb_get_head_image_buffers | 1187 | 4354 | getHeadImageBuffers (4349) | 可选 |
| wcdb_get_group_member_count | 954 | 3272 | getGroupMemberCount (3266) | 必需 |
| wcdb_get_group_member_counts | 958 | 3299 | getGroupMemberCounts (3282) | 可选 |
| wcdb_get_group_members | 964 | 3318 | getGroupMembers (3312) | 必需 |
| wcdb_get_group_nicknames | 968 | 3340 | getGroupNicknames (3331) | 可选 |

所有调用均为 `this.wcdbGetXxx(this.handle, ..., outPtr)` 同步调用，`outPtr = [null]`；
返回字符串统一经 `decodeJsonPtr()`（koffi.decode 'char',-1 + `wcdb_free_string`）解码并释放。
上层链路：wcdbWorker.ts（IPC worker，原样透传上述包装方法结果）→ chatService / groupAnalyticsService /
analyticsService / httpService / messagePushService / snsService / dualReportService /
annualReportService / export/*。
