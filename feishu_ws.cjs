/**
 * 飞书长连接入口（门槛六）。
 *
 * 用官方 SDK 的 WebSocket 长连接收事件：本机**不需要公网 IP、不用内网穿透、不开监听端口**，
 * 由本机主动连飞书。收到白名单内发来的文本命令 → 异步子进程调 python feishu_bridge.py →
 * 同一条消息回复状态摘要；A/B/C 正文按设计走企微 webhook（pipeline 里推）。
 *
 * 并发：最多同时 `REPLY_MAX_CONCURRENCY`(默 3) 条链路，再多进 `REPLY_MAX_QUEUE`(默 3) 队列，
 * 队列也满就当场回一句"先不接"，每条的排队位置与用时都写进 ack/日志。
 *
 * 环境变量（只存本机，绝不入库/不打日志明文）：
 *   REPLY_FEISHU_APP_ID      飞书自建应用 App ID（cli_xxx）
 *   REPLY_FEISHU_APP_SECRET  App Secret —— 开启「事件订阅 → 长连接」，
 *                            并订阅 im.message.receive_v1、开通机器人能力与 im:message 收发消息权限
 *   REPLY_FEISHU_ALLOW_OPEN_IDS  逗号分隔的 open_id 白名单。**必填**：
 *                            没白名单等于任何同事都能驱动你的微信导出链路，因此拒绝启动。
 *   REPLY_FEISHU_ALLOW_GROUP_CHAT_IDS  逗号分隔的群 chat_id；**默认空＝只在单聊里响应**。
 *                            状态行含联系人显示名，群里回就等于把它公开给全群。
 *   REPLY_PY_BIN             python 可执行（默认 python）
 *   REPLY_BRIDGE_PY          feishu_bridge.py 路径（默认与本文件同目录）
 *   REPLY_BRIDGE_TIMEOUT_MS  单次链路超时（默认 900000；pipeline 会按剩余预算夹紧各步超时）
 *
 * 链路以**异步子进程**运行：spawnSync 会冻住事件循环，SDK 的 ws 心跳发不出去，
 * 飞书判掉线并重连，长任务几乎必然丢回复。
 *
 * 启动：node feishu_ws.cjs      （Ctrl+C 停；断线由 SDK 自动重连）
 */
const fs = require('fs');
const path = require('path');
const { spawn } = require('child_process');

/** 读本机 .env（真实环境变量优先）。不读的话会出现最难查的失败：
 *  key 明明填在文件里，程序却静默按「未配置」走壳子。与 pipeline.py 的 _load_env_files 对齐。 */
function loadEnvFiles() {
  const found = [];
  const dirs = [__dirname, process.env.APPDATA ? path.join(process.env.APPDATA, 'reply-assistant') : null]
    .filter(Boolean);
  for (const dir of dirs) {
    const p = path.join(dir, '.env');
    if (!fs.existsSync(p)) continue;
    for (const line of fs.readFileSync(p, 'utf8').replace(/^\uFEFF/, '').split(/\r?\n/)) {
      const s = line.trim();
      if (!s || s.startsWith('#')) continue;
      const i = s.indexOf('=');
      if (i < 1) continue;
      const k = s.slice(0, i).replace(/^export\s+/, '').trim();
      const v = s.slice(i + 1).trim().replace(/^["']|["']$/g, '');
      if (k && !process.env[k]) process.env[k] = v;
    }
    found.push(p);
  }
  return found;
}
const envFiles = loadEnvFiles();

function expandEnv(value) {
  let s = String(value || '').trim();
  s = s.replace(/%([^%]+)%/g, (_, k) => process.env[k] || `%${k}%`);
  if (s === '~' || s.startsWith('~/') || s.startsWith('~\\')) {
    s = path.join(process.env.USERPROFILE || process.env.HOME || '', s.slice(1));
  }
  return s;
}

/** 相对路径统一按项目根（__dirname）解析，不按当前工作目录。 */
function resolvePath(value, fallback = '') {
  let s = expandEnv(value || fallback);
  if (!s) return '';
  if (!path.isAbsolute(s)) s = path.join(__dirname, s);
  return path.normalize(s);
}

const Lark = require('@larksuiteoapi/node-sdk');

const appId = process.env.REPLY_FEISHU_APP_ID || '';
const appSecret = process.env.REPLY_FEISHU_APP_SECRET || '';
const allowOpenIds = (process.env.REPLY_FEISHU_ALLOW_OPEN_IDS || '')
  .split(',').map((s) => s.trim()).filter(Boolean);
// 群聊默认关闭。要放行就填群 chat_id（oc_xxx），逗号分隔
const allowGroupChatIds = (process.env.REPLY_FEISHU_ALLOW_GROUP_CHAT_IDS || '')
  .split(',').map((s) => s.trim()).filter(Boolean);
const pyBin = process.env.REPLY_PY_BIN || 'python';
const bridgePy = resolvePath(process.env.REPLY_BRIDGE_PY || 'feishu_bridge.py');
// 导出 + 只读取密钥 + 语音 ASR + 两次云端调用，一条给足余量
const BRIDGE_TIMEOUT_MS = Number(process.env.REPLY_BRIDGE_TIMEOUT_MS || 900000);
const MAX_REPLY_CHARS = 3500;
// 并发闸门：同时最多跑几条链路，再多进队列；队列也满就当场拒绝（宁可不跑，
// 也别让十几个导出进程 + 云端调用同时挤在一台上把机器打穿）。
const MAX_RUN = Math.max(1, Number(process.env.REPLY_MAX_CONCURRENCY || 3));
const MAX_QUEUE = Math.max(0, Number(process.env.REPLY_MAX_QUEUE || 3));

// 账号探针（纯 stat，不开库）：入口启动时 + 每 REPLY_ACCOUNT_POLL_MS 看一次「现在登录的是谁」。
// 换号后旧 store 里没有新号密钥，链路会读不到数据 —— 这里负责立刻说出来并自动重扫。
const exporterBin = resolvePath(process.env.REPLY_EXPORTER_BIN || 'bin/reply-exporter.exe');
const dataRoot = resolvePath(process.env.REPLY_DATA_ROOT || '');
const wordlistDir = resolvePath(
  process.env.REPLY_WORDLIST_DIR || process.env.REPLY_CONFIG_DIR || 'config');
const ACCOUNT_POLL_MS = Math.max(15000, Number(process.env.REPLY_ACCOUNT_POLL_MS || 60000));
const NOTIFY_SWITCH = (process.env.REPLY_NOTIFY_ACCOUNT_SWITCH || '1') !== '0';
const ACCOUNT_STATE = path.join(process.env.APPDATA || __dirname, 'reply-assistant', 'last_account.json');

const ts = () => new Date().toTimeString().slice(0, 8);
// 日志必须落盘：入口一旦被关掉/被回收，"发消息没回复"就只有靠这文件才能分清
// 是消息没到（连接断了）、还是到了但链路慢/挂了。窗口里看不看得见不重要。
const LOG_DIR = resolvePath(process.env.REPLY_LOG_DIR || 'logs');
const LOG_FILE = path.join(LOG_DIR, 'feishu_entry.log');
// feishu_entry.log 含命令原文，会无限增长（media_diag.log 由 Rust 侧自己轮转）。
// 每 200 行查一次大小，超过 5MB 就把旧的改名成 .1（Windows 上 rename 不能覆盖，先 unlink）。
const LOG_MAX_BYTES = Number(process.env.REPLY_LOG_MAX_BYTES || 5 * 1024 * 1024);
let _logWrites = 0;
function fileWrite(line) {
  try {
    fs.mkdirSync(LOG_DIR, { recursive: true });
    fs.appendFileSync(LOG_FILE, line + '\n', 'utf8');
    if (++_logWrites % 200 === 0) {
      try {
        if (fs.statSync(LOG_FILE).size > LOG_MAX_BYTES) {
          try { fs.unlinkSync(LOG_FILE + '.1'); } catch { /* 不存在就跳过 */ }
          fs.renameSync(LOG_FILE, LOG_FILE + '.1');
        }
      } catch { /* 轮转失败不影响写日志 */ }
    }
  } catch (e) {
    // 写日志失败绝不能带走链路
  }
}
function log(...a) {
  console.log(`[${ts()}] [feishu]`, ...a);
  fileWrite(`[${new Date().toISOString().slice(0, 19).replace('T', ' ')}] ` + a.join(' '));
}
function logErr(...a) {
  console.error(`[${ts()}] [feishu]`, ...a);
  fileWrite(`[${new Date().toISOString().slice(0, 19).replace('T', ' ')}][ERR] ` + a.join(' '));
}

function die(msg) {
  logErr(msg);
  process.exit(1);
}

if (!appId || !appSecret) {
  die('缺少 REPLY_FEISHU_APP_ID / REPLY_FEISHU_APP_SECRET。' +
      '在飞书开放平台自建应用里拿，并把「事件订阅」改成使用长连接、订阅 im.message.receive_v1。');
}
if (allowOpenIds.length === 0) {
  die('缺少 REPLY_FEISHU_ALLOW_OPEN_IDS（逗号分隔的 open_id 白名单）。' +
      '第一次启动可以先填占位值（如 o_todo），给机器人发一条消息，' +
      '从日志的「忽略消息（sender_not_allowed）open_id=…」里复制真值再重启。');
}

const client = new Lark.Client({ appId, appSecret });

/** 启动自检：把"填了却没生效"这类静默失效当场说出来（只报槽位是否配齐，绝不报值）。
 *  踩过的坑：凭据写在项目内 .env 而代码不读 .env ⇒ 一路按「未配置」走壳子，
 *  看起来链路是通的、其实一个请求都没发。 */
function selfCheck() {
  const g = (k) => (process.env[k] || '').trim();
  const slots = {
    文本模型: !!(g('REPLY_LLM_BASE_URL') && g('REPLY_LLM_API_KEY') && g('REPLY_LLM_MODEL')),
    多模态: !!(g('REPLY_VLM_BASE_URL') && g('REPLY_VLM_API_KEY') && g('REPLY_VLM_MODEL')),
    企微回程: !!g('REPLY_WECOM_WEBHOOK'),
    飞书凭据: !!(g('REPLY_FEISHU_APP_ID') && g('REPLY_FEISHU_APP_SECRET')),
  };
  const missing = Object.entries(slots).filter(([, ok]) => !ok).map(([k]) => k);
  log(`自检：env 来源 ${envFiles.join(' + ') || '（没找到 .env，只用进程环境变量）'}`);
  log(`自检：槽位 ${Object.entries(slots).map(([k, v]) => `${k}${v ? '✓' : '✗'}`).join(' ')}`);
  for (const [label, p] of Object.entries({ 导出器: exporterBin, 数据根: dataRoot,
                                             词表目录: wordlistDir, Python桥: bridgePy })) {
    log(`路径：${label}=${p} ${fs.existsSync(p) ? '✓' : '✗'}`);
  }
  const pathMissing = [
    [exporterBin, '导出器'],
    [dataRoot, '微信数据根'],
    [wordlistDir, '人名词表目录'],
    [bridgePy, 'Python 桥'],
  ].filter(([p]) => !fs.existsSync(p))
    .map(([p, label]) => (!p ? `${label} 未配置（.env 里填，如 REPLY_DATA_ROOT）` : `${label} ${p}`));
  if (pathMissing.length) {
    logErr(`⚠ 路径不存在：${pathMissing.join('；')}。检查 .env 的 REPLY_DATA_ROOT / 项目是否完整。`);
  }
  if (missing.length) {
    logErr(`⚠ 还没配齐：${missing.join('、')}。缺的那段会**静默跳过**而不是报错` +
           '（比如没配文本模型就只打印 prompt、不出 A/B/C）。');
  }
}

function probeAccount() {
  return new Promise((resolve) => {
    if (!fs.existsSync(exporterBin)) {
      resolve({ error: `找不到导出器 ${exporterBin}` });
      return;
    }
    if (!fs.existsSync(dataRoot)) {
      resolve({ error: `找不到微信数据根 ${dataRoot}` });
      return;
    }
    const p = spawn(exporterBin, [dataRoot, '--whoami'], { windowsHide: true });
    let out = '';
    p.stdout.on('data', (d) => (out += d));
    p.on('error', () => resolve(null));
    const timer = setTimeout(() => { try { killTree(p.pid); } catch { /* ignore */ } }, 20000);
    p.on('close', () => {
      clearTimeout(timer);
      try { resolve(JSON.parse(out.trim())); } catch { resolve(null); }
    });
  });
}

function accountText(w) {
  const login = (w && w.login) || {};
  const acc = w && w.account;
  const nick = acc && acc.nick_name ? acc.nick_name : '（未取到昵称）';
  const alias = acc && acc.alias ? acc.alias : '—';
  const tail = login.live_tail || (acc && acc.wxid ? acc.wxid.slice(-4) : '?');
  const key = login.live_has_key ? '密钥✓' : '密钥✗（下一条命令会自动重扫）';
  return `账号：${nick}｜微信号：${alias}｜wxid…${tail}｜${key}`;
}

function readLastAccount() {
  try { return JSON.parse(fs.readFileSync(ACCOUNT_STATE, 'utf8')); } catch { return null; }
}

function saveLastAccount(dir, login) {
  try {
    fs.mkdirSync(path.dirname(ACCOUNT_STATE), { recursive: true });
    fs.writeFileSync(ACCOUNT_STATE, JSON.stringify({
      dir, tail: login && login.live_tail, at: new Date().toISOString(),
    }, null, 2), 'utf8');
  } catch (e) { logErr(`记录账号状态失败：${e.message}`); }
}

function runRefreshKeys() {
  const py = process.env.REPLY_PY_BIN || 'python';
  const p = spawn(py, [path.join(__dirname, 'pipeline.py'), '--refresh-keys'], {
    cwd: __dirname, windowsHide: true,
    env: { ...process.env, PYTHONIOENCODING: 'utf-8' },
  });
  let err = '';
  p.stderr.on('data', (d) => (err += d));
  p.on('close', (code) => {
    log(code === 0 ? '密钥重扫完成' : `密钥重扫未成功（exit ${code}），发「状态」看详情`);
    if (err.trim()) logErr('重扫输出：' + err.trim().split('\n').slice(-3).join(' | '));
  });
  setTimeout(() => { try { killTree(p.pid); } catch { /* ignore */ } }, 12 * 60 * 1000).unref();
}

async function notifyOwners(text) {
  let sent = 0;
  for (const rid of allowOpenIds) {
    try {
      await client.im.message.create({
        params: { receive_id_type: 'open_id' },
        data: { receive_id: rid, msg_type: 'text', content: JSON.stringify({ text }) },
      });
      sent += 1;
    } catch (e) {
      logErr(`切换通知发送失败：${describeErr(e)}`);
    }
  }
  if (sent) log(`切换通知已发送（${sent} 个 open_id）`);
}

async function checkAccountSwitch({ announce = false } = {}) {
  const w = await probeAccount();
  if (!w || !w.login || w.login.available !== true) {
    if (announce) logErr('自检：读不到本机登录账号（' + ((w && w.error) || '导出器无输出') + '）');
    return w;
  }
  const live = w.login.live_account || '';
  const prev = readLastAccount();
  if (prev && prev.dir && live && prev.dir !== live) {
    const from = prev.tail || '?';
    const to = w.login.live_tail || '?';
    logErr(`账号已切换：尾号 ${from} → ${to}；正在自动重扫密钥…`);
    runRefreshKeys();
    if (NOTIFY_SWITCH) {
      await notifyOwners(`微信账号已切换：尾号 ${from} → ${to}。正在自动重扫密钥；` +
                         '若回复读不到数据，发「状态」看详情。');
    }
  }
  if (announce) log('自检：' + accountText(w));
  if (live) saveLastAccount(live, w.login);
  return w;
}
// 飞书会重投事件，按 message_id 去重。用 Map 存时间戳并按 LRU 淘汰：
// 以前是 Set + 满 500 条 clear() —— 一次清空等于把最近 500 条里除最新一条以外的
// 全部忘掉，长跑进程遇到重投就会重放历史命令（重复导出、重复计费、重复推送）。
// **并且要落盘**：只在内存里的话，入口重启 Map 就空了，而飞书断线重连时 SDK 可能重投
// 断线期间的事件 —— 同一条「回复 张三 30」会被当成新命令再跑一遍（重复导出/计费/推送）。
// 用 JSONL 追加（一行一个 {i,t}），启动时读回最近 SEEN_MAX 条；文件过大只读尾部。
const SEEN_FILE = path.join(process.env.APPDATA || __dirname, 'reply-assistant', 'seen_ids.jsonl');
const seen = new Map();
const SEEN_TTL_MS = 24 * 3600 * 1000;
const SEEN_MAX = 2000;

function loadSeen() {
  try {
    if (!fs.existsSync(SEEN_FILE)) return 0;
    const st = fs.statSync(SEEN_FILE);
    let buf;
    if (st.size > 2 * 1024 * 1024) {   // 只读尾部 2MB；第一行可能是半条，解析失败会被跳过
      const fd = fs.openSync(SEEN_FILE, 'r');
      buf = Buffer.alloc(2 * 1024 * 1024);
      fs.readSync(fd, buf, 0, buf.length, st.size - buf.length);
      fs.closeSync(fd);
    } else {
      buf = fs.readFileSync(SEEN_FILE);
    }
    const cutoff = Date.now() - SEEN_TTL_MS;
    for (const line of buf.toString('utf8').split('\n')) {
      if (!line.trim()) continue;
      try {
        const o = JSON.parse(line);
        if (o && o.i && Number(o.t) >= cutoff) seen.set(String(o.i), Number(o.t));
      } catch { /* 半行/坏行跳过 */ }
    }
    while (seen.size > SEEN_MAX) seen.delete(seen.keys().next().value);
    return seen.size;
  } catch { return 0; }
}

function recordSeen(id) {
  const t = Date.now();
  seen.set(id, t);
  const cutoff = t - SEEN_TTL_MS;
  for (const [k, ts] of seen) {   // Map 按插入顺序：先删过期，再按最旧淘汰，绝不整集清空
    if (seen.size <= SEEN_MAX && ts >= cutoff) break;
    seen.delete(k);
  }
  try {
    fs.mkdirSync(path.dirname(SEEN_FILE), { recursive: true });
    fs.appendFile(SEEN_FILE, JSON.stringify({ i: id, t }) + '\n', () => {});
  } catch { /* 落盘失败不影响收消息 */ }
}

loadSeen();
let running = 0;
const waiting = [];

function truncate(s) {
  const t = (s || '').trim();
  return t.length > MAX_REPLY_CHARS ? `${t.slice(0, MAX_REPLY_CHARS)}\n…（已截断）` : t;
}

/** 把飞书的业务码翻成人话。SDK 的 e.message 只有 "Request failed with status code 400"，
 *  真原因在 e.response.data 里——不看它就等于"发了没反应"这种查不动的故障。 */
function describeErr(e) {
  const d = (e && e.response && e.response.data) || {};
  const code = d.code !== undefined ? d.code : (d.errcode !== undefined ? d.errcode : '');
  const msg = d.msg || d.errmsg || e.message || '';
  let hint = '';
  if (String(code) === '99991401') {
    hint = ' ⇒ 飞书应用的「IP 白名单」挡住了本机出口 IP：开发者后台 → 安全设置(IP 白名单) 里加上本机公网 IP（或清空该白名单）。'
         + ' 注意：事件推送的长连接不受影响，所以现象是"命令跑通了但回复发不出去"。';
  } else if (String(code) === '99991663' || /app.*secret/i.test(msg)) {
    hint = ' ⇒ App Secret 不对或已重置。';
  } else if (String(code) === '230002') {
    hint = ' ⇒ 机器人没有该会话的发言权限（检查 im:message 收发权限与机器人能力）。';
  }
  return `code=${code || '?'} msg=${msg}${hint}`;
}

function splitText(text, limit) {
  if (text.length <= limit) return [text];
  const out = [];
  let buf = '';
  for (let line of text.split('\n')) {
    if (buf && buf.length + line.length + 1 > limit) {
      out.push(buf);
      buf = '';
    }
    while (line.length > limit) {
      out.push(line.slice(0, limit));
      line = line.slice(limit);
    }
    buf += (buf ? '\n' : '') + line;
  }
  if (buf) out.push(buf);
  return out;
}

async function sendText(messageId, text) {
  const full = (text || '').trim() || '（空回复）';
  // 关系分析报告可能比单条上限长：按行切成多条依次回复，而不是悄悄截断。
  // 只有第一条带"回复"语义，其余也是同一会话的连续消息。
  const chunks = splitText(full, Math.min(MAX_REPLY_CHARS, 3200));
  for (let i = 0; i < chunks.length; i += 1) {
    const body = chunks[i];
    try {
      await client.im.v1.message.reply({
        path: { message_id: messageId },
        data: { msg_type: 'text', content: JSON.stringify({ text: body }) },
      });
      log(`回复已送达 ${body.length} 字（${i + 1}/${chunks.length}）→ ${messageId.slice(-8)}`);
    } catch (e) {
      logErr(`回复发不出去（${describeErr(e)}）`);
      logErr('链路本身可能已经跑完了：A/B/C 是按设计走企微 webhook 的，去企微群里看；本机日志也有。');
      return;
    }
  }
}

/** 有界并发池。返回这条命令的去处：立刻跑 / 排队（前面还有几条）/ 当场拒收。 */
function submit(job) {
  if (running < MAX_RUN) {
    start(job);
    return { slot: 'run', ahead: 0 };
  }
  if (waiting.length < MAX_QUEUE) {
    waiting.push(job);
    return { slot: 'queue', ahead: waiting.length };
  }
  return { slot: 'full', ahead: waiting.length };
}

function start(job) {
  running += 1;
  const t0 = Date.now();
  log(`开始一条链路，当前在跑 ${running}/${MAX_RUN}${waiting.length ? `，排队 ${waiting.length}` : ''}`);
  Promise.resolve()
    .then(job)
    .catch((e) => logErr(`链路异常：${e && e.message ? e.message : e}`))
    .then(() => {
      running -= 1;
      log(`本条结束，用时 ${((Date.now() - t0) / 1000).toFixed(1)}s；剩余在跑 ${running}/${MAX_RUN}，排队 ${waiting.length}`);
      if (waiting.length) start(waiting.shift());
    });
}

/** Windows：python 之下还有 Rust 导出器和 ffmpeg，只杀直接子进程会留孤儿
 *  （继续占着临时媒体目录、继续吃 CPU）。taskkill /T 整棵树一起收。 */
function killTree(pid) {
  try {
    if (process.platform === 'win32') {
      const k = spawn('taskkill', ['/PID', String(pid), '/T', '/F'], { windowsHide: true });
      k.on('error', () => {});
      return;
    }
    process.kill(pid, 'SIGKILL');
  } catch (e) { /* 进程已退出 */ }
}

/** 异步跑本机链路。绝不能用 spawnSync：它把 Node 事件循环整个冻住，
 *  而 SDK 每 120s 要发一次 ws ping，超时 300s ⇒ 飞书判定掉线、断开重连，
 *  命令跑完了回复也发不出去（还可能在重连后重复投递）。 */
function runBridge(text) {
  return new Promise((resolve) => {
    const child = spawn(pyBin, [bridgePy, text], {
      cwd: __dirname,
      env: Object.assign({}, process.env, { PYTHONIOENCODING: 'utf-8' }),
      windowsHide: true,
    });
    let out = '';
    let err = '';
    let timedOut = false;
    child.stdout.setEncoding('utf-8');
    child.stderr.setEncoding('utf-8');
    child.stdout.on('data', (d) => { out += d; });
    child.stderr.on('data', (d) => { err = (err + d).slice(-4000); });
    const timer = setTimeout(() => { timedOut = true; killTree(child.pid); }, BRIDGE_TIMEOUT_MS);
    child.on('error', (e) => {
      clearTimeout(timer);
      resolve({ out: '', err: e.message, code: -1 });
    });
    child.on('close', (code, signal) => {
      clearTimeout(timer);
      resolve({ out: out.trim(), err: err.trim(), code: timedOut ? 'timeout' : code, signal });
    });
  });
}

/** 入口门禁：返回 '' 才允许跑链路。
 *  群聊状态行里带联系人显示名，等于把「谁在跟谁聊」公开给全群，而白名单只约束谁能触发、
 *  管不住谁看得见回复 —— 所以群默认拒绝，要放行必须显式把 chat_id 加进白名单。 */
function gate(msg, senderOpenId) {
  if (!allowOpenIds.includes(senderOpenId)) return 'sender_not_allowed';
  if (msg.chat_type !== 'p2p' && !allowGroupChatIds.includes(msg.chat_id || '')) return 'group_not_allowed';
  return '';
}

async function runChain(messageId, text) {
  const r = await runBridge(text);
  if (r.code === 'timeout') {
    await sendText(messageId, `本机链路超过 ${Math.round(BRIDGE_TIMEOUT_MS / 1000)} 秒未返回，已连同导出子进程一起终止。`);
    return;
  }
  if (r.code === -1) {
    await sendText(messageId, `调用本机链路失败：${r.err}`);
    return;
  }
  if (r.code !== 0 && !r.out) {
    await sendText(messageId, `链路退出码 ${r.code}：${r.err || '无输出'}`);
    return;
  }
  await sendText(messageId, r.out || r.err || '链路无输出');
}

async function onMessage(data) {
  const msg = data.message || {};
  const senderOpenId = (data.sender && data.sender.sender_id && data.sender.sender_id.open_id) || '';
  const messageId = msg.message_id || '';
  if (!messageId || seen.has(messageId)) return;
  recordSeen(messageId);

  const reason = gate(msg, senderOpenId);
  if (reason) {
    // 不回内容，只记录来路，方便你把 open_id / chat_id 加进白名单
    log(`忽略消息（${reason}）open_id=${senderOpenId} chat=${msg.chat_id || ''}`);
    return;
  }
  if (msg.message_type !== 'text') {
    await sendText(messageId, '只接受文本命令，例：回复 王总 30');
    return;
  }

  let text = '';
  try {
    text = (JSON.parse(msg.content || '{}').text || '').trim();
  } catch (e) {
    await sendText(messageId, '消息内容解析失败');
    return;
  }
  // 群聊里 @机器人 的前缀会被飞书替换成 @_user_1，直接去掉引用段
  text = text.replace(/@_user_\d+/g, '').trim();
  if (!text) return;

  log(`命令：${text}`);
  const verdict = submit(() => runChain(messageId, text));
  if (verdict.slot === 'full') {
    await sendText(messageId, `这条先不接：${MAX_RUN} 条在跑、${MAX_QUEUE} 条排队（上限 ${MAX_RUN}+${MAX_QUEUE}），` +
                             `发「状态」可以看它们在忙什么，或稍后再发一次。`);
    return;
  }
  await sendText(messageId, verdict.slot === 'queue'
    ? `已收到「${text}」，排在第 ${verdict.ahead} 位（并发上限 ${MAX_RUN}，跑完自动接手）。`
    : `已收到「${text}」，开始导出→脱敏→生成（并发 ${running}/${MAX_RUN}；本机实测云端生成 24~112 秒，最长 ${Math.round(BRIDGE_TIMEOUT_MS / 1000)} 秒）。`);
}

// 被 require 时（回归测试 runBridge/gate/并发池）只暴露函数，不建长连接
module.exports = { runBridge, killTree, gate, submit, selfCheck, loadSeen, recordSeen, logFileLine: fileWrite,
                   probeAccount, accountText, checkAccountSwitch,
                   paths: () => ({ exporterBin, dataRoot, wordlistDir, bridgePy }),
                   poolStats: () => ({ running, waiting: waiting.length, MAX_RUN, MAX_QUEUE }) };

if (require.main === module) {
  selfCheck();
  checkAccountSwitch({ announce: true }).catch((e) => logErr(`账号自检失败：${e.message}`));
  setInterval(() => checkAccountSwitch({}).catch(() => { /* 轮询失败不吵 */ }), ACCOUNT_POLL_MS)
    .unref();
  const dispatcher = new Lark.EventDispatcher({}).register({
    'im.message.receive_v1': onMessage,
  });

  const ws = new Lark.WSClient({
    appId,
    appSecret,
    loggerLevel: Lark.LoggerLevel.info,
    onReady: () => log('长连接已建立，等待命令…'),
    onReconnecting: () => log('断线，正在重连…'),
    onReconnected: () => log('已重连'),
    onError: (e) => logErr(`连接错误：${e.message}`),
  });

  ws.start({ eventDispatcher: dispatcher });
  log(`启动中：env 文件 ${envFiles.map((p) => path.basename(p)).join('+') || '（无）'}，` +
      `导出器 ${exporterBin}，数据根 ${dataRoot}，` +
      `白名单 ${allowOpenIds.length} 个 open_id，群聊放行 ${allowGroupChatIds.length} 个（0=只在单聊响应），` +
      `并发上限 ${MAX_RUN}+排队 ${MAX_QUEUE}，python=${pyBin}，` +
      `媒体模式=${(process.env.REPLY_MEDIA_MODE || 'local').toLowerCase()}`);

  process.on('SIGINT', () => {
    console.log('\n[feishu] 停止');
    ws.close({ force: true });
    process.exit(0);
  });
}
