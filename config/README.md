# config/ 说明

本目录只放**可随项目走的非隐私配置**：

- `脱敏规则.json`：正则规则、占位符模板、词表文件名。内容不含真实人名。

**人名词表不在这里**，因为含真实姓名/备注，属隐私数据。用 `.env` 的
`REPLY_WORDLIST_DIR` 指向外部私有目录，例如：

```env
REPLY_CONFIG_DIR=config
REPLY_WORDLIST_DIR=%USERPROFILE%/wechat-reply/private-wordlists
```

外部目录需要包含 `脱敏规则.json` 里 `dictionaries[].file` 指定的文件：

```
脱敏词表-人名与备注名.txt
脱敏词表-地名与地址.txt
脱敏词表-组织与项目代号.txt
```

解析规则：

- `REPLY_CONFIG_DIR` 支持绝对路径、`%VAR%`、`~` 和相对项目根；
- `REPLY_WORDLIST_DIR` 同上；
- 未设置 `REPLY_WORDLIST_DIR` 时退回 `REPLY_CONFIG_DIR`（兼容旧布局）；
- 「补昵称」写入的是 `REPLY_WORDLIST_DIR/脱敏词表-人名与备注名.txt`，不会写回项目 `config/`；
- 不要把外部词表复制进本目录或提交到 git。
