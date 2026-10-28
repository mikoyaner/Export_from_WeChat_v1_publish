//! 微信 .dat 媒体解密/重加密。
//!
//! 严格复刻 WeFlow `electron/services/imageDecryptService.ts` 中 JS 参考实现的行为：
//! - V1（含未识别版本）：全文件逐字节 XOR；
//! - V2：15 字节头（6 字节 magic + aesSize:i32le@6 + xorSize:i32le@10 + flag:u8@14），
//!   payload = AES-128-ECB(PKCS7) 前段 + raw 中段 + XOR 尾段。

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{anyhow, bail, Result};

pub const SIG_V1: [u8; 6] = [0x07, 0x08, 0x56, 0x31, 0x08, 0x07];
pub const SIG_V2: [u8; 6] = [0x07, 0x08, 0x56, 0x32, 0x08, 0x07];
const V2_HEADER_LEN: usize = 0x0f;

/// 与 TS `NativeDatMeta` 对应（nativeImageDecrypt.ts 同时接受 camel/snake，
/// 上层输出统一为 snake_case）。
#[derive(Debug, Clone, Default)]
pub struct DatMeta {
    pub version: Option<u32>,
    pub aes_size: Option<u32>,
    pub xor_size: Option<u32>,
    pub raw_size: Option<u32>,
    pub flag: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct DecryptedDat {
    pub data: Vec<u8>,
    /// 不含点号的小写扩展名；WXGF 容器为 "wxgf"。
    pub ext: String,
    pub is_wxgf: bool,
    pub version: u32,
    pub meta: DatMeta,
}

/// 对应 TS `getDatVersion`：0=未知 1=V1 2=V2。
pub fn dat_version(data: &[u8]) -> u32 {
    if data.len() < 6 {
        return 0;
    }
    if data[..6] == SIG_V1 {
        1
    } else if data[..6] == SIG_V2 {
        2
    } else {
        0
    }
}

/// 对应 TS `detectImageExtension`，并补充 native 侧需要的 mp4/wxgf 识别。
pub fn detect_ext(data: &[u8]) -> Option<&'static str> {
    if data.len() < 12 {
        return None;
    }
    if data.starts_with(&[0x77, 0x78, 0x67, 0x66]) {
        return Some("wxgf");
    }
    if data.starts_with(&[0x47, 0x49, 0x46]) {
        return Some("gif");
    }
    if data.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        return Some("png");
    }
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("jpg");
    }
    if data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Some("webp");
    }
    if data.starts_with(b"BM") {
        return Some("bmp");
    }
    if &data[4..8] == b"ftyp" {
        return Some("mp4");
    }
    None
}

fn ecb_decrypt(key16: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key16));
    let mut out = data.to_vec();
    for chunk in out.chunks_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
    }
    out
}

fn ecb_encrypt(key16: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key16));
    let mut out = data.to_vec();
    for chunk in out.chunks_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }
    out
}

fn strip_pkcs7_strict(data: &[u8]) -> Result<Vec<u8>> {
    let n = data.len();
    if n == 0 || n % 16 != 0 {
        bail!("pkcs7: invalid length {n}");
    }
    let pad = data[n - 1] as usize;
    if pad == 0 || pad > 16 || pad > n {
        bail!("pkcs7: invalid padding byte {pad}");
    }
    if data[n - pad..].iter().any(|&b| b as usize != pad) {
        bail!("pkcs7: padding bytes mismatch");
    }
    Ok(data[..n - pad].to_vec())
}

fn add_pkcs7(data: &[u8]) -> Vec<u8> {
    let pad = 16 - (data.len() % 16);
    let mut out = data.to_vec();
    out.extend(std::iter::repeat(pad as u8).take(pad));
    out
}

fn xor_bytes(data: &[u8], key: u8) -> Vec<u8> {
    data.iter().map(|b| b ^ key).collect()
}

/// aesKey 为 ASCII 字符串，取前 16 字节（与 TS `Buffer.from(aesKeyText,'ascii').subarray(0,16)` 一致）。
fn normalize_aes_key(aes_key: Option<&str>) -> Option<[u8; 16]> {
    let text = aes_key?.trim();
    if text.len() < 16 {
        return None;
    }
    let mut key = [0u8; 16];
    key.copy_from_slice(&text.as_bytes()[..16]);
    Some(key)
}

/// 解密 dat 字节。`xor_key` 只使用低 8 位（与 JS `data[i] ^ xorKey` 写入 Buffer 的截断一致）。
///
/// 行为与 WeFlow fallback 对齐：
/// - 未加密文件（直接可识别扩展名）原样返回，version=0；
/// - V2 且提供了合法 aesKey 才走 AES 路径，否则视为失败（TS 中 V2 无 aesKey 不产生候选）；
/// - 解密结果无法识别为任何媒体格式时返回 Err（让上层回退到 JS 解密路径）。
pub fn decrypt_dat(data: &[u8], xor_key: u32, aes_key: Option<&str>) -> Result<DecryptedDat> {
    let xk = (xor_key & 0xff) as u8;

    if let Some(ext) = detect_ext(data) {
        let is_wxgf = ext == "wxgf";
        return Ok(DecryptedDat {
            data: data.to_vec(),
            ext: ext.to_string(),
            is_wxgf,
            version: 0,
            meta: DatMeta { version: Some(0), ..Default::default() },
        });
    }

    let version = dat_version(data);
    if version == 2 {
        let Some(key16) = normalize_aes_key(aes_key) else {
            bail!("dat v2 requires a 16+ char aesKey");
        };
        if data.len() < V2_HEADER_LEN {
            bail!("dat v2 file too small");
        }
        let header = &data[..V2_HEADER_LEN];
        let aes_size = u32::from_le_bytes(header[6..10].try_into()?) as usize;
        let xor_size = u32::from_le_bytes(header[10..14].try_into()?) as usize;
        let flag = header[14] as u32;
        let payload = &data[V2_HEADER_LEN..];

        let aligned = aes_size + (16 - aes_size % 16);
        if aligned > payload.len() {
            bail!("invalid aes size: {aes_size} (payload {})", payload.len());
        }
        let plain_aes = strip_pkcs7_strict(&ecb_decrypt(&key16, &payload[..aligned]))?;

        let remaining = &payload[aligned..];
        if xor_size > remaining.len() {
            bail!("invalid xor size: {xor_size} (remaining {})", remaining.len());
        }
        let raw_len = remaining.len() - xor_size;

        let mut out = Vec::with_capacity(plain_aes.len() + remaining.len());
        out.extend_from_slice(&plain_aes);
        out.extend_from_slice(&remaining[..raw_len]);
        out.extend_from_slice(&xor_bytes(&remaining[raw_len..], xk));

        let ext = detect_ext(&out).ok_or_else(|| anyhow!("unrecognized media format after decrypt"))?;
        let is_wxgf = ext == "wxgf";
        return Ok(DecryptedDat {
            data: out,
            ext: ext.to_string(),
            is_wxgf,
            version: 2,
            meta: DatMeta {
                version: Some(2),
                aes_size: Some(aes_size as u32),
                xor_size: Some(xor_size as u32),
                raw_size: Some(raw_len as u32),
                flag: Some(flag),
            },
        });
    }

    // V1 / 未识别版本：全文件 XOR。能走到这里说明原始字节不是可识别图片
    // （明文直通分支已在前面返回），因此这就是一个 V1 加密容器，版本记为 1。
    let out = xor_bytes(data, xk);
    let ext = detect_ext(&out).ok_or_else(|| anyhow!("unrecognized media format after decrypt"))?;
    let is_wxgf = ext == "wxgf";
    let v1_version = if version == 0 { 1 } else { version };
    Ok(DecryptedDat {
        data: out,
        ext: ext.to_string(),
        is_wxgf,
        version: v1_version,
        meta: DatMeta {
            version: Some(v1_version),
            aes_size: Some(0),
            xor_size: Some(data.len() as u32),
            raw_size: Some(0),
            flag: None,
        },
    })
}

/// 重新加密（编辑场景，对应 `encryptDatNative`）。
///
/// - version 取 `meta.version`，缺省时：有 aesKey → 2，否则 → 1；
/// - V2：aes_size 取 `meta.aes_size`（缺省 1023，并夹到明文长度），xor_size 取
///   `meta.xor_size`（缺省 0），flag 取 `meta.flag`（缺省 0）。
pub fn encrypt_dat(
    plain: &[u8],
    xor_key: u32,
    aes_key: Option<&str>,
    meta: Option<&DatMeta>,
) -> Result<Vec<u8>> {
    let xk = (xor_key & 0xff) as u8;
    let key16 = normalize_aes_key(aes_key);
    let version = meta
        .and_then(|m| m.version)
        .unwrap_or(if key16.is_some() { 2 } else { 1 });

    if version != 2 {
        return Ok(xor_bytes(plain, xk));
    }

    let Some(key16) = key16 else {
        bail!("dat v2 encrypt requires a 16+ char aesKey");
    };
    let aes_size = meta
        .and_then(|m| m.aes_size)
        .map(|v| v as usize)
        .unwrap_or(1023.min(plain.len()));
    if aes_size > plain.len() {
        bail!("aes_size {aes_size} exceeds plaintext length {}", plain.len());
    }
    let xor_size = meta
        .and_then(|m| m.xor_size)
        .map(|v| v as usize)
        .unwrap_or(0)
        .min(plain.len() - aes_size);
    let flag = meta.and_then(|m| m.flag).unwrap_or(0) as u8;

    let aligned = aes_size + (16 - aes_size % 16);
    let enc = ecb_encrypt(&key16, &add_pkcs7(&plain[..aes_size]));
    debug_assert_eq!(enc.len(), aligned);

    let raw = &plain[aes_size..plain.len() - xor_size];
    let xor_tail = xor_bytes(&plain[plain.len() - xor_size..], xk);

    let mut out = Vec::with_capacity(V2_HEADER_LEN + aligned + raw.len() + xor_tail.len());
    out.extend_from_slice(&SIG_V2);
    out.extend_from_slice(&(aes_size as u32).to_le_bytes());
    out.extend_from_slice(&(xor_size as u32).to_le_bytes());
    out.push(flag);
    out.extend_from_slice(&enc);
    out.extend_from_slice(raw);
    out.extend_from_slice(&xor_tail);
    Ok(out)
}

pub fn decrypt_dat_file(path: &str, xor_key: u32, aes_key: Option<&str>) -> Result<DecryptedDat> {
    let data = std::fs::read(path).map_err(|e| anyhow!("read {path}: {e}"))?;
    decrypt_dat(&data, xor_key, aes_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_jpeg(len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        v[0] = 0xff;
        v[1] = 0xd8;
        v[2] = 0xff;
        for i in 3..len {
            v[i] = (i % 251) as u8;
        }
        v
    }

    #[test]
    fn v2_roundtrip() {
        let plain = fake_jpeg(5000);
        let aes_key = "0123456789abcdef0123456789abcdef";
        let enc = encrypt_dat(&plain, 0x5a, Some(aes_key), None).unwrap();
        assert_eq!(&enc[..6], &SIG_V2);
        let dec = decrypt_dat(&enc, 0x5a, Some(aes_key)).unwrap();
        assert_eq!(dec.data, plain);
        assert_eq!(dec.ext, "jpg");
        assert!(!dec.is_wxgf);
        assert_eq!(dec.version, 2);
        assert_eq!(dec.meta.aes_size, Some(1023));
    }

    #[test]
    fn v2_roundtrip_with_meta() {
        let plain = fake_jpeg(3000);
        let aes_key = "abcdef9876543210";
        let meta = DatMeta {
            version: Some(2),
            aes_size: Some(512),
            xor_size: Some(256),
            raw_size: None,
            flag: Some(7),
        };
        let enc = encrypt_dat(&plain, 0xa5, Some(aes_key), Some(&meta)).unwrap();
        assert_eq!(enc[14], 7);
        let dec = decrypt_dat(&enc, 0xa5, Some(aes_key)).unwrap();
        assert_eq!(dec.data, plain);
        assert_eq!(dec.meta.xor_size, Some(256));
        assert_eq!(dec.meta.flag, Some(7));
    }

    #[test]
    fn v1_roundtrip() {
        let plain = fake_jpeg(800);
        let enc: Vec<u8> = plain.iter().map(|b| b ^ 0x3c).collect();
        let dec = decrypt_dat(&enc, 0x3c, None).unwrap();
        assert_eq!(dec.data, plain);
        assert_eq!(dec.version, 1);
        let re = encrypt_dat(&plain, 0x3c, None, Some(&DatMeta { version: Some(1), ..Default::default() })).unwrap();
        assert_eq!(re, enc);
    }

    #[test]
    fn plaintext_passthrough() {
        let plain = fake_jpeg(100);
        let dec = decrypt_dat(&plain, 0x11, None).unwrap();
        assert_eq!(dec.version, 0);
        assert_eq!(dec.ext, "jpg");
    }

    #[test]
    fn wxgf_detection() {
        let mut data = vec![0u8; 64];
        data[..4].copy_from_slice(b"wxgf");
        assert_eq!(detect_ext(&data), Some("wxgf"));
        // 全 XOR 的 V1 容器
        let enc: Vec<u8> = data.iter().map(|b| b ^ 0x77).collect();
        let dec = decrypt_dat(&enc, 0x77, None).unwrap();
        assert!(dec.is_wxgf);
        assert_eq!(dec.ext, "wxgf");
    }

    #[test]
    fn v2_without_key_fails() {
        let enc = encrypt_dat(&fake_jpeg(2000), 0x22, Some("0123456789abcdef"), None).unwrap();
        assert!(decrypt_dat(&enc, 0x22, None).is_err());
        assert!(decrypt_dat(&enc, 0x22, Some("short")).is_err());
    }

    #[test]
    fn unrecognized_output_fails() {
        let noise: Vec<u8> = (0..512u32).map(|i| (i * 7 % 256) as u8).collect();
        assert!(decrypt_dat(&noise, 0x99, None).is_err());
    }
}
