//! 只读探针：判断本机 `business/emoticon/Persist/<md5>` 是什么格式（明文/加密/容器）。
//! 用法：cargo run -p weflow-core --example emoji_probe -- <文件> <账号目录名>
//! 只打印大小、魔数、尝试结果；不写任何文件。

use weflow_core::dat::{decrypt_dat, detect_ext};
use weflow_core::keystore::KeyStore;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 2 {
        eprintln!("usage: emoji_probe <file> <accountDirName>");
        std::process::exit(2);
    }
    let bytes = std::fs::read(&a[0]).expect("read");
    let head: String = bytes.iter().take(16).map(|b| format!("{:02x}", b)).collect();
    println!("size={} head={} raw_ext={:?}", bytes.len(), head, detect_ext(&bytes));
    let ks = KeyStore::load_default();
    match ks.image_key(Some(a[1].as_str())) {
        Some(k) => {
            let xor = (k.xor_key & 0xff) as u32;
            println!("image_key xor={} aes_len={}", xor, k.aes_key.as_ref().map(|s| s.len()).unwrap_or(0));
            match decrypt_dat(&bytes, xor, k.aes_key.as_deref()) {
                Ok(d) => println!("dat_ok version={} wxgf={} ext={} len={}", d.version, d.is_wxgf, d.ext, d.data.len()),
                Err(e) => println!("dat_err {e}"),
            }
        }
        None => println!("no image_key for account"),
    }
    // 也试一下"整文件单字节异或"（老格式 V1）是否直接出图片
    for xk in [0u32, 0x37, 0x5a] {
        let out: Vec<u8> = bytes.iter().map(|b| *b ^ (xk as u8)).collect();
        if let Some(e) = detect_ext(&out) {
            println!("xor_ok key={:#x} ext={}", xk, e);
        }
    }
}
