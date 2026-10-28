"""本地脱敏层（门槛四，真实实现，纯标准库 + 随文档交付的规则文件）。

两级：词表精确匹配（最长优先）→ 正则模式匹配；规则外置在 脱敏规则.json + 三份词表。
占位符「本轮上下文内稳定映射」：同一词始终同一占位符，编号按各词表分组。

硬要求（来自需求 §5）：导出的发送者显示名 / 会话名**必须**过人名词表；
未命中不原样出境——本实现为未出现的名字**自动分配稳定 [人物N]**（比统一 [人物?] 更能让
AI 区分不同人，同样杜绝真名外泄），并记进日志提示补词表。消息正文里出现的这些名字也会被替换。

绝不记录/外发原文；日志只含「词条 → 占位符 → 次数 → 来源」。
"""
import json
import os
import re
import unicodedata
from typing import Dict, List, Optional


def _norm(s: str) -> str:
    """匹配用归一化：全半角折叠 + casefold（逐字符，与 _norm_map 同口径）。"""
    return "".join(unicodedata.normalize("NFKC", ch).casefold() for ch in s)


def _norm_map(s: str):
    """逐字符归一化，同时给出「归一化串下标 → 原文下标」映射。

    为什么需要映射：NFKC/casefold 会改变长度（½→"1⁄2"、㎏→"kg"、ß→"ss"），
    以前直接拿归一化串里的下标去切原文。实测 "½ 假名010 明天到" 被切成
    "假名[人物10]天到" —— 名字前缀留在出境文本里，而"整词是否还在"的自查
    又查不出来（词已经被拆开了）。逐字符映射保证每个归一化字符都能落回原文区间。"""
    parts, idx = [], []
    for oi, ch in enumerate(s):
        n = unicodedata.normalize("NFKC", ch).casefold()
        for _ in n:
            idx.append(oi)
        parts.append(n)
    return "".join(parts), idx


class Desensitizer:
    def __init__(self, config_dir: str, wordlist_dir: str = ""):
        self.config_dir = config_dir
        self.wordlist_dir = wordlist_dir or config_dir
        with open(os.path.join(config_dir, "脱敏规则.json"), encoding="utf-8") as f:
            self.cfg = json.load(f)
        self.skip_all = bool(self.cfg.get("skip_all"))
        self.match_order = self.cfg.get("match_order", ["dictionary", "regex"])
        # 载入各词表词条
        self.dict_terms: Dict[str, List[str]] = {}
        self.placeholder_tpl: Dict[str, str] = {}
        for d in self.cfg.get("dictionaries", []):
            if not d.get("enabled", True):
                continue
            path = os.path.join(self.wordlist_dir, d["file"])
            terms: List[str] = []
            if os.path.isfile(path):
                with open(path, encoding="utf-8") as tf:
                    for line in tf:
                        line = line.strip()
                        if not line or line.startswith("#"):
                            continue
                        line = line.split("#", 1)[0].strip()
                        if line:
                            terms.append(line)
            uniq: Dict[str, str] = {}
            for t in terms:
                uniq.setdefault(_norm(t), t)
            self.dict_terms[d["id"]] = sorted(uniq.values(), key=len, reverse=True)
            self.placeholder_tpl[d["id"]] = d["placeholder"]
        # persons 桶必须存在（即使词表为空），供自动登记导出显示名
        self.dict_terms.setdefault("persons", [])
        self.placeholder_tpl.setdefault("persons", "[人物{n}]")
        self._persons_norm = {_norm(t) for t in self.dict_terms["persons"]}
        # 本轮稳定映射：term_norm -> placeholder；计数：dict_id -> next n
        self.mapping: Dict[str, str] = {}
        self.counters: Dict[str, int] = {k: 1 for k in self.dict_terms}
        self.enabled_regex = [
            re.compile(r["pattern"], re.UNICODE) for r in self.cfg.get("regex_rules", []) if r.get("enabled")
        ]
        self.regex_repl = {r["id"]: r["replace"] for r in self.cfg.get("regex_rules", []) if r.get("enabled")}
        self._compiled_by_id = {r["id"]: re.compile(r["pattern"], re.UNICODE) for r in self.cfg.get("regex_rules", []) if r.get("enabled")}
        self.log: List[dict] = []

    def _placeholder(self, dict_id: str, term: str) -> str:
        key = _norm(term)
        if key in self.mapping:
            return self.mapping[key]
        n = self.counters.get(dict_id, 1)
        self.counters[dict_id] = n + 1
        ph = self.placeholder_tpl.get(dict_id, "[" + dict_id + "{n}]").replace("{n}", str(n))
        self.mapping[key] = ph
        return ph

    def register_person_aliases(self, names: list, source: str = "会话名/昵称") -> str:
        """同一个人有多个名字（显示名/备注/昵称/微信号）：全部指向**同一个**占位符。
        只登记显示名时，档案或正文里写昵称/微信号就会原样出境（真机已遇到）。"""
        clean = [(n or "").strip() for n in names]
        canonical = next((n for n in clean if n and n != "我"), "")
        if not canonical:
            return ""
        ph = self.register_person(canonical, source)
        for n in clean:
            if not n or n == "我" or n == canonical:
                continue
            key = _norm(n)
            if key in self.mapping:
                continue
            self.mapping[key] = ph
            if key not in self._persons_norm:
                self._persons_norm.add(key)
                self.dict_terms["persons"] = sorted(
                    self.dict_terms["persons"] + [n], key=len, reverse=True
                )
        return ph

    def register_person(self, name: str, source: str = "导出显示名") -> str:
        """登记一个显示名进人名桶：命中文件词条用其号，否则自动编号；
        同时把该名字加入人名词表，使正文里出现的同名也被替换（杜绝真名出境）。
        "我"/空名不登记（自己不需打码）。"""
        name = (name or "").strip()
        if not name or name == "我":
            return "我" if name == "我" else ""
        key = _norm(name)
        if key in self.mapping:
            return self.mapping[key]
        # 加入 persons 词表（最长优先重排），正文替换会覆盖到它
        if key not in self._persons_norm:
            self._persons_norm.add(key)
            self.dict_terms["persons"] = sorted(
                self.dict_terms["persons"] + [name], key=len, reverse=True
            )
        ph = self._placeholder("persons", name)
        self.log.append({"term": name, "placeholder": ph, "occurrences": 0, "source": source,
                         "note": "词表未覆盖，已自动编号（建议补进 脱敏词表-人名与备注名.txt）"})
        return ph

    def _apply_dict(self, text: str) -> str:
        # 归一化串 + 下标映射按"文本每变一次"重算：词表 400+ 条时逐词归一化全篇太慢。
        norm = idx = None
        for dict_id, terms in self.dict_terms.items():
            for term in terms:
                if not term:
                    continue
                if norm is None:
                    norm, idx = _norm_map(text)
                replaced, hits = self._ci_replace(text, norm, idx, term, dict_id)
                if not hits:
                    continue
                self.log.append({"term": term, "placeholder": self._placeholder(dict_id, term),
                                 "occurrences": hits, "source": f"词表:{dict_id}"})
                text = replaced
                norm = idx = None
        return text

    def _ci_replace(self, text: str, norm: str, idx: list, needle: str, dict_id: str):
        """在归一化串上定位、回填原文区间。返回 (新文本, 命中次数)。

        命中次数直接就是真实替换数（以前用 _count_ci 另算，遇到上面那种切偏会给出
        与日志不一致的数字）。找不到匹配时**不分配占位符编号**——编号按出现顺序发，
        不再按词表顺序虚高（以前 _placeholder() 作为实参先求值，400 条词表会给每个
        词条都发一个号）。"""
        nneedle = _norm(needle)
        if not nneedle or nneedle not in norm:
            return text, 0
        spans, i = [], 0
        while True:
            j = norm.find(nneedle, i)
            if j < 0:
                break
            spans.append((idx[j], idx[j + len(nneedle) - 1] + 1))
            i = j + len(nneedle)
        if not spans:
            return text, 0
        ph = self._placeholder(dict_id, needle)
        out, cur = [], 0
        for a, b in spans:
            if a < cur:  # 重叠命中（理论上不会）：跳过，别把已替换的段落再切一次
                continue
            out.append(text[cur:a])
            out.append(ph)
            cur = b
        out.append(text[cur:])
        res = "".join(out)
        # 兜底自检：替换完归一化串里若还有这个词（且不是占位符本身带的），说明切偏/漏切。
        # 这种残留"整词还在"的自查查不出来（词可能被切开），所以必须在这里记一笔让 audit 拦住。
        if nneedle in _norm(res) and nneedle not in _norm(ph):
            self.log.append({"term": needle, "placeholder": ph, "occurrences": 0,
                             "residual": True, "source": f"词表:{dict_id}"})
        return res, len(spans)

    def _apply_regex(self, text: str) -> str:
        for r in self.cfg.get("regex_rules", []):
            if not r.get("enabled"):
                continue
            cre = self._compiled_by_id.get(r["id"])
            if not cre:
                continue
            new = cre.sub(r["replace"], text)
            if new != text:
                self.log.append({"term": f"<regex:{r['name']}>", "occurrences": len(cre.findall(text)),
                                 "source": "正则"})
                text = new
        return text

    def process_text(self, text: str) -> str:
        if self.skip_all:
            return text
        for stage in self.match_order:
            if stage == "dictionary":
                text = self._apply_dict(text)
            elif stage == "regex":
                text = self._apply_regex(text)
        return text

    def desensitize_export(self, export: dict, targets=None) -> dict:
        """把 reply-exporter 的 JSON 变成可发 AI 的脱敏结构 + 上下文文本。
        targets：本轮要回应的那几个人的**占位符**（群聊里"针对某成员"用）。
        他们的行前加 ★，旁人发言保留为背景 —— 上下文不能砍，砍了就答非所问。"""
        # 先登记会话名与所有发送者显示名，保证正文里的出现也被替换
        session_ph = self.register_person_aliases([
            export.get("session_display", ""),
            export.get("session_remark", ""),
            export.get("session_nickname", ""),
            export.get("session_alias", ""),
        ], "会话名/昵称/微信号")
        for m in export.get("messages", []):
            self.register_person(m.get("sender_display", ""), "发送者显示名")
        want = set(targets or set())
        out_msgs = []
        for m in export.get("messages", []):
            disp = m.get("sender_display", "")
            # 栏位必须走 register_person（它会 strip 后用同一个 key 登记并返回占位符）。
            # 以前这里是 self.mapping.get(_norm(disp), disp) —— key 归一化口径和登记时不一致，
            # 实测有 2 个联系人名（无备注、导出器回退成原始 wxid）就这样**在发言人栏位原样出境**，
            # 而同一个词在正文里被替换得好好的：两处口径不同，漏的就是这种"只在一处泄漏"。
            sender_ph = "我" if m.get("is_me") else (self.register_person(disp, "发送者显示名") or "对方")
            text = self.process_text(m.get("text", ""))
            out_msgs.append({
                "sender": sender_ph,
                "is_me": bool(m.get("is_me")),
                "is_target": (not m.get("is_me")) and sender_ph in want,
                "create_time": m.get("create_time"),
                "type_label": m.get("type_label"),
                "text": text,
            })
        context_lines = []
        for m in out_msgs:
            ts = _fmt_ts(m["create_time"])
            label = _egress_label(m["type_label"])
            line = (f"[{ts}] {m['sender']}（{label}）：{m['text']}"
                    if label else f"[{ts}] {m['sender']}：{m['text']}")
            context_lines.append(("★ " if m["is_target"] else "  ") + line)
        return {
            "session_placeholder": session_ph,
            "is_group": bool(export.get("is_group")),
            "exported": export.get("exported"),
            "messages": out_msgs,
            "context_text": "\n".join(context_lines),
            "context_legend": "（★ = 本轮要回应的对象；其余人是聊天背景，也要参考）"
                              if any(m["is_target"] for m in out_msgs) else "",
            "mapping": dict(self.mapping),
        }


def _egress_label(label) -> str:
    """上下文行里的类型标签：只留中文短词，**绝不留数字**。
    旧版这里直接把 local_type 编号（11~12 位、可能以 1[3-9] 开头）拼进出境文本，
    既没信息量又会撞手机号/卡号类判据（实测被出境自查拦下过一次）。"""
    s = (label or "").strip()
    if not s or s == "text":
        return ""
    known = {"image": "图片", "voice": "语音", "video": "视频", "file": "文件", "card": "卡片",
             "rich": "卡片", "location": "位置", "merged": "合并转发", "call": "通话",
             "emoji": "表情", "app": "分享", "system": "系统", "other": "未识别类型"}
    if s in known:
        return known[s]
    if any(c.isdigit() for c in s):
        return "未识别类型"
    return s


def _fmt_ts(epoch: int) -> str:
    try:
        import datetime as dt
        # 微信 create_time 为秒；异常值退回原样
        if epoch > 10**12:
            epoch //= 1000
        return dt.datetime.fromtimestamp(epoch).strftime("%Y-%m-%d %H:%M")
    except Exception:
        return str(epoch)


if __name__ == "__main__":
    import sys
    # 自检：拿规则里的 test_samples 过一遍
    here = os.path.dirname(os.path.abspath(__file__))
    cdir = sys.argv[1] if len(sys.argv) > 1 else os.path.join(here, "config")
    wdir = sys.argv[2] if len(sys.argv) > 2 else cdir
    d = Desensitizer(cdir, wdir)
    for s in d.cfg.get("test_samples", []):
        print(f"原文: {s}\n脱敏: {d.process_text(s)}\n")
    print("日志样例:", json.dumps(d.log[:3], ensure_ascii=False))
