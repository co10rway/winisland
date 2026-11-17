//! 待办「新增」对话框 —— 独立自绘小窗（原生 EDIT 保证中文输入法 / 剪贴板可用）
//!
//! 为什么不直接在主面板内嵌输入框：主窗口是 WS_EX_NOACTIVATE 的自绘 D2D 窗口，
//! 内嵌的原生控件无法聚焦、无法调出输入法。所以做成一个独立、可激活的小对话框，
//! 视觉与设置窗口一致（深色底 + 强调色 + 12px 圆角），回车确认、Esc 取消。
//!
//! 采用模态消息循环：打开期间该线程在此阻塞，但循环仍用 GetMessageW 取并派发
//! 本线程的所有消息（含主窗口的绘制/点击），因此主窗口不会卡死，只是帧驱动
//! 停在输入对话框期间（输入时本就无需动画）。

use std::cell::RefCell;

use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWM_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateFontW, CreateSolidBrush, EndPaint, InvalidateRect,
    SetBkColor, SetBkMode, SetTextColor, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, DEFAULT_CHARSET,
    DEFAULT_PITCH, FW_NORMAL, HDC, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::render::{s, si, Canvas, W_NORMAL, W_SEMI};

// 原生控件消息/按键（windows crate 未导出，手写值）
const EM_SETMARGINS: u32 = 0x00D3;
const EM_SETCUEBANNER: u32 = 0x1501;
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;

// ───────────────────────── 尺寸 / 颜色 ─────────────────────────
const W: f32 = 400.0;
const H: f32 = 196.0;
const TITLE_H: f32 = 62.0;
const INPUT_H: f32 = 40.0;
const BTN_H: f32 = 36.0;
const BTN_W: f32 = 104.0;

const BG: u32 = 0x0F0F13;
const CARD: u32 = 0x16161B;
const INPUT_BG: u32 = 0x1E1E25;
const TXT: u32 = 0xF0F0F5;
const TXT2: u32 = 0x9C9CA7;
const BTN_BG: u32 = 0x23232B;

thread_local! {
    static WIN: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static EDIT: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static CANVAS: RefCell<Option<Canvas>> = const { RefCell::new(None) };
    static EDIT_FONT: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static EDIT_BRUSH: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 确认时（销毁窗口前）读到的输入文本，供 prompt() 返回时使用
    static RESULT: RefCell<String> = const { RefCell::new(String::new()) };
}

pub enum Result2 {
    /// 用户确认
    Confirm(String),
    /// 用户取消 / 关闭
    Cancel,
}

/// 打开「新增待办」对话框（模态），返回用户输入结果。
/// 返回前会恢复主窗口置顶。
pub fn prompt() -> Result2 {
    unsafe {
        if WIN.with(|c| c.get()) != 0 {
            return Result2::Cancel;
        }
        let hinst = GetModuleHandleW(None).unwrap();
        ensure_class(hinst.into());
        let class_name = w!("WinIslandTodoAdd");
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("新增待办"),
            WS_POPUP,
            (sw - si(W)) / 2,
            (sh - si(H)) / 2 - si(120.0),
            si(W),
            si(H),
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("CreateWindowExW todo-add");

        // Win11：圆角 + 去边框线
        let pref: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        let no_border: u32 = 0xFFFF_FFFE;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &no_border as *const _ as *const core::ffi::c_void, 4);

        WIN.with(|c| c.set(hwnd.0 as isize));
        DONE.with(|c| c.set(false));
        RESULT.with(|c| c.borrow_mut().clear());
        build_edit_ui(hwnd);
        crate::app::set_island_topmost(false);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let h = EDIT.with(|c| c.get());
        if h != 0 {
            let _ = SetFocus(Some(HWND(h as *mut core::ffi::c_void)));
        }
        let _ = InvalidateRect(Some(hwnd), None, false);

        // ── 模态消息循环 ──
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // EDIT 内按回车 → 视为确认
            let is_edit_enter = msg.hwnd == HWND(EDIT.with(|c| c.get()) as *mut core::ffi::c_void)
                && msg.message == WM_KEYDOWN
                && msg.wParam.0 as u32 == VK_RETURN;
            if msg.hwnd == hwnd && msg.message == WM_KEYDOWN && msg.wParam.0 as u32 == VK_ESCAPE {
                DONE.with(|c| c.set(true));
                let _ = DestroyWindow(hwnd);
                continue;
            }
            if is_edit_enter {
                commit_and_destroy(hwnd);
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if DONE.with(|c| c.get()) {
                break;
            }
        }

        let txt = RESULT.with(|c| c.borrow().clone());
        WIN.with(|c| c.set(0));
        EDIT.with(|c| c.set(0));
        crate::app::set_island_topmost(true);
        if txt.is_empty() {
            Result2::Cancel
        } else {
            Result2::Confirm(txt)
        }
    }
}

unsafe fn ensure_class(hinst: HINSTANCE) {
    let class_name = w!("WinIslandTodoAdd");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinst.into(),
        lpszClassName: class_name,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&wc);
}

unsafe fn build_edit_ui(parent: HWND) {
    let font = CreateFontW(
        -((13.0 * s(1.0)).round() as i32),
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        DEFAULT_PITCH.0 as u32,
        w!("Microsoft YaHei UI"),
    );
    EDIT_FONT.with(|c| c.set(font.0 as isize));
    let brush = CreateSolidBrush(cref(INPUT_BG));
    EDIT_BRUSH.with(|c| c.set(brush.0 as isize));

    let ex = s(24.0);
    let ew = s(W) - s(48.0);
    let ey = s(TITLE_H + 12.0);
    let h = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("EDIT"),
        w!(""),
        WINDOW_STYLE((ES_LEFT | ES_AUTOHSCROLL) as u32) | WS_CHILD | WS_VISIBLE | WS_TABSTOP,
        (ex + s(9.0)) as i32,
        (ey + s(5.0)) as i32,
        (ew - s(18.0)) as i32,
        (s(INPUT_H) - s(10.0)) as i32,
        Some(parent),
        Some(HMENU(101usize as *mut core::ffi::c_void)),
        Some(GetModuleHandleW(None).unwrap().into()),
        None,
    )
    .expect("CreateWindowExW edit");
    let _ = SendMessageW(h, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    // 用经典渲染（与自绘框 + ey+5 的位置参数配套，文字垂直位置才对得上）。
    // 设成 "Explorer" 主题会给 EDIT 加内部 padding，导致文字在框里偏上。
    let _ = SetWindowTheme(h, w!(""), w!(""));
    let _ = SendMessageW(h, EM_SETMARGINS, Some(WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize)), Some(LPARAM(0)));
    let wide = HSTRING::from("输入待办内容，回车添加");
    let _ = SendMessageW(h, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(wide.as_ptr() as isize)));
    EDIT.with(|c| c.set(h.0 as isize));
}

/// 确认操作：必须在销毁窗口【之前】读取 EDIT 文本。
/// 否则控件随窗口一起销毁，GetWindowTextW 只能拿到空串——这正是
/// 「点了添加，面板里却不显示」的根因。
unsafe fn commit_and_destroy(hwnd: HWND) {
    let txt = input_text();
    RESULT.with(|c| *c.borrow_mut() = txt);
    DONE.with(|c| c.set(true));
    let _ = DestroyWindow(hwnd);
}

unsafe fn input_text() -> String {
    let h = EDIT.with(|c| c.get());
    if h == 0 {
        return String::new();
    }
    let h = HWND(h as *mut core::ffi::c_void);
    let n = GetWindowTextLengthW(h) as usize;
    let mut buf = vec![0u16; n + 1];
    let got = GetWindowTextW(h, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..got]).trim().to_string()
}

/// 判断点是否命中自绘控件
fn in_r(x: i32, y: i32, x0: f32, y0: f32, w0: f32, h0: f32) -> bool {
    let (x, y) = (x as f32, y as f32);
    x >= x0 && x <= x0 + w0 && y >= y0 && y <= y0 + h0
}

fn draw(cv: &mut Canvas, w: f32, h: f32) {
    // 背景
    cv.rrect(0.0, 0.0, w, h, s(12.0), BG, 1.0);
    // 标题
    cv.text1(s(24.0), s(20.0), w - s(48.0), s(30.0), "新增待办", 17.0, W_SEMI, TXT, 1.0);
    // 输入框外框
    cv.rrect(s(24.0), s(TITLE_H + 8.0), w - s(48.0), s(INPUT_H), s(9.0), CARD, 1.0);
    // 底部按钮区
    let cyy = h - s(16.0) - s(BTN_H);
    // 取消（左）——文字水平/垂直居中
    cv.rrect(s(24.0), cyy, s(BTN_W), s(BTN_H), s(9.0), BTN_BG, 1.0);
    cv.text_ex(s(24.0), cyy, s(BTN_W), s(BTN_H), "取消", 12.5, W_NORMAL, TXT2, 1.0, 1, 1);
    // 添加（右，强调色）
    let apex = w - s(24.0) - s(BTN_W);
    cv.rrect(apex, cyy, s(BTN_W), s(BTN_H), s(9.0), crate::theme::main(), 0.95);
    cv.text_ex(apex, cyy, s(BTN_W), s(BTN_H), "添加", 12.5, W_SEMI, crate::theme::tint(), 1.0, 1, 1);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE if wparam.0 != 0 => LRESULT(0),
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (w, h) = (rc.right.max(1), rc.bottom.max(1));
            CANVAS.with(|cell| {
                let mut slot = cell.borrow_mut();
                if slot.is_none() {
                    *slot = Canvas::new().ok();
                }
                if let Some(cv) = slot.as_mut() {
                    let memdc = cv.draw_to_mem(w as i32, h as i32, BG, |cv, w, h| {
                        draw(cv, w, h);
                    });
                    let _ = BitBlt(hdc, 0, 0, w as i32, h as i32, Some(memdc), 0, 0, SRCCOPY);
                }
            });
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            let _ = (x, y);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let cyy = s(H) - s(16.0) - s(BTN_H);
            // 取消（左）
            if in_r(x, y, s(24.0), cyy, s(BTN_W), s(BTN_H)) {
                DONE.with(|c| c.set(true));
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            // 添加（右）
            let apex = s(W) - s(24.0) - s(BTN_W);
            if in_r(x, y, apex, cyy, s(BTN_W), s(BTN_H)) {
                commit_and_destroy(hwnd);
                return LRESULT(0);
            }
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            let _ = SetTextColor(hdc, cref(TXT));
            let _ = SetBkColor(hdc, cref(INPUT_BG));
            let _ = SetBkMode(hdc, TRANSPARENT);
            LRESULT(EDIT_BRUSH.with(|c| c.get()))
        }
        WM_KEYDOWN => {
            if wparam.0 == VK_ESCAPE as usize {
                DONE.with(|c| c.set(true));
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) != 0 {
                crate::app::set_island_topmost(false);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            DONE.with(|c| c.set(true));
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn cref(hex: u32) -> windows::Win32::Foundation::COLORREF {
    windows::Win32::Foundation::COLORREF(hex as u32)
}
