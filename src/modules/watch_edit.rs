//! 盯盘助手「编辑自选与持仓」对话框 —— 列表式 UI 编辑器。
//!
//! 每行一条自选 = 代码 / 现价（只读）/ 成本价 / 数量 / 提醒%（该股独立涨跌提醒阈值）。
//! 代码列输入 6 位代码（或带 sh/sz/bj 前缀）；名称与现价从本地行情快照即时补全，
//! **不联网、不启用拼音识别**（上一版的联网识别太卡，已撤销）。
//! 成本价 / 数量 / 提醒% 列限制数字输入（含小数点），提醒填 0 表示这只不提醒。
//! 自绘圆角槽位 + 原生 EDIT 透明背景浮在上面，保证中文输入法 / 剪贴板可用。
//! 聚焦槽主题色描边、hover 行微亮，行尾圆形 × 删除（hover 变红），
//! 底部「＋ 添加条目」幽灵按钮 + 取消 / 保存。视觉与主面板一致。
//!
//! 打开时把 watchlist.txt 解析成行，编辑完成后返回 Vec<Item> 由调用方写回。
//! 采用模态消息循环（GetMessageW 派发同线程所有消息），主窗口不卡死；
//! 主窗口退出（WM_QUIT）时会主动销毁本弹窗并把退出信号回投主循环，避免窗口残留。

use std::cell::RefCell;
use std::collections::HashMap;

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWM_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateFontW, DeleteObject, EndPaint, GetStockObject,
    InvalidateRect, SetBkMode, SetTextColor, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, DEFAULT_PITCH, FW_NORMAL, HDC, NULL_BRUSH, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
    SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::Win32::UI::Input::Ime::{ImmAssociateContext, HIMC};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::modules::stock::Item;
use crate::render::{s, si, Canvas, W_NORMAL, W_SEMI};

// 原生控件常量（windows crate 未导出）
const EM_SETMARGINS: u32 = 0x00D3;
const EM_SETSEL: u32 = 0x00B1;
const VK_ESCAPE: u32 = 0x1B;
const EC_LEFTMARGIN: usize = 0x0001;
const EN_CHANGE: u32 = 0x0300;
const GWLP_WNDPROC: i32 = -4;
const WM_CHAR: u32 = 0x0102;
const WM_IME_STARTCOMPOSITION: u32 = 0x010D;
const WM_LBUTTONUP: u32 = 0x0202;
const MSG_MOUSELEAVE: u32 = 0x02A3;
/// 定时从内存快照回填名称/现价（异步按需查询完成后会被这里捕获）
const TIMER_REFRESH: usize = 1;
const REFRESH_MS: u32 = 350;

// ───────────────────────── 尺寸 / 颜色 ─────────────────────────
const W: f32 = 592.0;
const H: f32 = 560.0;
const ROW_H: f32 = 52.0;
const SLOT_H: f32 = 44.0;
const EDIT_H: f32 = 20.0;
const EDIT_DY: f32 = 4.0;
const BTN_H: f32 = 40.0;

const TITLE_Y: f32 = 22.0;
const SUB_Y: f32 = 54.0;
const HEAD_Y: f32 = 90.0;
const LIST_TOP: f32 = 118.0;
const LIST_BOT: f32 = 488.0;

// 列布局（逻辑值）：代码（下方显示本地快照名称） / 现价 / 成本价 / 数量 / 提醒% / 删除
const PADX: f32 = 20.0;
const INDENT: f32 = 11.0;
const CODE_X: f32 = 20.0;
const CODE_W: f32 = 140.0;
const PRICE_X: f32 = 168.0;
const PRICE_W: f32 = 76.0;
const COST_X: f32 = 252.0;
const COST_W: f32 = 92.0;
const QTY_X: f32 = 352.0;
const QTY_W: f32 = 74.0;
const ALERT_X: f32 = 434.0;
const ALERT_W: f32 = 92.0;
const DEL_X: f32 = 534.0;
const DEL_W: f32 = 28.0;

const BG: u32 = 0x0D0D11;
const SLOT_BG: u32 = 0x1E1E25;
const SLOT_HOVER: u32 = 0x232330;
const TXT: u32 = 0xF0F0F5;
const TXT2: u32 = 0x9C9CA7;
const TXT3: u32 = 0x62626D;
const BTN_BG: u32 = 0x23232B;
const GHOST_BORDER: u32 = 0x2E2E38;
const RED: u32 = 0xF4554F;
const GREEN: u32 = 0x35C07A;

thread_local! {
    static WIN: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 自选行数据（字符串态）：代码 / 成本价 / 数量 / 提醒%
    static ROWS: RefCell<Vec<[String; 4]>> = const { RefCell::new(Vec::new()) };
    /// 每行 4 个 EDIT 控件的句柄（代码 / 成本价 / 数量 / 提醒%）
    static EDITS: RefCell<Vec<[isize; 4]>> = const { RefCell::new(Vec::new()) };
    /// 保存结果：在 EDIT 控件销毁【之前】构造好；取消 / 关闭保持 None。
    /// 模态循环结束后直接取用，绝不从已销毁的控件再读文本。
    static RESULT: RefCell<Option<Vec<Item>>> = const { RefCell::new(None) };
    /// 从本地行情快照补全的证券名称（只读）
    static NAMES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// 从本地行情快照读取的现价文本（只读）
    static PRICES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// 现价颜色（A 股红涨绿跌）
    static PRICE_COLORS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    /// 数字 EDIT 的原始窗口过程（用于子类化拦截 WM_CHAR）
    static ORIG_PROCS: RefCell<HashMap<isize, isize>> = RefCell::new(HashMap::new());
    static SCROLL: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
    static HOVER: std::cell::Cell<isize> = const { std::cell::Cell::new(-1) };
    static CANVAS: RefCell<Option<Canvas>> = const { RefCell::new(None) };
    static EDIT_FONT: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    /// build_edits 期间抑制 EN_CHANGE（SetWindowTextW 会触发，但此刻 EDITS 尚未就绪）
    static BUILDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 打开「编辑自选与持仓」对话框（模态）。保存返回 Some(条目列表)，取消返回 None。
pub fn edit() -> Option<Vec<Item>> {
    unsafe {
        if WIN.with(|c| c.get()) != 0 {
            return None;
        }
        let hinst = GetModuleHandleW(None).unwrap();
        ensure_class(hinst.into());
        setup_resources();
        let class_name = w!("WinIslandWatchEdit");
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("编辑自选与持仓"),
            WS_POPUP,
            (sw - si(W)) / 2,
            (sh - si(H)) / 2 - si(40.0),
            si(W),
            si(H),
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("CreateWindowExW watch-edit");

        let pref: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        let no_border: u32 = 0xFFFF_FFFE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &no_border as *const _ as *const core::ffi::c_void,
            4,
        );

        WIN.with(|c| c.set(hwnd.0 as isize));
        DONE.with(|c| c.set(false));
        SCROLL.with(|c| c.set(0.0));
        HOVER.with(|c| c.set(-1));
        RESULT.with(|c| *c.borrow_mut() = None);

        // 由现有 watchlist 初始化行数据；为空时给一行空模板便于直接输入
        let src = crate::modules::stock::read_items();
        ROWS.with(|r| {
            let mut rows = r.borrow_mut();
            rows.clear();
            if src.is_empty() {
                rows.push(default_row());
            } else {
                for it in &src {
                    rows.push([
                        it.code.clone(),
                        fmt_num(it.cost),
                        fmt_int(it.qty),
                        fmt_num(it.alert),
                    ]);
                }
            }
        });
        // 名称 / 现价：全部从本地快照读取，零联网
        let mut names = quote_fields_or(&src, |q| q.name.clone(), String::new);
        let mut prices = quote_fields_or(&src, |q| {
            if q.price > 0.0 { format!("{:.2}", q.price) } else { "--".into() }
        }, || "--".into());
        let mut colors = quote_fields_or(&src, |q| pct_color(q.chg_pct), || TXT3);
        if src.is_empty() {
            names.push(String::new());
            prices.push("--".into());
            colors.push(TXT3);
        }
        NAMES.with(|n| *n.borrow_mut() = names);
        PRICES.with(|p| *p.borrow_mut() = prices);
        PRICE_COLORS.with(|p| *p.borrow_mut() = colors);

        build_edits(hwnd);

        crate::app::set_island_topmost(false);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        // 定时检查快照：新代码的异步行情查到后自动回填名称 / 现价（不阻塞输入）
        let _ = SetTimer(Some(hwnd), TIMER_REFRESH, REFRESH_MS, None);
        focus_first();
        let _ = InvalidateRect(Some(hwnd), None, false);

        // ── 模态消息循环 ──
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // 循环顶部先检查 DONE，避免下面 ESC 分支 destroy 后 continue 到 GetMessage
            // 空等下一条消息（没有下一条时模态会卡死）。
            if DONE.with(|c| c.get()) { break; }
            if msg.hwnd == hwnd && msg.message == WM_KEYDOWN && msg.wParam.0 as u32 == VK_ESCAPE {
                DONE.with(|c| c.set(true));
                let _ = DestroyWindow(hwnd);
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if DONE.with(|c| c.get()) {
                break;
            }
        }

        // 主窗口退出会投递 WM_QUIT：模态循环取到它后返回 FALSE 直接跳出，
        // 此时弹窗自身尚未销毁、WM_QUIT 也已被本循环消费 —— 必须销毁弹窗并回投，
        // 否则会出现「灵动岛关了但编辑面板还留着」的幽灵窗口。
        if msg.message == WM_QUIT {
            DONE.with(|c| c.set(true));
            let _ = DestroyWindow(hwnd);
            cleanup(hwnd);
            crate::app::set_island_topmost(true);
            PostQuitMessage(0);
            return None;
        }

        // 结果在保存按钮里、EDIT 控件尚存活时就已构造好并存入 RESULT。
        // 绝不能在这之后调用 sync_edits_to_rows() —— 控件已随 DestroyWindow 销毁，
        // 读回只会是全空串，这正是「保存了持仓却仍显示无持仓」的根因。
        let items = RESULT.with(|c| c.borrow_mut().take());
        cleanup(hwnd);
        crate::app::set_island_topmost(true);
        items
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

fn default_row() -> [String; 4] {
    [String::new(), String::new(), String::new(), String::new()]
}

/// 从当前 ROWS（已 sync 过控件文本）构造条目列表。必须在 EDIT 控件销毁【之前】调用。
fn collect_items() -> Option<Vec<Item>> {
    let items = ROWS.with(|r| {
        NAMES.with(|n| {
            let rows = r.borrow();
            let names = n.borrow();
            rows.iter()
                .enumerate()
                .filter_map(|(i, row)| {
                    let code = digits6(&row[0])?;
                    let cost = row[1].trim().parse::<f64>().unwrap_or(0.0);
                    let qty = row[2].trim().parse::<f64>().unwrap_or(0.0);
                    let alert = row[3].trim().parse::<f64>().unwrap_or(0.0).max(0.0);
                    let name = names.get(i).cloned().unwrap_or_default();
                    let name = if name.is_empty() {
                        crate::modules::stock::name_from_snapshot(&code)
                    } else {
                        name
                    };
                    Some(Item { code, name, cost, qty, alert })
                })
                .collect::<Vec<_>>()
        })
    });
    if items.is_empty() { None } else { Some(items) }
}

/// 从输入里提取 6 位纯数字代码
fn digits6(s: &str) -> Option<String> {
    let d: String = s.trim().chars().filter(|c| c.is_ascii_digit()).collect();
    if d.len() == 6 { Some(d) } else { None }
}

/// 对每行从内存快照提取字段（代码支持带前缀，归一后查快照），缺失时用 default 占位
fn quote_fields_or<T, F: Fn(&crate::modules::stock::Quote) -> T, D: Fn() -> T>(
    src: &[Item],
    f: F,
    default: D,
) -> Vec<T> {
    let snap = crate::modules::stock::snapshot();
    src.iter()
        .map(|it| {
            let code = digits6(&it.code).unwrap_or_else(|| it.code.clone());
            crate::modules::stock::quote_for(&snap, &code)
                .map(&f)
                .unwrap_or_else(&default)
        })
        .collect()
}

fn pct_color(pct: f64) -> u32 {
    if pct > 0.0 { RED } else if pct < 0.0 { GREEN } else { TXT2 }
}

fn fmt_num(v: f64) -> String {
    if v.abs() < 1e-9 { String::new() } else { format!("{:.2}", v) }
}
fn fmt_int(v: f64) -> String {
    if v.abs() < 1e-9 { String::new() } else { (v.round() as i64).to_string() }
}

// ───────────────────────── 资源与原生 EDIT 管理 ─────────────────────────

unsafe fn setup_resources() {
    let font = CreateFontW(
        -((13.0 * s(1.0)).round() as i32),
        0, 0, 0,
        FW_NORMAL.0 as i32,
        0, 0, 0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        DEFAULT_PITCH.0 as u32,
        w!("Microsoft YaHei UI"),
    );
    EDIT_FONT.with(|c| c.set(font.0 as isize));
}

unsafe fn free_resources() {
    let f = EDIT_FONT.with(|c| c.get());
    if f != 0 {
        let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(f as *mut core::ffi::c_void));
        EDIT_FONT.with(|c| c.set(0));
    }
}

/// 数字输入拦截：只放行 0-9 / 小数点 / 控制键（退格、Tab、回车、Delete），
/// 吞掉字母、中文、符号。小数点在全串已存在时也不放行，避免 "1.2.3"。
unsafe extern "system" fn numeric_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 数字列拦掉输入法组合，杜绝中文输入法 composition 残留导致的光标错位。
    if msg == WM_IME_STARTCOMPOSITION {
        return LRESULT(0);
    }
    if msg == WM_CHAR {
        let c = wparam.0 as u32;
        let ok = c < 0x20
            || c == 0x7F
            || (c >= b'0' as u32 && c <= b'9' as u32)
            || c == b'.' as u32;
        if !ok {
            return LRESULT(0);
        }
        if c == b'.' as u32 && edit_text(hwnd).contains('.') {
            return LRESULT(0);
        }
    }
    let ret = call_orig_edit(hwnd, msg, wparam, lparam);
    // 文本/插入点可能变化的消息之后统一异步归尾（PostMessage 在 EDIT 默认处理后执行）：
    // 打字、退格、删除、鼠标点击定位、粘贴。归尾只是移动插入点、不会删错字。
    if matches!(msg, WM_CHAR | WM_KEYUP | WM_LBUTTONUP) {
        caret_to_end(hwnd);
    }
    ret
}

unsafe fn call_orig_edit(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let old = ORIG_PROCS.with(|m| m.borrow().get(&(hwnd.0 as isize)).copied().unwrap_or(0));
    if old != 0 {
        let proc: WNDPROC = std::mem::transmute(old);
        CallWindowProcW(proc, hwnd, msg, wparam, lparam)
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// 代码列输入过滤：只放行 ASCII 字母/数字/控制键（支持 sh600000 之类前缀），
/// 中文/全角字符一律吞掉，从源头杜绝 IME composition 与光标错位。
unsafe extern "system" fn code_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 代码列只认 ASCII，拦掉输入法组合请求，从源头杜绝 composition 光标残留。
    if msg == WM_IME_STARTCOMPOSITION {
        return LRESULT(0);
    }
    if msg == WM_CHAR {
        let c = wparam.0 as u32;
        let ok = c < 0x20
            || c == 0x7F
            || (c >= b'0' as u32 && c <= b'9' as u32)
            || (c >= b'a' as u32 && c <= b'z' as u32)
            || (c >= b'A' as u32 && c <= b'Z' as u32);
        if !ok {
            return LRESULT(0);
        }
    }
    let ret = delegate_edit(hwnd, msg, wparam, lparam);
    // 所有可能改变文本/插入点的消息之后都异步归尾（PostMessage 保证在 EDIT
    // 默认处理完成后执行）：打字、退格、删除、鼠标点击定位、粘贴。
    if matches!(msg, WM_CHAR | WM_KEYUP | WM_LBUTTONUP) {
        caret_to_end(hwnd);
    }
    ret
}

unsafe fn delegate_edit(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let old = ORIG_PROCS.with(|m| m.borrow().get(&(hwnd.0 as isize)).copied().unwrap_or(0));
    if old != 0 {
        let proc: WNDPROC = std::mem::transmute(old);
        CallWindowProcW(proc, hwnd, msg, wparam, lparam)
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

unsafe fn subclass_into(h: HWND, proc: WNDPROC) {
    let old = GetWindowLongPtrW(h, WINDOW_LONG_PTR_INDEX(GWLP_WNDPROC));
    if old == 0 {
        return;
    }
    ORIG_PROCS.with(|m| { m.borrow_mut().insert(h.0 as isize, old); });
    let _ = SetWindowLongPtrW(
        h,
        WINDOW_LONG_PTR_INDEX(GWLP_WNDPROC),
        std::mem::transmute::<WNDPROC, isize>(proc),
    );
}

unsafe fn subclass_numeric(h: HWND) {
    subclass_into(h, Some(numeric_edit_proc));
}

unsafe fn subclass_code(h: HWND) {
    subclass_into(h, Some(code_edit_proc));
}

unsafe fn create_edit(
    parent: HWND,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    hmenu: usize,
    text: &str,
    numeric: bool,
) -> HWND {
    let e = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("EDIT"),
        w!(""),
        WINDOW_STYLE((ES_LEFT | ES_AUTOHSCROLL) as u32) | WS_CHILD | WS_VISIBLE | WS_TABSTOP,
        x.round() as i32,
        y.round() as i32,
        w.round() as i32,
        h.round() as i32,
        Some(parent),
        Some(HMENU(hmenu as *mut core::ffi::c_void)),
        Some(GetModuleHandleW(None).unwrap().into()),
        None,
    )
    .expect("CreateWindowExW edit");
    let font = EDIT_FONT.with(|c| c.get()) as usize;
    let _ = SendMessageW(e, WM_SETFONT, Some(WPARAM(font)), Some(LPARAM(1)));
    let theme_str = w!("");
    let _ = SetWindowTheme(e, theme_str, theme_str);
    let _ = SendMessageW(
        e,
        EM_SETMARGINS,
        Some(WPARAM(EC_LEFTMARGIN)),
        Some(LPARAM(s(INDENT).round() as isize)),
    );
    let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowTextW(
        e,
        &windows::core::HSTRING::from(text),
    );
    // 关键：SetWindowTextW 会把单行 EDIT 的插入点重置到文本【开头】，
    // 随后按退格时光标在 index 0，什么也删不掉，看起来就是"光标乱跳"。
    // 这里立刻把插入点放回末尾（无选中）。
    caret_to_end(e);
    // 代码/数字列不需要中文输入法；禁用 IME 可避免 composition 状态下
    // 退格取消组合导致的光标错位与卡顿。
    let _ = ImmAssociateContext(e, HIMC(std::ptr::null_mut()));
    if numeric {
        subclass_numeric(e);
    } else {
        subclass_code(e);
    }
    e
}

/// 把 EDIT 插入点放到文本末尾且不留选中区。
/// 关键：必须用 PostMessage 投递 EM_SETSEL，不能在子类过程处理 WM_CHAR 期间
/// 用 SendMessage 同步设置——那样 EDIT 随后的默认处理会把插入点又改回去，
/// 归尾根本不生效（这正是前几版改了却"问题还在"的原因）。
/// PostMessage 会在当前消息（含 IME composition 上屏）完整处理完之后才执行。
unsafe fn caret_to_end(h: HWND) {
    let n = GetWindowTextLengthW(h) as usize;
    let _ = PostMessageW(Some(h), EM_SETSEL, WPARAM(n), LPARAM(n as isize));
}

unsafe fn destroy_edits() {
    EDITS.with(|cell| {
        let ed = cell.borrow();
        for row in ed.iter() {
            for &h in row {
                if h != 0 {
                    let _ = DestroyWindow(HWND(h as *mut core::ffi::c_void));
                }
            }
        }
    });
    EDITS.with(|c| c.borrow_mut().clear());
    ORIG_PROCS.with(|m| m.borrow_mut().clear());
}

fn scroll_max() -> f32 {
    let n = ROWS.with(|r| r.borrow().len()) as f32;
    (n * ROW_H - (LIST_BOT - LIST_TOP)).max(0.0)
}

fn slot_y(i: usize, sc: f32) -> f32 {
    s(LIST_TOP + i as f32 * ROW_H - sc)
}

/// 重建全部 EDIT：直接按当前 ROWS + SCROLL 重建。行数变化（增/删）后调用。
///
/// 注意：这里【绝不能】再调用 sync_edits_to_rows()。增/删分支已经在改 ROWS 之前
/// 同步过一次；若在移除行之后再回写一次「旧控件文本」，旧 EDITS 的下标还没变，
/// 会把已删除行的文本覆盖到前移后的新行上（删 B 却把 C 变成 B），
/// 正是「删除的自选没同步到面板」的根因。
unsafe fn build_edits(hwnd: HWND) {
    BUILDING.with(|b| b.set(true));
    destroy_edits();
    let sc = SCROLL.with(|c| c.get());
    let rows = ROWS.with(|r| r.borrow().clone());
    let mut ed = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let ey = slot_y(i, sc) + s(EDIT_DY);
        let eh = s(EDIT_H);
        let bx = s(PADX);
        let dx = |colx: f32| bx + s(colx - PADX);
        let handles = [
            create_edit(hwnd, dx(CODE_X), ey, s(CODE_W), eh, 100 + i * 4 + 0, &row[0], false),
            create_edit(hwnd, dx(COST_X), ey, s(COST_W), eh, 100 + i * 4 + 1, &row[1], true),
            create_edit(hwnd, dx(QTY_X), ey, s(QTY_W), eh, 100 + i * 4 + 2, &row[2], true),
            create_edit(hwnd, dx(ALERT_X), ey, s(ALERT_W), eh, 100 + i * 4 + 3, &row[3], true),
        ];
        ed.push([
            handles[0].0 as isize,
            handles[1].0 as isize,
            handles[2].0 as isize,
            handles[3].0 as isize,
        ]);
    }
    EDITS.with(|c| *c.borrow_mut() = ed);
    BUILDING.with(|b| b.set(false));
}

/// 滚动时仅移动各 EDIT 到新位置（不重建、不丢文本）。
unsafe fn position_edits() {
    let sc = SCROLL.with(|c| c.get());
    EDITS.with(|cell| {
        let ed = cell.borrow();
        let bx = s(PADX);
        let xs = [
            bx + s(CODE_X - PADX),
            bx + s(COST_X - PADX),
            bx + s(QTY_X - PADX),
            bx + s(ALERT_X - PADX),
        ];
        let widths = [s(CODE_W), s(COST_W), s(QTY_W), s(ALERT_W)];
        let eh = s(EDIT_H);
        for (i, row) in ed.iter().enumerate() {
            let ey = slot_y(i, sc) + s(EDIT_DY);
            for (k, &h) in row.iter().enumerate() {
                if h != 0 {
                    let _ = SetWindowPos(
                        HWND(h as *mut core::ffi::c_void),
                        None,
                        xs[k].round() as i32,
                        ey.round() as i32,
                        widths[k].round() as i32,
                        eh.round() as i32,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
        }
    });
}

fn sync_edits_to_rows() {
    unsafe {
        let ed = EDITS.with(|c| c.borrow().clone());
        ROWS.with(|r| {
            let mut rows = r.borrow_mut();
            for i in 0..rows.len().min(ed.len()) {
                for k in 0..4 {
                    let h = ed[i][k];
                    if h != 0 {
                        rows[i][k] = edit_text(HWND(h as *mut core::ffi::c_void));
                    }
                }
            }
        });
    }
}

/// 代码列输入变化：本地同步 + 从内存快照刷新该行名称/现价。
/// 代码满 6 位但快照里没有时，登记一次异步按需查询（后台线程，不阻塞 UI；
/// 同一代码只查一次），查到后由 WM_TIMER 自动回填 —— 输入框不会卡。
/// 仅当名称/现价文本真的变化时才请求重绘。
fn on_code_changed(idx: usize) -> bool {
    let txt = unsafe {
        let h = EDITS.with(|c| c.borrow().get(idx).map(|r| r[0]).unwrap_or(0));
        if h == 0 { String::new() } else { edit_text(HWND(h as *mut core::ffi::c_void)) }
    };
    ROWS.with(|r| {
        if let Some(row) = r.borrow_mut().get_mut(idx) {
            row[0] = txt.clone();
        }
    });

    let (name, price, color) = match digits6(&txt) {
        Some(code) => {
            // 新代码不在 watchlist 里 → 主轮询不会拉它 → 快照查不到。
            // 这里登记一次异步查询（瞬间返回），拿到后 timer 会自动刷新本行。
            crate::modules::stock::request_lookup(&code);
            let snap = crate::modules::stock::snapshot();
            match crate::modules::stock::quote_for(&snap, &code) {
                Some(q) => (
                    q.name.clone(),
                    if q.price > 0.0 { format!("{:.2}", q.price) } else { "--".into() },
                    pct_color(q.chg_pct),
                ),
                None => (String::new(), "--".into(), TXT3),
            }
        }
        None => (String::new(), "--".into(), TXT3),
    };

    let old_name = NAMES.with(|n| n.borrow().get(idx).cloned().unwrap_or_default());
    let old_price = PRICES.with(|p| p.borrow().get(idx).cloned().unwrap_or_default());
    let changed = old_name != name || old_price != price;
    NAMES.with(|n| { if let Some(slot) = n.borrow_mut().get_mut(idx) { *slot = name; } });
    PRICES.with(|p| { if let Some(slot) = p.borrow_mut().get_mut(idx) { *slot = price; } });
    PRICE_COLORS.with(|p| { if let Some(slot) = p.borrow_mut().get_mut(idx) { *slot = color; } });
    changed
}

/// 定时扫描每一行的【EDIT 控件文本】，按需查询 + 从快照回填名称 / 现价。
///
/// 关键点：代码文本以 EDIT 控件为准，绝不能只读 ROWS —— ROWS 仅在 EN_CHANGE
/// 回调（on_code_changed）里更新，而本弹窗的 EDIT 经过子类化 + 禁用 IME 后，
/// EN_CHANGE 在部分 Windows 环境不会送达父窗口；此时 ROWS[0] 永远是空串，
/// digits6 直接返回 None，定时器什么也不会做 —— 这正是「输入 6 位代码后名称 /
/// 现价一直不显示」的根因。定时器直接读控件文本即可彻底绕开该依赖。
/// 查询结果异步写入 stock 快照，下一次 tick 即可回填；只有文本真变化才重绘。
unsafe fn refresh_from_snapshot(hwnd: HWND) {
    let eds = EDITS.with(|c| c.borrow().clone());
    let mut changed = false;

    for (i, row) in eds.iter().enumerate() {
        if row[0] == 0 { continue; }
        let code_txt = edit_text(HWND(row[0] as *mut core::ffi::c_void));
        let Some(code) = digits6(&code_txt) else { continue };

        // 快照没有这只（多为刚输入、尚未进 watchlist 的新代码）→ 登记一次异步查询
        let snap = crate::modules::stock::snapshot();
        if crate::modules::stock::quote_for(&snap, &code).is_none() {
            crate::modules::stock::request_lookup(&code);
            continue;
        }
        let Some(q) = crate::modules::stock::quote_for(&snap, &code) else { continue };
        let name = q.name.clone();
        let price = if q.price > 0.0 { format!("{:.2}", q.price) } else { "--".into() };
        let color = pct_color(q.chg_pct);

        NAMES.with(|n| {
            let mut n = n.borrow_mut();
            if n.get(i) != Some(&name) {
                if let Some(slot) = n.get_mut(i) { *slot = name; }
                changed = true;
            }
        });
        PRICES.with(|p| {
            let mut p = p.borrow_mut();
            if p.get(i) != Some(&price) {
                if let Some(slot) = p.get_mut(i) { *slot = price; }
                changed = true;
            }
        });
        PRICE_COLORS.with(|p| {
            let mut p = p.borrow_mut();
            if p.get(i) != Some(&color) {
                if let Some(slot) = p.get_mut(i) { *slot = color; }
                changed = true;
            }
        });
    }
    if changed {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// 清洗数字列的非法字符（粘贴中文时的兜底；键盘输入已由 WM_CHAR 拦截）
unsafe fn sanitize_numeric(idx: usize, col: usize) {
    let h = EDITS.with(|c| c.borrow().get(idx).map(|r| r[col]).unwrap_or(0));
    if h == 0 { return; }
    let h = HWND(h as *mut core::ffi::c_void);
    let raw = edit_text(h);
    let mut clean = String::new();
    let mut dot = false;
    for ch in raw.chars() {
        if ch.is_ascii_digit() {
            clean.push(ch);
        } else if ch == '.' && !dot {
            clean.push(ch);
            dot = true;
        }
    }
    if clean != raw {
        BUILDING.with(|b| b.set(true));
        let _ = SetWindowTextW(h, &windows::core::HSTRING::from(&clean));
        // SetWindowTextW 会把插入点重置到开头，清洗后必须放回末尾，
        // 否则下一次退格又删不掉字符。
        caret_to_end(h);
        BUILDING.with(|b| b.set(false));
        ROWS.with(|r| {
            if let Some(row) = r.borrow_mut().get_mut(idx) {
                row[col] = clean;
            }
        });
    }
}

unsafe fn edit_text(h: HWND) -> String {
    let n = GetWindowTextLengthW(h) as usize;
    let mut buf = vec![0u16; n + 1];
    let got = GetWindowTextW(h, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..got]).trim().to_string()
}

fn focus_row() -> Option<usize> {
    unsafe {
        let fw = GetFocus();
        if fw.0.is_null() { return None; }
        EDITS.with(|c| {
            let ed = c.borrow();
            ed.iter().position(|row| {
                row.iter().any(|&h| h != 0 && HWND(h as *mut core::ffi::c_void) == fw)
            })
        })
    }
}

fn focus_first() {
    unsafe {
        if let Some(h) = EDITS.with(|c| c.borrow().first().map(|r| r[0])) {
            if h != 0 {
                let _ = SetFocus(Some(HWND(h as *mut core::ffi::c_void)));
            }
        }
    }
}

fn focus_last() {
    unsafe {
        if let Some(h) = EDITS.with(|c| c.borrow().last().map(|r| r[0])) {
            if h != 0 {
                let _ = SetFocus(Some(HWND(h as *mut core::ffi::c_void)));
            }
        }
    }
}

unsafe fn cleanup(hwnd: HWND) {
    let _ = KillTimer(Some(hwnd), TIMER_REFRESH);
    destroy_edits();
    free_resources();
    WIN.with(|c| c.set(0));
    HOVER.with(|c| c.set(-1));
}

// ───────────────────────── 窗口类 / 自绘 ─────────────────────────

fn ensure_class(hinst: HINSTANCE) {
    unsafe {
        let class_name = w!("WinIslandWatchEdit");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class_name,
            hIcon: crate::modules::tray::app_icon(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
}

fn in_r(x: f32, y: f32, x0: f32, y0: f32, w0: f32, h0: f32) -> bool {
    x >= x0 && x <= x0 + w0 && y >= y0 && y <= y0 + h0
}

fn draw(cv: &mut Canvas, w: f32, h: f32) {
    let accent = crate::theme::main();
    cv.rrect(0.0, 0.0, w, h, s(14.0), BG, 1.0);
    cv.text1(s(24.0), s(TITLE_Y), w - s(48.0), s(26.0), "编辑自选与持仓", 17.0, W_SEMI, TXT, 1.0);
    cv.text1(
        s(24.0), s(SUB_Y), w - s(48.0), s(18.0),
        "现价自动显示 · 成本/数量按真实持仓填写 · 提醒% 每行独立（0 不提醒）",
        10.5, W_NORMAL, TXT2, 1.0,
    );

    let cols: [(f32, f32, &str); 5] = [
        (CODE_X, CODE_W, "代码"),
        (PRICE_X, PRICE_W, "现价"),
        (COST_X, COST_W, "成本价"),
        (QTY_X, QTY_W, "数量"),
        (ALERT_X, ALERT_W, "提醒%"),
    ];
    for (cx, _cw, label) in cols {
        cv.text1(s(cx + INDENT), s(HEAD_Y), s(110.0), s(16.0), label, 10.0, W_SEMI, TXT3, 1.0);
    }

    let sc = SCROLL.with(|c| c.get());
    let hover = HOVER.with(|c| c.get());
    let focus = focus_row().map(|r| r as isize).unwrap_or(-1);
    let n = ROWS.with(|r| r.borrow().len());
    let names = NAMES.with(|x| x.borrow().clone());
    let prices = PRICES.with(|x| x.borrow().clone());
    let colors = PRICE_COLORS.with(|x| x.borrow().clone());
    for i in 0..n {
        let sy = slot_y(i, sc);
        let bg = if i as isize == focus || i as isize == hover { SLOT_HOVER } else { SLOT_BG };
        for (cx, cw, _) in cols {
            let x = s(cx);
            cv.rrect(x, sy, s(cw), s(SLOT_H), s(8.0), bg, 1.0);
            if i as isize == focus {
                cv.rrect_stroke(x, sy, s(cw), s(SLOT_H), s(8.0), accent, 0.9, s(1.5));
            }
        }
        // 现价（只读，红涨绿跌），与成本价 EDIT 同一水平线
        let pt = prices.get(i).map(|x| x.as_str()).unwrap_or("--");
        let pc = colors.get(i).copied().unwrap_or(TXT3);
        cv.text_ex(
            s(PRICE_X), sy + s(EDIT_DY), s(PRICE_W), s(EDIT_H),
            pt, 11.0, W_SEMI, pc, 1.0, 2, 1,
        );
        // 本地快照名称：代码槽下半部分小字
        let nm = names.get(i).map(|x| x.as_str()).unwrap_or("");
        if !nm.is_empty() {
            cv.text1(s(CODE_X + INDENT), sy + s(25.0), s(CODE_W - INDENT - 6.0), s(15.0), nm, 9.0, W_NORMAL, accent, 1.0);
        } else {
            cv.text1(s(CODE_X + INDENT), sy + s(25.0), s(CODE_W - INDENT - 6.0), s(15.0), "行情快照自动补名", 8.5, W_NORMAL, TXT3, 1.0);
        }
        // 删除按钮
        let dcx = s(DEL_X) + s(DEL_W) / 2.0;
        let dcy = sy + s(SLOT_H) / 2.0;
        let hovered = i as isize == hover;
        cv.circle(dcx, dcy, s(9.0), if hovered { 0x3A2226 } else { 0x26262E }, 1.0);
        let xc = if hovered { RED } else { TXT3 };
        cv.line(dcx - s(3.2), dcy - s(3.2), dcx + s(3.2), dcy + s(3.2), xc, 1.0, s(1.4));
        cv.line(dcx + s(3.2), dcy - s(3.2), dcx - s(3.2), dcy + s(3.2), xc, 1.0, s(1.4));
    }

    let cyy = h - s(16.0) - s(BTN_H);
    cv.rrect(s(20.0), cyy, s(140.0), s(BTN_H), s(10.0), BG, 1.0);
    cv.rrect_stroke(s(20.0), cyy, s(140.0), s(BTN_H), s(10.0), GHOST_BORDER, 1.0, s(1.0));
    cv.text_ex(s(20.0), cyy, s(140.0), s(BTN_H), "＋  添加条目", 12.0, W_SEMI, TXT2, 1.0, 1, 1);
    let cancel_x = w - s(20.0) - s(92.0) * 2.0 - s(8.0);
    cv.rrect(cancel_x, cyy, s(92.0), s(BTN_H), s(10.0), BTN_BG, 1.0);
    cv.text_ex(cancel_x, cyy, s(92.0), s(BTN_H), "取消", 12.5, W_NORMAL, TXT2, 1.0, 1, 1);
    let save_x = w - s(20.0) - s(92.0);
    cv.rrect(save_x, cyy, s(92.0), s(BTN_H), s(10.0), accent, 0.95);
    cv.text_ex(save_x, cyy, s(92.0), s(BTN_H), "保存", 12.5, W_SEMI, crate::theme::tint(), 1.0, 1, 1);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE if wparam.0 != 0 => LRESULT(0),
        WM_ERASEBKGND => LRESULT(1),
        WM_TIMER if wparam.0 == TIMER_REFRESH => {
            refresh_from_snapshot(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (w, h) = (rc.right.max(1), rc.bottom.max(1));
            CANVAS.with(|cell| {
                let mut slot = cell.borrow_mut();
                if slot.is_none() { *slot = Canvas::new().ok(); }
                if let Some(cv) = slot.as_mut() {
                    let memdc = cv.draw_to_mem(w as i32, h as i32, BG, |cv, w, h| draw(cv, w, h));
                    let _ = BitBlt(hdc, 0, 0, w as i32, h as i32, Some(memdc), 0, 0, SRCCOPY);
                }
            });
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as usize;
            let notify = ((wparam.0 >> 16) & 0xFFFF) as u32;
            if id >= 100 && !BUILDING.with(|b| b.get()) {
                let k = (id - 100) % 4;
                let idx = (id - 100) / 4;
                if notify == EN_CHANGE {
                    match k {
                        0 => {
                            // 代码列：本地补名/现价，仅文本变化时重绘
                            if on_code_changed(idx) {
                                let _ = InvalidateRect(Some(hwnd), None, false);
                            }
                        }
                        1 | 2 | 3 => {
                            // 成本 / 数量 / 提醒：粘贴兜底清洗（键盘输入已被 WM_CHAR 拦截）
                            sanitize_numeric(idx, k);
                        }
                        _ => {}
                    }
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            let _ = SetTextColor(hdc, cref(TXT));
            let _ = SetBkMode(hdc, TRANSPARENT);
            LRESULT(GetStockObject(NULL_BRUSH).0 as isize)
        }
        WM_MOUSEMOVE => {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            let ly = y / s(1.0);
            let sc = SCROLL.with(|c| c.get());
            let idx = ((ly - LIST_TOP + sc) / ROW_H).floor() as isize;
            let n = ROWS.with(|r| r.borrow().len()) as isize;
            let newh = if ly >= LIST_TOP && ly <= LIST_BOT && idx >= 0 && idx < n { idx } else { -1 };
            if newh != HOVER.with(|c| c.get()) {
                HOVER.with(|c| c.set(newh));
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        MSG_MOUSELEAVE => {
            HOVER.with(|c| c.set(-1));
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) as i16) as f32;
            let cur = SCROLL.with(|c| c.get());
            let max = scroll_max();
            let next = (cur - delta / 120.0 * ROW_H).clamp(0.0, max);
            if (next - cur).abs() > 0.01 {
                SCROLL.with(|c| c.set(next));
                position_edits();
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = ((lparam.0 & 0xFFFF) as i16) as f32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
            let cyy = s(H) - s(16.0) - s(BTN_H);
            if in_r(x, y, s(20.0), cyy, s(140.0), s(BTN_H)) {
                sync_edits_to_rows();
                // 新行预填设置里的默认提醒阈值（0 则留空）
                let mut row = default_row();
                let def = crate::config::Config::load().stock.alert_pct;
                if def > 0.0 {
                    row[3] = format!("{:.2}", def);
                }
                ROWS.with(|r| r.borrow_mut().push(row));
                NAMES.with(|n| n.borrow_mut().push(String::new()));
                PRICES.with(|p| p.borrow_mut().push("--".into()));
                PRICE_COLORS.with(|p| p.borrow_mut().push(TXT3));
                SCROLL.with(|c| c.set(scroll_max()));
                build_edits(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, false);
                focus_last();
                return LRESULT(0);
            }
            let cancel_x = s(W) - s(20.0) - s(92.0) * 2.0 - s(8.0);
            if in_r(x, y, cancel_x, cyy, s(92.0), s(BTN_H)) {
                RESULT.with(|c| *c.borrow_mut() = None);
                DONE.with(|c| c.set(true));
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            let save_x = s(W) - s(20.0) - s(92.0);
            if in_r(x, y, save_x, cyy, s(92.0), s(BTN_H)) {
                // 关键：趁 EDIT 控件还活着，先把文本同步进 ROWS，再构造条目存进 RESULT。
                // 之后 DestroyWindow 销毁控件也不影响 —— RESULT 里已是完整数据。
                sync_edits_to_rows();
                // 保存就是保存：即使列表为空（用户删光了自选）也要写回空文件。
                // 不能让 collect_items() 的 None 把「保存空列表」误判成「取消」，
                // 否则删空后点保存面板仍显示旧自选，看起来就是按钮没生效。
                let items = collect_items().unwrap_or_default();
                RESULT.with(|c| *c.borrow_mut() = Some(items));
                DONE.with(|c| c.set(true));
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            let sc = SCROLL.with(|c| c.get());
            let idx = ((y / s(1.0) - LIST_TOP + sc) / ROW_H).floor() as isize;
            let n = ROWS.with(|r| r.borrow().len()) as isize;
            if idx >= 0 && idx < n {
                let slot_cy = slot_y(idx as usize, sc) + s(SLOT_H) / 2.0;
                // 垂直只认当前行本身（±半行）。原来用 ±整行高会波及相邻行，
                // 点在「保存」按钮上沿时可能被判成删除最后一行，看起来像保存没反应。
                if x >= s(DEL_X) - s(6.0) && x <= s(DEL_X) + s(DEL_W) + s(6.0) && (y - slot_cy).abs() <= s(SLOT_H) / 2.0 {
                    sync_edits_to_rows();
                    ROWS.with(|r| { r.borrow_mut().remove(idx as usize); });
                    NAMES.with(|n| { n.borrow_mut().remove(idx as usize); });
                    PRICES.with(|p| { p.borrow_mut().remove(idx as usize); });
                    PRICE_COLORS.with(|p| { p.borrow_mut().remove(idx as usize); });
                    let max = scroll_max();
                    let sc2 = SCROLL.with(|c| c.get()).min(max);
                    SCROLL.with(|c| c.set(sc2));
                    HOVER.with(|c| c.set(-1));
                    build_edits(hwnd);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
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
