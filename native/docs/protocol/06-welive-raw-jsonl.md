# 06 WeLive raw-jsonl 导出契约（welive-export.exe ↔ WeFlow）

> **来源与边界**：本文是**接口行为描述**（函数名、字段名、返回形状、错误路径），源自对 WeFlow 项目（Electron/TypeScript）`wcdbCore.ts` 等文件的阅读笔记；**不含其源代码**，也不声称与上游实现完全一致。第三方权利与许可以 WeFlow 原项目为准。


> 本文档从 WeFlow TypeScript 源码逆向提取，是 Rust 实现 `welive-export.exe` 必须严格对照的契约。
> 字段名差一个字母都会导致导出失败（WeFlow 按别名列表逐键取值，列表外的键一律忽略）。
>
> 主要来源文件（下文用短名引用）：
> - **bridge** = `<WeFlow>\electron\services\weliveBridge.ts`
> - **worker** = `<WeFlow>\electron\exportWorker.ts`
> - **ctx** = `<WeFlow>\electron\services\export\core\ExportContext.ts`
> - **jsonlReader** = `<WeFlow>\electron\services\export\utils\jsonlLineReader.ts`
> - **timestamp** = `<WeFlow>\electron\services\export\utils\timestamp.ts`
> - **fileNaming** = `<WeFlow>\electron\services\export\utils\fileNaming.ts`
> - **types** = `<WeFlow>\electron\services\export\types.ts`
> - **chatService** = `<WeFlow>\electron\services\chatService.ts`
> - **main** = `<WeFlow>\electron\main.ts`
> - **ChatLabFormatter** = `<WeFlow>\electron\services\export\formatters\ChatLabFormatter.ts`

---

### 1 进程协议

#### 1.1 拉起方式（argv / stdio）

- WeFlow 用 `child_process.spawn(exe, [...weliveArgsPrefix, 'weflow-export'], { stdio: ['pipe','pipe','pipe'], windowsHide: true })` 拉起（bridge:160-163）。
  - 子命令固定为 **`weflow-export`**；`weliveArgsPrefix` 由调用方传入，当前主进程未配置（main.ts:4555-4577 的 workerData 只有 `welivePath: cfg.get('welivePath')`），实现应容忍任意前缀参数。
  - 可执行文件路径只认用户显式配置的 `welivePath`，WeFlow 不做内置查找（bridge:128-134，`resolveWeliveExecutable` 恒返回 null；bridge:137-150 路径缺失/不存在时直接报错不拉起）。
  - 非 win32 平台拉起前会 `chmod 755`（bridge:151-157）。
- **stdin**：WeFlow 写入 `JSON.stringify(request)` 一次后立即 `end()`（bridge:357-361）。引擎应读完 stdin 全部字节、解析为一个 JSON 对象。
- **stdout**：NDJSON——每行一个 JSON 事件，按 `\n` 切分；每行先 `trim()` 再 `JSON.parse`，空行忽略；解析失败的行不会中断流程，只把 `[welive-bridge] 无法解析事件: <line>` 追加进 stderr 摘要（bridge:269-289）。**stdout 上只允许输出 JSON 事件行，任何调试日志都会污染协议**（日志请写 stderr 或文件）。
- **stderr**：全量收集，失败时作为错误信息主体展示（bridge:291-294、340）。

#### 1.2 退出码与成功判定

成功判定（bridge:324-355）：

```
success = 未取消 && exit code === 0 && 收到过 type:'result' 事件 && result.success !== false
```

- 退出码非 0、或没有发出 `result` 事件、或 `result.success === false`，整个导出被判失败；错误文本为 stderr 内容 + 退出码诊断（`exit=<code> (<hex>)`，bridge:98-106、339-353）。
- `result` 事件之后再退出 0 即可。事件在 `close`（stdout/stderr 均关闭）时统一排空结算（bridge:322-326），所以**必须在退出前把 result 行 flush 完并换行**。
- `success_count` / `fail_count` 缺省时分别回退为"created_file/result 中收集到的会话输出路径数"与"session_error+failed_sessions 的会话数"（bridge:343-344）。

#### 1.3 取消

- WeFlow 取消 = AbortSignal → `child.kill()`（Windows 上即终止信号）；2 秒后进程仍存活则在 Windows 上补 `taskkill /pid <pid> /T /F`（bridge:186-211）。
- 取消后 WeFlow 侧结果为 `stopped:true, error:'用户已取消'`，不算失败（bridge:296-320、341-354；worker:489-504）。引擎收到终止信号直接退出即可，无需特殊响应。

#### 1.4 stdin 请求字段表（`WeliveExportRequest`）

类型定义见 bridge:45-70；**实际构造与取值见 worker:434-463**（这是权威）。

| 字段 | 类型 | 实际取值（WeFlow 侧来源） | 说明 |
|---|---|---|---|
| `account.sessionDb` | string | `resolveWeliveSessionDb()`：`config.sessionDbPath` → `dbPath`（若本身是文件）→ `accountDir\db_storage\session\session.db` → `accountDir\db_storage\session.db` → dbPath 下同名候选（worker:171-195） | 会话库 session.db 绝对路径 |
| `account.dbKey` | string | `config.decryptKey` trim（worker:443） | SQLCipher 密钥 |
| `account.myAccountId` | string? | 空串时省略（worker:444） | 当前登录账号（wxid） |
| `account.accountDir` | string? | 空串时省略（worker:445） | 账号数据目录 |
| `account.imageXorKey` | string \| number? | `normalizeImageXorKey`：有限 number 原样，否则非空字符串（worker:154-158、446） | 图片 .dat 异或密钥 |
| `account.imageAesKey` | string? | 空串时省略（worker:447） | 图片 .dat AES 密钥 |
| `sessionIds` | string[] | 本批会话 ID（worker:449）。**纯文本导出每进程最多 100 个会话（`WELIVE_TEXT_SESSION_CHUNK_SIZE=100`）；媒体导出每进程 1 个会话**（worker:427-432） | |
| `outputDir` | string | 用户选择的输出根目录（worker:327、450） | 仅供参考，最终格式化文件由 WeFlow 自己写 |
| `exportsDir` | string | `mkdtempSync(os.tmpdir(), 'weflow-welive-raw-')` + `\exports`（worker:328、360、451） | **raw JSONL 必须写在这里**；整个临时根目录在导出结束后被 `rm -rf`（worker:667） |
| `mediaDir` | string? | 仅当本批 `sessionIds.length === 1` 时传：`resolve(mediaRootDir + mediaRelativePrefix)`（worker:399-406、452）；多会话批为 `undefined` | 媒体文件落盘根目录，见 §3 |
| `mediaTypes` | Array<'image'\|'voice'\|'video'\|'emoji'\|'file'> | 由 `exportImages/exportVoices/exportVideos/exportEmojis/exportFiles` 逐一映射（worker:392-398）。mediaTypes 非空 ⇒ 必然单会话批且 `mediaDir` 有值（ctx:740-742 `isMediaExportEnabled` 与 worker:430） | 需要导出的媒体类型 |
| `emojiCacheDir` | string? | `config.emojiCacheDir` → `cachePath\Emojis` → `userDataPath\Emojis`（worker:160-169；main.ts:4551） | 表情解码缓存目录，见 §3.4 |
| `format` | string | **固定 `'raw-jsonl'`**（worker:455） | |
| `parseContent` | boolean | `true`（worker:456） | WeFlow 不解释；语义由 WeLive 定义（未确定）。但结果要求见 §2 的 `message_content` 说明 |
| `preserveMessageContent` | boolean | `true`（worker:457） | 同上；WeFlow 依赖每行保留完整明文 content（含 XML） |
| `compactRaw` | boolean | `true`（worker:458） | WeFlow 不解释（未确定），推测为"行内省略冗余列" |
| `sanitize` | boolean | `config.options?.sanitize === true`，默认 false（worker:459） | WeFlow 不解释（未确定） |
| `batchSize` | number | `config.options?.batchSize \|\| 20000`（worker:460） | 读取/写入批量提示 |
| `ascending` | boolean | `config.options?.ascending !== false`，默认 true（worker:461） | 行序提示；WeFlow 读完后总会重排（ctx:4538-4544） |
| `options` | object | `{...effectiveOptions}`，完整 `ExportOptions`（worker:462；types:65-90：format/contentType/dateRange/senderUsername/exportImages/…/sessionLayout/exportWriteLayout 等） | 透传参考。**dateRange/senderUsername 过滤由 WeFlow 在读取 raw JSONL 时自行执行（ctx:4357-4361、4390），WeLive 应导出会话全量消息** |
| `formattedDir` / `preferredOutputPath` | string? | 本流程**不传**（worker:440-463 未设置） | 可忽略 |

#### 1.5 stdout 事件类型总览（bridge:15-43）

| type | 字段 | WeFlow 处理 |
|---|---|---|
| `ready` | `total?`, `output_dir?`, `exports_dir?`, `media_dir?` | 仅映射为 progress（phase='ready'→'preparing'，worker:197-220）；三个 dir 字段不被消费 |
| `progress` | 见 §5 | 映射为导出进度（worker:197-244、464-486） |
| `created_file` | `path?`, `session_id?` | `session_id`+`path` 均有值时记入 `sessionOutputPaths[session_id]=path`（bridge:227-229，作为 raw_session_output_paths 的兜底）；worker 侧：path 位于临时根目录（rawRoot）下的被过滤，其余进 `queueCreatedFile` → 主进程 `exportTaskControlService.recordCreatedFile`（worker:464-468；main.ts:4609-4615），用于任务取消/失败时清理产物 |
| `created_dir` | `path?` | 同上，非临时目录路径进 `queueCreatedDir`（worker:468；bridge:29-30） |
| `session_error` | `session_id?`, `error?` | 记入 `failedSessionErrors[session_id]`（bridge:223-226） |
| `result` | 见 §4 | 结算（bridge:230-266） |
| 其他 type | — | 透传给 onEvent 后被忽略（bridge:43、215-222） |

---

### 2 raw JSONL 每行记录 schema

消费端唯一入口：`ctx.collectMessagesFromWeliveRaw`（ctx:4261-4547），经 `iterateJsonlLines` 逐行读取。

**硬性行级要求**：
- 编码 UTF-8；分隔符**只认 ASCII LF（\n）**，行尾 `\r` 会被剥掉（\r\n 可用）；U+2028/U+2029 不是行分隔符、可以合法出现在 JSON 字符串内（jsonlReader:1-46）。
- 每非空行必须是一次 `JSON.parse` 成功的对象；**任何一行解析失败 → 整个会话导出失败**（ctx:4344-4355）。
- 空行/纯空白行被跳过且不计入行数（ctx:4340-4342）；`manifest.rows` 必须等于**非空行数**（ctx:4511-4518）。
- 若 rows>0，文件最后一个字节必须是 `0x0A`（ctx:4306-4323）。
- 文件字节数在读取前后都必须等于 `manifest.bytes`（ctx:4296-4305、4501-4510）——**写完再报 result，之后不得再改动文件**。

**字段取值规则**：WeFlow 按"别名列表顺序取第一个非 undefined/null/'' 的键"（`getRowField` ctx:1076-1087；`getIntFromRow` 对值做 `parseInt(String(v),10)`，ctx:1065-1074）。下表"键别名（按优先级）"即完整契约；**建议使用每组第一个 snake_case 键**。

#### 2.1 消息行字段清单

| 语义 | 键别名（按优先级） | 类型 | 必需 | 说明 / 来源 |
|---|---|---|---|---|
| 时间戳 | `create_time` \| `createTime` \| `CreateTime` \| `msgCreateTime` \| `msg_create_time` \| `timestamp` \| `Timestamp` | number/string | 是（缺省按 0） | 秒/毫秒/微秒/纳秒均可，WeFlow 统一归一到秒（timestamp:1-10、63-70）。用于日期过滤、排序、显示 |
| 消息类型 | `local_type` \| `localType` \| `type` \| `msg_type` \| `msgType` \| `WCDB_CT_local_type` | int | 是（缺省 1） | ctx:4363-4365。微信 localType：1 文本、3 图片、34 语音、42 名片、43 视频、47 表情、48 位置、49 应用消息（其 XML `<type>`=6 文件、19 合并转发聊天记录、57 引用回复）、10000 与 266287972401 系统/撤回消息（ctx:4377、4408-4449） |
| 消息内容 | `message_content` \| `messageContent` \| `msg_content` \| `msgContent` \| `strContent` \| `content` \| `WCDB_CT_message_content` | string | 是（缺省 ''） | ctx:4366-4368。**必须是解压/解密后的完整明文**：WeFlow 自己从 content 再解析——位置 XML（4408-4416）、表情 cdnurl/md5（4418-4425、3581-3609）、图片 dat 名（3547-3576）、视频 md5（3611-3627）、appmsg 文件元数据/聊天记录 type19（4430-4449）、撤回者与引用回复 refermsg（4377-4388；ctx:2361-2375 `resolveQuotedMessagesForExport` 在 raw 模式直接 return，引用信息只能来自 content）。这也是 `preserveMessageContent:true` 的实际含义 |
| 是否本人发送 | `computed_is_send` \| `is_send` \| `isSend` | '0'/'1'/number/bool | 是（缺省 '0'） | ctx:4373-4374：`parseInt===1` 或 `===true` 视为本人 |
| 发送者 | `sender_username` \| `senderUsername` \| `talker` | string | 群聊必需 | ctx:4375。实际发送者推导：本人→myAccountId；对方→sender_username，缺省回退 sessionId；系统消息（10000/266287972401）按撤回解析特殊处理（ctx:4376-4388） |
| 本地消息 ID | `local_id` \| `localId` \| `LocalId` \| `msg_local_id` \| `msgLocalId` \| `MsgLocalId` \| `msg_id` \| `msgId` \| `MsgId` \| `id` \| `WCDB_CT_local_id` | int | 建议（缺省 0） | ctx:4457。用于排序次键与消息去重键（ctx:1101-1111、4538-4544） |
| 服务器消息 ID | `server_id` \| `serverId` \| `ServerId` \| `msg_server_id` \| `msgServerId` \| `MsgServerId` \| `svr_id` \| `svrId` \| `msg_svr_id` \| `msgSvrId` \| `MsgSvrId` \| `WCDB_CT_server_id` | int/string | 建议（缺省 0） | ctx:4458-4459；同时保留原始无符号字符串 `serverIdRaw`（大数用字符串避免精度丢失） |
| 图片 MD5 | `image_md5` \| `imageMd5` | string | 否 | ctx:4427；缺省时 localType=3 会从 content 提取（4440-4442） |
| 图片 dat 名（列） | `image_path` \| `imagePath` \| `image_dat_name` \| `imageDatName` \| `img_path` \| `imgPath` \| `img_name` \| `imgName` | string | 否 | ctx:3547-3558，取 16-64 位 hex token（3507-3527） |
| packed 二进制列 | `packed_info_data` \| `packedInfoData` \| `packed_info_blob` \| `packedInfoBlob` \| `packed_info` \| `packedInfo` \| `BytesExtra` \| `bytes_extra` \| `WCDB_CT_packed_info` \| `reserved0` \| `Reserved0` \| `WCDB_CT_Reserved0` | hex 或 base64 字符串 / 字节数组 | 否 | ctx:3559-3573、3701-3711、3629-3655。用于提取图片 dat 名与视频 md5 的兜底。**若已给出 media_path，这些列在 raw 模式仅做补充** |
| 视频 MD5/文件名 | `video_md5` \| `videoMd5` \| `raw_md5` \| `rawMd5` \| `video_file_name` \| `videoFileName` | string | 否 | ctx:3712-3716；兜底从 packed 列或 content 提取 |
| 表情 CDN URL | `emoji_cdn_url` \| `emojiCdnUrl` | string | 否（localType=47） | ctx:4419、4422；缺省从 content 的 `cdnurl=` 提取 |
| 表情 MD5 | `emoji_md5` \| `emojiMd5` | 32 位 hex | 否（localType=47） | ctx:4420、4423；非 32hex 会被丢弃（ctx:1126-1130），缺省从 content 提取 |
| 文件消息元数据 | `xml_type`/`xmlType`（'6'=文件）、`file_name`/`fileName`、`file_ext`/`fileExt`、`file_size`/`fileSize`（亦认 `total_len`/`totalLen`/`totallen`）、`file_md5`/`fileMd5`（32hex） | string/int | 否（文件消息建议给） | ctx:797-818；缺省时从 content 的 appmsg XML 解析（4430-4439、845-876） |
| **媒体文件路径** | `media_path` \| `mediaPath` \| `video_path` \| `videoPath` \| `image_path` \| `imagePath` \| `voice_path` \| `voicePath` \| `file_path` \| `filePath` \| `local_path` \| `localPath` \| `source_path` \| `sourcePath` \| `full_path` \| `fullPath` \| `absolute_path` \| `absolutePath`；嵌套 `media.{path, mediaPath, media_path, videoPath, video_path, localPath, local_path, filePath, file_path}` | string | 媒体消息必需 | ctx:2803-2841（取第一个非空）、4475。也接受 `http(s)://` URL（ctx:4154-4156，原样作为引用）。落盘规则见 §3.2 |
| 媒体类型 | `media_type` \| `mediaType` | 'image'\|'voice'\|'emoji'\|'video'\|'file'（小写） | 否 | ctx:4136、4476；缺省按 local_type 推断（4139-4149） |
| 媒体导出失败原因 | `media_error` \| `mediaError` | string | 否 | ctx:4274-4275、4477。**媒体缺失不允许中断导出**：写 media_error 并保留 content 中的原始 XML/文本，WeFlow 会退回 `[图片]`/`[视频]`/`[文件]` 等文本占位符 |

映射到 WeFlow 内部行对象的完整代码见 ctx:4454-4483（`sessionId/session_id/localId/serverId/serverIdRaw/createTime/localType/content/senderUsername/isSend/imageMd5/imageDatName/emojiCdnUrl/emojiMd5/videoMd5/xmlType/fileName/fileSize/fileExt/fileMd5/mediaPath/mediaType/mediaError/location*/chatRecordList`）；其中 `location*` 与 `chatRecordList` 由 WeFlow 从 content 自行解析，raw 行无需提供。

#### 2.2 特殊行

- **系统消息/撤回**（local_type 10000、266287972401）：仍是普通行；发送者由 WeFlow 从 content 解析撤回者（ctx:4377-4388）。
- **引用回复**（local_type 49 且 content 含 `<refermsg>`）：无需额外字段，WeFlow 从 content 解析（ctx:2361-2375 表明 raw 模式不查库补引用，content 必须自带）。
- **合并转发聊天记录**（local_type 49、appmsg type=19）：WeFlow 用 `parseChatHistory(content)` 解析（ctx:4445-4449），content 必须保留完整 chatrecord XML。
- 除消息行外**不支持**其它类型的行（没有 meta/header 行的概念；每一非空行都会被当作消息解析并计入 rows）。

#### 2.3 parseContent / compactRaw / sanitize 的影响（WeFlow 侧视角）

- 三个 flag WeFlow 均只透传、不解释（bridge:63-66；worker:456-459），确切语义属 WeLive 自定义（**未确定**）。
- WeFlow 对结果的硬性依赖：`message_content` 为完整明文（§2.1），行内其余字段可缺省（走别名/兜底/缺省值）。`compactRaw:true` 下省略未列出的数据库列是安全的；`sanitize` 若影响文件名，注意 `file_name` 与 media_path 落盘名应一致（WeFlow 引用的是 media_path 实际文件，见 §3.2）。

---

### 3 文件落盘布局

#### 3.1 raw JSONL（每会话一个文件）

- 位置：`exportsDir`（= `%TEMP%\weflow-welive-raw-*\exports`）下（worker:328、360、451）。
- 文件名：**WeFlow 无命名约束**，路径通过 `result.raw_session_output_paths[sessionId]`（或 `created_file` 事件的 `session_id`+`path`）回传（**未确定**，建议 `<sessionId>.jsonl`）。
- 生命周期：导出结束（含失败/取消）后整个临时根目录被删除（worker:491、553、667），JSONL 是中间产物。
- `created_file` 事件中位于临时根目录下的路径会被 WeFlow 过滤、不进入产物清理清单（worker:465-468），报不报均可。
- 校验：`manifest.path` 必须与 `raw_session_output_paths` 的路径 resolve 后一致（ctx:4286-4295）；文件必须存在（ctx:4276-4284）。

##### 3.1.1 写出方式与行序（本引擎的内存约束）

- **流式写**：引擎按分库游标逐行读→归一→序列化→写入 `BufWriter`（256 KiB），内存占用只与"当前一行"有关，与整会话消息数无关。整会话一次性读进 `Vec` 的写法在百万级消息下会 OOM，已弃用（`weflow-core::messages::stream_session_message_rows`）。
- **行序**：文件内是"分库顺序 + 库内 `sort_seq`（缺则 `create_time`）顺序"的分段序列，**不做跨分库全局排序**。
- **为什么安全**：WeFlow 读完 raw JSONL 后**无条件**按 `createTime`→`localId` 重排（ctx:4401-4407 `rows.sort`，`collectMessagesFromWeliveRaw` 末尾），所以最终导出产物顺序与引擎写出顺序无关；`ascending` 字段本来就只是提示（见 §1.4）。
- **约束**：同一分库的行必须连续成段、段内有序（回归测试 `welive-export/tests/protocol.rs` 断言这点）；失败/中断时半截 JSONL 一律删除，不留给 WeFlow 读（否则会当成完整数据）。


#### 3.2 媒体文件（image/voice/video/emoji/file）

- **raw 模式下 WeFlow 不复制、不移动、不重命名任何媒体**：
  - `collectMediaMessagesForExport` 在 raw 模式返回 `[]`（ctx:4180-4191），各 formatter 的"阶段1 媒体导出循环"因此整体跳过（ChatLabFormatter:121-185）；`exportMediaForMessage`/`adoptWeliveExportedMedia`（ctx:2843-3015）不会被调用。
  - `preloadWeliveRawEmojiMedia` 是显式 no-op："WeLive raw export owns media resolution"（ctx:4166-4178）。
- 引用方式：formatter 写每条消息时调用 `resolveWeliveRawMediaItem`（ctx:4132-4164；调用点如 ChatLabFormatter:278-287、HtmlFormatter:344-347、ExportContext:5589-5596）：
  1. 取行的 `media_path`（§2.1 别名表）；
  2. 类型判定：`media_type` 或 local_type（3=image、34=voice、47=emoji、43=video、文件= xml_type '6'/文件元数据）且对应 `options.export*` 开启；
  3. **`resolve(media_path)` 必须位于 `mediaBaseDir = resolve(mediaRootDir/mediaRelativePrefix)`（即请求中的 `mediaDir`）之内**，否则返回 null、该消息退回文本占位（ctx:4158-4161）；
  4. 最终写入格式文件的引用 = `relative(mediaRootDir, sourcePath)`，分隔符统一 `/`（ctx:4162），再按 `exportPathStyle` 转成 `\`（windows）或保持 posix（ctx:287-298）。
  - **结论：JSONL 行里的 media_path 应为绝对路径（或相对引擎 CWD 可解析的路径），指向 mediaDir 内的实际文件；WeFlow 自己换算成相对最终输出文件的引用。**
- `mediaDir` 的构成（worker:399-406 + `getMediaLayout` ctx:4107-4130）：
  - 布局 B（默认）+ per-session：`<outputDir>\[texts\]\<会话目录名>\media`；
  - 布局 B + shared：`<exportBaseDir>\media\<输出文件基名>`；
  - 布局 A（最终文件在 `texts\` 子目录）：mediaRootDir=`<outputDir>\texts`、prefix=`..`，即 `mediaDir = <outputDir>`。
  - WeLive 不需要理解这些规则，只管往收到的 `mediaDir` 里写。
- **mediaDir 内部子目录/命名**：WeFlow 在 raw 模式不强制（原地引用），但 WeFlow 自有采纳逻辑的目标布局是权威参考（ctx:2949-2990；fileNaming:14-19），建议 WeLive 直接产出同样布局，使引用路径整洁：
  - 图片：`mediaDir/images/<文件名>`
  - 语音：`mediaDir/voices/<文件名>`
  - 表情：`mediaDir/emojis/<emoji_md5><扩展名>`（无扩展名时 `.gif`，ctx:2977-2981）
  - 视频：`mediaDir/videos/<文件名>`（封面惯例 `<名>_thumb.jpg`，参见 ctx:3661 `_thumb` 处理）
  - 文件附件：`mediaDir/file/<扩展名小写目录，无扩展名用 'no-extension'>/<清洗后原文件名>`（非法字符 `[\\/:*?"<>|]`→`_`，ctx:2961-2985；fileNaming:14-19）
- 大小限制：`options.maxFileSizeMb` 仅对 file 类生效（ctx:2970-2973，超限跳过）；raw 模式下实际由 WeLive 自行执行（WeFlow 不再 stat 媒体文件——**未确定**，建议 WeLive 遵守）。
- **产物上报**：每个写入 mediaDir 的文件/目录应发 `created_file`/`created_dir` 事件（worker:464-468 → main.ts 任务清理登记）。漏报时只有"取消/停止"场景有目录快照兜底（worker:407-412、489-491、665），正常失败清理会遗漏。

#### 3.3 多会话批（纯文本导出）

- `sessionIds.length > 1` 时 `mediaDir` 为 `undefined`（worker:452）；此时 mediaTypes 必为空（§1.4），无需导出任何媒体。

#### 3.4 emojiCacheDir

- WeFlow 传入 `<cachePath>\Emojis` 或 `<userDataPath>\Emojis`（worker:160-169；main.ts:4551）。
- WeFlow 自身的表情缓存布局为 `<emojiCacheDir>\accounts\v2-<sha256(accountScope)前24位 hex>\`（chatService:8945-8960）。
- **raw 导出流程中 WeFlow 完全不读取该目录**（§3.2 引用的两处均 no-op/绕过）；它只是给 WeLive 的跨运行缓存提示。WeLive 应如何用（是否复用 v2-<hash> 布局、缓存什么内容）**未确定**，由 WeLive 自行定义；最保守做法是仅作为解码后表情图片的私有缓存根目录。

---

### 4 result 事件契约

事件形状（bridge:32-42），消费逻辑（bridge:230-266；worker:506-534；ctx:4261-4323、4501-4519）：

| 字段 | 类型 | 语义 / 校验 |
|---|---|---|
| `success` | boolean | `false` ⇒ 整体失败（bridge:327）。部分会话失败时应为 `true` 并配合 `failed_sessions` |
| `success_count` | number | 成功会话数；缺省回退为已收集输出路径数（bridge:343） |
| `fail_count` | number | 失败会话数；缺省回退（bridge:344） |
| `failed_sessions` | `[{session_id, error}]` | 逐项写入 `failedSessionErrors`（bridge:260-265）；error 缺省文案 'WeLive 会话导出失败' |
| `session_output_paths` | `Record<sessionId, path>` | 与 `created_file` 事件共同构成兜底：worker 取 raw 路径时 `rawSessionOutputPaths[sid] || sessionOutputPaths[sid]`（worker:507-511；bridge:241-243） |
| `raw_session_output_paths` | `Record<sessionId, rawJsonlPath>` | **每会话必需**：该会话 raw JSONL 的绝对路径。缺失 ⇒ 该会话记失败 'WeLive 未返回该会话的原始导出文件'（worker:528-533） |
| `raw_export_manifests` | `Record<sessionId, manifest>` | **每会话必需**，manifest 结构见下。缺失或 rows/bytes 非"安全整数且 ≥0" ⇒ 该会话记失败 'WeLive 未返回该会话的完整性清单'（worker:512-534；bridge:247-259 会把缺失字段归零） |
| `elapsed_ms` | number | WeFlow 不消费（bridge:41），可选 |

manifest（`WeliveRawExportManifest`，bridge:4-13）：

| 字段 | 类型 | 语义 / 校验 |
|---|---|---|
| `path` | string | 必须与 `raw_session_output_paths[sid]` resolve 后完全一致，否则 'WeLive 完整性清单路径不匹配'（ctx:4286-4295）。bridge 侧缺省回填 rawOutputMap 路径（bridge:250） |
| `rows` | number | **非空行数**（空行不计，ctx:4340-4342、4511-4518）；不匹配 ⇒ 会话失败 '行数不匹配' |
| `bytes` | number | 文件精确字节数；读取前（ctx:4296-4305）与读取后（ctx:4501-4510）各校验一次，文件在导出后被改动 ⇒ 失败。rows>0 时末字节必须为 `\n`（ctx:4306-4323） |
| `images_failed` / `voices_failed` / `videos_failed` / `emojis_failed` / `files_failed` | number? | 各类媒体导出失败计数。WeFlow 只求和为 `mediaFailures` 用于展示/统计（worker:538-551），**不影响成败**；缺省按 0（bridge:253-257） |

任何会话级校验失败都会在格式化阶段抛错并使该会话失败（ctx:4549-4567，'WeLive 原始导出数据不完整: …'），因此 **result 只能在 JSONL 完整落盘（含结尾换行、字节数定型）之后发出**。

---

### 5 progress 事件契约

事件字段（bridge:17-28）：`phase?`, `current?`, `total?`, `session_id?`, `label?`, `exported_messages?`, `estimated_total_messages?`, `phase_progress?`, `phase_total?`。`ready` 事件（`total?/output_dir?/exports_dir?/media_dir?`）也走同一映射（phase 视为 'ready'）。

WeFlow 的处理链：

1. **worker 侧重归一**（worker:469-483）：
   - `total` 被覆写为**全局** `sessionIds.length`（跨 chunk 总数）；
   - `current` = `min(全局total, chunkStart + localCurrent)`，其中 `localCurrent`：事件的 `current` 为有限数字则直接用，否则回退为 `session_id` 在本批中的索引（complete 阶段为 索引+1）（worker:470-475）。
   - **因此：`current` 应表示本批（stdin 的 sessionIds）内已完成/进行中的会话序号（0 起），`session_id` 建议每条 progress 都带**（缺 session_id 时 WeFlow 用 chunk 首会话兜底，worker:481）。
2. **`mapWeliveEventToProgress`**（worker:197-244）：
   - phase 字符串映射：`formatting`→`writing`；`complete`→`complete`；`ready`/`loading`/`initializing`/`opening_account`/`counting`→`preparing`；`parsing`/`parsed`→`exporting-media`；**其余任意值**→`exporting`（worker:212-220）。
   - 会话内小数进度：`sessionRatio = min(0.98, phase_progress / phase_total)`；`phase_progress` 缺省取 `exported_messages`，`phase_total` 缺省取 `estimated_total_messages`（worker:202-208）。
   - 展示值 `current`：complete 阶段 = 整数 current；否则 = `current + sessionRatio`（保留 3 位小数）（worker:209-211、232）。
   - 产出的内部进度对象字段：`current/total/currentSession/currentSessionId(=session_id)/phase/phaseProgress/phaseTotal/collectedMessages/exportedMessages(=exported_messages)/estimatedTotalMessages/phaseLabel(=label 或兜底文案)`（worker:231-243）。
3. **节流**：非 complete 的进度每 ≥180ms 才向主进程转发一次，complete 立即转发（worker:39、92-122）。progress 发送频率可以高于此，但没必要。

推荐 phase 取值序列（与映射表对齐）：`ready` →（`opening_account`/`counting`）→ `parsing`（会话内消息/媒体阶段，配合 `phase_progress`/`phase_total` 或 `exported_messages`/`estimated_total_messages`）→ `complete`（最后，`current` = 本批会话数）。`label` 会原样显示为进度文案（缺省用兜底文案，worker:221-229）。

---

### 附：未确定项汇总

1. `parseContent` / `compactRaw` / `sanitize` 的精确语义：WeFlow 只透传（bridge:63-66；worker:456-459），无消费代码；只能确定结果约束（§2.1 content 必须完整明文）。
2. raw JSONL 文件名/每会话文件数量的约定：WeFlow 完全依赖 `raw_session_output_paths` 回传（§3.1），无命名规则。
3. `emojiCacheDir` 的内部布局与缓存内容：raw 流程中 WeFlow 不读该目录（§3.4）；chatService 的 `accounts/v2-<hash>` 布局是 WeFlow 自用惯例，WeLive 是否必须兼容未确定。
4. `ready` 事件的 `output_dir/exports_dir/media_dir` 与 `result.elapsed_ms`：WeFlow 不消费，是否必填未确定（建议省略或如实回显）。
5. mediaDir 内部子目录结构在 raw 模式不被 WeFlow 强制（原地引用，§3.2）；`images/voices/emojis/videos/file/<ext>` 布局是从 WeFlow 非 raw 采纳逻辑（ctx:2949-2990）推断的推荐值。
6. `options.maxFileSizeMb` 在 raw 模式的执行方：WeFlow 侧对应逻辑（ctx:2970-2973）在 raw 模式不会运行，应由 WeLive 执行，但无代码强制。
7. WeLive 是否应按 `options.dateRange/senderUsername` 预过滤：WeFlow 读取时会再过滤（ctx:4357-4361、4390），预过滤不违反校验，但契约上以"全量导出 + manifest 自洽"为准（§1.4）。
8. 进度 `current` 的精确基准（0 起已完成数 vs 1 起进行中数）：代码只做数值回退与钳制（worker:470-481），无强校验；按 §5 推荐语义实现即可。
