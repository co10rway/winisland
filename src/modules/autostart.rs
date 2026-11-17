//! 开机自启 — HKCU\...\Run 注册表键（仅本机，无网络）

use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::System::Registry::*;
use windows::core::{w, PCWSTR};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("WinIsland");

fn exe_path() -> String {
    std::env::current_exe()
        .map(|p| format!("\"{}\"", p.display()))
        .unwrap_or_default()
}

pub fn is_enabled() -> bool {
    unsafe {
        let mut hkey = Default::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_READ, &mut hkey) != WIN32_ERROR(0) {
            return false;
        }
        let mut buf = [0u8; 1024];
        let mut len = buf.len() as u32;
        let ok = RegQueryValueExW(hkey, VALUE_NAME, None, None, Some(buf.as_mut_ptr()), Some(&mut len)) == WIN32_ERROR(0);
        let _ = RegCloseKey(hkey);
        ok
    }
}

pub fn set_enabled(on: bool) -> bool {
    unsafe {
        let mut hkey = Default::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        ) != WIN32_ERROR(0)
        {
            return false;
        }
        let result = if on {
            let path = exe_path();
            RegSetValueExW(hkey, VALUE_NAME, None, REG_SZ, Some(path.as_bytes())) == WIN32_ERROR(0)
        } else {
            RegDeleteValueW(hkey, VALUE_NAME) == WIN32_ERROR(0) || !is_enabled()
        };
        let _ = RegCloseKey(hkey);
        result
    }
}
