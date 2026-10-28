"""只读 SQLCipher 数据库密钥扫描器（我们自己的实现，纯标准库）。

微信 4.1.15 实测：进程内存里**不再**残留明文 `x'<key>'` 串。WCDB 运行时改用一个
`com.Tencent.WCDB.Config.Cipher` 的 std::string 对象，其配置块被一个**固定 32 字节掩码**
XOR 混淆，块内含每个库的 `x'<64hex 密钥><32hex salt>'` 字面量。本脚本据这一**技术规格**
重新实现只读提取（不抄任何第三方代码）：

  1. 只读枚举微信进程可读已提交内存区（VirtualQueryEx + ReadProcessMemory）。
     权限仅需 PROCESS_VM_READ | PROCESS_QUERY_INFORMATION —— 不注入、不写进程、不要 admin、不重启微信。
  2. 定位 `com.Tencent.WCDB.Config.Cipher` 字样地址（needle 表）。
  3. 在内存里找「(8字节 needle 地址)(8字节 长度=29)」配对 → 反推 std::string 节点：
        node+0x10 = 数据指针（须命中 needle）, node+0x18 = 长度(29), node+0x28 = config_ptr
     config_ptr+0x88 起读 0x28 字节对象：obj+0x8 = blob 指针, obj+0x10 = blob 长度(≤1024)
  4. 读 blob，按固定掩码循环 XOR 解码，正则取 `[xX]'([0-9a-f]{64,192})'` → 候选 (key, salt)。
  5. 用 HMAC-SHA512 校验每个候选对某 .db page1（verify_enc_key）确认 → 该库密钥。
  6. 结果写 %APPDATA%\\weflow-key-provider\\store.json（weflow-core::keystore 的 KeyStore schema），
     供 Rust `Account::open` 使用。

红线：全程只读；不写微信数据目录（只读 .db 头）；完整密钥只写进本机 store.json，
控制台只打 salt（可公开定位）与掩码后的 key 片段，绝不整串打印、绝不入仓库。
账号目录/wxid 用运行时发现值，不写死、不入库。
"""
import ctypes
import ctypes.wintypes as wt
import hashlib
import hmac as hmac_mod
import json
import os
import re
import struct
import sys
import time

PAGE_SZ = 4096
KEY_SZ = 32
SALT_SZ = 16
HMAC_SZ = 64
RESERVE_SZ = 80
IV_SZ = 16

# ---- Config.Cipher 版本稳定常量（微信 4.1.10+，在 4.1.12.55 验证；本机 4.1.15 实测对齐）----
CONFIG_CIPHER_NAME = b"com.Tencent.WCDB.Config.Cipher"
NAME_LEN = len(CONFIG_CIPHER_NAME)  # 29
CONFIG_XOR_MASK = bytes.fromhex(
    "d2c7442458020000004889442450488b"
    "450048844c2448488944254048584c24"
)
CONFIG_BLOB_MAX = 1024
CONFIG_LITERAL_RE = re.compile(rb"[xX]'([0-9a-fA-F]{64,192})'")
MAX_USER_ADDRESS = 0x0000_8000_0000_0000

_KNOWN_EXE_NAMES = {"weixin.exe", "wechat.exe"}

kernel32 = ctypes.windll.kernel32
psapi = ctypes.windll.psapi
# 64 位句柄必须显式声明返回类型：ctypes 默认把返回值当 32 位 int 截断，
# 高位非零的句柄会变成另一个数——INVALID_HANDLE_VALUE 比较失效、CloseHandle 关错句柄。
kernel32.CreateToolhelp32Snapshot.restype = ctypes.c_void_p
kernel32.OpenProcess.restype = ctypes.c_void_p
# 配套：句柄以 64 位 int 回传后，无 argtypes 的函数实参默认按 32 位 c_int 传——同样截断。
kernel32.Process32First.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
kernel32.Process32Next.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
kernel32.CloseHandle.restype = ctypes.c_int
psapi.GetProcessMemoryInfo.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint32]
MEM_COMMIT = 0x1000
READABLE = {0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80}
TH32CS_SNAPPROCESS = 0x00000002
INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value
PROCESS_QUERY_INFORMATION = 0x0400
PROCESS_VM_READ = 0x0010


class MBI(ctypes.Structure):
    _fields_ = [
        ("BaseAddress", ctypes.c_uint64), ("AllocationBase", ctypes.c_uint64),
        ("AllocationProtect", wt.DWORD), ("_pad1", wt.DWORD),
        ("RegionSize", ctypes.c_uint64), ("State", wt.DWORD),
        ("Protect", wt.DWORD), ("Type", wt.DWORD), ("_pad2", wt.DWORD),
    ]


class ProcessEntry32(ctypes.Structure):
    _fields_ = [
        ("dwSize", wt.DWORD), ("cntUsage", wt.DWORD), ("th32ProcessID", wt.DWORD),
        ("th32DefaultHeapID", ctypes.POINTER(ctypes.c_ulong)), ("th32ModuleID", wt.DWORD),
        ("cntThreads", wt.DWORD), ("th32ParentProcessID", wt.DWORD),
        ("pcPriClassBase", wt.LONG), ("dwFlags", wt.DWORD), ("szExeFile", ctypes.c_char * 260),
    ]


class ProcessMemoryCounters(ctypes.Structure):
    _fields_ = [
        ("cb", wt.DWORD), ("PageFaultCount", wt.DWORD),
        ("PeakWorkingSetSize", ctypes.c_size_t), ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t), ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t), ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t), ("PeakPagefileUsage", ctypes.c_size_t),
    ]


def weixin_pids():
    """[(rss_bytes, pid), ...] 按内存降序；纯只读快照。"""
    snap = kernel32.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
    if snap == INVALID_HANDLE_VALUE:
        raise RuntimeError("无法创建进程快照")
    out = []
    pe = ProcessEntry32()
    pe.dwSize = ctypes.sizeof(ProcessEntry32)
    if kernel32.Process32First(snap, ctypes.byref(pe)):
        while True:
            name = pe.szExeFile.decode("utf-8", "replace").lower()
            if name in _KNOWN_EXE_NAMES:
                pid = pe.th32ProcessID
                rss = 0
                h = kernel32.OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, False, pid)
                if h:
                    try:
                        pmc = ProcessMemoryCounters()
                        pmc.cb = ctypes.sizeof(ProcessMemoryCounters)
                        if psapi.GetProcessMemoryInfo(h, ctypes.byref(pmc), pmc.cb):
                            rss = pmc.WorkingSetSize
                    finally:
                        kernel32.CloseHandle(h)
                out.append((rss, pid))
            if not kernel32.Process32Next(snap, ctypes.byref(pe)):
                break
    kernel32.CloseHandle(snap)
    if not out:
        raise RuntimeError("微信(Weixin.exe/WeChat.exe)未运行")
    out.sort(key=lambda x: x[0], reverse=True)
    return out


def open_process(pid):
    return kernel32.OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, False, pid)


def readable_regions(h):
    regs = []
    addr = 0
    mbi = MBI()
    while addr < 0x7FFFFFFFFFFF:
        if kernel32.VirtualQueryEx(h, ctypes.c_uint64(addr), ctypes.byref(mbi), ctypes.sizeof(mbi)) == 0:
            break
        if mbi.State == MEM_COMMIT and mbi.Protect in READABLE and 0 < mbi.RegionSize < 500 * 1024 * 1024:
            regs.append((mbi.BaseAddress, mbi.RegionSize))
        nxt = mbi.BaseAddress + mbi.RegionSize
        if nxt <= addr:
            break
        addr = nxt
    return regs


def read_mem(h, addr, size):
    buf = ctypes.create_string_buffer(size)
    got = ctypes.c_size_t(0)
    if kernel32.ReadProcessMemory(h, ctypes.c_uint64(addr), buf, size, ctypes.byref(got)):
        return buf.raw[: got.value]
    return None


def iter_chunks(h, regions, chunk_size=2 * 1024 * 1024, overlap=0):
    """分块读取，块间保留 overlap 字节，yield (abs_base_of_data, data)。"""
    for base, size in regions:
        offset = 0
        tail = b""
        tail_base = base
        while offset < size:
            cur = min(chunk_size, size - offset)
            chunk = read_mem(h, base + offset, cur) or b""
            data_base = tail_base if tail else base + offset
            data = tail + chunk
            if data:
                yield data_base, data
                if overlap:
                    tail = data[-overlap:]
                    tail_base = data_base + max(0, len(data) - len(tail))
                else:
                    tail = b""
                    tail_base = base + offset + cur
            else:
                tail = b""
                tail_base = base + offset + cur
            offset += cur


def u64_at(data, off):
    if off < 0 or off + 8 > len(data):
        return 0
    return struct.unpack_from("<Q", data, off)[0]


def xor_repeat(data, mask):
    return bytes(v ^ mask[i % len(mask)] for i, v in enumerate(data))


def probable_key(key_bytes):
    return (len(key_bytes) == KEY_SZ and len(set(key_bytes)) >= 15
            and key_bytes not in (b"\x00" * KEY_SZ, b"\xff" * KEY_SZ))


def verify_enc_key(enc_key: bytes, page1: bytes) -> bool:
    """HMAC-SHA512 校验 SQLCipher page1 确认密钥正确。"""
    if len(page1) < PAGE_SZ:
        return False
    salt = page1[:SALT_SZ]
    mac_salt = bytes(b ^ 0x3A for b in salt)
    mac_key = hashlib.pbkdf2_hmac("sha512", enc_key, mac_salt, 2, dklen=KEY_SZ)
    hmac_data = page1[SALT_SZ: PAGE_SZ - RESERVE_SZ + IV_SZ]
    stored_hmac = page1[PAGE_SZ - HMAC_SZ: PAGE_SZ]
    hm = hmac_mod.new(mac_key, hmac_data, hashlib.sha512)
    hm.update(struct.pack("<I", 1))
    return hm.digest() == stored_hmac


def blob_key_candidates(blob):
    """XOR 解码配置块，yield (key_hex, embedded_salt_or_None)。"""
    if not blob or len(blob) > CONFIG_BLOB_MAX:
        return
    decoded = xor_repeat(blob, CONFIG_XOR_MASK)
    seen = set()
    for m in CONFIG_LITERAL_RE.finditer(decoded):
        run = m.group(1).decode("ascii").lower()
        starts = [0]
        if len(run) > 96:
            starts.extend(range(0, len(run) - 63, 32))
            starts.append(len(run) - 64)
        for start in dict.fromkeys(starts):
            if start < 0 or start + 64 > len(run):
                continue
            key_hex = run[start:start + 64]
            try:
                key = bytes.fromhex(key_hex)
            except ValueError:
                continue
            if not probable_key(key):
                continue
            embedded = run[start + 64:start + 96] if start + 96 <= len(run) else None
            item = (key_hex, embedded)
            if item not in seen:
                seen.add(item)
                yield item


def discover_db_files(data_root):
    """枚举各账号 db_storage 下真实 .db（排除 -wal/-shm），只读 page1。
    返回 (list of (acct_norm, rel, abs_path, page1), salt_to_idxs, salt_to_rel)."""
    files = []
    for acct in sorted(os.listdir(data_root)):
        acct_dir = os.path.join(data_root, acct)
        storage = os.path.join(acct_dir, "db_storage")
        if not os.path.isdir(storage):
            continue
        for dirpath, _dirs, fns in os.walk(storage):
            for fn in fns:
                low = fn.lower()
                if not low.endswith(".db") or low.endswith(("-wal.db", "-shm.db")):
                    continue
                abs_path = os.path.join(dirpath, fn)
                try:
                    with open(abs_path, "rb") as f:
                        page1 = f.read(PAGE_SZ)
                except OSError:
                    continue
                if len(page1) < PAGE_SZ:
                    continue
                rel = os.path.relpath(abs_path, storage).replace("\\", "/").lower()
                acct_norm = acct_dir.replace("\\", "/").lower().rstrip("/")
                files.append((acct_norm, rel, abs_path, page1))
    salt_to_idxs = {}
    for i, (_a, _r, _p, page1) in enumerate(files):
        salt = page1[:SALT_SZ].hex().lower()
        salt_to_idxs.setdefault(salt, []).append(i)
    return files, salt_to_idxs


def mask_key(key_hex):
    return (key_hex[:4] + "…" + key_hex[-2:]) if len(key_hex) >= 8 else "****"


def scan_pid(h, regions, files, salt_to_idxs, resolved, remaining_salts, stats):
    needle_addrs = set()
    for base, data in iter_chunks(h, regions, overlap=NAME_LEN - 1):
        pos = data.find(CONFIG_CIPHER_NAME)
        while pos >= 0:
            needle_addrs.add(base + pos)
            pos = data.find(CONFIG_CIPHER_NAME, pos + 1)
    stats["needles"] += len(needle_addrs)
    if not needle_addrs:
        return
    pair_patterns = [struct.pack("<Q", a) + struct.pack("<Q", NAME_LEN) for a in needle_addrs]
    seen_cands = set()
    for base, data in iter_chunks(h, regions, overlap=0x80):
        if not remaining_salts:
            break
        for pat in pair_patterns:
            pos = data.find(pat)
            while pos >= 0:
                qaddr = base + pos
                node = read_mem(h, qaddr - 0x10, 0x50)
                if node and len(node) >= 0x40:
                    if (u64_at(node, 0x10) in needle_addrs and u64_at(node, 0x18) == NAME_LEN):
                        config_ptr = u64_at(node, 0x28)
                        if 0x10000 <= config_ptr < MAX_USER_ADDRESS:
                            stats["nodes"] += 1
                            obj = read_mem(h, config_ptr + 0x88, 0x28)
                            if obj and len(obj) >= 0x18:
                                data_ptr = u64_at(obj, 0x8)
                                data_len = u64_at(obj, 0x10)
                                if 0 < data_len <= CONFIG_BLOB_MAX and 0x10000 <= data_ptr < MAX_USER_ADDRESS:
                                    blob = read_mem(h, data_ptr, int(data_len))
                                    if blob and len(blob) == data_len:
                                        for key_hex, emb_salt in blob_key_candidates(blob):
                                            cand = (key_hex, emb_salt)
                                            if cand in seen_cands:
                                                continue
                                            seen_cands.add(cand)
                                            stats["candidates"] += 1
                                            if emb_salt:
                                                stats["cand_salts"].add(emb_salt)
                                                targets = [emb_salt] if emb_salt in remaining_salts else list(remaining_salts)
                                            else:
                                                targets = list(remaining_salts)
                                            key = bytes.fromhex(key_hex)
                                            for salt_hex in targets:
                                                if salt_hex not in remaining_salts:
                                                    continue
                                                hit = False
                                                for idx in salt_to_idxs[salt_hex]:
                                                    acct, rel, _p, page1 = files[idx]
                                                    if (acct, rel) in resolved:
                                                        continue
                                                    if verify_enc_key(key, page1):
                                                        resolved[(acct, rel)] = key_hex
                                                        stats["verified"] += 1
                                                        print(f"  [Cipher-FOUND] {rel} salt={salt_hex} key={mask_key(key_hex)}")
                                                        hit = True
                                                        break
                                                if hit:
                                                    remaining_salts.discard(salt_hex)
                                                    break
                pos = data.find(pat, pos + 1)


def merge_store(out, db_keys, db_key_maps):
    """把本次扫描结果并入已有 store.json，只覆盖本次命中的 (账号, 库)。

    内存扫描只拿得到 DB 密钥：image_keys 由离线 MMKV 派生写进同一个文件，
    直接整档覆盖会把图片密钥清空（现象：V2 .dat 突然全解不开），也会丢掉
    本次没扫到的其它账号密钥。"""
    old = {}
    try:
        with open(out, "r", encoding="utf-8") as f:
            loaded = json.load(f)
        if isinstance(loaded, dict):
            old = loaded
    except (OSError, ValueError):
        old = {}
    merged = {
        "db_keys": dict(old.get("db_keys") or {}),
        "db_key_maps": {a: dict(m) for a, m in (old.get("db_key_maps") or {}).items() if isinstance(m, dict)},
        "image_keys": old.get("image_keys") if isinstance(old.get("image_keys"), dict) else {},
    }
    for acct, rel_map in db_key_maps.items():
        merged["db_key_maps"].setdefault(acct, {}).update(rel_map)
    merged["db_keys"].update(db_keys)
    return merged


def write_store(out, store):
    """原子写：先写 .tmp 再 os.replace，避免半截 JSON 把密钥档案打坏。
    tmp 名带 pid：入口并发闸允许 3 条链路同时跑，两条同时扫钥时固定名会互相
    覆盖/触发 PermissionError（Windows 下 replace 打开中的文件会失败）；
    各写各的 tmp、replace 本身原子，最坏 last-writer-wins。"""
    tmp = f"{out}.{os.getpid()}.tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(store, f, indent=2, ensure_ascii=False)
    os.replace(tmp, out)


def main():
    if len(sys.argv) < 2:
        print("用法: python keyscan_readonly.py <数据根目录，如 <微信数据根>> [输出store.json路径]")
        sys.exit(2)
    data_root = sys.argv[1]
    if not os.path.isdir(data_root):
        print(f"[err] 数据根目录不存在: {data_root}")
        sys.exit(1)

    files, salt_to_idxs = discover_db_files(data_root)
    total_db = len(files)
    print(f"[scan] 发现 {total_db} 个 .db，{len(salt_to_idxs)} 个不同 salt")
    remaining_salts = set(salt_to_idxs)
    resolved = {}
    stats = {"needles": 0, "nodes": 0, "candidates": 0, "verified": 0, "cand_salts": set()}

    t0 = time.time()
    pids = weixin_pids()
    print(f"[scan] 微信进程(按内存降序): {[p for _, p in pids]}")
    for _rss, pid in pids:
        if not remaining_salts:
            break
        h = open_process(pid)
        if not h:
            print(f"[scan] PID {pid} OpenProcess 失败 err={kernel32.GetLastError()}")
            continue
        try:
            regions = readable_regions(h)
            print(f"[scan] PID {pid} 可读区域 {len(regions)} 个，"
                  f"{sum(r[1] for r in regions)//1024//1024} MB")
            scan_pid(h, regions, files, salt_to_idxs, resolved, remaining_salts, stats)
        finally:
            kernel32.CloseHandle(h)
    dt = time.time() - t0

    print(f"[stat] needles={stats['needles']} nodes={stats['nodes']} "
          f"candidates={stats['candidates']} verified={stats['verified']} 用时 {dt:.1f}s")
    print(f"[result] 命中 {len(resolved)} / {total_db} 个库")
    if not resolved:
        if stats["needles"] == 0:
            print("[result] 内存里没找到 Config.Cipher 字样 ⇒ 版本结构不同或非主进程；门槛一未通过。")
        elif stats["nodes"] == 0:
            print(f"[result] 找到 {stats['needles']} 处字样但无节点匹配 ⇒ 微信版本内存布局变化；门槛一未通过。")
        elif stats["candidates"] == 0:
            print("[result] 找到节点但配置块未解出候选 ⇒ 解码/读取问题；门槛一未通过。")
        elif stats["cand_salts"] and not (stats["cand_salts"] & set(salt_to_idxs)):
            print("[result] 候选密钥自带 salt 与被扫目录 salt 无交集 ⇒ 扫的不是当前登录账号目录；门槛一未通过。")
        else:
            print("[result] 有候选且 salt 有交集但全部未过校验 ⇒ 库已换钥或首页异常；门槛一未通过。")
        sys.exit(3)

    # 组装 KeyStore JSON：与已有 store 合并，绝不清空 image_keys / 其它账号密钥
    db_key_maps, db_keys = {}, {}
    for (acct, rel), key in resolved.items():
        db_key_maps.setdefault(acct, {})[rel] = key
        if rel == "session/session.db":
            db_keys[acct] = key
    appdata = os.environ.get("APPDATA") or os.path.expanduser("~")
    out = sys.argv[2] if len(sys.argv) > 2 else os.path.join(appdata, "weflow-key-provider", "store.json")
    out = os.path.abspath(out)
    store = merge_store(out, db_keys, db_key_maps)
    kept_img = len(store["image_keys"])
    kept_accts = len(store["db_key_maps"])
    dirname = os.path.dirname(out)
    if dirname:
        os.makedirs(dirname, exist_ok=True)
    try:
        write_store(out, store)
    except OSError as e:
        print(f"[store] 写入失败：{e}")
        sys.exit(4)
    print(f"[store] 已写入 {out}：本次命中 {len(resolved)} 库、账号 {len(db_keys) or len(db_key_maps)} 个；"
          f"合并保留 image_keys {kept_img} 组、库密钥共 {kept_accts} 个账号"
          "（完整密钥仅在此本机文件；控制台/仓库均无明文）")


if __name__ == "__main__":
    main()
