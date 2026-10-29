"""飞书命令入口适配器（门槛六）。

`handle_command(text) -> reply_text` 是纯函数：把机器人收到的文本接进来即可。
飞书这条回复只回「收到/状态/歧义候选」，AI 最终 A/B/C 走企微 webhook（见 pipeline）。
`last_result` 保存本次链路结果，供本地面板复用，避免同一条命令跑两遍（会重复打云端）。

歧义候选与「最近锁定对象」写在本机 %APPDATA%\\reply-assistant\\feishu_state.json：
feishu_ws.cjs 每条命令都是**新进程**，不落盘就没法「回复 X」→「选 2」。30 分钟过期。

命令语法（需求 §3.3）：
  回复 <联系人名/群名> [范围]      例：回复 王总 刚才那条 / 回复 张三 50
  选 <编号> 或直接发编号            上一条歧义后锁定哪个（直接发「2」也认）
  重扫                             只读内存扫描重取本机密钥（密钥失效时用）
  <自由文本>（同上下文微调）       例：更正式点、加个表情

飞书这条通道只回「状态/候选」。候选里**带微信号(wxid)**：同名联系人（实测两个备注都叫
「张三」）光靠显示名分不开，用户明确要凭据来认人。这条只发在**单聊**（群聊被
feishu_ws.cjs 的 gate 默认挡掉），可见范围就是你本人。
"""
import json
import os
import re
import time
from typing import Optional

import pipeline

HERE = os.path.dirname(os.path.abspath(__file__))
# 多轮状态（歧义候选、最近锁定的会话名）必须跨进程存活：飞书每条命令都是新 python 进程。
# 落本机 %APPDATA%\reply-assistant\，不进代码仓库；含会话显示名，30 分钟后自动作废。
STATE_PATH = os.path.join(os.environ.get("APPDATA") or HERE, "reply-assistant", "feishu_state.json")
STATE_TTL = 1800

_last_candidates: list = []
_last_name: Optional[str] = None
_last_sid: Optional[str] = None
_last_members: list = []
_last_n: int = 0
# 「补图」的意图必须跨「同名歧义 → 回编号」活下来：以前它只活在这一次函数调用的局部变量里，
# 用户选了编号就退化成普通回复（静默不补图，状态行也不提），真机链路会让人以为补过了。
_last_media_once: bool = False
_last_unlisted: list = []
last_result: Optional[dict] = None
_loaded = False


def _load_state() -> None:
    global _last_candidates, _last_name, _last_sid, _last_members, _last_n, _last_unlisted
    global _last_media_once, _loaded
    _loaded = True
    try:
        with open(STATE_PATH, encoding="utf-8") as f:
            s = json.load(f)
        if time.time() - float(s.get("ts", 0)) > STATE_TTL:
            return
        _last_candidates = s.get("candidates") or []
        _last_name = s.get("name")
        _last_sid = s.get("sid")
        _last_members = s.get("members") or []
        _last_n = int(s.get("n") or 0)
        _last_unlisted = s.get("unlisted") or []
        _last_media_once = bool(s.get("media_once"))
    except (OSError, ValueError):
        pass


def _save_state() -> None:
    try:
        os.makedirs(os.path.dirname(STATE_PATH), exist_ok=True)
        # 原子写：入口和面板可能同时写这个文件，直接 open(...,"w") 被读到半截 JSON 会丢上下文
        # （_load_state 能兜住不崩，但候选/会话确实会没）。先写 .tmp 再 replace。
        tmp = STATE_PATH + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump({"ts": time.time(), "candidates": _last_candidates,
                       "name": _last_name, "sid": _last_sid, "members": _last_members,
                       "n": _last_n,
                       "media_once": _last_media_once,
                       "unlisted": _last_unlisted},
                      ensure_ascii=False, fp=f)
        os.replace(tmp, STATE_PATH)
    except OSError:
        pass


def _n_note() -> str:
    """用户要的条数与实际窗口不一致时必须说出来，否则"要 1000 只给 30"看起来像 bug。"""
    want = _last_n
    got = pipeline.clamp_n(want) if want else 0
    return f"（条数 {want}→{got}，受 REPLY_MAX_MESSAGES 限制）" if want and want != got else ""


def _remember(res: dict, name, members) -> None:
    """任何"已经解析出会话"的结果都记住它（含被拦下的）：
    用户接着回「补昵称 / 放行」时必须有会话可重跑，否则只会绕回命令卡。"""
    global _last_name, _last_sid, _last_members, _last_candidates
    if res.get("session_id"):
        _last_name = name or _last_name
        _last_sid = res["session_id"]
        _last_members = members or []
        _last_candidates = []
    _save_state()


def _recent_blocked_session():
    """状态丢了但刚被拦过：从 last_run 里把会话捞回来（只认 30 分钟内的出境拦截）。"""
    try:
        with open(pipeline.LAST_RUN_PATH, encoding="utf-8") as f:
            d = json.load(f)
    except (OSError, ValueError):
        return None
    if time.time() - float(d.get("at") or 0) > 1800:
        return None
    if d.get("stage") != "egress_blocked" or not d.get("sid"):
        return None
    return d


# 「回复 王总 刚才那条」是需求里的原样例子：末尾这些指代/范围词不是人名的一部分，
# 不剥掉就拿「王总 刚才那条」去匹配 ⇒ 永远 no_match（此前实测必现）。
_REPLY_NOISE = re.compile(
    r"(?:刚才|刚刚|最近|最新|这条|那条|这句|那句|上一?条|下一条|前面|后面|的消息|消息|内容|的话|话)\s*$")


def _clean_name(raw: str) -> str:
    name = raw.strip()
    prev = None
    while prev != name:
        prev = name
        name = _REPLY_NOISE.sub("", name).strip()
    return name or raw.strip()


# 需求 §3.3 只写了「回复 X」，但真人会打「总结/概括/怎么回」。只收这几个高确定性的同义词：
# 「看看/整理」这类容易和意图词（"看看有没有提到价格"）撞车，故意不做成动词。
_REPLY_VERBS = "回复|总结|概括|归纳|帮我回|咋回|怎么回|如何回"
HELP_CARD = (
    "命令：\n"
    "· 回复 <名字> [条数]　例：回复 张三 30（总结/概括/怎么回 都算）\n"
    "· 群里只回某几个人：回复 <群名> 30 —张三 —李四　（也认「针对 张三、李四」；不带 —人名＝针对全群）\n"
    "· 选 <编号>　或直接发编号　（歧义后锁定是哪一个）\n"
    "· 更正式点 / 短一点 / 加个表情　（同上下文微调）\n"
    "· 重扫　只读内存扫描重取本机密钥（提示密钥打不开库时用）\n"
    "· 补昵称　把出境自查拦下的群昵称加进人名词表并自动重跑（也可「补昵称 张三 李四」）\n"
    "· 放行　确认出境自查是误报时，按原请求放行发一次（等价命令行 --send-anyway）\n"
    "· 建档 <名>　为联系人生成 15 项档案模板（背景/婚姻/关系/印象/忌讳…），填好下次自动带上\n"
    "· 打开档案 <名>　用记事本打开该联系人的档案\n"
    "· 补图 <名> [条数]　只这一条命令让微信自己补下未落地的图片（跑完自动还原）\n"
    "（1:1 且一次要 ≥200 条、又没建档案时，会先自动生成一份关系评估再出建议，多一次云端调用）\n"
    "· 状态　本机微信号 / 正在给谁出建议 / 最近一次结果与耗时（不花钱）\n"
    "· 排查　入口是否在跑、账号/密钥、上次为什么没成、下一步发什么（不花钱，不需要 AI）\n"
    "· 帮助　本条说明\n"
    "A/B/C 正文走企微群机器人，这里只回状态；AI 只出建议，发送永远是你自己。"
)


# 「名字」位如果被填成一句话，不该去查联系人并白跑 20 多秒云端。三层判据：
# 长度、句读、口语句子标记。判错了最坏结果是回一张命令卡（用户重发一次就好），
# 判错的反面是白花钱 + "没找到联系人"这种误导答复，所以宁可严一点。
_NAME_ODD = re.compile(r"[？?！!，,。;；]|\s{2,}")
_SENTENCE_MARKERS = re.compile(
    r"吗|呢|吧|要不要|是不是|能不能|可不可以|需不需要|觉得|怎么办|如何|什么|为什么|一下|这段|这条|那句|这个|那个|这些|那些")


def _looks_like_a_name(s: str) -> bool:
    return (0 < len(s) <= 16 and not _NAME_ODD.search(s)
            and not _SENTENCE_MARKERS.search(s))


# 群聊里指定要回应谁：`回复 项目群 30 —张三 —李四`（也接受 -/–/－ 与「针对 张三、李四」）。
# 只认「空白 + 破折号」起的尾巴，免得误伤名字里带横线的群（如 A-B群）。
_MEMBER_TAIL = re.compile(r"(?:^|\s)[—–\-－]\s*\S")
_MEMBER_WORDS = re.compile(r"(?:^|\s)(?:针对|只回应|只回)\s+")
_MEMBER_SPLIT = re.compile(r"[—–\-－、,，;；\s]+")


def _split_members(t: str):
    m = _MEMBER_TAIL.search(t)
    if m:
        head, tail = t[:m.start()].strip(), t[m.start():].strip()
    else:
        m2 = _MEMBER_WORDS.search(t)
        if not m2:
            return t, []
        head = t[:m2.start()].strip()
        tail = _MEMBER_WORDS.sub(" ", t[m2.start():].strip(), count=1)
    names = [x.lstrip("@") for x in _MEMBER_SPLIT.split(tail) if x.strip() and x not in ("针对", "只回应", "只回")]
    return head, names


def _parse(text: str):
    t = text.strip()
    low = t.lower().replace("—", "-")
    m = re.match(r"^(建档|新建档案|加档案)[\s:：]*(.+)$", t)
    if m:
        return ("profile_new", _clean_name(m.group(2)))
    m = re.match(r"^(打开档案|看档案|编辑档案)[\s:：]*(.+)$", t)
    if m:
        return ("profile_open", _clean_name(m.group(2)))
    if low in ("排查", "诊断", "自检", "出问题", "出问题了", "怎么办", "咋办"):
        return ("diagnose",)
    if low in ("放行", "强发", "强行发送", "send-anyway", "--send-anyway", "确认放行", "-放行") \
            or (len(low) <= 6 and low.startswith(("放行", "强发", "强行发送"))):
        return ("force",)
    m = re.match(r"^选\s*(\d+)$", t)
    if m:
        return ("pick", int(m.group(1)))
    if re.match(r"^\d{1,2}$", t):
        # 人打完一轮就会直接回「1」而不是「选 1」。之前这种被当成意图词，
        # 于是整条链路又白跑一遍（导出+脱敏+云端，20~30 秒和一次计费）——实测踩过。
        return ("pick", int(t))
    if t in ("重扫", "重扫密钥", "重新取密钥"):
        return ("rescan",)
    m = re.match(r"^(?:补昵称|补人名|加昵称)\s*(.*)$", t)
    if m:
        return ("fill", [x for x in re.split(r"[\s、,，]+", m.group(1).strip()) if x])
    if t in ("状态", "在忙什么", "忙吗", "status", "现在在干嘛"):
        return ("status",)
    if re.fullmatch(r"帮助|help|用法|指令|怎么用", t, re.I):
        return ("help",)
    # 「补图 <名>」= 只这一条命令注入微信补下（用户定的口径：不做常驻模式）
    media_once = False
    m = re.match(r"^(补图|这次补图|临时补图|补下图片|补下原图)[\s:：]+(.+)$", t)
    if m:
        media_once = True
        t = "回复 " + m.group(2)
    t_head, members = _split_members(t)
    m = re.match(rf"^(?:{_REPLY_VERBS})\s+(.+?)(?:\s+(\d+))?$", t_head)
    if m:
        name = _clean_name(m.group(1))
        if not _looks_like_a_name(name):
            return ("refine", t, members)
        n = int(m.group(2)) if m.group(2) else pipeline.N_DEFAULT
        return ("reply", name, n, members, media_once)
    # 真人会把名字放前面：「李四 回复 1000」。实测漏掉这种写法时，整句会被当成
    # "对上次会话的微调"（refine），用缓存重跑并重发旧内容——必须支持。
    m = re.match(rf"^(.+?)\s+(?:{_REPLY_VERBS})(?:\s+(\d+))?$", t_head)
    if m:
        name = _clean_name(m.group(1))
        if _looks_like_a_name(name):
            n = int(m.group(2)) if m.group(2) else pipeline.N_DEFAULT
            return ("reply", name, n, members, media_once)
    # 形似回复命令（含动词+数字）却没解析出来：宁可回一句格式提示，也别白跑一次云端
    if re.search(rf"(?:{_REPLY_VERBS})", t) and re.search(r"\d", t):
        # 「回复李四1000」这种无空格写法也支持：动词后必须跟非数字的名字，再跟数字
        m = re.match(rf"^(?:{_REPLY_VERBS})\s*([^\d\s]{{1,20}}?)\s*(\d+)$", t_head)
        if m and _looks_like_a_name(_clean_name(m.group(1))):
            return ("reply", _clean_name(m.group(1)), int(m.group(2)), members, media_once)
        return ("help_hint", t)
    return ("refine", t, members)


def _reply_text(res: dict, text: str) -> str:
    """成功路径追关系分析全文（用户要求：报告存文件 + 飞书全文）。
    但**缓存命中时不再重发**：同一份报告每条命令都贴一遍，用户会以为"又发了上次的内容"。"""
    rep = (res.get("relationship_report") or "").strip()
    rel = ((res.get("status") or {}).get("relationship") or {})
    if rep and not rel.get("cached"):
        # 报告里"关键证据"引用的是聊天原话（含对方的话），不加标题会被误读成"回复建议"。
        return (text + "\n\n——— 关系分析报告（引用的是聊天原话，不是回复建议；"
                "A/B/C 已发企业微信）———\n" + rep)
    return text


def _summary(res: dict) -> str:
    st = res["status"]
    v = st["vlm"]
    m = st.get("masks") or {}
    seg = []
    if m.get("dict_term_hits"):
        seg.append(f"词表替换 {m['dict_term_hits']} 处")
    if m.get("regex_hits"):
        seg.append(f"规则替换 {m['regex_hits']} 处")
    seg.append(f"登记名字 {m.get('names_registered', 0)} 个")
    bits = [f"导出 {st['exported']} 条", "脱敏：" + "、".join(seg)]
    if st.get("mine") is not None:
        # 本人/对方计数是"答错人"的哨兵：群聊里 mine 恒为 0 就说明本人身份判定出问题了
        bits.append(f"其中我 {st['mine']} 条、对方 {st['others']} 条")
    if st.get("targets"):
        bits.append("本轮只针对：" + "、".join(st["targets"]))
    if v.get("read_failed"):
        bits.append(f"⚠ {v['read_failed']} 张图片本机读取失败（已跳过，其余照常解读）")
    if v.get("skipped_oversized"):
        bits.append(f"图片 {v['skipped_oversized']} 张超字节上限未发（见 REPLY_VLM_MAX_IMAGE_BYTES）")
    if v.get("batches"):
        seg = f"图片逐张挂归属 {v.get('described', 0)}/{v.get('images_selected', 0)} 张（分 {v['batches']} 批）"
        if v.get("empty"):
            seg += f"、{v['empty']} 张模型没给出内容"
        if v.get("errors"):
            seg += f"、{len(v['errors'])} 批请求失败"
        if v.get("over_cap"):
            # 候选数与实发数对不上时必须说出来：真机 4 图 + 9 表情 = 13 个候选、上限 12，
            # 不说就等于悄悄丢一张，而下面那行"本机解出图片文件 4 张"又不是发给模型的数量。
            seg += f"；另有 {v['over_cap']} 个候选超出上限没送（REPLY_VLM_MAX_IMAGES）"
        bits.append(seg)
    md = st.get("media_diag") or {}
    if md.get("decrypt_failed"):
        bits.append(f"⚠ {md['decrypt_failed']} 张图片解不开（不是没下载；见 logs/media_diag.log）")
    if md.get("voice_global_rejected"):
        bits.append(f"{md['voice_global_rejected']} 条语音只在别处有同键记录，按口径未采纳")
    if st.get("layout_missing"):
        bits.append("⚠ 库布局缺：" + "、".join(st["layout_missing"]))
    route = st.get("session_route")
    if route and route != "md5":
        bits.append(f"⚠ 会话表路由降级为 {route}（正常应为 md5，见 logs/media_diag.log）")
    if st.get("session_time_fallback"):
        bits.append("⚠ 消息分表缺时间列，已退回全量读（会变慢）")
    rl = st.get("relationship") or {}
    if rl.get("triggered"):
        if rl.get("preview"):
            bits.append("关系分析：预览模式（真跑会先生成报告再出建议）")
        elif rl.get("cached"):
            bits.append(f"关系分析：复用缓存 {rl.get('chars', 0)} 字")
        elif rl.get("chars"):
            bits.append(f"关系分析：已生成 {rl['chars']} 字"
                        + (f"（{os.path.basename(rl['file'])}）" if rl.get("file") else ""))
        elif rl.get("note"):
            bits.append("关系分析未生成：" + str(rl["note"]))
    if st.get("images_decrypted"):
        # 这是"本机解出多少张图片文件"，不是"发给模型几张"——后者在上面的 9/12 那行。
        # 以前写成「图片 4 张（已解读）」，和 12 并排看就像两个互相矛盾的数。
        skip = v.get("skipped")
        why = {"budget_exhausted": "本轮时间预算不足", "vlm_not_configured": "没配多模态",
               "image_exit_consent_missing": "未同意图片出境", "all_over_limit": "全部超字节上限",
               "preview_no_egress": "预览模式不发请求"}.get(skip or "", skip or "")
        bits.append(f"本机解出图片文件 {st['images_decrypted']} 张"
                    + (f"（本轮未送多模态：{why}）" if skip else "（与表情一起排队送多模态）"))
    if st.get("emojis_fetched"):
        bits.append(f"表情取回 {st['emojis_fetched']} 张（HTTP，与图片同队送多模态）")
    if st["voices_transcribed"]:
        bits.append(f"语音转写 {st['voices_transcribed']} 条")
    elif st.get("voice_note"):
        # 语音是对方原话：转写没成必须说出来，否则 AI 只看到 [语音] 占位会当没这回事
        bits.append(st["voice_note"])
    if (st.get("media_mode") or "local").startswith("imghelper"):
        ih = st.get("imghelper") or {}
        tag = "本次临时补图" if st.get("media_mode") == "imghelper-once" else "imghelper"
        bits.append(f"媒体补下：{tag}" + (f"（已等 {ih.get('waited_s')}s，新增 {ih.get('new_files')} 个文件）"
                                        if ih.get("ok") else f"（未注入：{ih.get('error') or '未知'}）"))
    pf = st.get("profile") or {}
    if pf.get("files"):
        bits.append("档案：" + "、".join(f["name"] for f in pf["files"]) + f"（{pf.get('chars', 0)} 字）")
    elif pf.get("skipped"):
        bits.append("档案未加载：" + str(pf["skipped"][0].get("reason") or ""))
    bud = st.get("prompt_budget") or {}
    if (bud.get("segments") or 0) < 0:
        bits.append(f"prompt {bud.get('chars')} 字，真跑时会分段摘要")
    elif bud.get("segments"):
        bits.append(f"prompt 分 {bud['segments']} 段摘要后生成")
    if bud.get("trimmed_chars"):
        bits.append(f"聊天记录从最旧截断 {bud['trimmed_chars']} 字（档案未截断）")
    t = st.get("timings_ms") or {}
    if t:
        seg = []
        if t.get("export"):
            seg.append(f"导出 {t['export']/1000:.1f}s")
        if t.get("vlm"):
            seg.append(f"多模态 {t['vlm']/1000:.1f}s")
        if t.get("llm"):
            seg.append(f"AI {t['llm']/1000:.1f}s")
        if t.get("wecom_push"):
            seg.append(f"推送 {t['wecom_push']/1000:.1f}s")
        bits.append("耗时 " + "、".join(seg) + f"（合计 {t.get('total', 0)/1000:.1f}s）")
    if res["answer"] is None:
        bits.append("AI 未配置（设 REPLY_LLM_* 后出 A/B/C）")
    elif not str(res["answer"]).strip():
        bits.append("⚠ 模型返回空内容（已关思考重试仍空；见 logs/media_diag.log）")
    else:
        bits.append("已推企微" if res.get("wecom_resp") else "企微未配置，仅本机输出")
    if st.get("injection_risk"):
        # 对方正文里有「忽略以上指令」这类句式：AI 只出建议、发送由你，但这条得让你看见
        bits.append("⚠ 上下文含疑似指令句式（" + "、".join(st["injection_risk"])
                    + "），已按素材处理，粘贴前请自己读一遍")
    return (("[⚠ " + w + "]\n" if (w := res.get("account_warning") or st.get("account_warning")) else "")
            + "[调试] " + "；".join(bits) + "。")


def _keys_stale_reply(res: dict) -> str:
    e = res.get("export") or {}
    hint = (e.get("error") or e.get("message") or "keystore 里没有可打开的账号")
    return (f"本机密钥打不开微信数据库（{hint}）。"
            + ("已自动重扫一次仍失败。" if res.get("rescanned") and not res.get("keys_ok") else "")
            + "请确认微信在本机是登录运行状态，然后回「重扫」再试一次。")


def _candidates_reply(cands: list) -> str:
    lines = []
    for i, c in enumerate(cands):
        who = c.get("display") or "（无名称）"
        tail = "" if not c.get("is_group") else "（群）"
        wid = c.get("username") or "?"
        ts = c.get("last_time") or 0
        when = time.strftime("%m-%d %H:%M", time.localtime(ts)) if ts else "本机无消息"
        by = f"（按{c['matched_by']}命中）" if c.get("matched_by") else ""
        lines.append(f"{i + 1}. {who}{tail}{by}｜{wid}｜最近 {when}")
    return ("命中多个（先按匹配程度、再按备注>微信号>昵称排过序），回「选 编号」或直接发编号：\n"
            + "\n".join(lines))


def _roster_reply(res: dict) -> str:
    """「针对某成员」没解析出来：把本会话最近说过话的人列出来让人照抄，绝不猜。"""
    roster = res.get("roster") or []
    lines = [f"· {r.get('display')}" + (f"｜{r['username']}" if r.get("username") else "")
             + f"｜{r.get('count')} 条" for r in roster[:12]]
    return ("没认出你要回应的人：" + "；".join(res.get("problems") or [])
            + "\n本会话最近说过话的人：\n" + "\n".join(lines)
            + "\n照抄其中一个名字再发一次，形如：回复 <群名> 30 —"
              + (roster[0]["display"] if roster else "名字"))


def _status_reply() -> str:
    """「状态」：本机微信号 + 正在忙哪条 + 最近一次结果。只读本机文件，不跑链路、不花钱。"""
    jobs = pipeline.active_jobs()
    try:
        with open(pipeline.LAST_RUN_PATH, encoding="utf-8") as f:
            last = json.load(f)
    except (OSError, ValueError):
        last = None
    acct = (jobs[0].get("account") if jobs else "") or (last or {}).get("account") or ""
    lines = [pipeline.account_line(),
             pipeline.media_mode_line(),
             f"上次读取的账号目录：{acct or '（还没跑过，取不到）'}",
             f"正在跑的链路：{len(jobs)} 条" if jobs else "正在跑的链路：没有"]
    for j in jobs:
        who = j.get("target") or j.get("name") or "?"
        lines.append(f" · 给「{who}」出建议｜阶段：{j.get('phase')}｜已 {j.get('elapsed_s')} 秒"
                     + ("" if not j.get("dry_run") else "（预览，不发云端）"))
    if last:
        t = last.get("timings_ms") or {}
        seg = (f"最近一次：{time.strftime('%m-%d %H:%M', time.localtime(last.get('at', 0)))}"
               f"｜{last.get('stage')}｜对象「{last.get('target') or '?'}」"
               f"｜导出 {last.get('exported')} 条｜答复 {last.get('answer_chars')} 字"
               + ("｜已推企微" if last.get("wecom_pushed") else "｜未推企微"))
        if t:
            seg += f"｜导出 {t.get('export', 0)}ms、模型 {t.get('llm', 0)/1000:.1f}s、合计 {t.get('total', 0)/1000:.1f}s"
        if last.get("targets"):
            seg += f"｜只针对 {'、'.join(last['targets'])}"
        lines.append(seg)
        if last.get("findings"):
            lines.append("  ⚠ 出境自查拦下过：" + "；".join(last["findings"]))
    cfg = pipeline.config_status()
    lines.append("槽位：" + " ".join(f"{k}{'✓' if v else '✗'}" for k, v in cfg["configured"].items())
                 + f"｜图片出境同意={'是' if cfg['image_exit_consent'] else '否'}"
                 + f"｜单次出境条数上限 {pipeline.N_MAX}")
    return "\n".join(lines)


def _no_local_reply(res: dict, name) -> str:
    cands = res.get("candidates") or []
    who = "、".join((c.get("display") or c.get("username", "?")) for c in cands[:4])
    return (f"匹配到「{name}」→ {who}，但本机库里一条聊天记录都没有。"
            "（多半是这个号的历史消息没同步到这台电脑，或者你说的是另一个同号的人）"
            "换个更具体的名字/微信号再试，或回「状态」看本机登录的是哪个号。")


def _stage_reply(res: dict, name, members) -> str:
    """链路前置阶段的统一答复；返回 "" 表示阶段 ok、可以继续给摘要。
    （ambiguous 在调用方先处理，因为只有那里要存候选。）"""
    global _last_unlisted
    # 顺手记下"这次拦下的未入表昵称"，供下一条「补昵称」直接取用（跨进程走状态文件）
    _last_unlisted = res.get("unlisted_nicks") or []
    st = res["stage"]
    out = ""
    if st == "keys_stale":
        out = _keys_stale_reply(res)
    elif st == "members_unresolved":
        out = _roster_reply(res)
    elif st == "no_local_messages":
        out = _no_local_reply(res, name)
    elif st == "no_match":
        out = f"没找到联系人/群：{name}（注意区分群备注与群内昵称）"
    elif st == "egress_blocked":
        out = ("出境自查拦下了：" + "；".join(res.get("findings") or [])
               + "\n一条内容都没发给云端。回「补昵称」把列出的名字加进词表并自动重跑；"
                 "确认是误报就回「放行」。")
    elif st != "ok":
        out = f"链路停在 {st} 阶段，重新「回复 <名>」试试。"
    return out + _acct_note(res) if out else ""


def _acct_note(res: dict) -> str:
    """读的是旧号还是当前登录号——不一致就追一行，别让用户拿旧号答案当新号的。"""
    w = res.get("account_warning") or (res.get("status") or {}).get("account_warning") or ""
    return f"\n⚠ {w}" if w else ""


def handle_command(text: str) -> str:
    global _last_candidates, _last_name, _last_sid, _last_members, _last_n, _last_unlisted
    global _last_media_once, last_result
    if not _loaded:
        _load_state()
    kind, *rest = _parse(text)
    if kind == "help":
        return HELP_CARD
    if kind == "help_hint":
        return ("这条没被认成命令。格式是「回复 <名字> <条数>」，例如「回复 李四 1000」；"
                "名字放前面也可以：「李四 回复 1000」。")
    if kind == "status":
        return _status_reply()
    if kind == "diagnose":
        return pipeline.diagnose()
    if kind in ("profile_new", "profile_open"):
        import profiles
        who = rest[0] if rest else ""
        if not who:
            return "用法：「建档 <名>」生成模板；「打开档案 <名>」用记事本打开。"
        exp = pipeline.run_exporter(who, 1)
        if exp.get("ambiguous"):
            _last_candidates = exp.get("candidates") or []
            _save_state()
            return _candidates_reply(_last_candidates) + "\n选定后把名字写清楚再发一次「建档/打开档案」。"
        if exp.get("_exit") in (4, 5) or not exp.get("session_id"):
            return f"没找到联系人：{who}（用备注全名或微信号再试）"
        if exp.get("is_group"):
            return ("群聊不加载群档案；请对具体成员单独建档"
                    "（群里回复时用「—人名」指定，会自动带上他们的档案）。")
        meta = profiles.session_meta(exp)
        if kind == "profile_new":
            r = profiles.create_template(meta)
            if not r.get("ok"):
                return "建档失败：" + (r.get("reason") or "未知原因")
            tail = "（已存在，未覆盖）" if r.get("existed") else "（新建）"
            return (f"档案{tail}：{r['path']}\n"
                    "用记事本填写 15 个字段，空着的不影响；改完保存即可，下次「回复」自动带上。\n"
                    f"直接打开：发「打开档案 {who}」。")
        path, _raw, how = profiles.find(meta)
        if not path:
            return f"还没有「{who}」的档案；先发「建档 {who}」生成模板。"
        ok = profiles.open_in_editor(path)
        return (f"已打开：{path}" if ok else f"打开失败，请手动打开：{path}") + f"\n（匹配方式：{how}）"
    if kind == "force":
        if not (_last_sid or _last_name):
            lr = _recent_blocked_session()
            if lr:
                _last_sid, _last_name = lr.get("sid"), lr.get("name")
                _last_n = int(lr.get("n") or 0) or _last_n
        if not (_last_sid or _last_name):
            return "没有可放行的会话：先发「回复 <名>」，被出境自查拦下后再回「放行」。"
        res = pipeline.run_reply(None if _last_sid else _last_name, _last_n or pipeline.N_DEFAULT,
                                 sid=_last_sid, members=_last_members, allow_risk=True)
        last_result = res
        early = _stage_reply(res, _last_name, _last_members)
        if early:
            _remember(res, _last_name, _last_members)
            return early
        _save_state()
        return _reply_text(res, _summary(res) + _n_note()
                           + "\n（已确认放行：这次跳过了出境自查的拦截项）")
    if kind == "fill":
        terms = rest[0] if rest else []
        if not terms:
            terms = _last_unlisted
        if not terms:
            return ("现在没有待补的昵称（出境自查没拦到东西）。"
                    "被拦下时我会把名字列出来，那时回「补昵称」就一次补齐。")
        r = pipeline.add_names_to_dict(terms)
        if not r.get("ok"):
            return "补词表失败：" + (r.get("reason") or "未知原因")
        if r.get("nothing_new"):
            # 重复补同一个人很常见（上一次已经成功）。以前这里会崩在 bak 未赋值上，
            # 现在直接说清楚；也**不自动重跑**——省一次云端调用。
            return ("这些名字已经在人名词表里了，无需重复补。"
                    "如果仍被出境自查拦下，把被拦的那行原话发我（可能换了写法）。")
        msg = (f"已加进人名词表 {len(r.get('added') or [])} 个"
               + (f"，跳过 {len(r.get('skipped') or [])} 个不像人名的" if r.get("skipped") else "")
               + f"（原文件备份：{os.path.basename(r.get('backup') or '')}）。")
        if _last_sid or _last_name:
            res = pipeline.run_reply(None if _last_sid else _last_name, _last_n or pipeline.N_DEFAULT,
                                     sid=_last_sid, members=_last_members)
            last_result = res
            early = _stage_reply(res, _last_name, _last_members)
            if early:
                _remember(res, _last_name, _last_members)
                return msg + " 重跑仍被拦：" + early
            _save_state()
            return msg + " 已按同一会话重跑：" + _summary(res).removeprefix("[调试] ")
        return msg + " 现在重发那条「回复 …」即可。"
    if kind == "rescan":
        ok = pipeline.ensure_keys(True)
        base = ("已重扫本机密钥（只读内存扫描）：" + ("成功。" if ok
                else "失败——请确认微信在本机处于登录运行状态。"))
        if not (_last_sid or _last_name) or not ok:
            return base + ("" if _last_sid or _last_name else " 之前没有锁定会话，接着「回复 <名>」即可。")
        res = pipeline.run_reply(None if _last_sid else _last_name, _last_n or pipeline.N_DEFAULT,
                                sid=_last_sid, members=_last_members)
        last_result = res
        if res["stage"] != "ok":
            return base + f" 但链路仍未通过：{res['stage']}"
        return base + " 已按上次会话重跑：" + _summary(res).removeprefix("[调试] ")
    if kind == "reply":
        name, n_req = rest[0], rest[1]
        members = rest[2] if len(rest) > 2 else []
        media_once = bool(rest[3]) if len(rest) > 3 else False
        # 条数要跨「选编号 / 补昵称 / 重扫 / refine」保留：以前这些重跑都用 N_DEFAULT，
        # 用户要 1000、选了候选之后实际只导 30 条，且没有任何提示。
        _last_n = n_req
        _last_media_once = media_once
        n = pipeline.clamp_n(n_req)
        res = pipeline.run_reply(name, n, members=members, media_once=media_once)
        last_result = res
        if res["stage"] == "ambiguous":
            _last_candidates = res["candidates"]
            _last_members = members
            _last_name = None
            _last_sid = None
            _save_state()
            return _candidates_reply(_last_candidates) + (
                "\n（选编号后会临时补图一次：进入下一步时会在微信里打开目标聊天）" if media_once else "")
        early = _stage_reply(res, name, members)
        if early:
            # 被拦下也要把会话记住：否则用户回「补昵称 / 放行」时既没有会话可重跑，
            # 连"拦了谁、拦了什么词"都丢了（真机上就是这么绕回命令卡的）。
            _remember(res, name, members)
            return early
        _last_name = name
        _last_sid = res.get("session_id")
        _last_members = members
        _last_candidates = []
        _save_state()
        return _reply_text(res, _summary(res) + _n_note())
    if kind == "pick":
        idx = rest[0] - 1
        if not (0 <= idx < len(_last_candidates)):
            return "没有可编号的候选（或已过期 30 分钟），先「回复 <名>」。"
        sid = _last_candidates[idx]["username"]
        media_once, _last_media_once = _last_media_once, False
        res = pipeline.run_reply(None, _last_n or pipeline.N_DEFAULT, sid=sid,
                                 members=_last_members, media_once=media_once)
        last_result = res
        early = _stage_reply(res, _last_candidates[idx].get("display"), _last_members)
        if early:
            _remember(res, _last_candidates[idx].get("display"), _last_members)
            return early
        _last_name = _last_candidates[idx].get("display") or None
        _last_sid = res.get("session_id") or sid
        _last_candidates = []
        _save_state()
        return _reply_text(res, f"[调试] 选定 {_last_name}；" + _summary(res).removeprefix("[调试] ") + _n_note())
    # refine：锁定过会话就直接按 sid 重跑，避免同名词二次歧义
    intent_txt = rest[0] if rest else text
    asked_members = rest[1] if len(rest) > 1 else []
    if _last_sid or _last_name:
        members = asked_members or _last_members
        res = pipeline.run_reply(None if _last_sid else _last_name, _last_n or pipeline.N_DEFAULT,
                                 intent=intent_txt, sid=_last_sid, members=members)
        last_result = res
        early = _stage_reply(res, _last_name, members)
        if early:
            _remember(res, _last_name, members)
            return early
        _last_members = members
        _save_state()
        return _reply_text(res, _summary(res) + _n_note())
    # 没锁定过会话又没认出命令：直接给命令卡。含糊提示等于让人再猜一次，
    # 而猜错的代价是白跑一遍 20 多秒的云端链路。
    return HELP_CARD + f"\n\n（你刚才发的「{text[:40]}」没被认成命令，也没锁定过会话。）"


if __name__ == "__main__":
    import sys
    print(handle_command(" ".join(sys.argv[1:]) or "回复 测试"))
