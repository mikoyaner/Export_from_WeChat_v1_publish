# key-provider —— WeFlow `keyProviderPath` 组件

WeFlow 把"取密钥"完全外包给用户自备的外部可执行文件（`electron/services/keyProviderService.ts`）。
本 crate 就是那个可执行文件：它以 WeFlow 约定的 NDJSON 协议，把用户**事先以已授权方式取得、
并存入本地密钥库**的 SQLCipher DB 密钥 / `.dat` 图片密钥，**经真实库文件校验后**分发给 WeFlow。

> 安全边界：本组件**不实现、也不调用**任何从微信进程内存提取密钥的功能。
> 它只做三件事：保管（本地库）、校验（用 SQLCipher 打开你自有的 `.db`）、分发（NDJSON 协议）。
> 参考仓库 `r266-tech/wxkey` 的内存扫描步骤不在本组件范围内——那一步由你在自有设备/账号上
> 用你选择的已授权工具完成，拿到 64-hex 后用 `import` 存入即可。

## 构建

```bash
cargo build --release -p key-provider
# 产物: native/target/release/key-provider.exe（cargo workspace 根的 target/，整目录搬到哪都不用改）
```

## 与 WeFlow 对接

1. 获取 64-hex SQLCipher 密钥（你自有微信、自有机器、已登录，用你选择的已授权方式）。
2. 一次性导入并本地校验：
   ```bash
   key-provider.exe import --key <64hex> --account <wxid> --db-path "<微信数据根>\\\\<wxid>\\db_storage\\session\\session.db"
   ```
   `--db-path` 会用 SQLCipher 真正打开该库验证密钥，通过才落盘（也可省略，但建议首次带上）。
3. WeFlow 设置里把 **`keyProviderPath`** 指向 `key-provider.exe` 的绝对路径。
4. 之后 WeFlow 每次"自动获取密钥"都会拉起本 exe，它从本地库取候选、对 WeFlow 传入的 `dbPath`
   做 SQLCipher 校验，回 `{"type":"result","success":true,"key":"<64hex>","accountId":"..."}`。

## 协议（WeFlow ↔ 本 exe）

- 拉起：`spawn(exe, [], { stdio:['pipe','pipe','pipe'] })`（无额外参数 + stdin 管道 → 自动协议模式）。
- stdin：一行 JSON 请求，如 `{"action":"get_db_key","dbPath":"...","accountId":"...","internalDbKeyHex":"..."}`。
- stdout：0..n 行 `{"type":"progress","message","level"}`，随后**恰好一行** `{"type":"result",...}`。
- 支持 action：`get_db_key`、`get_image_key`；`scan_image_key_memory` 明确拒绝（本组件不扫内存）。

## 管理命令

| 命令 | 作用 |
|---|---|
| `import --key <64hex> [--account <wxid>] [--db-path <.db\|目录>]` | 导入 DB 密钥；带 `--db-path` 时先校验再落盘 |
| `image --xor <十进制\|0x..> [--aes <hex>] [--dat <样例.dat>] [--account <wxid>]` | 导入图片密钥；带 `--dat` 时解密校验 |
| `doctor --db-path <.db\|目录> [--key <64hex>] [--account <wxid>]` | 用指定/库内候选逐个尝试打开目标库，脱敏打印结果 |
| `list` | 列出本地密钥库（密钥脱敏） |

## 密钥库位置（优先级）

1. 环境变量 `WEFLOW_KEY_STORE`（完整文件路径）
2. 可执行文件同目录 `key-store.json`（若存在）
3. `%APPDATA%\weflow-key-provider\store.json`（Unix: `~/.config/weflow-key-provider/store.json`）

多账号：按 `accountId` 分键存储；取密钥时候选顺序为 指定账号 → `__default__` → 其余，
第一个能通过 `dbPath` SQLCipher 校验者胜出（校验选择，非穷举提取）。
