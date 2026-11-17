//! 剪贴板读写（纯 Win32，文本内容仅存本机内存与本地文件）

use windows::Win32::Foundation::{HANDLE, HGLOBAL, GlobalFree};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::Foundation::HWND;

const CF_UNICODETEXT: u32 = 13;

pub fn get_text(hwnd: HWND) -> Option<String> {
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return None;
        }
        let result = (|| {
            let h = GetClipboardData(CF_UNICODETEXT).ok()?;
            let hglobal = HGLOBAL(h.0);
            let ptr = GlobalLock(hglobal) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
                if len > 1 << 22 {
                    break;
                }
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            let s = String::from_utf16_lossy(slice);
            let _ = GlobalUnlock(hglobal);
            Some(s)
        })();
        let _ = CloseClipboard();
        result
    }
}

pub fn set_text(hwnd: HWND, text: &str) -> bool {
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let bytes = wide.len() * 2;
        let ok = (|| -> Option<bool> {
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
            let dst = GlobalLock(h) as *mut u16;
            if dst.is_null() {
                let _ = GlobalFree(Some(h));
                return Some(false);
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            let _ = GlobalUnlock(h);
            Some(SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h.0))).is_ok())
        })()
        .unwrap_or(false);
        let _ = CloseClipboard();
        ok
    }
}
