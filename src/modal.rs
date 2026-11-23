//! 模态对话框共享样板：自绘小窗（设置 / 待办新增 / 盯盘自选）曾各自维护一份
//! COLORREF 便捷构造 `cref()`、原生 EDIT 控件消息常量（EM_SETMARGINS /
//! EM_SETCUEBANNER），以及关闭 EDIT 主题、设置内边距的调用。现统一收口到这里，
//! 三处复用，改动一处全局生效。

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::core::w;

/// 原生控件消息（windows crate 未导出，手写值）
pub const EM_SETMARGINS: u32 = 0x00D3;
pub const EM_SETCUEBANNER: u32 = 0x1501;
pub const EC_LEFTMARGIN: u32 = 0x0001;
pub const EC_RIGHTMARGIN: u32 = 0x0002;

/// COLORREF 便捷构造
pub fn cref(hex: u32) -> COLORREF {
    COLORREF(hex)
}

/// 关闭 EDIT 的主题绘制（经典渲染）。Explorer 主题会给 EDIT 加内部 padding，
/// 导致文字在自绘框里偏上；设空主题后文字垂直位置才与手绘参数对得上。
pub fn set_edit_classic(h: HWND) {
    unsafe {
        let _ = SetWindowTheme(h, w!(""), w!(""));
    }
}

/// 设置 EDIT 左右内边距为 0（与自绘框的内缩尺寸配套）。
pub fn set_edit_margins(h: HWND) {
    unsafe {
        let _ = SendMessageW(
            h,
            EM_SETMARGINS,
            Some(WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize)),
            Some(LPARAM(0)),
        );
    }
}
