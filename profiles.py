"""联系人档案：用户手写的提示词文本，按联系人保存在本机，每次出建议时随对话一起给模型。

文件名规则（用户定的）：
  <微信号或wxid>_<备注>_<昵称>.txt；没有备注时 <微信号或wxid>_<昵称>.txt
- 微信号字段优先用 alias；没有 alias 就用 wxid（稳定，改备注不会丢）；
- 文件名里的 `\\ / : * ? " < > |` 换成 `_`；每段最长 40 字；
- 查找是**容错**的：先完整文件名 → 再按 `<微信号>_` 前缀唯一命中 → 再按 `_<昵称>.txt` 唯一命中。
  改名/改备注后仍能找到，但多个同时命中时宁可不用，也不猜错人。

隐私：档案里通常有婚姻、印象这类敏感信息。它和聊天一样要经过脱敏与出境自查才发给模型；
档案超过 1 万字时本轮不加载（RAG 检索尚未实现，见 worklog §5q）。
"""

import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))
PROFILE_DIR = (os.environ.get("REPLY_PROFILE_DIR") or "").strip() or os.path.join(HERE, "联系人档案")
MAX_PROFILE_CHARS = int(os.environ.get("REPLY_PROFILE_MAX_CHARS", "10000") or 10000)

_ILLEGAL = re.compile(r'[\\/:*?"<>|\r\n\t]')

TEMPLATE = """# 联系人档案：{name}
# 只写你愿意发给云端模型的内容；空着的字段会被忽略。保存为 UTF-8 即可。
# 改完保存，下一次「回复」自动生效。文件不要改名也行（会按微信号/昵称容错匹配）。

## 联系人背景
（职业、城市、认识多久、共同圈子）

## 婚姻 / 感情状态
（未婚 / 已婚 / 恋爱中 / 不明确）

## 和你的关系
（同事 / 客户 / 同学 / 亲戚 / 朋友；亲近程度、利益关系）

## 你对他的印象
（性格、靠谱程度、说话方式）

## 他对你的印象
（他怎么看你、是否信任、有没有过节）

## 称呼习惯
（平时怎么称呼；正式还是叫昵称）

## 忌讳话题
（不想聊的：收入、前任、政治…）

## 沟通风格
（直接 / 委婉 / 幽默 / 只谈正事；喜欢长回复还是短回复）

## 职业与工作节奏
（忙闲时段、是否常出差、能不能随时打扰）

## 家庭与重要日期
（孩子、生日、纪念日；只写你愿意发的）

## 兴趣爱好
（运动、游戏、追剧、宠物…）

## 价值观与敏感点
（政治 / 宗教 / 地域 / 消费观；有争议的写「回避」）

## 最近在忙什么
（近期项目、考试、搬家、看病…）

## 雷区与禁忌
（绝对不要提的事、不要开的玩笑）

## 共同经历 / 人情往来
（一起做过的事、欠的人情、送过的礼）
"""


def _seg(s) -> str:
    s = _ILLEGAL.sub("_", (s or "").strip())
    s = re.sub(r"\s+", " ", s).strip(" ._")
    return s[:40]


def ensure_dir() -> str:
    os.makedirs(PROFILE_DIR, exist_ok=True)
    return PROFILE_DIR


def session_meta(exp: dict) -> dict:
    """1:1 会话对应联系人的命名要素。"""
    return {"wxid": exp.get("session_id") or "",
            "alias": exp.get("session_alias") or "",
            "remark": exp.get("session_remark") or "",
            "nick": exp.get("session_nickname") or exp.get("session_display") or ""}


def member_metas(displays: list, roster: list) -> list:
    """群聊里被指定的成员：roster 有 wxid + 显示名，档案按 wxid_昵称 找。"""
    out = []
    want = {d.strip() for d in (displays or []) if d and d.strip()}
    for p in roster or []:
        disp = (p.get("display") or "").strip()
        if disp in want:
            out.append({"wxid": p.get("username") or "", "alias": "", "remark": "", "nick": disp})
    return out


def filename_for(meta: dict) -> str:
    key = _seg(meta.get("alias")) or _seg(meta.get("wxid"))
    nick = _seg(meta.get("nick"))
    remark = _seg(meta.get("remark"))
    parts = [p for p in (key, remark, nick) if p]
    return "_".join(parts) + ".txt" if parts else ""


def _read(path: str) -> str:
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return ""


def find(meta: dict):
    """返回 (path, text, how)。找不到返回 (None, "", "")。how 用于摘要里说明匹配方式。"""
    name = filename_for(meta)
    if not name:
        return None, "", ""
    d = ensure_dir()
    exact = os.path.join(d, name)
    if os.path.isfile(exact):
        return exact, _read(exact), "文件名"
    files = sorted(f for f in os.listdir(d) if f.lower().endswith(".txt"))
    prefix = _seg(meta.get("alias")) or _seg(meta.get("wxid"))
    if prefix:
        hit = [f for f in files if f.startswith(prefix + "_")]
        if len(hit) == 1:
            p = os.path.join(d, hit[0])
            return p, _read(p), "微信号前缀"
    nick = _seg(meta.get("nick"))
    if nick:
        hit = [f for f in files if os.path.splitext(f)[0].endswith("_" + nick)]
        if len(hit) == 1:
            p = os.path.join(d, hit[0])
            return p, _read(p), "昵称"
    return None, "", ""


def clean(text: str) -> str:
    """去掉模板里的 `#` 说明行和空段，只留有内容的字段。"""
    lines = []
    for ln in (text or "").splitlines():
        s = ln.rstrip()
        if s.startswith("#"):
            continue
        if s.startswith("##"):
            lines.append("")
            lines.append(s.lstrip("# ").strip())
            continue
        if s.strip().startswith("（") and s.strip().endswith("）"):
            continue  # 模板占位提示
        lines.append(s)
    out = re.sub(r"\n{3,}", "\n\n", "\n".join(lines)).strip()
    return out


def load_for(metas: list) -> dict:
    """按顺序加载多个档案（1:1 是一个；群聊是指定成员）。
    返回 {text, files:[{path,name,how,chars}], skipped:[...], chars}。"""
    info = {"text": "", "files": [], "skipped": [], "chars": 0}
    blocks = []
    seen = set()
    for m in metas:
        path, raw, how = find(m)
        if not path:
            continue
        if path in seen:
            continue
        seen.add(path)
        txt = clean(raw)
        if not txt:
            continue
        if len(txt) > MAX_PROFILE_CHARS:
            info["skipped"].append({"name": os.path.basename(path), "chars": len(txt),
                                    "reason": f"超过 {MAX_PROFILE_CHARS} 字，未加载（RAG 未启用）"})
            continue
        who = m.get("nick") or os.path.splitext(os.path.basename(path))[0]
        # 给模型的块头里不放文件名：文件名含真名/微信号，脱敏后会变成一串 [人物N]，
        # 反而让模型看不懂；来源信息只留在本机的 info["files"] 里给摘要用。
        blocks.append(f"### {who} 的档案\n{txt}")
        info["files"].append({"path": path, "name": os.path.basename(path), "how": how,
                              "chars": len(txt)})
    info["text"] = "\n\n".join(blocks)
    info["chars"] = len(info["text"])
    return info


def create_template(meta: dict) -> dict:
    """建档：不存在才创建；返回 {ok, path, existed, name}。"""
    name = filename_for(meta)
    if not name:
        return {"ok": False, "reason": "拿不到联系人的微信号/昵称"}
    d = ensure_dir()
    path = os.path.join(d, name)
    existed = os.path.isfile(path)
    if not existed:
        try:
            with open(path, "w", encoding="utf-8") as f:
                f.write(TEMPLATE.format(name=meta.get("nick") or name))
        except OSError as e:
            return {"ok": False, "reason": f"写入失败：{e}"}
    return {"ok": True, "path": path, "existed": existed, "name": name}


def open_in_editor(path: str) -> bool:
    try:
        os.startfile(path)  # noqa: S606 - 仅打开用户自己的档案文件
        return True
    except (OSError, AttributeError):
        return False
