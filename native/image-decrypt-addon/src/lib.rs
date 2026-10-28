//! image_decrypt_addon.node —— WeFlow `imageNativeAddonPath` 组件（napi-rs）。
//!
//! 接口见 WeFlow `electron/services/nativeImageDecrypt.ts`：
//! - `decryptDatNative(inputPath, xorKey, aesKey?) -> { data, ext, isWxgf/is_wxgf, version, aesSize..., flag }`
//! - `encryptDatNative(inputPath, xorKey, aesKey?, meta?) -> Buffer`
//!
//! 字段命名说明：TS 侧对输出同时接受 camelCase 与 snake_case（`aes_size ?? aesSize`），
//! 但 `encryptDatNative` 的入参 meta 由 TS 以 snake_case 构造，因此这里全部显式
//! `js_name` 固定为 snake_case，避免 napi 默认 camelCase 转换造成键名不匹配。

use napi::bindgen_prelude::Buffer;
use napi_derive::napi;
use std::fs;
use weflow_core::dat::{self, DatMeta};

#[napi(object)]
pub struct DecryptResult {
    pub data: Buffer,
    pub ext: String,
    #[napi(js_name = "is_wxgf")]
    pub is_wxgf: bool,
    pub version: u32,
    #[napi(js_name = "aes_size")]
    pub aes_size: Option<u32>,
    #[napi(js_name = "xor_size")]
    pub xor_size: Option<u32>,
    #[napi(js_name = "raw_size")]
    pub raw_size: Option<u32>,
    pub flag: Option<u32>,
}

#[napi(object)]
pub struct DatMetaInput {
    pub version: Option<u32>,
    #[napi(js_name = "aes_size")]
    pub aes_size: Option<u32>,
    #[napi(js_name = "aesSize")]
    pub aes_size_camel: Option<u32>,
    #[napi(js_name = "xor_size")]
    pub xor_size: Option<u32>,
    #[napi(js_name = "xorSize")]
    pub xor_size_camel: Option<u32>,
    #[napi(js_name = "raw_size")]
    pub raw_size: Option<u32>,
    #[napi(js_name = "rawSize")]
    pub raw_size_camel: Option<u32>,
    pub flag: Option<u32>,
}

#[napi(js_name = "decryptDatNative")]
pub fn decrypt_dat_native(
    input_path: String,
    xor_key: u32,
    aes_key: Option<String>,
) -> napi::Result<DecryptResult> {
    let data = fs::read(&input_path)
        .map_err(|e| napi::Error::from_reason(format!("read {input_path}: {e}")))?;
    let dec = dat::decrypt_dat(&data, xor_key, aes_key.as_deref())
        .map_err(|e| napi::Error::from_reason(e.to_string()))?;
    Ok(DecryptResult {
        data: Buffer::from(dec.data),
        ext: dec.ext,
        is_wxgf: dec.is_wxgf,
        version: dec.version,
        aes_size: dec.meta.aes_size,
        xor_size: dec.meta.xor_size,
        raw_size: dec.meta.raw_size,
        flag: dec.meta.flag,
    })
}

/// inputPath 指向明文文件，返回重新加密后的 dat 字节（编辑场景回写用）。
#[napi(js_name = "encryptDatNative")]
pub fn encrypt_dat_native(
    input_path: String,
    xor_key: u32,
    aes_key: Option<String>,
    meta: Option<DatMetaInput>,
) -> napi::Result<Buffer> {
    let plain = fs::read(&input_path)
        .map_err(|e| napi::Error::from_reason(format!("read {input_path}: {e}")))?;
    let meta = meta.map(|m| DatMeta {
        version: m.version,
        aes_size: m.aes_size.or(m.aes_size_camel),
        xor_size: m.xor_size.or(m.xor_size_camel),
        raw_size: m.raw_size.or(m.raw_size_camel),
        flag: m.flag,
    });
    let out = dat::encrypt_dat(&plain, xor_key, aes_key.as_deref(), meta.as_ref())
        .map_err(|e| napi::Error::from_reason(e.to_string()))?;
    Ok(Buffer::from(out))
}
