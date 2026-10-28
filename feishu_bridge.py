"""单条命令 → 返回给飞书的一行文本。由 feishu_ws.cjs 以子进程调用：
    python feishu_bridge.py "回复 王总 30"
输出 = 链路状态摘要（A/B/C 正文按设计只走企微 webhook，不重复推飞书）。
"""
import sys

# Windows 控制台默认 GBK：日志里带 emoji/生僻字时 print 自身会抛 UnicodeEncodeError，
# 把真正的失败原因盖掉。stdout 是给 feishu_ws.cjs 读的结果，必须 UTF-8；stderr 同开。
for _s in (sys.stdout, sys.stderr):
    try:
        _s.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, OSError):
        pass

if len(sys.argv) < 2 or not sys.argv[1].strip():
    print("用法：python feishu_bridge.py \"<命令文本>\"")
    sys.exit(2)

import feishu_adapter  # noqa: E402

try:
    print(feishu_adapter.handle_command(sys.argv[1]))
except Exception as e:
    print(f"运行失败：{type(e).__name__}: {e}")
    sys.exit(1)
