//! 本机凭据保护（Windows DPAPI）：给小段机密（邮箱密码 / CalDAV 密码）做本机加密后落盘。
//!
//! 密文格式：`dpapi:` + 十六进制。以该前缀识别「已加密 vs 旧版明文」：
//! 读到时是 `dpapi:` 前缀 → 解密还原为明文给上层使用；读到时是普通字符串
//! → 视为旧版明文，原样透传，等待下次保存时一并加密——升级平滑、不丢凭据。
//! DPAPI 使用当前登录用户凭据派生密钥，因此密文只能在本机当前用户下解开。

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
};

const PREFIX: &str = "dpapi:";

/// 加密：返回带 `dpapi:` 前缀的十六进制密文。入参为空串时原样返回空串。
/// DPAPI 失败（极罕见，如非交互会话）时返回 None，调用方应保留原值以免丢数据。
pub fn protect(plain: &str) -> Option<String> {
    if plain.is_empty() {
        return Some(String::new());
    }
    let bytes = plain.as_bytes();
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        // 描述符传 None（不写入持久描述）；无提示，纯本机解密，UI 全程不弹窗
        CryptProtectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .ok()?
    };
    let data = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) };
    let hex = data.iter().map(|b| format!("{b:02x}")).collect::<String>();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut core::ffi::c_void)));
    }
    Some(format!("{PREFIX}{hex}"))
}

/// 解密：输入须为 `dpapi:` 前缀密文；若是旧版明文则原样透传。
/// 解密失败（凭据换人 / 密文损坏）返回 None，调用方保留原值，不静默清空。
pub fn unprotect(enc: &str) -> Option<String> {
    if enc.is_empty() {
        return Some(String::new());
    }
    let Some(body) = enc.strip_prefix(PREFIX) else {
        return Some(enc.to_string()); // 旧版明文
    };
    let bytes = hex_decode(body)?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            .ok()?
    };
    let data = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) };
    let s = String::from_utf8_lossy(data).into_owned();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut core::ffi::c_void)));
    }
    Some(s)
}

fn hex_decode(body: &str) -> Option<Vec<u8>> {
    if body.len() % 2 != 0 {
        return None;
    }
    let b = body.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i + 1 < b.len() {
        let hi = (b[i] as char).to_digit(16)? as u8;
        let lo = (b[i + 1] as char).to_digit(16)? as u8;
        out.push(hi << 4 | lo);
        i += 2;
    }
    Some(out)
}
