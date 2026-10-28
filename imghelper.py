"""可选：让微信客户端自己补下未落地的图片（注入 img_helper.dll）。

默认关闭 —— `REPLY_MEDIA_MODE=local`。开启后本模块把 img_helper.dll 载入**本进程**，
由它自己向 Weixin.exe 注入并给 `mars::cdn::CdnCore::_startDownloadMedia` 打补丁：
微信按"白名单会话"补下原图，落到自己的 msg/attach 里，之后仍由只读导出器解密读取。

风险边界（对 dll 的静态核实，见 worklog §5o）：
- 导入表只有 OpenProcess/VirtualAllocEx/WriteProcessMemory/VirtualProtectEx 这类注入与内存补丁 API，
  **没有网络 API、没有文件/注册表写入**；不会外传任何东西，也不改磁盘上的聊天库。
- 但它确实修改微信进程内存：可能触发客户端异常/被风控注意到，微信更新后偏移变化也会失效。
  所以**必须是用户显式选择的模式**，且调用方要在 finally 里 `uninstall()` 还原补丁。
- dll 不存在就不启用（fail closed），绝不下载/分发这个 dll。
"""

import ctypes
import json
import os
import subprocess
import threading

HERE = os.path.dirname(os.path.abspath(__file__))


def _resolve_path(value: str) -> str:
    s = os.path.expandvars(os.path.expanduser((value or "").strip()))
    if not s:
        return ""
    if not os.path.isabs(s):
        s = os.path.join(HERE, s)
    return os.path.normpath(s)


DEFAULT_DLL = _resolve_path("bin/img_helper.dll")

_lock = threading.Lock()
_lib = None
_hooked_pid = None


def dll_path() -> str:
    return _resolve_path(os.environ.get("REPLY_IMGHELPER_DLL")) or DEFAULT_DLL


def available() -> bool:
    return os.name == "nt" and os.path.isfile(dll_path())


def _load():
    global _lib
    if _lib is not None:
        return _lib
    path = dll_path()
    if not os.path.isfile(path):
        raise FileNotFoundError(f"找不到 img_helper.dll：{path}")
    lib = ctypes.WinDLL(path)
    lib.InitImgHelper.argtypes = [ctypes.c_uint32, ctypes.c_char_p]
    lib.InitImgHelper.restype = ctypes.c_bool
    lib.UninstallImgHelper.argtypes = []
    lib.UninstallImgHelper.restype = None
    lib.GetImgHelperError.argtypes = []
    lib.GetImgHelperError.restype = ctypes.c_char_p
    _lib = lib
    return lib


def weixin_pid():
    """找 Weixin.exe 主进程（命令行最短的那个，与 WeFlow 的口径一致）。"""
    try:
        ps = ("[Console]::OutputEncoding=[Text.Encoding]::UTF8; "
              "$OutputEncoding=[Text.Encoding]::UTF8; "
              "Get-CimInstance Win32_Process -Filter \"Name = 'Weixin.exe'\" | "
              "Select-Object ProcessId,CommandLine | ConvertTo-Json -Compress")
        out = subprocess.run(["powershell", "-NoProfile", "-Command", ps],
                             capture_output=True, text=True, encoding="utf-8",
                             errors="replace", timeout=20).stdout
        data = json.loads(out or "null") or []
        if isinstance(data, dict):
            data = [data]
        cands = [p for p in data if (p.get("CommandLine") or "").lower().endswith("weixin.exe")
                 or "weixin.exe" in (p.get("CommandLine") or "").lower()]
        cands.sort(key=lambda p: len(p.get("CommandLine") or ""))
        return int(cands[0]["ProcessId"]) if cands else None
    except (OSError, ValueError, KeyError, subprocess.TimeoutExpired):
        return None


def init_for_session(sid):
    """给某个会话开补下。返回 {ok, pid, error}；sid 为空=全部会话（不推荐）。"""
    global _hooked_pid
    res = {"ok": False, "pid": None, "error": "", "dll": dll_path()}
    if os.name != "nt":
        res["error"] = "仅 Windows"
        return res
    if not os.path.isfile(dll_path()):
        res["error"] = f"找不到 img_helper.dll：{dll_path()}"
        return res
    pid = weixin_pid()
    if not pid:
        res["error"] = "没找到 Weixin.exe（微信需在运行）"
        return res
    with _lock:
        if _hooked_pid == pid:
            res.update(ok=True, pid=pid, error="")
            return res
        if _hooked_pid and _hooked_pid != pid:
            uninstall()
        lib = _load()
        wl = (("\0".join([sid]) + "\0\0").encode("utf-8") if sid else None)
        try:
            ok = bool(lib.InitImgHelper(pid, wl))
        except OSError as e:
            res["error"] = f"调用异常：{e}"
            return res
        if ok:
            _hooked_pid = pid
            res.update(ok=True, pid=pid)
        else:
            err = lib.GetImgHelperError()
            res["error"] = (err.decode("utf-8", "replace") if err else "") or "InitImgHelper 返回 false"
        return res


def uninstall():
    """还原补丁。异常也要吃掉：不能让退出路径再抛一次。"""
    global _hooked_pid
    with _lock:
        if not _hooked_pid:
            return
        try:
            if _lib is not None:
                _lib.UninstallImgHelper()
        except OSError:
            pass
        finally:
            _hooked_pid = None
