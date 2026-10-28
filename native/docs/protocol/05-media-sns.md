# 05 媒体 / 表情 / 语音 / Hardlink / 朋友圈 协议规格

> **来源与边界**：本文是**接口行为描述**（函数名、字段名、返回形状、错误路径），源自对 WeFlow 项目（Electron/TypeScript）`wcdbCore.ts` 等文件的阅读笔记；**不含其源代码**，也不声称与上游实现完全一致。第三方权利与许可以 WeFlow 原项目为准。


> 适用范围：wcdb.dll（C ABI，koffi 加载）中媒体、表情、语音、hardlink、朋友圈（SNS）函数组。
> 唯一协议来源：`WeFlow-main/electron/services/wcdbCore.ts`（5191 行），并追踪了 chatService.ts、imageDecryptService.ts、videoService.ts、snsService.ts、annualReportService.ts、dualReportService.ts、export/core/ExportContext.ts、main.ts、wcdbWorker.ts、preload.ts 及 src/ 前端消费方。
> 全局约定（koffi 绑定方式、`wcdb_status` 返回码体系、`wcdb_handle`=int64、`char**` 输出内存由 `wcdb_free_string` 释放等）见 `01` 号全局文档，本文不重复。
>
> 本文所有行号均指 `wcdbCore.ts`，除非另行注明文件。

## 0. 本函数组的公共事实

1. **所有 `char**` 输出（outUrl/outCaption/outHex/outJson/outError）分配的内存都必须能用 `wcdb_free_string(void*)` 释放**。TS 侧统一通过 `decodeJsonPtr()`（L1547-1557：`koffi.decode(ptr,'char',-1)` 后立即 `wcdbFreeString(ptr)`）或手动 decode+free（trigger 类，L4984-4988 等）处理。
2. 输出内容是 **NUL 结尾的 UTF-8 C 字符串**。三类语义：
   - **纯字符串**（非 JSON）：`wcdb_get_emoticon_cdn_url`、`wcdb_get_emoticon_caption`、`wcdb_get_emoticon_caption_strict`、`wcdb_get_voice_data`（hex 串）；
   - **JSON**：`wcdb_get_voice_data_batch`、hardlink 四个、`wcdb_scan_media_stream`、sns timeline/annual_stats/usernames/export_stats；
   - **错误消息纯文本**：trigger 三件套与 `wcdb_delete_sns_post` 的 outError。
3. 状态码检查模式：`result !== 0 || !outPtr[0]` 即判失败（错误消息只带数字码，**不解析 outError/outJson 内容**——本组函数失败时 TS 从不读取输出指针内容，失败时可不分配输出）。唯一例外：trigger 类函数在非 0 时会 decode outError 作为错误文本（见 §12-§15）。
4. `wcdb_install_sns_block_delete_trigger` 特殊：**返回 1 = 已安装（视为成功）**，其余非 0 = 失败。
5. 字符串入参为空时 TS 可能传 `null`（hardlink 的 accountDir/dbPath、export_stats 的 myAccountId）或空串 `''`（caption 的 dbPath、timeline 的 username/keyword），DLL 两者都要容忍。
6. 绑定处均包在 `try { lib.func(...) } catch { xxx = null }` 中（cdn_url 除外，L1101 为必选导出）；缺失时上层报"接口未就绪/当前数据服务版本不支持"。

---

### 1. wcdb_get_emoticon_cdn_url

- **签名**（绑定 L1101）：
  ```c
  int32 wcdb_get_emoticon_cdn_url(int64 handle, const char* dbPath, const char* md5, char** outUrl)
  ```
- **TS 调用点**：`getEmoticonCdnUrl()` L4188-4204（调用在 L4194）。上游：
  - `chatService.ts` L3145（`fallbackEmoticon`，dbPath = `<root>/<accountId>/db_storage/emoticon/emoticon.db` 或 `.../emotion/emoticon.db`，见 chatService L3190-3209）；
  - `dualReportService.ts` L711-712（**dbPath = 用户配置的根数据目录**，不是 emoticon.db 全路径）。
- **入参**：
  - `dbPath`：路径提示。可能是 emoticon.db 完整路径，也可能是账号根目录/db_storage 上层目录——**DLL 需自行容错解析定位表情库**；
  - `md5`：表情（emoji）内容的 md5 hex 串。
- **状态码与错误**：`result !== 0 || outUrl==NULL` → `{success:false, error:"获取表情 URL 失败: <code>"}`。失败时不读输出。
- **返回内容**：**纯字符串 URL（不是 JSON）**。TS 直接 `url: urlStr || undefined`（L4200）。空串视为"未命中"（success=true 但 url=undefined，上层回退本地 emoji 缓存目录）。
- **备注**：本函数是**必选导出**（绑定不在 try/catch 内，缺失会导致整个 initialize 抛异常）。

### 2. wcdb_get_emoticon_caption

- **签名**（绑定 L1104-1109，可选导出）：
  ```c
  int32 wcdb_get_emoticon_caption(int64 handle, const char* dbPath, const char* md5, char** outCaption)
  ```
- **TS 调用点**：`getEmoticonCaption()` L4206-4225（调用在 L4215，`dbPath || ''` 可传空串）。当前仓库内**无业务调用方**（仅 wcdbService.ts L682 / wcdbWorker.ts L215 透传，src/ 未使用）。
- **入参**：同 §1（dbPath 可为空串）。
- **状态码与错误**：同 §1 模式；错误文案"获取表情释义失败: <code>"。
- **返回内容**：**纯字符串**表情释义文本（caption），非 JSON。空串 → caption=undefined。
- **备注**：与 §3 的区别是带 dbPath 提示。

### 3. wcdb_get_emoticon_caption_strict

- **签名**（绑定 L1112-1117，可选导出）：
  ```c
  int32 wcdb_get_emoticon_caption_strict(int64 handle, const char* md5, char** outCaption)
  ```
- **TS 调用点**：`getEmoticonCaptionStrict()` L4227-4246（调用在 L4236）。上游：`export/core/ExportContext.ts` L1275（导出时为 localType=47 表情消息补全释义，md5 已归一化为小写）。
- **入参**：仅 `md5`。"strict" 指不依赖 dbPath 提示、由 DLL 在当前账号全部表情库中精确查找（语义推断，TS 侧无更多约束）。
- **状态码与错误**：同 §1 模式；错误文案"获取表情释义失败(strict): <code>"。
- **返回内容**：**纯字符串** caption，非 JSON。
- **备注**：TS 拿到后经 `normalizeEmojiCaption()` 清洗，空/空白视为未命中。

### 4. wcdb_get_voice_data

- **签名**（绑定 L1136-1140，可选导出）：
  ```c
  int32 wcdb_get_voice_data(int64 handle, const char* sessionId, int32 createTime,
                            int32 localId, int64 svrId, const char* candidatesJson, char** outHex)
  ```
- **TS 调用点**：`getVoiceData()` L4293-4308（调用在 L4298，`svrId` 经 `BigInt(svrId||0)` 直传 int64，避免 JSON 大整数精度问题——注释 L10315 明确说明单条接口优先于 batch 就是为了这点）。上游链路：
  - `chatService.ts` L10317（`getVoiceDataFromMediaDb`，先单条后批量）；
  - IPC `chat:getVoiceData`（main.ts L3930）← ChatPage.tsx L7609/L12688（播放/转写语音）。
- **入参**：
  - `sessionId`：会话 id（1v1 = 对方 wxid，群聊 = `xxx@chatroom`）；
  - `createTime`：消息时间戳（**秒**，int32，>=0）；
  - `localId`：消息表本地 id（int32，>=0，可能为 0）；
  - `svrId`：消息服务器 id（int64；可能为 0；**无符号语义**，TS 侧用 `normalizeUnsignedIntegerToken` 归一化，超过 2^63 的值以字符串传入后 BigInt 化）；
  - `candidatesJson`：**JSON 字符串数组**，内容为定位语音所属账号目录的候选 accountId，按优先级排序（chatService L10165-10182）：`[发送者 accountId, sessionId, 我的 accountId]`（去重、去空）。可能为空数组 `[]`。DLL 用它决定去哪个 `media_<md5(账号)>.db` / 账号子目录中查 VoiceInfo。chatService 还会先传"strict"候选集（剔除自己账号，L10298-10309）再传全集，即同一消息可能用不同 candidates 多次调用。
- **状态码与错误**：`result !== 0 || outHex==NULL` → `{success:false, error:"获取语音数据失败: <code>"}`。
- **返回内容**：**纯 hex 字符串（非 JSON）**，为 silk 语音 BLOB 的原始字节 hex 编码。TS 侧 `decodeVoiceBlob()`（chatService L8194-8214）先按 hex 解码（要求 `^[a-fA-F0-9]+$` 且偶数长度），失败再试 base64——**实现必须输出 hex**（大小写均可），base64 仅是 TS 的容错。空串 → `hex: undefined` 视为未命中（上层走 media.db 直查 fallback，chatService L10468+）。
- **备注**：定位条件本质上是 `session_id + create_time (+ local_id / svr_id)` 在候选账号的 media 库 VoiceInfo 表中命中记录；同一秒可能有多条语音，localId/svrId 用于消歧。

### 5. wcdb_get_voice_data_batch

- **签名**（绑定 L1141-1145，可选导出）：
  ```c
  int32 wcdb_get_voice_data_batch(int64 handle, const char* requestsJson, char** outJson)
  ```
- **TS 调用点**：`getVoiceDataBatch()` L4310-4331（调用在 L4318）。上游：
  - `chatService.ts` L10334（单条未命中后立即批量重试，请求数组长度 1）；
  - `chatService.ts` L10721（语音转写/预加载批量，chunk 8~128 条，默认 48）。
- **入参 `requestsJson`**：JSON 数组，元素结构（wcdbCore L4311 类型 + chatService L10662/10698-10704 构造）：
  ```jsonc
  [
    {
      "session_id": "wxid_xxx 或 xxx@chatroom",  // 必填（snake_case）
      "create_time": 1700000000,                  // 秒，int
      "local_id": 123,                            // 可选（TS 总是传）
      "svr_id": "1234567890123456789",            // 可选；数字或字符串均可（chatService 传 item.serverId 原始值，可能是字符串）
      "candidates": ["发送者accountId", "sessionId", "我的accountId"]  // 可选，同 §4 candidatesJson
    }
  ]
  ```
  非数组输入时 TS 兜底为 `[]`。**字段名固定 snake_case**（TS 直接 JSON.stringify 这些键）。
- **状态码与错误**：`result !== 0 || outJson==NULL` → "批量获取语音数据失败: <code>"。
- **返回 outJson**：
  ```jsonc
  [
    { "index": 0, "hex": "<silk blob 的 hex 串>" }
    // index = 请求数组下标（int）；hex 缺失/空 = 该条未命中
  ]
  ```
  TS 归一化（L4323-4326）：`index: Number(row?.index ?? 0)`，`hex: row?.hex ? String(row.hex) : undefined`。**顶层必须是数组**；允许只返回命中的条目（消费方按 index 建 Map，chatService L10726-10732），但 index 必须正确对应请求下标。
- **备注**：单条与批量的请求字段一一对应（单条是位置参数，批量是 snake_case JSON 字段）。

### 6. wcdb_resolve_image_hardlink

- **签名**（绑定 L1262-1266，可选导出）：
  ```c
  int32 wcdb_resolve_image_hardlink(int64 handle, const char* md5, const char* accountDir, char** outJson)
  ```
- **TS 调用点**：`resolveImageHardlink()` L4365-4388（调用在 L4377）。上游：`imageDecryptService.ts` L1594（`resolveHardlinkPath`，图片 .dat 解密前定位 hardlink 实体文件）。
- **入参**：
  - `md5`：图片 md5，TS 已归一化 **trim + 小写**（L4369）；为空则不调用 DLL；
  - `accountDir`：账号根目录绝对路径（`configService.getAccountDir(dbPath, accountId)`，即含 `db_storage` 与 `msg/attach` 的目录）；**空时传 NULL**（L4377 `normalizedAccountDir || null`）。DLL 应据此定位 `<accountDir>/db_storage/hardlink/hardlink.db` 并将相对路径拼接为绝对路径。
- **状态码与错误**：`result !== 0 || outJson==NULL` → "解析图片 hardlink 失败: <code>"。
- **返回 outJson**（单条 = data 对象本身，非包裹结构）：
  ```jsonc
  {
    "file_name": "xxxxxxxx...._h.dat",   // hardlink 记录的文件名（消费方校验其 base 去掉 .dat 后须为 md5 形态，允许 _h/.h/_hd/.hd/_b/.b/_thumb 等变体后缀）
    "full_path": "C:\\...\\msg\\attach\\...\\xxx.dat"  // 实体文件绝对路径（消费方 existsSync 校验）
  }
  ```
  消费点：imageDecryptService L495-496（batch）、L1596-1597（单条）——**只读 `file_name` 与 `full_path` 两个 snake_case 字段，二者缺一即视为未命中**。`JSON.parse(jsonStr) || {}`，因此顶层必须是对象（返回 `{}` 表示未命中亦可，TS 会因字段为空串而放弃）。
- **备注**：TS 侧有 10 分钟 TTL、2 万条上限的结果缓存（key = `md5\u001f accountDir`，均小写化，L1600-1604），成功结果（含 `{}`）会被缓存；DLL 无需缓存。

### 7. wcdb_resolve_image_hardlink_batch

- **签名**（绑定 L1267-1271，可选导出）：
  ```c
  int32 wcdb_resolve_image_hardlink_batch(int64 handle, const char* requestsJson, char** outJson)
  ```
- **TS 调用点**：`resolveImageHardlinkBatch()` L4415-4518（native 调用在 L4460）。上游：`imageDecryptService.ts` L489（`preloadImageHardlinkMd5s`，chunk ≤256）。
- **入参 `requestsJson`**（L4456-4459 构造，**注意 snake_case 且空值时整个键省略（undefined 被 JSON.stringify 丢弃）**）：
  ```jsonc
  [
    { "md5": "小写hex", "account_dir": "C:\\...\\accountId_xxx" },  // account_dir 为空时该键不出现
    { "md5": "..." }
  ]
  ```
  只包含缓存未命中的条目（下标是"未命中子集"的下标，不是原始请求下标）。
- **状态码与错误**：batch 调用整体失败（`result!==0`、outJson 空、JSON 解析异常）时 TS **静默回退**为逐条调用 §6（L4498-4513），不向上报错。
- **返回 outJson**（L4464-4484 解析）：
  ```jsonc
  [
    {
      "index": 0,             // 对应 requestsJson 数组下标；缺失/非法时按响应数组位置兜底
      "md5": "小写hex",       // 可选；缺失时用 index 反查请求兜底
      "success": true,        // 接受 true / 1 / "1" 三种写法（L4470）
      "data": { "file_name": "...", "full_path": "..." },  // 同 §6 data；非对象时按 {} 处理
      "error": "可选错误文本"
    }
  ]
  ```
  **顶层必须是数组**；允许只返回部分条目（未覆盖的下标由 TS 用单条接口补齐）。
- **备注**：请求/响应通过 `index` 关联；TS 把响应行映射回原始请求下标后写缓存。

### 8. wcdb_resolve_video_hardlink_md5

- **签名**（绑定 L1272-1276，可选导出）：
  ```c
  int32 wcdb_resolve_video_hardlink_md5(int64 handle, const char* md5, const char* dbPath, char** outJson)
  ```
- **TS 调用点**：`resolveVideoHardlinkMd5()` L4390-4413（调用在 L4402）。上游：`videoService.ts` L261（批量失败时的单条回退）。
- **入参**：
  - `md5`：消息 XML 中提取的视频 md5（trim+小写，为空不调用）；
  - `dbPath`：**hardlink.db 的完整路径**（videoService L189-205：`<accountDir>/db_storage/hardlink/hardlink.db`），空时传 NULL。注意与 §6 的 accountDir（目录）语义不同。
- **状态码与错误**：`result !== 0 || outJson==NULL` → "解析视频 hardlink 失败: <code>"。
- **返回 outJson**（data 对象本身）：
  ```jsonc
  { "resolved_md5": "视频实体文件的 md5/文件名 token（小写 hex，可含 _raw 后缀形态）" }
  ```
  消费点：videoService L248-250、L262-264——**只读 `data.resolved_md5`（snake_case）**，trim+小写后用于在 `msg/video/<年月>/` 目录索引中匹配实体文件名。缺失/空 = 未命中。
- **备注**：查询目标为 hardlink.db 的 `video_hardlink_info_v4` 表（videoService L207-209 注释）。TS 侧缓存同 §6（key = `md5\u001f dbPath`）。

### 9. wcdb_resolve_video_hardlink_md5_batch

- **签名**（绑定 L1277-1281，可选导出）：
  ```c
  int32 wcdb_resolve_video_hardlink_md5_batch(int64 handle, const char* requestsJson, char** outJson)
  ```
- **TS 调用点**：`resolveVideoHardlinkMd5Batch()` L4520-4623（native 调用在 L4565）。上游：`videoService.ts` L240。
- **入参 `requestsJson`**（L4561-4564，**snake_case，空 dbPath 时省略键**）：
  ```jsonc
  [ { "md5": "小写hex", "db_path": "C:\\...\\hardlink.db" } ]
  ```
- **状态码与错误 / 返回 outJson**：与 §7 完全同构（数组元素 `{index, md5, success, data, error}`，`data.resolved_md5` 为有效载荷）；失败静默回退单条 §8（L4603-4618）。videoService 消费时优先用 `index` 反查请求 md5，其次用行内 `md5` 字段（L243-247）。
- **备注**：同 §7。

### 10. wcdb_scan_media_stream

- **签名**（绑定 L1181-1185，可选导出）：
  ```c
  int32 wcdb_scan_media_stream(int64 handle, const char* sessionIdsJson, int32 mediaType,
                               int32 beginTimestamp, int32 endTimestamp,
                               int32 limit, int32 offset, char** outJson, int32* outHasMore)
  ```
- **TS 调用点**：`getMediaStream()` L2457-3061（native 调用在 L2884-2894）。上游：
  - IPC `chat:getMediaStream`（main.ts L3957-3966）← ResourcesPage.tsx L1359（"资源"页图片/视频流浏览）；
  - `export/core/ExportContext.ts` L922（media-fast 导出模式，PAGE_LIMIT=480 循环翻页直到 items 为空）。
- **入参**：
  - `sessionIdsJson`：**JSON 字符串数组**，按会话最后消息时间**降序**排列的 sessionId 列表（L2834-2855 由 getSessions 结果构造；指定单会话时只含 1 个元素）。DLL 应按给定顺序扫描各会话消息表；
  - `mediaType`：**枚举 int32：0=all（图片+视频）、1=image（localType 3）、2=video（localType 43）**（L2883）；
  - `beginTimestamp` / `endTimestamp`：Unix 秒（int32），**0 = 不限**（L2754-2755 `Math.max(0, toInt(...))`）；
  - `limit`：nativeLimit，范围 **[40, 1200]**（用户 limit∈[40,1200] 默认 240，nativeLimit = min(1200, max(limit, min(max(limit*4,320),720)))，即通常 240~1200，L2757-2758）——DLL 一次最多返回约 1200 行即可；
  - `offset`：全局偏移（>=0）。TS 有页缓存/inflight 合并（native 一次多取，前端小页从缓存切片），因此 **offset/limit 语义必须是稳定全序分页**（相同参数重复调用结果一致；建议按 create_time DESC, local_id DESC 之类稳定排序）。
  - `outHasMore`：`int32*` 出参，**1 = offset+limit 之后还有更多行，0 = 已到末尾**（L3038 `Number(outHasMore[0]) > 0`）。即使函数返回非 0，TS 也不读它。
- **状态码与错误**：`result !== 0 || outJson==NULL` → "扫描资源失败: <code>"；JSON 解码失败 → "解析资源失败"。
- **返回 outJson**：**顶层为数组**，元素是原始消息行对象（TS 用 `JSON.parse` 后做大量别名归一化，L2907-3009）。每行字段（**首选 snake_case；括号为 TS 同样接受的别名**）：
  ```jsonc
  {
    "session_id": "wxid_xxx",            // (sessionId)；缺失时用请求的单会话 id 兜底
    "local_id": 123,                     // (localId) int
    "server_id": "1234567890123456789",  // (serverId)；TS 用 String(value).trim() 转字符串。
                                         // 注意：此处 JSON.parse 无 int64 字符串化保护（parseMessageJson 未用于本接口），
                                         // 超过 2^53 的 server_id 必须以 JSON 字符串输出，否则精度丢失
    "create_time": 1700000000,           // (createTime) Unix 秒
    "local_type": 3,                     // (localType) 3=图片 43=视频
    "is_send": 0,                        // 仅此一个键名（无 camelCase 别名！L3003）；null/缺失 → isSend=null
    "sender_username": "wxid_yyy",       // (senderUsername / real_sender_id / realSenderId) 群聊内真实发送者
    // 内容字段：以下任一组即可，TS 按列表顺序取第一个非空
    "message_content": "<msg>...</msg>", // (messageContent/message_content_text/messageText/StrContent/str_content/
                                         //  msg_content/msgContent/strContent/content/rawContent/WCDB_CT_message_content)
    "compress_content": "<hex|base64>",  // (compressContent/msg_compress_content/msgCompressContent/WCDB_CT_compress_content)
                                         // zstd(magic 0xFD2FB528) 压缩内容的 hex 或 base64 编码；有则优先于 message_content 解码
    // 图片专用（localType=3）：
    "image_md5": "小写hex",              // (imageMd5)；缺失时 TS 从 content XML <md5>/<imgmd5> 或 packed_info 提取
    "image_path": "xxx.dat",             // (imagePath/image_dat_name/imageDatName/img_path/imgPath/img_name/imgName)
                                         // TS 归一化为去扩展名的 md5 形态 base
    // 视频专用（localType=43）：
    "video_md5": "小写hex",              // (videoMd5/raw_md5/rawMd5)；可为文件名形态，TS 会剥路径/扩展名/_thumb 后缀
    // 二进制附加信息（可选，任一别名）：hex 串 / base64 串 / 字节数组 / {data:[...]}
    "packed_info_data": "<hex>"          // (packedInfoData/packed_info_blob/packedInfoBlob/packed_info/packedInfo/
                                         //  BytesExtra/bytes_extra/WCDB_CT_packed_info/reserved0/Reserved0/WCDB_CT_Reserved0)
                                         // TS 从中提取 32 位 hex md5 与 *.dat 文件名做兜底
  }
  ```
  TS 将每行归一化为 camelCase item（sessionId/sessionDisplayName/mediaType/localId/serverId/createTime/localType/senderUsername/isSend/imageMd5/imageDatName/videoMd5/content）后交给前端——**前端只见归一化结果，DLL 只要保证上面任一别名可被读到即可，推荐输出 snake_case 首选名**。
- **备注**：
  - 允许行中省略无法提供的字段（TS 全部有兜底逻辑），但 `session_id`、`local_id`、`create_time`、`local_type` 强烈建议必给；
  - 二进制字段在 JSON 中只能是 hex 或 base64 字符串（或数字数组）——JSON 无法承载裸 Buffer；
  - `content` 仅对 localType=43 且 md5 提取失败时才需要解压消费，图片行可完全依赖 image_md5/image_path/packed_info_data。

### 11. wcdb_get_sns_timeline

- **签名**（绑定 L1204-1209，可选导出）：
  ```c
  int32 wcdb_get_sns_timeline(int64 handle, int32 limit, int32 offset, const char* username,
                              const char* keyword, int32 startTime, int32 endTime, char** outJson)
  ```
- **TS 调用点**：`getSnsTimeline()` L4769-4795（调用在 L4775-4784）。上游：
  - `snsService.ts` L1307（`getTimeline` → SnsPage 时间线、导出）；
  - `snsService.ts` L646/L675/L1147（以 `pageSize=500, offset 递增, 无过滤` 全量分页，用于收集发布者/统计）。
- **入参**：
  - `limit` / `offset`：分页（TS 常用 20/50/500）；
  - `username`：**JSON 字符串数组**（如 `["wxid_a","wxid_b"]`）；无过滤时传**空串 `''`**（不是 `[]`，L4774）；
  - `keyword`：全文关键字，无过滤时 `''`；
  - `startTime` / `endTime`：Unix 秒，0 = 不限。**endTime 按闭区间消费**：导出翻页取 `endTs = 本页最后一条.createTime - 1` 作为下一页 endTime（snsService L1546-1547），因此**结果必须按 createTime 降序**，且同秒边界不能丢/重；
  - 注意 C 参数名叫 `username`（单数）但内容是 JSON 数组。
- **状态码与错误**：`result !== 0 || outJson==NULL` → "获取朋友圈失败: <code>"；解码失败 → "解析朋友圈数据失败"。
- **返回 outJson**：**顶层为数组**，元素是 DLL 已解析好的帖子对象（SnsPost 形态，camelCase；snsService/前端直接消费，不再解析 contentxml）。每条：
  ```jsonc
  {
    "id": "朋友圈 feedId（字符串）",
    "tid": "SnsTimeLine 表主键（雪花 id 字符串，可选但强烈建议）——前端删除时优先传它（SnsPostItem.tsx L451: post.tid ?? post.id）",
    "username": "发布者 wxid",        // 兼容别名：user_name / userName（snsService L635 pickTimelineUsername）
    "nickname": "发布者昵称",          // 可空，TS 用联系人缓存兜底
    "avatarUrl": "可空",              // TS 总是用 contactCache 覆盖（snsService L1359）
    "createTime": 1700000000,         // Unix 秒（number）——导出翻页/时间过滤的关键字段
    "contentDesc": "文字内容（contentxml 中 <contentDesc> 的解码文本）",
    "type": 2,                        // 朋友圈类型 number；15=视频（snsService L1313），其余按微信 sns type 语义
    "media": [                        // 已解析的 <media> 列表
      {
        "url": "http(s) 原图/视频 CDN url",
        "thumb": "缩略图 url",
        "md5": "url 标签 md5 属性（可选）",
        "token": "url 标签 token 属性（可选）",
        "thumbToken": "thumb 标签 token（可选）",
        "key": "url/thumb 标签 key 属性（可选）",
        "encIdx": "enc_idx 属性（可选；别名 enc_idx 也被接受，snsService L1328/L1336）",
        "livePhoto": { "url": "...", "thumb": "...", "token": "...", "thumbToken": "...", "key": "...", "encIdx": "..." }  // 可选
      }
    ],
    "likes": ["点赞者昵称", "..."],    // string[]（旧格式）；详细点赞信息 TS 会从 rawXml 再解析
    "comments": [                      // 已解析评论（cmtid 升序，id 从 "1" 递增，snsService L1260-1261 注释）
      {
        "id": "cmtid 字符串",
        "nickname": "评论者昵称",
        "content": "评论文本",
        "refCommentId": "被回复评论的 cmtid，无则 '0' 或 ''",
        "refNickname": "被回复者昵称（可选，TS 会兜底补全）",
        "emojis": [ { "url": "...", "md5": "...", "width": 0, "height": 0, "encryptUrl": "...", "aesKey": "..." } ]  // 可选；空 url+空 encryptUrl 的项会被过滤
      }
    ],
    "location": {                      // 可选；键名接受 latitude|lat|x、longitude|lng|y，及 city/country/
                                       // poiName|poiname/poiAddress|poiaddress/poiAddressName|poiaddressname/label（snsService L824-831）
      "latitude": 31.23, "longitude": 121.47, "city": "...", "poiName": "..."
    },
    "rawXml": "完整 contentxml 原文（未解压前的 XML 字符串）",  // 关键字段：TS 用它兜底解析点赞(LikeUserList)、评论、位置、视频 <enc key="...">（snsService L1314-1318, L512, L542）
    "linkTitle": "链接卡片标题（可选）",
    "linkUrl": "链接卡片 url（可选）"
  }
  ```
- **备注**：
  - `rawXml` 中的 `&amp;` 等实体 TS 会自行处理；comments[].emojis 的 url/encryptUrl 中 `&amp;` 会被替换为 `&`（L1287/L1291），DLL 原样输出即可；
  - media[].url 的 http→https、尺寸后缀 /150|/200|/480→/0、token 拼接均由 TS `fixSnsUrl` 处理，DLL 输出库中原始 url+token 即可；
  - 前端 SnsPostItem.tsx 消费字段统计：media/type/rawXml/contentDesc/likes/comments/linkUrl/linkTitle/nickname/location/id/createTime/username/tid/avatarUrl——**均 camelCase**；
  - 分页终止判断是 `rows.length < pageSize`（L659/L690/L1161）与 `timeline.length >= pageSize`（L1548），返回条数必须精确等于 limit（有足量数据时）。

### 12. wcdb_get_sns_annual_stats

- **签名**（绑定 L1211-1216，可选导出）：
  ```c
  int32 wcdb_get_sns_annual_stats(int64 handle, int32 begin, int32 end, char** outJson)
  ```
- **TS 调用点**：`getSnsAnnualStats()` L4797-4820（调用在 L4807）。上游：`annualReportService.ts` L1205（年度报告"朋友圈"章节；`isAllTime` 时 begin=0、end=当前秒）。
- **入参**：`begin`/`end` Unix 秒；**begin=0 且 end=0（或 begin=0）表示全时段**——TS 注释（annualReportService L1232）提到"部分底层实现 begin/end 为 0 时会返回 0"，此时 TS 用 §14 兜底，说明 0 边界行为允许但建议实现为全时段统计。
- **状态码与错误**：`result !== 0 || outJson==NULL` → "getSnsAnnualStats failed: <code>"；解码失败 → "Failed to decode JSON"。绑定缺失 → "wcdbGetSnsAnnualStats 未找到"。
- **返回 outJson**（camelCase，annualReportService L1207-1229 消费）：
  ```jsonc
  {
    "totalPosts": 123,                 // number，区间内发帖总数
    "typeCounts": { "2": 100, "15": 23 },  // 可选；键=朋友圈 type 的字符串形式，值=条数（Record<string, number> 直接透传给年报前端）
    "topLikers": [ { "username": "wxid_a", "count": 10 } ],  // 给我点赞最多的人；只需 username+count，displayName/avatarUrl 由 TS 合并
    "topLiked":  [ { "username": "wxid_b", "count": 8 } ]    // 我点赞最多的人
  }
  ```
- **备注**：顶层必须是对象；`topLikers/topLiked` 缺失按 `[]` 处理，`typeCounts` 可省略。

### 13. wcdb_get_sns_usernames

- **签名**（绑定 L1217-1221，可选导出）：
  ```c
  int32 wcdb_get_sns_usernames(int64 handle, char** outJson)
  ```
- **TS 调用点**：`getSnsUsernames()` L4822-4836（调用在 L4827）。上游：snsService L976（空结果时回退到全量 timeline 扫描收集，L640-664）、main.ts L4014 / httpService L1379 / SnsPage.tsx L1607（筛选器数据源）。
- **入参**：仅 handle。
- **状态码与错误**：`result !== 0 || outJson==NULL` → "获取朋友圈用户名失败: <code>"。
- **返回 outJson**：**JSON 字符串数组**（去重后的朋友圈发布者 wxid 列表）：
  ```jsonc
  ["wxid_a", "wxid_b"]
  ```
  TS 侧 `Array.isArray` 校验 + `String(u||'').trim()` + 过滤空串（L4832）；非数组 → `[]`（触发上层 timeline 回退扫描）。
- **备注**：无。

### 14. wcdb_get_sns_export_stats

- **签名**（绑定 L1222-1226，可选导出）：
  ```c
  int32 wcdb_get_sns_export_stats(int64 handle, const char* myAccountId, char** outJson)
  ```
- **TS 调用点**：`getSnsExportStats()` L4838-4859（调用在 L4843，`myAccountId || null`——**未提供时传 NULL**）。上游：
  - `snsService.ts` L1001（导出统计；结果为 0 时回退 timeline 全量扫描统计，L1055-1079）；
  - `annualReportService.ts` L1234（年报 totalPosts 兜底）。
- **入参**：`myAccountId`：当前登录账号 wxid（可能带后缀，annualReport 传 cleaned 或 raw），用于统计"我的发帖数"；NULL = 不统计（my_posts 应返回 null）。
- **状态码与错误**：`result !== 0 || outJson==NULL` → "获取朋友圈导出统计失败: <code>"。
- **返回 outJson**（**snake_case**，L4847-4854 解析）：
  ```jsonc
  {
    "total_posts": 1234,     // SnsTimeLine 表总条数
    "total_friends": 300,    // 去重发布者数
    "my_posts": 56           // 我的发帖数；myAccountId 为 NULL 或无法判定时输出 JSON null
  }
  ```
  TS：`Number(raw.total_posts || 0)`、`Number(raw.total_friends || 0)`、`raw.my_posts === null||undefined ? null : Number(raw.my_posts || 0)`。
- **备注**：这是本组唯一 snake_case 输出字段的统计接口（对比 §12 的 camelCase），实现时切勿混用。

### 15. wcdb_check_sns_block_delete_trigger

- **签名**（绑定 L1318-1323，可选导出）：
  ```c
  int32 wcdb_check_sns_block_delete_trigger(int64 handle, int32* outInstalled)
  ```
- **TS 调用点**：`checkSnsBlockDeleteTrigger()` L5029-5042（调用在 L5034）。上游：snsService L1245 → main.ts L4124 / httpService L1502 → 设置页开关状态。
- **入参**：仅 handle；出参 `outInstalled`：**int32，1 = 拦截触发器已安装，0 = 未安装**（TS 判 `=== 1`，其余值一律视为 false）。
- **状态码与错误**：`status !== 0` → `{success:false, error:"DLL error <status>"}`。**无 outError 参数**。
- **返回 JSON**：无（纯出参函数）。
- **备注**：无。

### 16. wcdb_install_sns_block_delete_trigger

- **签名**（绑定 L1304-1309，可选导出）：
  ```c
  int32 wcdb_install_sns_block_delete_trigger(int64 handle, char** outError)
  ```
- **TS 调用点**：`installSnsBlockDeleteTrigger()` L4979-5001（调用在 L4984）。上游：snsService L1235 → main.ts L4116 / httpService L1511。
- **入参**：仅 handle。功能：在 SNS 库安装触发器，阻止微信客户端删除朋友圈记录（防删除）。
- **状态码与错误**（三态）：
  - `0` = 安装成功 → `{success:true, alreadyInstalled:false}`；
  - `1` = **已安装（也算成功）** → `{success:true, alreadyInstalled:true}`（L4990-4993）；
  - 其他非 0 = 失败 → `{success:false, error: outError 文本 || "DLL error <status>"}`。
- **outError**：失败时的**纯文本**错误消息（可 NULL）；TS 无论成败都会 decode 并 `wcdbFreeString`（L4986-4989，即成功时若写了指针也会被释放——建议成功时保持 NULL）。
- **返回 JSON**：无。
- **备注**：与消息防撤回 trigger（另一组）状态码语义一致。

### 17. wcdb_uninstall_sns_block_delete_trigger

- **签名**（绑定 L1311-1316，可选导出）：
  ```c
  int32 wcdb_uninstall_sns_block_delete_trigger(int64 handle, char** outError)
  ```
- **TS 调用点**：`uninstallSnsBlockDeleteTrigger()` L5006-5024（调用在 L5011）。上游：snsService L1240 → main.ts L4120 / httpService L1520。
- **入参**：仅 handle。
- **状态码与错误**：`0` = 成功；**非 0（含 1）一律失败** → `error = outError 文本 || "DLL error <status>"`。outError 处理同 §16。
- **返回 JSON**：无。
- **备注**：未安装时执行卸载应返回 0（幂等，TS 无 already 语义）。

### 18. wcdb_delete_sns_post

- **签名**（绑定 L1325-1330，可选导出）：
  ```c
  int32 wcdb_delete_sns_post(int64 handle, const char* postId, char** outError)
  ```
- **TS 调用点**：`deleteSnsPost()` L5044-5062（调用在 L5049）。上游：snsService L1250（成功后清派生缓存）→ main.ts L4127 / httpService L1535 ← SnsPostItem.tsx L451。
- **入参**：`postId`：**优先为 §11 的 `tid`（SnsTimeLine 表主键，雪花 id 字符串），无 tid 时回退 `id`（feedId）**——前端 `deleteSnsPost(post.tid ?? post.id)`。DLL 需能按主键删除；收到 feedId 形态时的行为**未确定**（TS 侧无区分，建议实现为两者都尝试）。注意：若安装了 §16 的拦截触发器，本函数是"授权删除"通道，实现时需绕过自身 trigger（推断，TS 无显式说明——标注：绕过方式未确定）。
- **状态码与错误**：`0` = 成功；非 0 → `error = outError 文本 || "DLL error <status>"`。outError 处理同 §16。
- **返回 JSON**：无。
- **备注**：无。

---

## 附：本组函数顶层返回结构速查

| 函数 | 输出形态 | 顶层结构 / 关键字段 |
|---|---|---|
| wcdb_get_emoticon_cdn_url | 纯字符串 | CDN URL 文本 |
| wcdb_get_emoticon_caption | 纯字符串 | caption 文本 |
| wcdb_get_emoticon_caption_strict | 纯字符串 | caption 文本 |
| wcdb_get_voice_data | 纯字符串 | silk BLOB 的 hex 串 |
| wcdb_get_voice_data_batch | JSON | `[{index:int, hex:string}]` |
| wcdb_resolve_image_hardlink | JSON | `{file_name, full_path}` |
| wcdb_resolve_image_hardlink_batch | JSON | `[{index, md5, success, data:{file_name,full_path}, error}]` |
| wcdb_resolve_video_hardlink_md5 | JSON | `{resolved_md5}` |
| wcdb_resolve_video_hardlink_md5_batch | JSON | `[{index, md5, success, data:{resolved_md5}, error}]` |
| wcdb_scan_media_stream | JSON + int32 出参 | `[原始消息行]`（snake_case 优先）+ outHasMore |
| wcdb_get_sns_timeline | JSON | `[SnsPost]`（camelCase，含 rawXml/media/comments/likes/location） |
| wcdb_get_sns_annual_stats | JSON | `{totalPosts, typeCounts, topLikers:[{username,count}], topLiked:[...]}`（camelCase） |
| wcdb_get_sns_usernames | JSON | `["wxid", ...]` |
| wcdb_get_sns_export_stats | JSON | `{total_posts, total_friends, my_posts|null}`（snake_case） |
| wcdb_check_sns_block_delete_trigger | int32 出参 | outInstalled: 1/0 |
| wcdb_install_sns_block_delete_trigger | 状态码 + outError 文本 | 0=成功 1=已安装 其他=失败 |
| wcdb_uninstall_sns_block_delete_trigger | 状态码 + outError 文本 | 0=成功 |
| wcdb_delete_sns_post | 状态码 + outError 文本 | 0=成功；postId=tid 优先 |
