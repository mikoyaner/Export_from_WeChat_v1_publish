"""微信社交关系分析模式（1:1、导出 ≥200 条、且没有联系人档案时触发）。

两步：
1) 本机先算**客观统计**（条数/比例/回复间隔/时段/断联/最后发言人/类型分布）——模型数不准 200+ 条，
   所以统计由本机给，模型只负责解读；
2) 关系分析 prompt（用户提供，存在项目内可改）→ 结构化报告 → 交飞书全文 + 存本机 Markdown；
3) 第二次调用（由 pipeline 负责）把报告当背景，照常出 A/B/C。

隐私：输入是已脱敏的上下文；报告输出会再过一次 process_text；报告文件只落本机，不进仓库。
"""

import datetime
import hashlib
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
ANALYSIS_DIR = os.path.join(os.environ.get("APPDATA") or HERE, "reply-assistant", "analysis")
PROMPT_PATH = os.path.join(HERE, "relationship_prompt.txt")
MIN_MESSAGES = int(os.environ.get("REPLY_ANALYSIS_MIN", "200") or 200)
# 分析输入比普通回复大得多（统计 + 抽样记录），报告也长；沿用 120 秒会直接超时。
ANALYSIS_TIMEOUT_S = int(os.environ.get("REPLY_ANALYSIS_TIMEOUT_S", "360") or 360)
# 实测（deepseek-v4-pro-0813）：思考会吃满 max_tokens 导致 content 为空（finish=length）。
# 默认第一次就关思考，省掉一次"思考到 3000 token"的无效调用；换模型若不认这个字段就设 0。
ANALYSIS_DISABLE_THINKING = (os.environ.get("REPLY_ANALYSIS_DISABLE_THINKING", "1").strip()
                             not in ("0", "false", "no"))
# 分析报告比 A/B/C 长得多，单独给一个 cap；推理模型的思考 token 也算在这里面。
# 实测（2026-10-21，deepseek-v4-pro，输入 10181 字）：思考一个人就吃掉 **8000/8000**，
# finish=length、content 空 —— 之前"思考常要 3~6k、8000 够用"是估的，估错了。
# 所以默认给 16000 留余量；调的依据是 logs/media_diag.log 里 `llm usage ... reasoning=` 那行。
# 设为 0 表示跟随 REPLY_LLM_MAX_TOKENS。
ANALYSIS_MAX_TOKENS = int(os.environ.get("REPLY_ANALYSIS_MAX_TOKENS", "16000") or 16000)
# 4 万字对关系评估是过量：24k 已足够覆盖 800 条里的代表性对话，且显著降低超时概率。
MAX_TRANSCRIPT = int(os.environ.get("REPLY_ANALYSIS_MAX_CHARS", "24000") or 24000)

DEFAULT_PROMPT = """你是一名专业的微信社交关系分析师。你的任务是基于用户提供的微信聊天记录文本：
1. 先自动识别双方属于哪种关系场景
2. 再用对应场景的评估标准，客观、有据地分析双方互动模式、对方态度、关系健康度
3. 输出结构化报告，不跨场景套用标准

第一步：关系类型识别（必须先做）
先判断这段聊天属于以下哪类关系（可多选，混合关系需注明主次）：
亲密关系（情侣/夫妻/暧昧）、家人亲属、朋友关系、上下级关系、同事平级、
师生关系、客户关系、商务合作、服务关系、陌生/弱关系。
判断依据：聊天内容主题、语气亲疏、话题边界。

第二步：通用行为维度分析（所有关系都必须覆盖）
A 互动频率与发起模式：双方条数占比、谁更主动、回复快慢、断联情况
B 消息质量与态度：敷衍式回应 vs 实质性回应、谁在撑话题、是否闭环、表情与语气词
C 关系对称性：条数是否失衡、主动性是否对等、是否存在单向输出、话语权
D 时间线与关系走势：时间跨度、升温/平稳/冷淡/断联、拐点、最后一条是谁发的
E 沟通效率（工作类重点）：事项是否说清、响应是否符合场景预期、有无已读不回

第三步：分场景专属信号检查
只检查第一步识别出的场景对应的信号项，不要跨场景套用。

输出格式（严格按此结构）：
## 微信聊天关系评估报告
### 一、关系识别
- 主要关系类型：（从清单选，混合注明主次）
- 判断依据：（1-2 句，引用聊天内容佐证）
### 二、关系健康度评分
- 综合评分：1-10 分
- 评分说明：（按对应场景解释）
- 对方态度：（热情主动/积极配合/礼貌客气/敷衍应付/冷淡回避/消极抵触）
- 一句话总结：（30 字以内）
### 三、关键证据
按 [互动频率]、[消息质量]、[关系对称性]、[场景专属信号]、[时间线走势] 列原文或数据
### 四、风险与隐忧
### 五、数据不足说明
无法判断的维度要明说，不要臆测。
"""


def load_prompt() -> str:
    """prompt 放项目里，用户可改；没有文件时用内置默认。"""
    try:
        with open(PROMPT_PATH, encoding="utf-8") as f:
            txt = f.read().strip()
            if txt:
                return txt
    except OSError:
        pass
    return DEFAULT_PROMPT


def _fmt_gap(sec: float) -> str:
    if sec < 60:
        return f"{int(sec)}秒"
    if sec < 3600:
        return f"{sec/60:.0f}分钟"
    if sec < 86400:
        return f"{sec/3600:.1f}小时"
    return f"{sec/86400:.1f}天"


def dialog_stats(exp: dict) -> dict:
    """本机统计（不调模型）：这些数字是第一步报告的锚点，模型自己数不准。"""
    msgs = [m for m in (exp.get("messages") or []) if m.get("create_time")]
    msgs.sort(key=lambda m: int(m.get("create_time") or 0))
    total = len(msgs)
    mine = sum(1 for m in msgs if m.get("is_me"))
    other = total - mine
    types = {}
    for m in msgs:
        t = m.get("type_label") or "other"
        types[t] = types.get(t, 0) + 1
    # 回复间隔：发送方切换时的间隔（同一人连发不计入"回复"）
    gaps = []
    for a, b in zip(msgs, msgs[1:]):
        if bool(a.get("is_me")) != bool(b.get("is_me")):
            g = int(b.get("create_time") or 0) - int(a.get("create_time") or 0)
            if g >= 0:
                gaps.append(g)
    gaps.sort()
    med = gaps[len(gaps) // 2] if gaps else 0
    long_gaps = [g for g in gaps if g >= 86400]
    # 敷衍式回应：对方 ≤3 字或纯表情
    short_other = 0
    for m in msgs:
        if m.get("is_me"):
            continue
        t = (m.get("text") or "").strip()
        if 0 < len(t) <= 3 or (t.startswith("[") and t.endswith("]")):
            short_other += 1
    # 提问未答：我发的问句后面没有对方消息
    unanswered = 0
    for i, m in enumerate(msgs):
        if m.get("is_me") and ("？" in (m.get("text") or "") or "?" in (m.get("text") or "")):
            if i + 1 >= len(msgs) or msgs[i + 1].get("is_me"):
                unanswered += 1
    first_ts = int(msgs[0]["create_time"]) if msgs else 0
    last_ts = int(msgs[-1]["create_time"]) if msgs else 0
    days = (last_ts - first_ts) / 86400 if last_ts > first_ts else 0
    last_is_me = bool(msgs[-1].get("is_me")) if msgs else False
    return {
        "total": total, "mine": mine, "other": other,
        "mine_ratio": round(mine / total, 2) if total else 0,
        "types": types,
        "first_ts": first_ts, "last_ts": last_ts, "days": round(days, 1),
        "reply_gaps": len(gaps), "median_gap_s": med,
        "median_gap_text": _fmt_gap(med) if gaps else "无",
        "long_gaps_24h": len(long_gaps),
        "short_other": short_other,
        "unanswered_questions": unanswered,
        "last_is_me": last_is_me,
    }


def stats_block(s: dict) -> str:
    """给模型看的统计块（明确声明：这些是权威数字，不要自己再数）。"""
    days = s["days"]
    return (
        "【本机统计（权威数字，直接引用；不要自己重新数）】\n"
        f"- 消息总数：{s['total']}（我 {s['mine']} / 对方 {s['other']}，我占 {int(s['mine_ratio']*100)}%）\n"
        f"- 时间跨度：约 {days:.1f} 天"
        + (f"（{datetime.datetime.fromtimestamp(s['first_ts']):%Y-%m-%d} ~ "
           f"{datetime.datetime.fromtimestamp(s['last_ts']):%Y-%m-%d}）" if s["first_ts"] else "")
        + "\n"
        f"- 类型分布：" + "、".join(f"{k} {v}" for k, v in sorted(s["types"].items(), key=lambda x: -x[1])) + "\n"
        f"- 回复间隔：共 {s['reply_gaps']} 次轮换，中位数 {s['median_gap_text']}；隔天以上 {s['long_gaps_24h']} 次\n"
        f"- 对方简短回应（≤3 字或纯表情）：{s['short_other']} 条\n"
        f"- 我发出后未获答复的问句：{s['unanswered_questions']} 条\n"
        f"- 最后一条：{'我发的（对方待回应）' if s['last_is_me'] else '对方发的'}\n"
    )


def _sample(text: str, limit: int) -> tuple:
    """超长时抽样：保留开头与结尾，中间按比例抽；返回 (文本, 是否抽样)。"""
    if len(text) <= limit:
        return text, False
    head = int(limit * 0.25)
    tail = int(limit * 0.45)
    mid_budget = limit - head - tail
    mid = text[head:len(text) - tail]
    lines = mid.splitlines()
    step = max(1, len(lines) // max(1, mid_budget // 40))
    picked = lines[::step]
    body = "\n".join(picked)
    while len(body) > mid_budget and len(picked) > 1:
        picked = picked[::2]
        body = "\n".join(picked)
    out = text[:head] + "\n…（中间按比例抽样）…\n" + body + "\n…（抽样结束）…\n" + text[len(text) - tail:]
    return out, True


def cache_paths(sid: str) -> tuple:
    key = hashlib.md5((sid or "").encode()).hexdigest()[:12]
    base = os.path.join(ANALYSIS_DIR, key)
    return base + ".md", base + ".json"


def load_cached(sid: str, last_ts: int):
    md, meta = cache_paths(sid)
    try:
        with open(meta, encoding="utf-8") as f:
            m = json.load(f)
        if int(m.get("last_ts") or 0) == int(last_ts) and os.path.isfile(md):
            with open(md, encoding="utf-8") as f:
                return f.read().strip(), m
    except (OSError, ValueError):
        pass
    return None, None


def save_report(sid: str, last_ts: int, report: str) -> str:
    md, meta = cache_paths(sid)
    os.makedirs(ANALYSIS_DIR, exist_ok=True)
    with open(md, "w", encoding="utf-8") as f:
        f.write(report.strip() + "\n")
    try:
        with open(meta, "w", encoding="utf-8") as f:
            json.dump({"sid": sid, "last_ts": last_ts,
                       "at": datetime.datetime.now().isoformat(timespec="seconds"),
                       "chars": len(report)}, f, ensure_ascii=False, indent=2)
    except OSError:
        pass
    return md
