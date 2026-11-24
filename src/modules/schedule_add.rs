//! 日程「新增」对话框 —— 独立自绘小窗（原生 EDIT 保证中文输入法 / 剪贴板可用）
//!
//! 与待办新增同一套骨架，但需要**内容 + 日期 + 时间 + 提前提醒**四块：
//! - 内容：任意文本；
//! - 日期：`YYYY-MM-DD`，默认今天。做成「Windows 日历那种」的自绘日历弹出层
//!   （深色月历，点哪选哪天），字段本身也可点分段直接输数字；
//! - 时间：`HH:MM`，默认下一整点。参考设置里「打工鼓励器」的时间输入——
//!   半透明遮罩 + 居中卡片，时/分两列 ▲▼ 步进 + 点数字框直接输入 + 确定/取消；
//! - 提前提醒：分钟数（0 = 按全局默认），自绘数值筛选器（▲▼ + 输入）。
//!
//! 全部控件自绘成与整体深色 UI 一致的样子，模态消息循环：参考 todo_add。

use std::cell::{Cell, RefCell};

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
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::mix;
use crate::render::{s, si, Canvas, W_NORMAL, W_MED, W_SEMI};

// 原生控件消息/按键（windows crate 未导出，手写值）
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_BACK: u32 = 0x08;
const VK_UP: u32 = 0x26;
const VK_DOWN: u32 = 0x28;

// ───────────────────────── 尺寸 / 颜色 ─────────────────────────
const W: f32 = 420.0;
const H: f32 = 396.0;
const INPUT_H: f32 = 38.0;
const LABEL_H: f32 = 16.0;
const BTN_H: f32 = 36.0;
const BTN_W: f32 = 104.0;

const BG: u32 = 0x0F0F13;
const CARD: u32 = 0x16161B;
const INPUT_BG: u32 = 0x1E1E25;
const ROW: u32 = 0x1F1F27;
const ROW2: u32 = 0x26262F;
const ROW3: u32 = 0x2D2D3A;
const TXT: u32 = 0xF0F0F5;
const TXT2: u32 = 0x9C9CA7;
const TXT3: u32 = 0x6E6E79;
const BTN_BG: u32 = 0x23232B;

/// 弹出层卡片尺寸（弹层覆盖整个窗口，卡片居中）
const CAL_W: f32 = 320.0;
const CAL_H: f32 = 348.0;
const PICK_W: f32 = 320.0;
const PICK_H: f32 = 320.0;

// 各行的 Y 坐标（draw 与命中测试共享）——逻辑值，绘制时经 s() 缩放
const CONTENT_Y: f32 = 86.0;
const DATE_ROW_Y: f32 = 154.0;
const TIME_Y: f32 = 222.0;
const REMIND_Y: f32 = 290.0;

/// 底部按钮区 Y（运行时经 DPI 缩放，故用函数而非常量）
fn btn_y() -> f32 {
    s(H) - s(16.0) - s(BTN_H)
}

/// Canvas 的 u32 颜色按 RGB 解释，而原生 GDI 控件的 COLORREF 是 BGR 字节序。
/// 直接把 RGB 十六进制透传给 COLORREF 会把 #1E1E25 显示成红棕色，必须交换。
fn gdi_rgb(hex: u32) -> u32 {
    ((hex & 0xFF) << 16) | (hex & 0xFF00) | ((hex >> 16) & 0xFF)
}

/// 数值段标识
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Seg {
    DateY,
    DateM,
    DateD,
    TimeH,
    TimeMin,
    Remind,
    None,
}

/// 当前打开的弹出层
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pop {
    None,
    Cal,   // 自绘日历（日期）
    Time,  // 设置风格时间面板（时间）
}

thread_local! {
    static WIN: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static EDIT_C: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    // 日期三段
    static DATE_Y: Cell<i32> = const { Cell::new(2026) };
    static DATE_M: Cell<u32> = const { Cell::new(1) };
    static DATE_D: Cell<u32> = const { Cell::new(1) };
    // 时间两段
    static TIME_H: Cell<u32> = const { Cell::new(0) };
    static TIME_MIN: Cell<u32> = const { Cell::new(0) };
    // 提前提醒（分钟；0 = 用全局默认）
    static REMIND: Cell<u32> = const { Cell::new(0) };
    static FOCUS: Cell<Seg> = const { Cell::new(Seg::None) };
    // 聚焦段的键盘数字缓冲
    static TYPING: Cell<u64> = const { Cell::new(0) };
    static TYPING_LEN: Cell<u32> = const { Cell::new(0) };
    // 当前弹出层
    static POP: Cell<Pop> = const { Cell::new(Pop::None) };
    // 日历导航用的「正在翻看的年月」（与选中日期 DATE_Y/M 不总相同）
    static CAL_Y: Cell<i32> = const { Cell::new(2026) };
    static CAL_M: Cell<u32> = const { Cell::new(1) };
    // 时间面板：哪个数字框在键盘输入（0=时 1=分），None=步进
    static TIME_EDIT: Cell<Option<usize>> = const { Cell::new(None) };
    static TIME_BUF: RefCell<[String; 2]> = const { RefCell::new([String::new(), String::new()]) };
    static CANVAS: RefCell<Option<Canvas>> = const { RefCell::new(None) };
    static EDIT_FONT: Cell<isize> = const { Cell::new(0) };
    static EDIT_BRUSH: Cell<isize> = const { Cell::new(0) };
    static DONE: Cell<bool> = const { Cell::new(false) };
    /// 确认时（销毁窗口前）读到的输入
    static RESULT: RefCell<(String, String, String, u32)> =
        const { RefCell::new((String::new(), String::new(), String::new(), 0)) };
}

pub enum Result3 {
    /// 用户确认（内容, 日期, 时间, 提前提醒分钟数）
    Confirm(String, String, String, u32),
    /// 用户取消 / 关闭
    Cancel,
}

/// 校验日期格式：`YYYY-MM-DD`
pub fn valid_date(s: &str) -> bool {
    let s = s.trim();
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && s[0..4].chars().all(|c| c.is_ascii_digit())
        && s[5..7].chars().all(|c| c.is_ascii_digit())
        && s[8..10].chars().all(|c| c.is_ascii_digit())
}

/// 校验时间格式：`HH:MM`
pub fn valid_time(s: &str) -> bool {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' {
        return false;
    }
    let (h, m) = (&s[0..2], &s[3..5]);
    let (Ok(h), Ok(m)) = (h.parse::<u32>(), m.parse::<u32>()) else {
        return false;
    };
    (0..24).contains(&h) && (0..60).contains(&m)
}

/// 打开「新增日程」对话框（模态），返回用户输入结果。
pub fn prompt() -> Result3 {
    unsafe {
        if WIN.with(|c| c.get()) != 0 {
            return Result3::Cancel;
        }
        let hinst = GetModuleHandleW(None).unwrap();
        ensure_class(hinst.into());
        let class_name = w!("WinIslandScheduleAdd");
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("新增日程"),
            WS_POPUP | WS_CLIPCHILDREN,
            (sw - si(W)) / 2,
            (sh - si(H)) / 2 - si(40.0),
            si(W),
            si(H),
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("CreateWindowExW schedule-add");

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
        RESULT.with(|r| *r.borrow_mut() = (String::new(), String::new(), String::new(), 0));
        init_vals();
        build_ui(hwnd);
        crate::app::set_island_topmost(false);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let h = EDIT_C.with(|c| c.get());
        if h != 0 {
            let _ = SetFocus(Some(HWND(h as *mut core::ffi::c_void)));
        }
        let _ = InvalidateRect(Some(hwnd), None, false);

        // ── 模态消息循环 ──
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_KEYDOWN {
                let edit_hwnd = HWND(EDIT_C.with(|c| c.get()) as *mut core::ffi::c_void);
                // 内容框回车 → 确认
                if msg.hwnd == edit_hwnd && msg.wParam.0 as u32 == VK_RETURN {
                    commit_and_destroy(hwnd);
                    continue;
                }
                // 模态窗口上的 Esc：先关弹出层，再取消对话框
                if msg.hwnd == hwnd && msg.wParam.0 as u32 == VK_ESCAPE {
                    if POP.with(|p| p.get()) != Pop::None {
                        POP.with(|p| p.set(Pop::None));
                        TIME_EDIT.with(|c| c.set(None));
                        let _ = SetFocus(Some(hwnd));
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        continue;
                    }
                    DONE.with(|c| c.set(true));
                    let _ = DestroyWindow(hwnd);
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if DONE.with(|c| c.get()) {
                break;
            }
        }

        if msg.message == WM_QUIT {
            DONE.with(|c| c.set(true));
            let _ = DestroyWindow(hwnd);
            clear_wins();
            crate::app::set_island_topmost(true);
            PostQuitMessage(0);
            return Result3::Cancel;
        }

        let (c, d, t, r) = RESULT.with(|r| r.borrow().clone());
        clear_wins();
        crate::app::set_island_topmost(true);
        let c = c.trim().to_string();
        if c.is_empty() || !valid_date(&d) || !valid_time(&t) {
            Result3::Cancel
        } else {
            Result3::Confirm(c, d, t, r)
        }
    }
}

fn init_vals() {
    let st = crate::modules::clock::local_time();
    DATE_Y.with(|c| c.set(st.wYear as i32));
    DATE_M.with(|c| c.set(st.wMonth as u32));
    DATE_D.with(|c| c.set(st.wDay as u32));
    let mut h = st.wHour + 1;
    if h >= 24 {
        h = 0;
    }
    TIME_H.with(|c| c.set(h as u32));
    TIME_MIN.with(|c| c.set(0));
    REMIND.with(|c| c.set(0));
    FOCUS.with(|c| c.set(Seg::None));
    TYPING.with(|c| c.set(0));
    TYPING_LEN.with(|c| c.set(0));
    POP.with(|c| c.set(Pop::None));
    TIME_EDIT.with(|c| c.set(None));
    TIME_BUF.with(|b| *b.borrow_mut() = [String::new(), String::new()]);
    CAL_Y.with(|c| c.set(st.wYear as i32));
    CAL_M.with(|c| c.set(st.wMonth as u32));
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

fn clamp_day() {
    let y = DATE_Y.with(|c| c.get());
    let m = DATE_M.with(|c| c.get());
    let max = days_in_month(y, m);
    let d = DATE_D.with(|c| c.get());
    if d > max {
        DATE_D.with(|c| c.set(max));
    }
}

/// Tomohiko Sakamoto：返回 0=周日 … 6=周六
fn weekday_of(y: i32, m: u32, d: u32) -> usize {
    let t = [0usize, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if m < 3 { y - 1 } else { y };
    (y as usize + (y / 4) as usize - (y / 100) as usize + (y / 400) as usize + t[(m - 1) as usize] + d as usize) % 7
}

fn seg_value(seg: Seg) -> String {
    match seg {
        Seg::DateY => format!("{}", DATE_Y.with(|c| c.get())),
        Seg::DateM => format!("{:02}", DATE_M.with(|c| c.get())),
        Seg::DateD => format!("{:02}", DATE_D.with(|c| c.get())),
        Seg::TimeH => format!("{:02}", TIME_H.with(|c| c.get())),
        Seg::TimeMin => format!("{:02}", TIME_MIN.with(|c| c.get())),
        Seg::Remind => {
            let r = REMIND.with(|c| c.get());
            if r == 0 {
                "默认".to_string()
            } else {
                format!("{}", r)
            }
        }
        Seg::None => String::new(),
    }
}

fn seg_bounds(seg: Seg) -> (i64, i64) {
    match seg {
        Seg::DateY => (2020, 2100),
        Seg::DateM => (1, 12),
        Seg::DateD => (1, days_in_month(DATE_Y.with(|c| c.get()), DATE_M.with(|c| c.get())) as i64),
        Seg::TimeH => (0, 23),
        Seg::TimeMin => (0, 59),
        Seg::Remind => (0, 1440),
        Seg::None => (0, 0),
    }
}

fn set_seg(seg: Seg, v: i64) {
    let (_lo, hi) = seg_bounds(seg);
    let v = v.clamp(_lo, hi);
    match seg {
        Seg::DateY => DATE_Y.with(|c| c.set(v as i32)),
        Seg::DateM => DATE_M.with(|c| c.set(v as u32)),
        Seg::DateD => DATE_D.with(|c| c.set(v as u32)),
        Seg::TimeH => TIME_H.with(|c| c.set(v as u32)),
        Seg::TimeMin => TIME_MIN.with(|c| c.set(v as u32)),
        Seg::Remind => REMIND.with(|c| c.set(v as u32)),
        Seg::None => {}
    }
    if seg == Seg::DateY || seg == Seg::DateM {
        clamp_day();
    }
}

fn seg_cur(seg: Seg) -> i64 {
    match seg {
        Seg::DateY => DATE_Y.with(|c| c.get()) as i64,
        Seg::DateM => DATE_M.with(|c| c.get()) as i64,
        Seg::DateD => DATE_D.with(|c| c.get()) as i64,
        Seg::TimeH => TIME_H.with(|c| c.get()) as i64,
        Seg::TimeMin => TIME_MIN.with(|c| c.get()) as i64,
        Seg::Remind => REMIND.with(|c| c.get()) as i64,
        Seg::None => 0,
    }
}

fn clear_wins() {
    WIN.with(|c| c.set(0));
    EDIT_C.with(|c| c.set(0));
    FOCUS.with(|c| c.set(Seg::None));
    POP.with(|c| c.set(Pop::None));
}

unsafe fn ensure_class(hinst: HINSTANCE) {
    let class_name = w!("WinIslandScheduleAdd");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        lpszClassName: class_name,
        hIcon: crate::modules::tray::app_icon(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&wc);
}

// ────────────────────── 几何（draw 与命中共用） ──────────────────────

#[derive(Clone, Copy)]
struct SegGeom {
    seg: Seg,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// 表单上的数值段几何：日期=三段+日历图标、时间=两段+时钟图标、提醒=整行
type Rect = (f32, f32, f32, f32);

/// 表单字段几何。日期/时间各是「一个完整容器 + 内部分段 + 右端图标」，
/// 不再把每段画成孤立方块；提醒仍是独立整行（自带步进胶囊）。
fn field_geoms(mw: f32) -> (Vec<SegGeom>, Rect, Rect) {
    let mx = s(24.0);
    let h = s(INPUT_H);
    let icon_w = s(48.0);

    // 日期：年 / 月 / 日 三段紧邻，共享一个容器；图标占容器最右
    let d_inner = mw - icon_w;
    let year_w = s(112.0);
    let d_each = (d_inner - year_w) / 2.0;
    let date_segs = [
        SegGeom { seg: Seg::DateY, x: mx, y: s(DATE_ROW_Y), w: year_w, h },
        SegGeom { seg: Seg::DateM, x: mx + year_w, y: s(DATE_ROW_Y), w: d_each, h },
        SegGeom { seg: Seg::DateD, x: mx + year_w + d_each, y: s(DATE_ROW_Y), w: d_each, h },
    ];
    let date_icon = (mx + d_inner, s(DATE_ROW_Y), icon_w, h);

    // 时间：时 / 分 两段紧邻
    let t_inner = mw - icon_w;
    let t_each = t_inner / 2.0;
    let time_segs = [
        SegGeom { seg: Seg::TimeH, x: mx, y: s(TIME_Y), w: t_each, h },
        SegGeom { seg: Seg::TimeMin, x: mx + t_each, y: s(TIME_Y), w: t_each, h },
    ];
    let time_icon = (mx + t_inner, s(TIME_Y), icon_w, h);

    let mut all = Vec::with_capacity(6);
    all.extend(date_segs.iter().copied());
    all.extend(time_segs.iter().copied());
    all.push(SegGeom { seg: Seg::Remind, x: mx, y: s(REMIND_Y), w: mw, h });
    (all, date_icon, time_icon)
}

fn in_r(x: f32, y: f32, r: (f32, f32, f32, f32)) -> bool {
    x >= r.0 && x <= r.0 + r.2 && y >= r.1 && y <= r.1 + r.3
}

// ────────────────────── 输入逻辑 ──────────────────────

fn flush_typing() {
    let seg = FOCUS.with(|c| c.get());
    if seg == Seg::None {
        return;
    }
    let n = TYPING.with(|c| c.get());
    let len = TYPING_LEN.with(|c| c.get());
    if len > 0 {
        let (_lo, hi) = seg_bounds(seg);
        let v = (n as i64).clamp(_lo, hi);
        set_seg(seg, v);
        TYPING.with(|c| c.set(0));
        TYPING_LEN.with(|c| c.set(0));
    }
}

fn focus_seg(seg: Seg) {
    flush_typing();
    TYPING.with(|c| c.set(0));
    TYPING_LEN.with(|c| c.set(0));
    FOCUS.with(|c| c.set(seg));
}

fn type_digit(d: u8) {
    let seg = FOCUS.with(|c| c.get());
    if seg == Seg::None {
        return;
    }
    let (_lo, hi) = seg_bounds(seg);
    let max_len = if hi >= 1000 { 4 } else if hi >= 100 { 3 } else { 2 };
    let len = TYPING_LEN.with(|c| c.get());
    if len as usize >= max_len {
        return;
    }
    let mut n = TYPING.with(|c| c.get());
    n = n * 10 + d as u64;
    if (n as i64) > hi {
        return;
    }
    TYPING.with(|c| c.set(n));
    TYPING_LEN.with(|c| c.set(len + 1));
}

fn backspace() {
    let len = TYPING_LEN.with(|c| c.get());
    if len > 0 {
        let n = TYPING.with(|c| c.get());
        TYPING.with(|c| c.set(n / 10));
        TYPING_LEN.with(|c| c.set(len - 1));
        return;
    }
    let seg = FOCUS.with(|c| c.get());
    if seg != Seg::None {
        set_seg(seg, seg_cur(seg) - 1);
    }
}

fn step_seg(dir: i64) {
    let seg = FOCUS.with(|c| c.get());
    if seg == Seg::None {
        return;
    }
    flush_typing();
    set_seg(seg, seg_cur(seg) + dir);
}

// ────────────────────── 弹出层：日历 ──────────────────────

/// 日历卡片居中矩形
fn cal_card(w: f32, h: f32) -> (f32, f32, f32, f32) {
    let cw = s(CAL_W);
    let ch = s(CAL_H);
    (((w - cw) / 2.0), ((h - ch) / 2.0), cw, ch)
}

/// 日历第 day 天的格子矩形（本地坐标基于卡片）
fn cal_day_rect(cx: f32, cy: f32, cw: f32, ch: f32, idx: usize) -> (f32, f32, f32, f32) {
    let head_h = s(44.0);
    let wk_h = s(24.0);
    let grid_top = cy + head_h + wk_h;
    let grid_h = ch - head_h - wk_h - s(46.0);
    let cell_w = (cw - s(24.0)) / 7.0;
    let cell_h = grid_h / 6.0;
    let row = idx / 7;
    let col = idx % 7;
    let x = cx + s(12.0) + col as f32 * cell_w;
    let y = grid_top + row as f32 * cell_h;
    (x, y, cell_w, cell_h)
}

fn cal_step(delta: i32) {
    let y = CAL_Y.with(|c| c.get());
    let m = CAL_M.with(|c| c.get());
    let mut m = m as i32 + delta;
    let mut y = y;
    if m < 1 {
        m = 12;
        y -= 1;
    } else if m > 12 {
        m = 1;
        y += 1;
    }
    CAL_Y.with(|c| c.set(y));
    CAL_M.with(|c| c.set(m as u32));
}

/// 星期几中文（weekday_of 返回 0=周日 … 6=周六）
fn cn_weekday(y: i32, m: u32, d: u32) -> &'static str {
    const WD: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];
    WD[weekday_of(y, m, d)]
}

/// 日历左右导航按钮（圆形淡底，draw 与命中共用）
fn cal_nav_rects(cx: f32, cy: f32, cw: f32) -> (Rect, Rect) {
    let r = s(13.0);
    let ay = cy + s(22.0);
    (
        (cx + s(24.0) - r, ay - r, r * 2.0, r * 2.0),
        (cx + cw - s(24.0) - r, ay - r, r * 2.0, r * 2.0),
    )
}

/// 日历底部「今天」快捷按钮（draw 与命中共用）
fn cal_today_rect(cx: f32, cy: f32, cw: f32, ch: f32) -> Rect {
    (cx + s(12.0), cy + ch - s(40.0), cw - s(24.0), s(30.0))
}

/// 线性 chevron（日历左右翻页用）
fn draw_chevron(cv: &mut Canvas, cx: f32, cy: f32, left: bool, color: u32, a: f32) {
    let d = s(4.0);
    if left {
        cv.line(cx + d * 0.5, cy - d, cx - d * 0.5, cy, color, a, s(1.6));
        cv.line(cx - d * 0.5, cy, cx + d * 0.5, cy + d, color, a, s(1.6));
    } else {
        cv.line(cx - d * 0.5, cy - d, cx + d * 0.5, cy, color, a, s(1.6));
        cv.line(cx + d * 0.5, cy, cx - d * 0.5, cy + d, color, a, s(1.6));
    }
}

/// 线性 caret（时间步进用，上/下）
fn draw_caret(cv: &mut Canvas, cx: f32, cy: f32, up: bool, color: u32, a: f32) {
    let d = s(4.0);
    if up {
        cv.line(cx - d, cy + d * 0.5, cx, cy - d * 0.5, color, a, s(1.6));
        cv.line(cx, cy - d * 0.5, cx + d, cy + d * 0.5, color, a, s(1.6));
    } else {
        cv.line(cx - d, cy - d * 0.5, cx, cy + d * 0.5, color, a, s(1.6));
        cv.line(cx, cy + d * 0.5, cx + d, cy - d * 0.5, color, a, s(1.6));
    }
}

fn draw_calendar(cv: &mut Canvas, w: f32, h: f32) {
    let (cx, cy, cw, ch) = cal_card(w, h);
    let accent = crate::theme::main();
    cv.rrect(0.0, 0.0, w, h, 0.0, 0x000000, 0.55);
    cv.rrect(cx, cy, cw, ch, s(16.0), 0x1C1C23, 1.0);
    cv.rrect_stroke(cx + 0.5, cy + 0.5, cw - 1.0, ch - 1.0, s(16.0), 0x3A3A48, 1.0, s(1.0));

    let (cy0, cm) = (CAL_Y.with(|c| c.get()), CAL_M.with(|c| c.get()));
    let (sy, sm, sd) = (DATE_Y.with(|c| c.get()), DATE_M.with(|c| c.get()), DATE_D.with(|c| c.get()));
    let today = crate::modules::clock::local_time();

    // 头部：圆形淡底导航 + 线性 chevron；年月文字居中，不与按钮抢视觉
    let (nl, nr) = cal_nav_rects(cx, cy, cw);
    for (r, left) in [(nl, true), (nr, false)] {
        let ccx = r.0 + r.2 / 2.0;
        let ccy = r.1 + r.3 / 2.0;
        cv.circle(ccx, ccy, r.2 / 2.0, ROW3, 1.0);
        draw_chevron(cv, ccx, ccy, left, TXT2, 1.0);
    }
    cv.text_c(
        cx + s(44.0), cy + s(2.0), cw - s(88.0), s(40.0),
        &format!("{} 年 {} 月", cy0, cm), 14.0, W_SEMI, TXT, 1.0,
    );

    // 星期表头（周一开头）：周末略亮，但不用主色，避免与选中态冲突
    let wk_y = cy + s(44.0);
    let cell_w = (cw - s(24.0)) / 7.0;
    let names = ["一", "二", "三", "四", "五", "六", "日"];
    for (i, nm) in names.iter().enumerate() {
        let x = cx + s(12.0) + i as f32 * cell_w;
        cv.text_c(x, wk_y, cell_w, s(24.0), nm, 10.5, W_SEMI,
                  if i >= 5 { TXT2 } else { TXT3 }, 0.95);
    }

    // 日期格子：极简——没有默认方块底，只有「选中=实心圆 / 今天=描边圆」，其余纯文字
    let days = days_in_month(cy0, cm);
    let lead = (weekday_of(cy0, cm, 1) + 6) % 7; // 周一开头
    let grid_top = cy + s(44.0) + s(24.0);
    let grid_h = ch - s(44.0) - s(24.0) - s(46.0);
    let cell_h = grid_h / 6.0;
    let rad = s(15.0);
    for idx in 0..42usize {
        if idx < lead || idx >= lead + days as usize {
            continue;
        }
        let x = cx + s(12.0) + (idx % 7) as f32 * cell_w;
        let y = grid_top + (idx / 7) as f32 * cell_h;
        let ccx = x + cell_w / 2.0;
        let ccy = y + cell_h / 2.0;
        let day = (idx - lead + 1) as u32;
        let selected = day == sd && cm == sm && cy0 == sy;
        let tday = today.wYear as i32 == cy0 && today.wMonth as u32 == cm && today.wDay as u32 == day;
        let wkend = (idx % 7) >= 5;
        if selected {
            cv.circle(ccx, ccy, rad, accent, 1.0);
            cv.text_c(x, y, cell_w, cell_h, &day.to_string(), 11.5, W_SEMI, crate::theme::tint(), 1.0);
        } else if tday {
            cv.circle_stroke(ccx, ccy, rad, accent, 1.0, s(1.5));
            cv.text_c(x, y, cell_w, cell_h, &day.to_string(), 11.5, W_SEMI, accent, 1.0);
        } else {
            cv.text_c(x, y, cell_w, cell_h, &day.to_string(), 11.5, W_MED,
                      if wkend { TXT2 } else { TXT }, 0.95);
        }
    }

    // 底部「今天」快捷：与翻到的月份解耦，任何时候都能一键回到今天
    let tr = cal_today_rect(cx, cy, cw, ch);
    cv.line(tr.0, tr.1 - s(8.0), tr.0 + tr.2, tr.1 - s(8.0), 0x30303C, 1.0, s(1.0));
    cv.rrect(tr.0, tr.1, tr.2, tr.3, s(7.0), ROW2, 1.0);
    let label = format!("今天 · {}月{}日 周{}", today.wMonth, today.wDay,
                        cn_weekday(today.wYear as i32, today.wMonth as u32, today.wDay as u32));
    cv.text_c(tr.0, tr.1, tr.2, tr.3, &label, 11.5, W_MED, TXT2, 1.0);
}

/// 日历点击：返回 true 表示本次点击已被日历消费
fn cal_click(x: f32, y: f32, w: f32, h: f32) -> bool {
    let (cx, cy, cw, ch) = cal_card(w, h);
    // 卡片外 → 关闭
    if !in_r(x, y, (cx, cy, cw, ch)) {
        POP.with(|c| c.set(Pop::None));
        return true;
    }
    let (nl, nr) = cal_nav_rects(cx, cy, cw);
    if in_r(x, y, nl) {
        cal_step(-1);
        return true;
    }
    if in_r(x, y, nr) {
        cal_step(1);
        return true;
    }
    // 今天快捷 → 选中今天并关闭
    if in_r(x, y, cal_today_rect(cx, cy, cw, ch)) {
        let t = crate::modules::clock::local_time();
        DATE_Y.with(|c| c.set(t.wYear as i32));
        DATE_M.with(|c| c.set(t.wMonth as u32));
        DATE_D.with(|c| c.set(t.wDay as u32));
        FOCUS.with(|c| c.set(Seg::None));
        POP.with(|c| c.set(Pop::None));
        return true;
    }
    // 点某一天 → 选中并关闭
    let cy0 = CAL_Y.with(|c| c.get());
    let cm = CAL_M.with(|c| c.get());
    let days = days_in_month(cy0, cm);
    let lead = (weekday_of(cy0, cm, 1) + 6) % 7;
    for idx in 0..42usize {
        if idx < lead || idx >= lead + days as usize {
            continue;
        }
        let r = cal_day_rect(cx, cy, cw, ch, idx);
        if in_r(x, y, r) {
            let day = (idx - lead + 1) as u32;
            DATE_Y.with(|c| c.set(cy0));
            DATE_M.with(|c| c.set(cm));
            DATE_D.with(|c| c.set(day));
            FOCUS.with(|c| c.set(Seg::None));
            POP.with(|c| c.set(Pop::None));
            return true;
        }
    }
    false
}

// ────────────────────── 弹出层：时间面板 ──────────────────────

fn time_card(w: f32, h: f32) -> (f32, f32, f32, f32) {
    let pw = s(PICK_W);
    let ph = s(PICK_H);
    (((w - pw) / 2.0), ((h - ph) / 2.0), pw, ph)
}

/// 快捷时间：当前时间 + 三个常用整点。点击只填入数值、不关闭面板，方便再微调。
fn quick_times() -> Vec<(String, u32, u32)> {
    let t = crate::modules::clock::local_time();
    vec![
        ("现在".to_string(), t.wHour as u32, t.wMinute as u32),
        ("09:00".to_string(), 9, 0),
        ("12:00".to_string(), 12, 0),
        ("18:00".to_string(), 18, 0),
    ]
}

fn in_circle(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> bool {
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= r * r
}

fn tw(txt: &str, size: f32) -> f32 {
    let sz = s(size);
    txt.chars().map(|ch| if (ch as u32) > 0x2E7F { sz } else { sz * 0.56 }).sum()
}

/// 时间面板的一列（时/分）：圆形 caret 步进 + 大数字框 + 单位。
/// 用线性 caret 替代原本的方块 ▲▼ 文字，去掉笨重感。
fn draw_time_col(cv: &mut Canvas, x: f32, y: f32, w: f32, val: u32, unit: &str, part: usize) {
    let accent = crate::theme::main();
    let editing = TIME_EDIT.with(|c| c.get()) == Some(part);
    let buf = TIME_BUF.with(|b| b.borrow()[part].clone());
    let ux = x + w / 2.0;

    // 上步进：圆形淡底 + 线性 caret
    let uy = y + s(13.0);
    cv.circle(ux, uy, s(12.0), ROW3, 1.0);
    draw_caret(cv, ux, uy, true, TXT2, 1.0);

    // 数字框：聚焦时主色描边 + 光标
    let num_y = y + s(26.0);
    let num_h = s(54.0);
    cv.rrect(x, num_y, w, num_h, s(10.0), INPUT_BG, 1.0);
    cv.rrect_stroke(
        x + 0.5, num_y + 0.5, w - 1.0, num_h - 1.0, s(10.0),
        if editing { accent } else { 0x34343F }, 1.0,
        if editing { s(1.6) } else { s(1.0) },
    );
    let disp = if editing && !buf.is_empty() { buf.clone() } else { format!("{val:02}") };
    cv.text_c(x, num_y, w, num_h, &disp, 26.0, W_SEMI, TXT, 1.0);
    if editing {
        let txt_w = tw(&disp, 26.0);
        let cursor_x = x + (w - txt_w) / 2.0 + txt_w / 2.0 + s(2.0);
        cv.rrect(cursor_x, num_y + s(12.0), s(2.0), num_h - s(24.0), 0.0, 0x7FB4FF, 1.0);
    }

    // 下步进
    let dy = y + s(99.0);
    cv.circle(ux, dy, s(12.0), ROW3, 1.0);
    draw_caret(cv, ux, dy, false, TXT2, 1.0);

    cv.text_c(x, y + s(116.0), w, s(14.0), unit, 10.5, W_NORMAL, TXT3, 0.9);
}

fn draw_time_picker(cv: &mut Canvas, w: f32, h: f32) {
    let (px, py, pw, ph) = time_card(w, h);
    let accent = crate::theme::main();
    cv.rrect(0.0, 0.0, w, h, 0.0, 0x000000, 0.55);
    cv.rrect(px, py, pw, ph, s(16.0), 0x1C1C23, 1.0);
    cv.rrect_stroke(px + 0.5, py + 0.5, pw - 1.0, ph - 1.0, s(16.0), 0x3A3A48, 1.0, s(1.0));

    cv.text_c(px, py + s(14.0), pw, s(26.0), "选择时间", 15.0, W_SEMI, TXT, 1.0);
    cv.text_c(px, py + s(42.0), pw, s(16.0), "点数字输入 · 箭头微调", 10.5, W_NORMAL, TXT3, 0.9);

    let col_y = py + s(66.0);
    let col_w = s(96.0);
    let gap = s(24.0);
    let hx = px + (pw - col_w * 2.0 - gap) / 2.0;
    let mx = hx + col_w + gap;
    draw_time_col(cv, hx, col_y, col_w, TIME_H.with(|c| c.get()), "时", 0);
    draw_time_col(cv, mx, col_y, col_w, TIME_MIN.with(|c| c.get()), "分", 1);
    // 冒号与两个数字框垂直居中对齐（旧版 y 偏移导致冒号飘在上方）
    cv.text_c(hx + col_w, col_y + s(26.0), gap, s(54.0), ":", 22.0, W_MED, TXT3, 0.9);

    // 快捷时间 chips
    let qy = py + s(212.0);
    let qh = s(28.0);
    let ch_w = s(62.0);
    let qgap = s(8.0);
    let total = ch_w * 4.0 + qgap * 3.0;
    let qx = px + (pw - total) / 2.0;
    for (i, (label, _, _)) in quick_times().iter().enumerate() {
        let cx0 = qx + i as f32 * (ch_w + qgap);
        cv.rrect(cx0, qy, ch_w, qh, s(8.0), ROW2, 1.0);
        cv.rrect_stroke(cx0 + 0.5, qy + 0.5, ch_w - 1.0, qh - 1.0, s(8.0), 0x34343F, 1.0, s(1.0));
        cv.text_c(cx0, qy, ch_w, qh, label, 11.0, W_MED, TXT2, 1.0);
    }

    // 底部取消 / 确定
    let bw = s(110.0);
    let by = py + ph - s(52.0);
    let cancel_x = px + (pw - bw * 2.0 - s(16.0)) / 2.0;
    let ok_x = cancel_x + bw + s(16.0);
    cv.rrect(cancel_x, by, bw, s(38.0), s(9.0), BTN_BG, 1.0);
    cv.rrect_stroke(cancel_x + 0.5, by + 0.5, bw - 1.0, s(37.0), s(9.0), 0x3A3A48, 1.0, s(1.0));
    cv.text_c(cancel_x, by, bw, s(38.0), "取消", 12.5, W_NORMAL, TXT2, 1.0);
    cv.rrect(ok_x, by, bw, s(38.0), s(9.0), accent, 0.95);
    cv.text_c(ok_x, by, bw, s(38.0), "确定", 12.5, W_SEMI, crate::theme::tint(), 1.0);
}

/// 时间面板点击：返回 true 表示已消费
unsafe fn time_picker_click(x: f32, y: f32, w: f32, h: f32) -> bool {
    let (px, py, pw, ph) = time_card(w, h);
    if !in_r(x, y, (px, py, pw, ph)) {
        POP.with(|c| c.set(Pop::None));
        TIME_EDIT.with(|c| c.set(None));
        return true;
    }
    let col_w = s(96.0);
    let gap = s(24.0);
    let col_y = py + s(66.0);
    let hx = px + (pw - col_w * 2.0 - gap) / 2.0;
    let mx = hx + col_w + gap;
    let num_y = col_y + s(26.0);
    let num_h = s(54.0);
    let rad = s(12.0);
    for (part, colx) in [(0usize, hx), (1usize, mx)] {
        let ux = colx + col_w / 2.0;
        let uy = col_y + s(13.0);
        let dy = col_y + s(99.0);
        // 上 / 下步进：圆形命中，外扩一点容错
        if in_circle(x, y, ux, uy, rad + s(3.0)) || in_circle(x, y, ux, dy, rad + s(3.0)) {
            let dir = if in_circle(x, y, ux, uy, rad + s(3.0)) { 1i64 } else { -1i64 };
            TIME_EDIT.with(|c| c.set(None));
            TIME_BUF.with(|b| *b.borrow_mut() = [String::new(), String::new()]);
            if part == 0 {
                let v = TIME_H.with(|c| c.get());
                TIME_H.with(|c| c.set((((v as i64 + dir) % 24 + 24) % 24) as u32));
            } else {
                let v = TIME_MIN.with(|c| c.get());
                TIME_MIN.with(|c| c.set((((v as i64 + dir) % 60 + 60) % 60) as u32));
            }
            return true;
        }
        if in_r(x, y, (colx, num_y, col_w, num_h)) {
            TIME_EDIT.with(|c| c.set(Some(part)));
            TIME_BUF.with(|b| b.borrow_mut()[part].clear());
            let hw = WIN.with(|c| c.get());
            if hw != 0 {
                let _ = SetFocus(Some(HWND(hw as *mut core::ffi::c_void)));
            }
            return true;
        }
    }
    // 快捷时间 chips：填入数值，不关闭面板
    let qy = py + s(212.0);
    let qh = s(28.0);
    let ch_w = s(62.0);
    let qgap = s(8.0);
    let total = ch_w * 4.0 + qgap * 3.0;
    let qx = px + (pw - total) / 2.0;
    for (i, (_, hh, mm)) in quick_times().iter().enumerate() {
        let cx0 = qx + i as f32 * (ch_w + qgap);
        if in_r(x, y, (cx0, qy, ch_w, qh)) {
            TIME_H.with(|c| c.set(*hh));
            TIME_MIN.with(|c| c.set(*mm));
            TIME_EDIT.with(|c| c.set(None));
            TIME_BUF.with(|b| *b.borrow_mut() = [String::new(), String::new()]);
            return true;
        }
    }
    let bw = s(110.0);
    let by = py + ph - s(52.0);
    let cancel_x = px + (pw - bw * 2.0 - s(16.0)) / 2.0;
    let ok_x = cancel_x + bw + s(16.0);
    if in_r(x, y, (cancel_x, by, bw, s(38.0))) {
        POP.with(|c| c.set(Pop::None));
        TIME_EDIT.with(|c| c.set(None));
        return true;
    }
    if in_r(x, y, (ok_x, by, bw, s(38.0))) {
        for part in 0..2usize {
            let b = TIME_BUF.with(|bb| bb.borrow()[part].clone());
            if !b.is_empty() {
                if let Ok(v) = b.parse::<u32>() {
                    if part == 0 {
                        TIME_H.with(|c| c.set(v.min(23)));
                    } else {
                        TIME_MIN.with(|c| c.set(v.min(59)));
                    }
                }
            }
        }
        FOCUS.with(|c| c.set(Seg::None));
        POP.with(|c| c.set(Pop::None));
        TIME_EDIT.with(|c| c.set(None));
        TIME_BUF.with(|b| *b.borrow_mut() = [String::new(), String::new()]);
        return true;
    }
    false
}

// ────────────────────── 表单绘制 ──────────────────────

fn draw_icon(cv: &mut Canvas, ic: (f32, f32, f32, f32), kind: IconKind) {
    let (x, y, w, h) = ic;
    let accent = crate::theme::main();
    // 图标是「打开选择器」的入口：与输入框连为一体，只保留线性符号，不再单独套方块
    let cx = x + w / 2.0;
    let cy = y + h / 2.0;
    let ct = accent;
    match kind {
        IconKind::Cal => {
            let bw = s(14.0);
            let bh = s(11.0);
            let bx = cx - bw / 2.0;
            let by = cy - bh / 2.0 + s(1.0);
            cv.rrect_stroke(bx, by, bw, bh, s(2.0), ct, 0.9, s(1.3));
            cv.line(bx + s(3.5), by - s(2.0), bx + s(3.5), by + s(1.5), ct, 0.9, s(1.3));
            cv.line(bx + bw - s(3.5), by - s(2.0), bx + bw - s(3.5), by + s(1.5), ct, 0.9, s(1.3));
            cv.line(bx + s(1.5), by + s(3.5), bx + bw - s(1.5), by + s(3.5), ct, 0.7, s(1.0));
        }
        IconKind::Clock => {
            cv.circle_stroke(cx, cy, s(7.0), ct, 0.9, s(1.3));
            cv.line(cx, cy, cx, cy - s(3.6), ct, 0.9, s(1.3));
            cv.line(cx, cy, cx + s(2.6), cy + s(1.2), ct, 0.9, s(1.3));
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum IconKind {
    Cal,
    Clock,
}

/// 一个完整的日期/时间输入容器：统一底 + 描边 + 右端图标按钮区。
/// 替代原本「每段一个孤立方块」的做法，视觉上收敛成一个 field。
fn draw_group_field(cv: &mut Canvas, x: f32, y: f32, w: f32, h: f32, icon_x: f32, icon_w: f32) {
    cv.rrect(x, y, w, h, s(9.0), CARD, 1.0);
    cv.rrect_stroke(x + 0.5, y + 0.5, w - 1.0, h - 1.0, s(9.0), 0x30303C, 1.0, s(1.0));
    // 图标按钮区与主容器连为一体，仅用淡底 + 分隔线区分，不再单独描边
    cv.rrect(icon_x + s(2.0), y + s(3.0), icon_w - s(5.0), h - s(6.0), s(6.0), ROW, 1.0);
    cv.line(icon_x, y + s(8.0), icon_x, y + h - s(8.0), 0x30303C, 1.0, s(1.0));
}

/// 组合字段内的一个数值段：聚焦时淡主色底 + 主色字；段间用细竖线分隔。
fn draw_inline_seg(cv: &mut Canvas, g: &SegGeom, divider: bool) {
    let focused = g.seg == FOCUS.with(|c| c.get());
    let accent = crate::theme::main();
    let mut disp = seg_value(g.seg);
    if focused {
        let tl = TYPING_LEN.with(|c| c.get());
        if tl > 0 {
            disp = format!("{}", TYPING.with(|c| c.get()));
        }
        cv.rrect(
            g.x + s(3.0), g.y + s(3.0), g.w - s(6.0), g.h - s(6.0),
            s(7.0), mix(accent, CARD, 0.14), 1.0,
        );
    }
    if divider {
        cv.line(g.x + g.w, g.y + s(9.0), g.x + g.w, g.y + g.h - s(9.0), 0x2E2E3A, 1.0, s(1.0));
    }
    cv.text_c(
        g.x, g.y, g.w, g.h, &disp, 12.5,
        if focused { W_SEMI } else { W_MED },
        if focused { accent } else { TXT }, 1.0,
    );
}

fn draw_seg(cv: &mut Canvas, g: &SegGeom) {
    let focus = FOCUS.with(|c| c.get());
    let focused = g.seg == focus;
    let accent = crate::theme::main();
    let (is_default, mut disp) = match g.seg {
        Seg::Remind if REMIND.with(|c| c.get()) == 0 => (true, "默认".to_string()),
        _ => (false, seg_value(g.seg)),
    };
    cv.rrect(g.x, g.y, g.w, g.h, s(8.0), ROW, 1.0);
    cv.rrect_stroke(g.x, g.y, g.w, g.h, s(8.0), if focused { accent } else { 0x2A2A35 }, 1.0, s(1.1));
    if focused {
        let tl = TYPING_LEN.with(|c| c.get());
        if tl > 0 {
            disp = format!("{}", TYPING.with(|c| c.get()));
        }
    }
    let tc = if focused {
        accent
    } else if is_default {
        TXT3
    } else {
        TXT
    };
    if g.seg == Seg::Remind {
        // 下拉框文字左对齐（符合下拉习惯），为右侧步进器留白
        cv.text1(g.x + s(14.0), g.y, g.w - s(52.0), g.h, &disp, 12.5,
                 if focused { W_SEMI } else { W_MED }, tc, 1.0);
        // 上下箭头收进一个整体胶囊，替代原本两个分离的方块
        let btn_w = s(26.0);
        let bh = g.h - s(10.0);
        let bx = g.x + g.w - btn_w - s(6.0);
        let by = g.y + s(5.0);
        cv.rrect(bx, by, btn_w, bh, s(5.0), if focused { ROW3 } else { ROW2 }, 1.0);
        cv.line(bx, by + bh / 2.0, bx + btn_w, by + bh / 2.0, 0x2A2A35, 1.0, s(1.0));
        let ccx = bx + btn_w / 2.0;
        let a = s(3.2);
        let uy = by + bh * 0.28;
        let dy = by + bh * 0.72;
        cv.line(ccx - a, uy + a * 0.5, ccx, uy - a * 0.5, TXT2, 1.0, s(1.3));
        cv.line(ccx, uy - a * 0.5, ccx + a, uy + a * 0.5, TXT2, 1.0, s(1.3));
        cv.line(ccx - a, dy - a * 0.5, ccx, dy + a * 0.5, TXT2, 1.0, s(1.3));
        cv.line(ccx, dy + a * 0.5, ccx + a, dy - a * 0.5, TXT2, 1.0, s(1.3));
    } else {
        cv.text_c(g.x, g.y, g.w, g.h, &disp, 12.5, if focused { W_SEMI } else { W_MED }, tc, 1.0);
    }
}

fn draw(cv: &mut Canvas, w: f32, h: f32) {
    // 弹出层优先
    if POP.with(|p| p.get()) == Pop::Cal {
        draw_calendar(cv, w, h);
        return;
    }
    if POP.with(|p| p.get()) == Pop::Time {
        draw_time_picker(cv, w, h);
        return;
    }

    cv.rrect(0.0, 0.0, w, h, s(12.0), BG, 1.0);
    // 标题：单一视觉焦点；下方细分隔线把「标题区 / 表单区」分开
    cv.text1(s(24.0), s(14.0), w - s(48.0), s(28.0), "新增日程", 16.0, W_SEMI, TXT, 1.0);
    cv.line(s(24.0), s(50.0), w - s(24.0), s(50.0), 0x2A2A35, 1.0, s(1.0));

    let mx = s(24.0);
    let mw = w - s(48.0);

    cv.rrect(mx, s(CONTENT_Y), mw, s(INPUT_H), s(9.0), CARD, 1.0);
    // 标签统一 11px：LABEL_H=16 让标签底与控件顶之间留 4px 呼吸，不再互相压盖
    cv.text1(mx, s(CONTENT_Y) - s(LABEL_H), mw, s(16.0), "内容", 11.0, W_SEMI, TXT2, 1.0);
    cv.text1(mx, s(DATE_ROW_Y) - s(LABEL_H), mw, s(16.0), "日期", 11.0, W_SEMI, TXT2, 1.0);
    cv.text1(mx, s(TIME_Y) - s(LABEL_H), mw, s(16.0), "时间", 11.0, W_SEMI, TXT2, 1.0);
    cv.text1(mx, s(REMIND_Y) - s(LABEL_H), mw, s(16.0), "提前提醒", 11.0, W_SEMI, TXT2, 1.0);
    cv.text1(mx + s(64.0), s(REMIND_Y) - s(LABEL_H), mw - s(64.0), s(16.0), "· 分钟，0 = 全局默认", 10.0, W_NORMAL, TXT3, 1.0);

    let (geoms, date_icon, time_icon) = field_geoms(mw);
    // 日期 / 时间：各画成一个完整容器，段内用细线分隔
    draw_group_field(cv, mx, s(DATE_ROW_Y), mw, s(INPUT_H), date_icon.0, date_icon.2);
    draw_group_field(cv, mx, s(TIME_Y), mw, s(INPUT_H), time_icon.0, time_icon.2);
    for g in &geoms {
        match g.seg {
            Seg::Remind => draw_seg(cv, g),
            Seg::DateD | Seg::TimeMin => draw_inline_seg(cv, g, false),
            _ => draw_inline_seg(cv, g, true),
        }
    }
    draw_icon(cv, date_icon, IconKind::Cal);
    draw_icon(cv, time_icon, IconKind::Clock);

    let cyy = btn_y();
    cv.rrect(mx, cyy, s(BTN_W), s(BTN_H), s(9.0), BTN_BG, 1.0);
    cv.rrect_stroke(mx, cyy, s(BTN_W), s(BTN_H), s(9.0), 0x33333F, 1.0, s(1.0));
    cv.text_ex(mx, cyy, s(BTN_W), s(BTN_H), "取消", 12.5, W_NORMAL, TXT2, 1.0, 1, 1);
    let apex = w - s(24.0) - s(BTN_W);
    cv.rrect(apex, cyy, s(BTN_W), s(BTN_H), s(9.0), crate::theme::main(), 0.95);
    cv.text_ex(apex, cyy, s(BTN_W), s(BTN_H), "添加", 12.5, W_SEMI, crate::theme::tint(), 1.0, 1, 1);
}

unsafe fn make_edit(parent: HWND, x: f32, y: f32, w: f32, id: usize) -> HWND {
    let h = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("EDIT"),
        w!(""),
        WINDOW_STYLE((ES_LEFT | ES_AUTOHSCROLL) as u32) | WS_CHILD | WS_VISIBLE | WS_TABSTOP,
        (x + s(9.0)) as i32,
        (y + s(5.0)) as i32,
        (w - s(18.0)) as i32,
        (s(INPUT_H) - s(10.0)) as i32,
        Some(parent),
        Some(HMENU((id * 4) as *mut core::ffi::c_void)),
        Some(GetModuleHandleW(None).unwrap().into()),
        None,
    )
    .expect("CreateWindowExW edit");
    let font = EDIT_FONT.with(|c| c.get());
    let _ = SendMessageW(h, WM_SETFONT, Some(WPARAM(font as usize)), Some(LPARAM(1)));
    crate::modal::set_edit_classic(h);
    crate::modal::set_edit_margins(h);
    h
}

unsafe fn build_ui(parent: HWND) {
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
        crate::render::ui_font_pcwstr(),
    );
    EDIT_FONT.with(|c| c.set(font.0 as isize));
    let brush = CreateSolidBrush(crate::modal::cref(gdi_rgb(INPUT_BG)));
    EDIT_BRUSH.with(|c| c.set(brush.0 as isize));

    let mx = s(24.0);
    let mw = s(W) - s(48.0);
    let c = make_edit(parent, mx, s(CONTENT_Y), mw, 0);
    let cue = HSTRING::from("输入日程内容，如：产品评审会");
    let _ = SendMessageW(c, crate::modal::EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(cue.as_ptr() as isize)));
    EDIT_C.with(|cell| cell.set(c.0 as isize));
}

/// 提交：把缓冲写进聚焦段，再读四个值。必须在销毁窗口【之前】读取。
unsafe fn commit_and_destroy(hwnd: HWND) {
    flush_typing();
    let c = input_text(EDIT_C.with(|c| c.get()));
    let d = format!("{:04}-{:02}-{:02}",
        DATE_Y.with(|c| c.get()),
        DATE_M.with(|c| c.get()),
        DATE_D.with(|c| c.get()));
    let t = format!("{:02}:{:02}",
        TIME_H.with(|c| c.get()),
        TIME_MIN.with(|c| c.get()));
    let r = REMIND.with(|c| c.get());
    RESULT.with(|res| *res.borrow_mut() = (c, d, t, r));
    DONE.with(|c| c.set(true));
    let _ = DestroyWindow(hwnd);
}

unsafe fn input_text(h: isize) -> String {
    if h == 0 {
        return String::new();
    }
    let h = HWND(h as *mut core::ffi::c_void);
    let n = GetWindowTextLengthW(h) as usize;
    let mut buf = vec![0u16; n + 1];
    let got = GetWindowTextW(h, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..got]).trim().to_string()
}

// ────────────────────── 弹出层开合 ──────────────────────

fn open_calendar() {
    CAL_Y.with(|c| c.set(DATE_Y.with(|c| c.get())));
    CAL_M.with(|c| c.set(DATE_M.with(|c| c.get())));
    TYPING.with(|c| c.set(0));
    TYPING_LEN.with(|c| c.set(0));
    FOCUS.with(|c| c.set(Seg::None));
    POP.with(|c| c.set(Pop::Cal));
    unsafe {
        let hw = WIN.with(|c| c.get());
        if hw != 0 {
            let _ = SetFocus(Some(HWND(hw as *mut core::ffi::c_void)));
        }
    }
}

fn open_time_picker() {
    TIME_EDIT.with(|c| c.set(None));
    TIME_BUF.with(|b| *b.borrow_mut() = [String::new(), String::new()]);
    TYPING.with(|c| c.set(0));
    TYPING_LEN.with(|c| c.set(0));
    FOCUS.with(|c| c.set(Seg::None));
    POP.with(|c| c.set(Pop::Time));
    unsafe {
        let hw = WIN.with(|c| c.get());
        if hw != 0 {
            let _ = SetFocus(Some(HWND(hw as *mut core::ffi::c_void)));
        }
    }
}

/// 退出程序时主动关闭可能开着的弹窗（避免残留）
pub fn close_for_quit() {
    let h = WIN.with(|c| c.get());
    if h != 0 {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(h as *mut core::ffi::c_void)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

// ────────────────────── 窗口过程 ──────────────────────

fn win_w() -> f32 {
    s(W)
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
                    let memdc = cv.draw_to_mem(w, h, BG, |cv, w, h| {
                        draw(cv, w, h);
                    });
                    let _ = BitBlt(hdc, 0, 0, w, h, Some(memdc), 0, 0, SRCCOPY);
                }
            });
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as f32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            let w = win_w();
            let h = s(H);

            match POP.with(|p| p.get()) {
                Pop::Cal => {
                    cal_click(x, y, w, h);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
                Pop::Time => {
                    time_picker_click(x, y, w, h);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
                Pop::None => {}
            }

            let mw = s(W) - s(48.0);
            let (geoms, date_icon, time_icon) = field_geoms(mw);
            // 图标按钮优先（日历/时钟）
            if in_r(x, y, date_icon) {
                open_calendar();
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            if in_r(x, y, time_icon) {
                open_time_picker();
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            // 数值段
            for g in &geoms {
                if in_r(x, y, (g.x, g.y, g.w, g.h)) {
                    // 提醒段右侧步进按钮
                    if g.seg == Seg::Remind {
                        let btn_w = s(22.0);
                        let bx = g.x + g.w - btn_w - s(4.0);
                        let by = g.y + s(3.0);
                        let bh = g.h - s(6.0);
                        let half = bh / 2.0;
                        if in_r(x, y, (bx, by, btn_w, half)) {
                            flush_typing();
                            FOCUS.with(|f| f.set(Seg::Remind));
                            TYPING.with(|t| t.set(0));
                            TYPING_LEN.with(|t| t.set(0));
                            set_seg(Seg::Remind, seg_cur(Seg::Remind) + 1);
                            let _ = SetFocus(Some(hwnd));
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                        if in_r(x, y, (bx, by + half, btn_w, half)) {
                            flush_typing();
                            FOCUS.with(|f| f.set(Seg::Remind));
                            TYPING.with(|t| t.set(0));
                            TYPING_LEN.with(|t| t.set(0));
                            set_seg(Seg::Remind, seg_cur(Seg::Remind) - 1);
                            let _ = SetFocus(Some(hwnd));
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                    }
                    focus_seg(g.seg);
                    let _ = SetFocus(Some(hwnd));
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
            }
            // 底部按钮
            let cyy = btn_y();
            let mxi = s(24.0);
            if in_r(x, y, (mxi, cyy, s(BTN_W), s(BTN_H))) {
                DONE.with(|c| c.set(true));
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            let apex = s(W) - s(24.0) - s(BTN_W);
            if in_r(x, y, (apex, cyy, s(BTN_W), s(BTN_H))) {
                commit_and_destroy(hwnd);
                return LRESULT(0);
            }
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            let _ = SetTextColor(hdc, crate::modal::cref(gdi_rgb(TXT)));
            let _ = SetBkColor(hdc, crate::modal::cref(gdi_rgb(INPUT_BG)));
            let _ = SetBkMode(hdc, TRANSPARENT);
            LRESULT(EDIT_BRUSH.with(|c| c.get()))
        }
        WM_KEYDOWN => {
            // 时间面板数字输入态
            if POP.with(|p| p.get()) == Pop::Time && TIME_EDIT.with(|c| c.get()).is_some() {
                let k = wparam.0 as u32;
                match k {
                    VK_BACK => {
                        let part = TIME_EDIT.with(|c| c.get()).unwrap();
                        TIME_BUF.with(|b| {
                            b.borrow_mut()[part].pop();
                        });
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                    (0x30..=0x39) => {
                        let part = TIME_EDIT.with(|c| c.get()).unwrap();
                        TIME_BUF.with(|b| {
                            let mut buf = b.borrow_mut();
                            if buf[part].len() < 2 {
                                buf[part].push((k as u8) as char);
                            }
                        });
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                    _ => {}
                }
            }
            // 常规聚焦输入（提前提醒等；弹出层未开时）
            if FOCUS.with(|f| f.get()) != Seg::None && POP.with(|p| p.get()) == Pop::None {
                let k = wparam.0 as u32;
                match k {
                    VK_ESCAPE => {
                        DONE.with(|c| c.set(true));
                        let _ = DestroyWindow(hwnd);
                        return LRESULT(0);
                    }
                    VK_RETURN => {
                        commit_and_destroy(hwnd);
                        return LRESULT(0);
                    }
                    VK_BACK => {
                        backspace();
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                    VK_UP => {
                        step_seg(1);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                    VK_DOWN => {
                        step_seg(-1);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                    (0x30..=0x39) => {
                        type_digit((k - 0x30) as u8);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                    _ => {}
                }
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
