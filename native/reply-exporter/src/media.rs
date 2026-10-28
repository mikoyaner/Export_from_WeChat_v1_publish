//! 图片定位与解密（自研，逻辑对齐本仓 wcdb-native/src/media.rs，非第三方代码）。
//!
//! 链路：消息 type=3 内容 zstd 解压得 `<img md5="…" …/>` → 用 md5 查 hardlink.db
//! `*_hardlink_info_v4` 得 `file_name` → 在账号 `msg/attach` 下定位 .dat 绝对路径
//! （顺序＝hardlink 目录列直拼 → **本会话子树** `attach/<md5(session_id)>` → 整棵 attach 索引）
//! → 读字节 → 按账号图片密钥（xor + 可选 aes）解密成真实媒体。
//!
//! 全程只读：只读 hardlink.db 与 attach 目录，不写微信数据目录。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use weflow_core::dat::{decrypt_dat, DecryptedDat};
use weflow_core::db::{list_tables, table_columns, Account};
use weflow_core::schema::{find_candidate, msg_table_hash, sanitize_ident};

const HL_MD5: &[&str] = &["md5"];
const HL_FILE_NAME: &[&str] = &["file_name", "fileName", "filename"];
const HL_DIR1: &[&str] = &["dir1"];
const HL_DIR2: &[&str] = &["dir2"];

/// 微信 3.x 迁移图片按变体名落盘，按质量优先顺序尝试。
const DAT_VARIANT_NAMES: &[&str] = &[
    "{k}.dat",
    "{k}_h.dat",
    "{k}_hd.dat",
    "{k}_w.dat",
    "{k}_b.dat",
    "{k}_c.dat",
    "{k}_t.dat",
    "{k}_t_w.dat",
    "{k}.jpg",
    "{k}.png",
];

fn attach_cache() -> &'static Mutex<HashMap<String, Arc<HashMap<String, PathBuf>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<HashMap<String, PathBuf>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取得（必要时构建）账号 `msg/attach` 的「文件名(小写) → 完整路径」索引。
pub fn attach_index(account: &Account) -> Arc<HashMap<String, PathBuf>> {
    let root = match account.account_dir() {
        Some(d) => d.join("msg").join("attach"),
        None => return Arc::new(HashMap::new()),
    };
    if !root.is_dir() {
        return Arc::new(HashMap::new());
    }
    let key = root.to_string_lossy().replace('\\', "/").to_lowercase();
    if let Ok(g) = attach_cache().lock() {
        if let Some(hit) = g.get(&key) {
            return Arc::clone(hit);
        }
    }
    let mut map = HashMap::new();
    walk_attach(&root, &mut map);
    let arc = Arc::new(map);
    if let Ok(mut g) = attach_cache().lock() {
        g.insert(key, Arc::clone(&arc));
    }
    arc
}

fn walk_attach(dir: &Path, map: &mut HashMap<String, PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_attach(&path, map);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            map.entry(name.to_ascii_lowercase()).or_insert_with(|| path.clone());
        }
    }
}

/// 由图片 md5 定位磁盘上的 .dat（或变体）。
///
/// 先按 `hardlink.db` 里的 `dir1`/`dir2`/`file_name` **直接拼路径**（一次 SELECT 就够）；
/// 拼不出来才**惰性**构建 attach 文件名索引（本机实测 6.9 万个文件，一条命令一个新进程，
/// 内存缓存不跨命令复用 ⇒ 每次白扫几秒）。
/// 图片定位器。定位顺序（结果完全一致，只是常见情况不用再扫全盘）：
/// 1. `hardlink.db` 的 `dir1`/`dir2`/`file_name` **直接拼路径**（本机 4.1.15 这两列是空的，
///    留着给会写这两列的版本，一次 SELECT 顺带取出，不额外花钱）；
/// 2. **该会话自己的 attach 子树** `attach/<md5(session_id)>/**`（本机实测：一级目录 140~428 个、
///    全 attach 6.9 万个文件，而单会话子树只有 5~142 个）；
/// 3. 前两步全落空，才**惰性**构建整棵 attach 的文件名索引（一条命令一个新进程，
///    内存缓存不跨命令复用 ⇒ 以前每条命令都白扫几秒）。
#[derive(Default)]
pub struct Locator {
    scoped: Option<(String, Arc<HashMap<String, PathBuf>>)>,
    full: Option<Arc<HashMap<String, PathBuf>>>,
    /// `hardlink.db/dir2id`：dir1/dir2 的整数外键 → 「会话哈希 / 年月」。
    /// 不解析这层就会把 82/83 当目录名直接拼，实测 direct 命中恒为 0。
    dir2id: Option<HashMap<i64, String>>,
    /// 命中途径计数：[hardlink 直拼, 会话子树, 全量索引, 未定位]
    pub hits: [usize; 4],
    /// 慢路（整棵 attach 扫盘）到底有没有被走到 —— P2 验收就看这个布尔
    pub full_walk_used: bool,
    /// 最近一次 resolve 实际走的路由（进每张图的诊断，排查"为什么没找到"）
    pub last_route: &'static str,
}

struct HlRow {
    file_name: String,
    dir1: String,
    dir2: String,
}

impl Locator {
    pub fn new() -> Self {
        Self::default()
    }

    fn full_index(&mut self, account: &Account) -> Arc<HashMap<String, PathBuf>> {
        if let Some(i) = self.full.as_ref() {
            return Arc::clone(i);
        }
        let built = attach_index(account);
        self.full_walk_used = true;
        self.full = Some(Arc::clone(&built));
        built
    }

    /// 会话子树索引；`sid` 为空或该子树不存在时返回 None（调用方直接走慢路判定）。
    fn scoped_index(&mut self, account: &Account, sid: &str) -> Option<Arc<HashMap<String, PathBuf>>> {
        if sid.is_empty() {
            return None;
        }
        let key = sid.to_ascii_lowercase();
        if let Some((s, m)) = self.scoped.as_ref() {
            if *s == key {
                return if m.is_empty() { None } else { Some(Arc::clone(m)) };
            }
        }
        let root = account.account_dir().map(|d| d.join("msg").join("attach"))?;
        let dir = root.join(msg_table_hash(sid));
        let map: HashMap<String, PathBuf> = if dir.is_dir() {
            let mut m = HashMap::new();
            walk_attach(&dir, &mut m);
            m
        } else {
            HashMap::new()
        };
        let arc = Arc::new(map);
        let out = if arc.is_empty() { None } else { Some(Arc::clone(&arc)) };
        self.scoped = Some((key, arc));
        out
    }

    /// 解析图片：`md5` 来自消息 XML，`dat_name` 来自 `packed_info_data`。
    /// 两者是**两棵树**（WeFlow 实测：消息 md5 与磁盘文件名本来就不是一个值）；
    /// 只按 md5 查 hardlink/扫描会漏掉"有 hardlink 行、但名字对不上"的那批图。
    pub fn resolve(&mut self, account: &Account, md5: &str, dat_name: Option<&str>, sid: &str) -> Option<PathBuf> {
        let md5 = md5.trim().to_ascii_lowercase();
        let dat_name = dat_name.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
        let mut bases: Vec<String> = Vec::new();
        if !md5.is_empty() {
            bases.push(md5.clone());
        }
        if let Some(d) = &dat_name {
            if !bases.iter().any(|b| b.eq_ignore_ascii_case(d)) {
                bases.push(d.clone());
            }
        }
        if bases.is_empty() {
            self.last_route = "no_md5";
            return None;
        }
        let mut rows: Vec<HlRow> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for b in &bases {
            for h in self.hardlink_rows(account, b) {
                if seen.insert(format!("{}|{}|{}", h.file_name, h.dir1, h.dir2)) {
                    rows.push(h);
                }
            }
        }
        let dir_map = self.dir2id_map(account).clone();
        if let Some(root) = account.account_dir().map(|d| d.join("msg").join("attach")) {
            for h in &rows {
                if let Some(p) = direct_path(&root, h, Some(&dir_map)) {
                    self.hits[0] += 1;
                    self.last_route = "direct";
                    return Some(p);
                }
            }
        }
        let mut variants: Vec<String> = Vec::new();
        for b in &bases {
            if is_md5_hex(b) {
                variants.extend(DAT_VARIANT_NAMES.iter().map(|t| t.replace("{k}", b)));
            } else {
                variants.push(format!("{b}.dat"));
            }
        }
        if let Some(idx) = self.scoped_index(account, sid) {
            if let Some(p) = find_in(&idx, &rows, &variants) {
                self.hits[1] += 1;
                self.last_route = "scoped";
                return Some(p);
            }
        }
        // 只有 hardlink 里真有至少一行，才值得付「整棵 attach 扫盘」的代价：
        // 没有 hardlink 行＝这张图本机从未落地（本机实测 400 张里只有 7 张有行），
        // 再扫 6.9 万个文件名也扫不出来，白白每条命令多花两秒。
        if rows.is_empty() {
            self.hits[3] += 1;
            self.last_route = "no_hardlink_row";
            return None;
        }
        let index = self.full_index(account);
        if let Some(p) = find_in(&index, &rows, &variants) {
            self.hits[2] += 1;
            self.last_route = "full";
            return Some(p);
        }
        self.hits[3] += 1;
        self.last_route = "full_miss";
        None
    }

    fn hardlink_rows(&self, account: &Account, md5: &str) -> Vec<HlRow> {
        let hl = match account.layout.hardlink_db.as_ref() {
            Some(h) => h.clone(),
            None => return Vec::new(),
        };
        let tables: Vec<String> = account
            .with_conn(&hl, |c| list_tables(c))
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                let l = t.to_ascii_lowercase();
                l.contains("hardlink_info") || l.contains("hardlink")
            })
            .collect();
        for t in tables {
            let rows = lookup_hardlink(account, &hl, &t, md5);
            if !rows.is_empty() {
                return rows;
            }
        }
        Vec::new()
    }

    /// 读 `dir2id`（rowid → username）。一次读入，后续直拼路径都靠它。
    fn dir2id_map(&mut self, account: &Account) -> &HashMap<i64, String> {
        if self.dir2id.is_none() {
            let mut map: HashMap<i64, String> = HashMap::new();
            if let Some(hl) = account.layout.hardlink_db.as_ref() {
                let _ = account.with_conn(hl, |c| -> anyhow::Result<()> {
                    if c.prepare("SELECT rowid, username FROM dir2id").is_ok() {
                        let mut st = c.prepare("SELECT rowid, username FROM dir2id")?;
                        let mut rows = st.query([])?;
                        while let Some(r) = rows.next()? {
                            let id: i64 = r.get(0)?;
                            let name: String = r.get(1).unwrap_or_default();
                            if !name.trim().is_empty() {
                                map.insert(id, name);
                            }
                        }
                    }
                    Ok(())
                });
            }
            self.dir2id = Some(map);
        }
        self.dir2id.as_ref().unwrap()
    }
}

/// 先按 hardlink 给出的文件名找，再按 md5 变体名找。
fn find_in(
    index: &HashMap<String, PathBuf>,
    rows: &[HlRow],
    variants: &[String],
) -> Option<PathBuf> {
    for h in rows {
        if let Some(p) = index.get(&h.file_name.to_ascii_lowercase()) {
            return Some(p.clone());
        }
    }
    for cand in variants {
        if let Some(p) = index.get(cand) {
            return Some(p.clone());
        }
    }
    None
}

fn is_md5_hex(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 把 hardlink 表里的目录列当**相对路径**用：只接受不含盘符、不含 `..`、不以 / 开头的值，
/// 拼出来的路径必须仍在 attach 根之下（防止库里的异常值把读取范围带出账号目录）。
fn safe_sub(root: &Path, rel: &str) -> Option<PathBuf> {
    let r = rel.replace('\\', "/");
    if r.is_empty() || r.starts_with('/') || r.contains(':') || r.split('/').any(|c| c == "..") {
        return None;
    }
    let mut p = root.to_path_buf();
    for part in r.split('/').filter(|s| !s.is_empty()) {
        p.push(part);
    }
    Some(p)
}

/// 给诊断用：只回「这种拼法在本机到底存不存在」，不外泄路径内容。
pub fn direct_path_probe(root: &Path, file_name: &str, dir1: &str, dir2: &str) -> bool {
    direct_path(root, &HlRow { file_name: file_name.to_string(), dir1: dir1.to_string(), dir2: dir2.to_string() }, None).is_some()
}

/// dir1/dir2 是 `dir2id` 的整数外键（82/83 → 会话哈希 / 年月），不是目录名。
/// 解析后再拼 `attach/<哈希>/<年月>/Img/<file>`；同时保留"它们本身就是目录名"的拼法兜底。
/// 只认磁盘上真实存在的文件；全落空才回退到文件名索引。
fn direct_path(root: &Path, h: &HlRow, dir2id: Option<&HashMap<i64, String>>) -> Option<PathBuf> {
    let name = h.file_name.trim();
    if name.is_empty() {
        return None;
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    let d1 = h.dir1.trim().trim_matches('/');
    let d2 = h.dir2.trim().trim_matches('/');
    // 主路径：整数外键 → 名字。实测 dir1=82/dir2=83 解出 b809…/2026-10。
    if let Some(map) = dir2id {
        if let (Ok(i1), Ok(i2)) = (d1.parse::<i64>(), d2.parse::<i64>()) {
            if let (Some(u1), Some(u2)) = (map.get(&i1), map.get(&i2)) {
                for (a, b) in [(u1, u2), (u2, u1)] {
                    for sub in ["Img", "Image", "File", "Video", ""] {
                        let rel = if sub.is_empty() {
                            format!("{a}/{b}/{name}")
                        } else {
                            format!("{a}/{b}/{sub}/{name}")
                        };
                        cands.extend(safe_sub(root, &rel));
                    }
                }
            }
        }
    }
    if !d1.is_empty() && !d2.is_empty() {
        cands.extend(safe_sub(root, &format!("{d1}/{d2}/{name}")));
        cands.extend(safe_sub(root, &format!("{d2}/{d1}/{name}")));
    }
    if !d1.is_empty() {
        cands.extend(safe_sub(root, &format!("{d1}/{name}")));
    }
    if !d2.is_empty() {
        cands.extend(safe_sub(root, &format!("{d2}/{name}")));
    }
    cands.extend(safe_sub(root, name));
    cands.into_iter().find(|p| p.is_file())
}

/// 列值统一转字符串：驱动会把 INTEGER 当类型不匹配拒绝 `get::<String>`。
fn cell_str(r: &rusqlite::Row<'_>, idx: usize) -> String {
    match r.get_ref(idx) {
        Ok(rusqlite::types::ValueRef::Integer(i)) => i.to_string(),
        Ok(rusqlite::types::ValueRef::Text(t)) => String::from_utf8_lossy(t).into_owned(),
        Ok(rusqlite::types::ValueRef::Real(f)) => f.to_string(),
        _ => String::new(),
    }
}

fn lookup_hardlink(account: &Account, hl: &Path, table: &str, md5: &str) -> Vec<HlRow> {
    let cols = match account.columns(hl, table) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let md5_col = match find_candidate(&cols, HL_MD5) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let name_col = match find_candidate(&cols, HL_FILE_NAME) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let dir1 = find_candidate(&cols, HL_DIR1);
    let dir2 = find_candidate(&cols, HL_DIR2);
    let mut select: Vec<String> = vec![format!("\"{}\"", sanitize_ident(&name_col).unwrap_or_default())];
    select.push(match &dir1 {
        Some(c) => format!("\"{}\"", sanitize_ident(c).unwrap_or_default()),
        None => "''".to_string(),
    });
    select.push(match &dir2 {
        Some(c) => format!("\"{}\"", sanitize_ident(c).unwrap_or_default()),
        None => "''".to_string(),
    });
    let hash_clause = if cols.iter().any(|c| c.eq_ignore_ascii_case("md5_hash")) {
        " OR lower(\"md5_hash\") = ?1"
    } else {
        ""
    };
    let sql = format!(
        "SELECT {} FROM \"{}\" WHERE lower(\"{}\") = ?1{}",
        select.join(", "),
        sanitize_ident(table).unwrap_or_default(),
        sanitize_ident(&md5_col).unwrap_or_default(),
        hash_clause,
    );
    account
        .with_conn(hl, |c| -> anyhow::Result<Vec<HlRow>> {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([md5], |r| {
                Ok(HlRow {
                    file_name: cell_str(r, 0),
                    // dir1/dir2 在 4.x 是 INTEGER 外键；用 get::<String> 会失败，
                    // 再 unwrap_or_default 就变空串 ⇒ 直拼路径永远拼不出来（实测 direct=0）。
                    dir1: cell_str(r, 1),
                    dir2: cell_str(r, 2),
                })
            })?;
            Ok(rows.flatten().filter(|h| !h.file_name.trim().is_empty()).collect())
        })
        .unwrap_or_default()
}

/// 从 `packed_info_data`（BLOB 被 weflow-core hex 化）里取磁盘文件名基：
/// WeFlow `parseImageDatNameFromRow` 同口径——把字节转可打印，先找 `<hex>.dat` / `<hex>.t.dat`，
/// 再退到任意 16+ 位十六进制串。消息 md5 与这个 token 是**两棵树**，必须都查。
pub fn dat_name_from_packed(packed_hex: &str) -> Option<String> {
    let s = packed_hex.trim();
    if s.len() < 8 || s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        bytes.push((hi * 16 + lo) as u8);
        i += 2;
    }
    let printable: Vec<char> = bytes
        .iter()
        .map(|&x| if (0x20..=0x7e).contains(&x) { x as char } else { ' ' })
        .collect();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut k = 0usize;
    while k < printable.len() {
        if printable[k].is_ascii_hexdigit() {
            let start = k;
            while k < printable.len() && printable[k].is_ascii_hexdigit() {
                k += 1;
            }
            runs.push((start, k));
        } else {
            k += 1;
        }
    }
    for (st, en) in &runs {
        if en - st >= 8 {
            let rest: String = printable[*en..].iter().take(6).collect();
            if rest.starts_with(".dat") || rest.starts_with(".t.dat") {
                return Some(printable[*st..*en].iter().collect::<String>().to_ascii_lowercase());
            }
        }
    }
    runs.iter()
        .find(|(st, en)| en - st >= 16)
        .map(|(st, en)| printable[*st..*en].iter().collect::<String>().to_ascii_lowercase())
}

/// 从 `<img … md5="…" …>` 里取 md5 属性值。
/// 只在 `<img>` 元素**内部**找：同一条正文里别处也可能有 `md5="…"`（表情、文件卡片），
/// 抓错就等于把 A 图的密钥用到 B 图上。
pub fn img_md5_from_xml(xml: &str) -> Option<String> {
    let b = xml.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = xml[from..].find("<img") {
        let start = from + rel;
        let after = start + 4;
        // `<img` 后面必须紧跟空白/`>`/`/>`，否则是 `<imageXX` 这种别的元素名
        let is_elem = after >= b.len()
            || (b[after].is_ascii_whitespace() || b[after] == b'>' || b[after] == b'/');
        // 元素里没写 `>`（正文被截断）时也别直接放弃：限长 4096 内当标签尾处理
        let end = match xml[after..].find('>') {
            Some(e) => after + e + 1,
            None => (after + 4096).min(b.len()),
        };
        if is_elem {
            if let Some(v) = xml_attr(&xml[after..end], "md5") {
                let v = v.trim().to_ascii_lowercase();
                if is_md5_hex(&v) {
                    return Some(v);
                }
            }
        }
        from = end;
    }
    None
}

/// 在一段标签文本里按属性名取值；属性名前面必须是空白（挡 `xmd5=`）。
/// 只按 ASCII 边界推进：`md5` 是 ASCII、空白也是 ASCII，所以 byte 索引同时就是字符边界。
fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut from = 0usize;
    loop {
        let key = from + tag[from..].find(name)?;
        let after_name = key + name.len();
        let rest = tag.get(after_name..)?;
        let preceded_by_space = key == 0 || bytes[key - 1].is_ascii_whitespace();
        let eq = after_name + (rest.len() - rest.trim_start().len());
        if !preceded_by_space || !tag[eq..].starts_with('=') {
            from = after_name;
            continue;
        }
        let after_eq = tag.get(eq + 1..)?;
        let qrel = after_eq.find(['"', '\''])?;
        let quote = after_eq.as_bytes()[qrel] as char;
        let start = eq + 1 + qrel + 1;
        let end = tag[start..].find(quote)? + start;
        return Some(tag[start..end].to_string());
    }
}

/// 把动图（gif/webp）转成首帧 JPG，供只吃静态图的多模态使用。
/// 转不动就返回 None，调用方保留原文件并标注，不静默吞掉。
pub fn first_frame_jpg(data: &[u8], ext: &str, ffmpeg: Option<&Path>) -> Option<Vec<u8>> {
    use std::process::{Command, Stdio};
    let ff = ffmpeg?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let uid = format!("{}_{}", std::process::id(), nanos);
    let tmp = std::env::temp_dir();
    let inp = tmp.join(format!("emoji_{uid}.{ext}"));
    let outp = tmp.join(format!("emoji_{uid}.jpg"));
    if std::fs::write(&inp, data).is_err() {
        return None;
    }
    let mut cmd = Command::new(ff);
    cmd.args(["-y", "-i"]).arg(&inp).args(["-frames:v", "1", "-q:v", "2"]).arg(&outp);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let ok = match cmd.spawn() {
        Ok(mut c) => wait_bounded(&mut c),
        Err(_) => false,
    };
    let out = if ok { read_if_jpeg(&outp) } else { None };
    if out.is_none() {
        set_convert_error("动图转首帧失败（ffmpeg）");
    }
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
    out
}

/// 在指定元素（`<img` / `<emoji` / `<voicemsg`）内部按属性名取值，
/// 元素名后必须紧跟空白/`>`/`/`，正文被截断时按 4096 字节当标签尾处理。
pub(crate) fn elem_attr(xml: &str, elem: &str, name: &str) -> Option<String> {
    let b = xml.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = xml[from..].find(elem) {
        let start = from + rel;
        let after = start + elem.len();
        let is_elem = after >= b.len()
            || (b[after].is_ascii_whitespace() || b[after] == b'>' || b[after] == b'/');
        let end = match xml[after..].find('>') {
            Some(e) => after + e + 1,
            None => (after + 4096).min(b.len()),
        };
        if is_elem {
            if let Some(v) = xml_attr(&xml[after..end], name) {
                return Some(v);
            }
        }
        from = end.max(after);
    }
    None
}

/// 读取并解密 .dat 为真实媒体字节。
pub fn decrypt_dat_file(path: &Path, xor_key: u32, aes_key: Option<&str>) -> anyhow::Result<DecryptedDat> {
    let bytes = std::fs::read(path)?;
    decrypt_dat(&bytes, xor_key, aes_key)
}

/// 发现 ffmpeg 可执行文件：显式参数 > 环境变量 WEFLOW_FFMPEG > 我们 exe 同级 resources >
/// 随 exe 目录 > PATH 扫描。（不猜别的项目路径——那会让发布版在别人机器上静默用错 ffmpeg）
/// 只做文件系统存在性检查，绝不 spawn 进程探测（避免每条命令闪窗、也更快）。
pub fn find_ffmpeg(explicit: Option<&str>) -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        if !p.trim().is_empty() {
            cands.push(PathBuf::from(p));
        }
    }
    if let Ok(p) = std::env::var("WEFLOW_FFMPEG") {
        if !p.trim().is_empty() {
            cands.push(PathBuf::from(p));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("resources").join("runtime").join("win32").join("ffmpeg.exe"));
            cands.push(dir.join("ffmpeg.exe"));
        }
    }
    for c in &cands {
        if c.is_file() {
            return Some(c.clone());
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for entry in std::env::split_paths(&path) {
            let exe = entry.join("ffmpeg.exe");
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

/// 解 wxgf 容器 → 可读静态图。返回 (字节, 不含点的小写扩展名)。
/// 先找内嵌 JPEG/PNG（微信部分 wxgf 只是包装）；否则视作 HEVC Annex-B 裸流经 ffmpeg 转 JPG。
/// ffmpeg 为 None 时不做转码、返回 None（调用方降级写原始 .wxgf）。
pub fn wxgf_to_image(data: &[u8], ffmpeg: Option<&Path>) -> Option<(Vec<u8>, String)> {
    if data.len() < 4 || &data[..4] != b"wxgf" {
        return None;
    }
    if let Some(hit) = find_embedded_image(data) {
        return Some(hit);
    }
    let ffmpeg = match ffmpeg {
        Some(p) => p,
        None => {
            // 没装/没找到 ffmpeg 与"ffmpeg 转码失败"必须能分开，否则日志只会说"图片挂了"
            set_convert_error("没找到 ffmpeg（设 REPLY_FFMPEG 或 WEFLOW_FFMPEG）");
            return None;
        }
    };
    let hevc = extract_hevc_annexb(data)
        .filter(|h| h.len() >= 100)
        .unwrap_or_else(|| data[4..].to_vec());
    ffmpeg_hevc_to_jpg(&hevc, ffmpeg)
}

/// 在 wxgf 负载里扫描内嵌的传统图片魔数（对齐公开规格：头部之后小窗口内找 JPEG/PNG）。
fn find_embedded_image(data: &[u8]) -> Option<(Vec<u8>, String)> {
    let upper = data.len().saturating_sub(12).min(4096);
    let mut i = 4usize;
    while i < upper {
        if data[i] == 0xFF && data[i + 1] == 0xD8 && data[i + 2] == 0xFF {
            return Some((data[i..].to_vec(), "jpg".to_string()));
        }
        if data[i] == 0x89 && data[i + 1] == 0x50 && data[i + 2] == 0x4E && data[i + 3] == 0x47 {
            return Some((data[i..].to_vec(), "png".to_string()));
        }
        i += 1;
    }
    None
}

/// 从 wxgf 负载提取 HEVC Annex-B NALU，重拼成统一 4 字节起始码的裸流。
/// 自研按 H.265 规格实现：起始码 000001/00000001，丢弃 forbidden_zero_bit(最高位)非 0 的单元。
fn extract_hevc_annexb(data: &[u8]) -> Option<Vec<u8>> {
    let mut marks: Vec<(usize, usize)> = Vec::new(); // (起始码位置, 起始码长度)
    let mut i = 4usize;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
            marks.push((i, 4));
            i += 4;
            continue;
        }
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            marks.push((i, 3));
            i += 3;
            continue;
        }
        i += 1;
    }
    if marks.is_empty() {
        return None;
    }
    let mut out: Vec<u8> = Vec::new();
    for k in 0..marks.len() {
        let (code_pos, prefix) = marks[k];
        let payload_start = code_pos + prefix;
        let payload_end = if k + 1 < marks.len() { marks[k + 1].0 } else { data.len() };
        if payload_start >= payload_end {
            continue;
        }
        let payload = &data[payload_start..payload_end];
        if payload.len() < 2 || payload[0] & 0x80 != 0 {
            continue;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(payload);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// ffmpeg 卡住是有现实理由的：坏流、缺解码器、磁盘忙。本工具是「一条命令一个新进程」，
/// 卡住就等于整条链路挂死（Python 侧只能靠 180s 超时兜底，还要连带别几张图一起丢）。
fn ffmpeg_timeout() -> std::time::Duration {
    let ms = std::env::var("REPLY_FFMPEG_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(25_000);
    std::time::Duration::from_millis(ms.max(1_000))
}

/// 有界等待：轮询 try_wait（标准库没有 wait_timeout，也不想为此引第三方 crate）。
/// 超时或出错一律 kill + reap，绝不留孤儿继续占 TEMP。
/// 最近一次转码失败的原因（ffmpeg 的 stderr 截断 300 字）。
/// 过去 stderr 直接丢进 null，导致"没 ffmpeg / 转码失败 / 产物不是 JPEG"在日志里长得一样。
static LAST_CONVERT_ERR: std::sync::OnceLock<std::sync::Mutex<String>> = std::sync::OnceLock::new();

fn set_convert_error(msg: &str) {
    let slot = LAST_CONVERT_ERR.get_or_init(|| std::sync::Mutex::new(String::new()));
    if let Ok(mut s) = slot.lock() {
        *s = msg.chars().take(300).collect();
    }
}

pub fn last_convert_error() -> String {
    LAST_CONVERT_ERR
        .get_or_init(|| std::sync::Mutex::new(String::new()))
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default()
}

fn wait_bounded(child: &mut std::process::Child) -> bool {
    let deadline = std::time::Instant::now() + ffmpeg_timeout();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.success(),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
}

/// 用 ffmpeg 把 HEVC 裸流转成单帧 JPG。先 `-f hevc` 指定，失败再让 ffmpeg 自动嗅探。
/// 临时文件落系统 TEMP，用完即删；输出必须确为 JPEG 且非空。
fn ffmpeg_hevc_to_jpg(hevc: &[u8], ffmpeg: &Path) -> Option<(Vec<u8>, String)> {
    use std::process::{Command, Stdio};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let uid = format!("{}_{}", std::process::id(), nanos);
    let tmp = std::env::temp_dir();
    let inp = tmp.join(format!("wxgf_{uid}.hevc"));
    let outp = tmp.join(format!("wxgf_{uid}.jpg"));
    if std::fs::write(&inp, hevc).is_err() {
        return None;
    }

    let attempts: [(&str, &[&str]); 2] = [
        ("hevc", &["-y", "-f", "hevc", "-i"]),
        ("auto", &["-y", "-i"]),
    ];
    let mut result: Option<(Vec<u8>, String)> = None;
    for (_label, pre) in attempts {
        let _ = std::fs::remove_file(&outp);
        let errf = tmp.join(format!("wxgf_{uid}.err"));
        let mut cmd = Command::new(ffmpeg);
        cmd.args(pre).arg(&inp).args(["-frames:v", "1", "-q:v", "2"]).arg(&outp);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        cmd.stdout(Stdio::null());
        match std::fs::File::create(&errf) {
            Ok(f) => cmd.stderr(Stdio::from(f)),
            Err(_) => cmd.stderr(Stdio::null()),
        };
        let ok = match cmd.spawn() {
            Ok(mut c) => wait_bounded(&mut c),
            Err(e) => {
                set_convert_error(&format!("ffmpeg 启动失败：{e}"));
                false
            }
        };
        if !ok {
            let txt = std::fs::read_to_string(&errf).unwrap_or_default();
            if !txt.trim().is_empty() {
                set_convert_error(&format!("ffmpeg {_label}: {}", txt.trim().replace('\n', " ")));
            }
            let _ = std::fs::remove_file(&errf);
            continue;
        }
        if let Some(img) = read_if_jpeg(&outp) {
            result = Some((img, "jpg".to_string()));
            let _ = std::fs::remove_file(&errf);
            break;
        }
        let txt = std::fs::read_to_string(&errf).unwrap_or_default();
        set_convert_error(&format!("ffmpeg {_label}: 产物不是 JPEG{}",
                                   if txt.trim().is_empty() { String::new() }
                                   else { format!("（{}）", txt.trim().replace('\n', " ")) }));
        let _ = std::fs::remove_file(&errf);
    }
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
    result
}

fn read_if_jpeg(path: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 200 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(bytes)
    } else {
        None
    }
}

// ── 语音：从 media_*.db 的 VoiceInfo 取 silk blob（自研，逻辑对齐本仓 wcdb-native/src/media.rs，非第三方代码）──

const VOICE_DATA: &[&str] = &["voice_data", "buf", "voicebuf", "voice_buf", "data"];
const VOICE_SVR: &[&str] = &["svr_id", "svrId", "server_id", "svrid", "msg_server_id"];
const VOICE_LOCAL: &[&str] = &["local_id", "localId", "msg_local_id", "local_id_"];
const VOICE_CREATE: &[&str] = &["create_time", "createTime", "create_time_", "time"];
const VOICE_CHAT: &[&str] = &["chat_name_id", "chat_name", "name_id", "chat_id"];
const N2I_NAME: &[&str] = &["user_name", "username", "user_name_", "name"];

/// 取一条语音消息的 silk blob 原始字节。按 svr_id > local_id > create_time 命中，
/// 经同库 Name2Id(chat_name_id) 收窄到本会话用户名。`names` 传 [会话id, 发送者] 等候选。
/// 命中不到返回 None。全程只读，绝不写微信目录。
pub fn voice_silk_bytes(
    account: &Account,
    create_time: i64,
    local_id: i64,
    svr_id: i64,
    names: &[String],
) -> (Option<Vec<u8>>, &'static str) {
    let mut last: &'static str = "no_media_db";
    for db in &account.layout.media_dbs {
        let tables: Vec<String> = account
            .with_conn(db, |c| list_tables(c))
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.to_ascii_lowercase().contains("voice"))
            .collect();
        for vt in tables {
            let (b, route) = voice_in_table(account, db, &vt, names, create_time, local_id, svr_id);
            if let Some(v) = b {
                return (Some(v), route);
            }
            last = route;
        }
    }
    (None, last)
}

#[allow(clippy::too_many_arguments)]
fn voice_in_table(
    account: &Account,
    db: &Path,
    vtable: &str,
    names: &[String],
    create_time: i64,
    local_id: i64,
    svr_id: i64,
) -> (Option<Vec<u8>>, &'static str) {
    let cols = match account.columns(db, vtable) {
        Ok(c) => c,
        Err(_) => return (None, "no_columns"),
    };
    let data_col = match find_candidate(&cols, VOICE_DATA) {
        Some(c) => c,
        None => return (None, "no_data_col"),
    };
    let svr_col = find_candidate(&cols, VOICE_SVR);
    let local_col = find_candidate(&cols, VOICE_LOCAL);
    let create_col = find_candidate(&cols, VOICE_CREATE);
    let chat_col = find_candidate(&cols, VOICE_CHAT);
    let safe_data = match sanitize_ident(&data_col) {
        Ok(s) => s,
        Err(_) => return (None, "bad_data_col"),
    };
    let safe_table = match sanitize_ident(vtable) {
        Ok(s) => s,
        Err(_) => return (None, "bad_table"),
    };

    // 定位谓词：svr_id > local_id > create_time（单一绑定参数 ?1）
    let (id_where, id_param): (Option<String>, Option<i64>) = match (&svr_col, &local_col, &create_col) {
        (Some(sc), _, _) if svr_id != 0 => (sanitize_ident(sc).ok().map(|c| format!("\"{c}\" = ?1")), Some(svr_id)),
        (_, Some(lc), _) if local_id != 0 => (sanitize_ident(lc).ok().map(|c| format!("\"{c}\" = ?1")), Some(local_id)),
        (_, _, Some(cc)) if create_time != 0 => (sanitize_ident(cc).ok().map(|c| format!("\"{c}\" = ?1")), Some(create_time)),
        _ => (None, None),
    };
    let (id_where, id_param) = match (id_where, id_param) {
        (Some(w), Some(p)) => (w, p),
        _ => return (None, "no_id_col"),
    };

    // Name2Id 收窄（同库）：把会话用户名映射成 chat_name_id 的 rowid 集合
    let rowids: Vec<i64> = if chat_col.is_some() && !names.is_empty() {
        account
            .with_conn(db, |c| {
                let tables = list_tables(c).unwrap_or_default();
                let n2i = tables
                    .iter()
                    .find(|t| t.eq_ignore_ascii_case("Name2Id"))
                    .or_else(|| tables.iter().find(|t| t.to_ascii_lowercase().contains("name2id")))
                    .cloned();
                Ok(match n2i {
                    Some(t) => name2id_rowids(c, &t, names),
                    None => Vec::new(),
                })
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let run = |with_chat: bool| -> Option<Vec<u8>> {
        let where_sql = if with_chat && !rowids.is_empty() {
            let cc = chat_col.as_ref().and_then(|c| sanitize_ident(c).ok())?;
            let inlist = rowids.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(",");
            format!("\"{cc}\" IN ({inlist}) AND {id_where}")
        } else {
            id_where.clone()
        };
        let sql = format!("SELECT \"{safe_data}\" FROM \"{safe_table}\" WHERE {where_sql} LIMIT 2");
        let hits: Vec<Vec<u8>> = account
            .with_conn(db, |c| -> anyhow::Result<Vec<Vec<u8>>> {
                let mut stmt = c.prepare(&sql)?;
                let params: [i64; 1] = [id_param];
                let mut rows = stmt.query(params)?;
                let mut acc: Vec<Vec<u8>> = Vec::new();
                while let Some(row) = rows.next()? {
                    match row.get_ref(0)? {
                        rusqlite::types::ValueRef::Blob(b) => acc.push(b.to_vec()),
                        rusqlite::types::ValueRef::Text(t) => {
                            if let Some(v) = hex_decode(&String::from_utf8_lossy(t)) {
                                acc.push(v);
                            }
                        }
                        _ => {}
                    }
                    if acc.len() > 1 {
                        break;
                    }
                }
                Ok(acc)
            })
            .ok()?;
        match hits.len() {
            0 => None,
            1 => hits.into_iter().next(),
            // 同键多行：只有内容完全一致才算同一份，否则视为歧义
            _ => {
                let first = &hits[0];
                if hits.iter().all(|h| h == first) { Some(first.clone()) } else { None }
            }
        }
    };
    let valid = |b: Vec<u8>| !b.is_empty() && looks_like_silk(&b);
    let scoped_possible = !rowids.is_empty();
    if let Some(b) = run(true).filter(|b| valid(b.clone())) {
        return (Some(b), "scoped");
    }
    // 会话收窄失败后才放开约束；**即使唯一命中也不采纳**（用户定的口径）：
    // 同键跨会话撞行时，取第一条等于把别人的原话当成本会话的。
    if let Some(_b) = run(false).filter(|b| valid(b.clone())) {
        return (None, if scoped_possible { "global_rejected" } else { "no_session_match" });
    }
    (None, if scoped_possible { "none" } else { "no_session_match" })
}

/// WeChat 语音 blob 以 `0x02 + "#!SILK_V3"` 开头（少数变体没有前导 0x02、或写成 V2）。
/// 当作「这坨字节确实是语音负载」的判据，配合上面的唯一性检查防止拿错会话。
fn looks_like_silk(b: &[u8]) -> bool {
    let skip = usize::from(b.first() == Some(&0x02));
    b.len() >= skip + 6 && &b[skip..skip + 6] == b"#!SILK"
}

fn name2id_rowids(conn: &rusqlite::Connection, table: &str, names: &[String]) -> Vec<i64> {
    let cols = match table_columns(conn, table) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let name_col = match find_candidate(&cols, N2I_NAME) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let safe = match sanitize_ident(&name_col) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let safe_table = match sanitize_ident(table) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let placeholders = vec!["?"; names.len()].join(",");
    let sql = format!("SELECT rowid FROM \"{safe_table}\" WHERE \"{safe}\" IN ({placeholders})");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let refs: Vec<&dyn rusqlite::ToSql> = names.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
    let out = match stmt.query_map(refs.as_slice(), |r| r.get::<_, i64>(0)) {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    };
    out
}

/// 宽松 hex 解码（voice_data 正常是 BLOB，文本 hex 只是兜底）。
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.is_empty() || s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MD5: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "ffffffffffffffffffffffffffffffff";

    #[test]
    fn img_md5_reads_the_img_element() {
        let xml = format!("<msg><img length=\"1541\" playlength=\"0\" md5=\"{MD5}\" thumb=\"a.jpg\"/></msg>");
        assert_eq!(img_md5_from_xml(&xml).as_deref(), Some(MD5));
    }

    #[test]
    fn img_md5_ignores_other_elements_md5() {
        // 表情/文件卡片里也有 md5=：抓错就把 A 图的密钥用到 B 图上
        let xml = format!("<msg><emoji md5=\"{OTHER}\" encryVer=\"0\"/><img md5=\"{MD5}\"/></msg>");
        assert_eq!(img_md5_from_xml(&xml).as_deref(), Some(MD5));
    }

    #[test]
    fn img_md5_refuses_lookalikes() {
        assert_eq!(img_md5_from_xml(&format!(r#"<img md5="{MD5}"/>"#).replace(MD5, &"z".repeat(32))), None);
        // 自闭合斜杠落在引号外：属性值不是合法 32 hex ⇒ 不取
        assert_eq!(img_md5_from_xml(r#"<img md5="0123456789abcdef0123456789abcdef/>"#), None);
        // 元素名不是 img
        assert_eq!(img_md5_from_xml(r#"<imageX md5="0123456789abcdef0123456789abcdef"/>"#), None);
        // 属性名不是 md5
        assert_eq!(img_md5_from_xml(r#"<img xmd5="0123456789abcdef0123456789abcdef"/>"#), None);
        // 没有闭合引号
        assert_eq!(img_md5_from_xml("<img md5=\""), None);
    }

    #[test]
    fn img_md5_tolerates_spacing_and_single_quotes() {
        let xml = format!("<img {MD5_Q}", MD5_Q = format!("md5 = '{MD5}'"));
        assert_eq!(img_md5_from_xml(&xml).as_deref(), Some(MD5));
        let tabbed = format!("<img\tmd5=\"{MD5}\"\t/>");
        assert_eq!(img_md5_from_xml(&tabbed).as_deref(), Some(MD5));
    }

    #[test]
    fn attr_parsing_is_case_sensitive_on_name() {
        assert_eq!(xml_attr(" md5=\"abc\" ", "md5").as_deref(), Some("abc"));
        assert_eq!(xml_attr(" MD5=\"abc\" ", "md5"), None);
        assert_eq!(xml_attr(" admd5=\"abc\" ", "md5"), None);
    }

    #[test]
    fn safe_sub_stays_under_root() {
        let root = PathBuf::from("attach");
        assert!(safe_sub(&root, "../evil").is_none());
        assert!(safe_sub(&root, "a/../../b").is_none());
        assert!(safe_sub(&root, "E:/x").is_none());
        assert!(safe_sub(&root, "/abs").is_none());
        assert!(safe_sub(&root, "").is_none());
        assert_eq!(safe_sub(&root, "a\\b\\c.dat"), Some(root.join("a").join("b").join("c.dat")));
    }

    #[test]
    fn direct_path_tries_both_dir_orders_and_only_returns_real_files() {
        let root = std::env::temp_dir().join(format!("media_locator_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("d1").join("d2")).unwrap();
        std::fs::write(root.join("d1").join("d2").join("x.dat"), b"x").unwrap();
        let hit = direct_path(&root, &HlRow { file_name: "x.dat".into(), dir1: "d1".into(), dir2: "d2".into() }, None);
        assert_eq!(hit, Some(root.join("d1").join("d2").join("x.dat")));
        // 目录列顺序反过来也要能拼到
        let hit2 = direct_path(&root, &HlRow { file_name: "x.dat".into(), dir1: "d2".into(), dir2: "d1".into() }, None);
        assert!(hit2.is_some());
        // 文件不存在 ⇒ 一个都不许返回
        let miss = direct_path(&root, &HlRow { file_name: "nope.dat".into(), dir1: "d1".into(), dir2: "".into() }, None);
        assert_eq!(miss, None);
        // 目录列里塞 `..` ⇒ 一律拒绝，绝不跳出 attach 根（就算 attach 根外面真有同名文件）
        let evil = direct_path(&root, &HlRow { file_name: "x.dat".into(), dir1: "..".into(), dir2: "d1".into() }, None);
        assert_eq!(evil, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn silk_magic_accepts_wechat_prefixes() {
        assert!(looks_like_silk(b"\x02#!SILK_V3abc"));
        assert!(looks_like_silk(b"#!SILK_V2abc"));
        assert!(!looks_like_silk(b"\x02\x00#!SILK"));
        assert!(!looks_like_silk(b"RIFF.WAVE"));
        assert!(!looks_like_silk(b""));
    }
}
