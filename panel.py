"""本地面板（门槛六 · 备用入口，仅监听 localhost）。

在电脑前时用：浏览器打开 → 输入「回复 王总 30」→ 后端复用 pipeline 同一链路 →
展示脱敏后上下文 + A/B/C + 一键复制。标准库 http.server，零额外依赖。

红线：只绑 127.0.0.1，不开公网端口；AI 只出建议、面板不提供任何自动发送。
启动：python panel.py [端口默认 8765]
"""
import html
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pipeline
import feishu_adapter


# 面板跑一条命令要几十秒、也真的会花钱；ThreadingHTTPServer 会真并发。
# 而 handle_command 用的是模块全局（_last_*、last_result），并发就会串号：
# 两个标签页同时提交时，A 标签页可能看到 B 联系人的脱敏上下文。
# 面板是备用入口，串行最省事：拿不到锁立刻回 409，而不是让用户等一个会串号的结果。
_RUN_LOCK = threading.Lock()


PAGE = """<!doctype html><meta charset=utf-8><title>AI 微信回复助手 · 本地面板</title>
<style>body{font-family:system-ui;max-width:820px;margin:2rem auto;padding:0 1rem}
textarea{width:100%;height:2.5em}pre{background:#f5f5f5;padding:.8rem;border-radius:6px;white-space:pre-wrap}
button{margin-top:.6rem}</style>
<h2>本地面板（备用入口）</h2>
<p>输入命令：<code>回复 王总 30</code>，或 <code>选 2</code>，或意图词「更正式点」。</p>
<form id=f><textarea name=cmd placeholder="回复 王总 30"></textarea><button>生成建议</button></form>
<div id=out></div>
<script>
document.getElementById('f').onsubmit=async e=>{e.preventDefault();
 const cmd=new FormData(e.target).get('cmd');
 const r=await fetch('/run',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({cmd})});
 const j=await r.json();
 document.getElementById('out').innerHTML=
   '<h4>'+ (j.ok?'完成':('提示')) +'</h4><pre>'+ (j.reply||'').replace(/</g,'&lt;') +'</pre>'+
   (j.context?('<h4>脱敏后上下文</h4><pre>'+j.context.replace(/</g,'&lt;')+'</pre>'):'')+
   (j.answer?('<h4>AI 建议（复制后去微信粘贴）</h4><pre id=a>'+j.answer.replace(/</g,'&lt;')+
     '</pre><button onclick=\\'navigator.clipboard.writeText(document.getElementById("a").textContent)\\'>复制到剪贴板</button>'):'');
}
</script>"""


class H(BaseHTTPRequestHandler):
    def _send(self, code, body, ctype="text/html; charset=utf-8"):
        data = body.encode("utf-8") if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(data)

    def _local_only(self) -> bool:
        """只认本机回环的 Host/Origin。

        只绑 127.0.0.1 并不够：恶意网页可以把域名解析到 127.0.0.1（DNS rebinding），
        浏览器就把它当同源，POST /run 的响应里带 `context`（脱敏后的聊天正文）——
        等于把本机聊天数据读走。跨站表单也走同一道门。
        Host 头缺失（本机脚本/curl）放行；带了就必须是回环地址本身。"""
        port = self.server.server_port
        hosts = {f"127.0.0.1:{port}", f"localhost:{port}", f"[::1]:{port}"}
        host = (self.headers.get("Host") or "").strip().lower()
        if host and host not in hosts:
            return False
        origin = (self.headers.get("Origin") or "").strip().lower()
        if origin and origin not in {f"http://{h}" for h in hosts}:
            return False
        return True

    def do_GET(self):
        if not self._local_only():
            self._send(403, "forbidden（只允许从本机 127.0.0.1 打开）")
            return
        self._send(200, PAGE)

    def do_POST(self):
        if not self._local_only():
            self._send(403, b'{"error":"forbidden"}', "application/json")
            return
        if self.path != "/run":
            self._send(404, b'{"error":"not found"}', "application/json")
            return
        if not _RUN_LOCK.acquire(blocking=False):
            body = json.dumps({"ok": False, "reply": "正在处理另一条命令，稍后再试",
                               "context": "", "answer": None}, ensure_ascii=False)
            self._send(409, body.encode("utf-8"), "application/json")
            return
        try:
            self._run()
        finally:
            _RUN_LOCK.release()

    def _run(self):
        try:
            n = int(self.headers.get("Content-Length", 0))
        except ValueError:
            n = 0
        if n <= 0 or n > 64 * 1024:
            self._send(400, b'{"error":"bad content-length"}', "application/json")
            return
        try:
            cmd = json.loads(self.rfile.read(n) or b"{}").get("cmd", "").strip()
        except (json.JSONDecodeError, UnicodeDecodeError):
            self._send(400, b'{"error":"bad json"}', "application/json")
            return
        try:
            reply = feishu_adapter.handle_command(cmd)
        except Exception as e:  # noqa: BLE001 面板绝不能抛：抛了浏览器只看到连接断开
            # 一条链路要跑几十秒、也真的往云端发过一次，栈崩在这里必须把原因显示出来
            print(f"[panel] 链路异常：{type(e).__name__}: {e}", file=sys.stderr)
            self._send(200, json.dumps({"ok": False,
                                        "reply": f"运行失败：{type(e).__name__}: {str(e)[:400]}",
                                        "context": "", "answer": None},
                                       ensure_ascii=False), "application/json")
            return
        # 复用刚才那一次链路结果，绝不再跑一遍（跑两遍=重复出境+重复花钱）
        res = feishu_adapter.last_result or {}
        context = ""
        answer = None
        if res.get("stage") == "ok":
            context = res["desensitized"]["context_text"]
            answer = res.get("answer")
        vlm = (res.get("status") or {}).get("vlm") or {}
        note = ""
        if vlm.get("images_selected"):
            state = "已由多模态解读" if res.get("image_notes") else f"未解读（{vlm.get('skipped') or '未配置'}）"
            note = (f"送多模态 {vlm['images_selected']} 张（本机可读 "
                    f"{vlm.get('images_readable', vlm['images_selected'])} 张）：{state}")
        self._send(200, json.dumps({"ok": res.get("stage", "ok") == "ok",
                                    "reply": "\n".join(x for x in [reply, note] if x),
                                    "context": context, "answer": answer},
                                   ensure_ascii=False), "application/json")

    def log_message(self, *a):
        pass


def main():
    # Windows 控制台默认 GBK，异常信息里带 emoji/生僻字会让 print 本身再崩一次
    for s in (sys.stdout, sys.stderr):
        try:
            s.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, OSError):
            pass
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
    srv = ThreadingHTTPServer(("127.0.0.1", port), H)
    print(f"[panel] http://127.0.0.1:{port}  （仅本机可访问，Ctrl+C 停）")
    srv.serve_forever()


if __name__ == "__main__":
    main()
