//! 盯盘助手「编辑自选与持仓」对话框 —— 列表式 UI 编辑器。
//!
//! 每行一条自选 = 代码 / 成本价 / 数量 三个输入槽 + 名称自动补全（代码满 6 位时
//! 从行情快照即时识别，显示在代码槽下方；保存时对识别不到的代码联网兜底）。
//! 自绘圆角槽位 + 原生 EDIT 透明背景浮在上面，保证中文输入法 / 剪贴板可用。
//! 聚焦槽主题色描边、hover 行微亮，行尾圆形 × 删除（hover 变红），
//! 底部「＋ 添加条目」幽灵按钮 + 取消 / 保存。视觉与主面板一致。
//!
//! 打开时把 watchlist.txt 解析成行，编辑完成后返回 Vec<Item> 由调用方写回。
//! 采用模态消息循环（GetMessageW 派发同线程所有消息），主窗口不卡死。

use std::cell::RefCell;

use windows::core::{w, HSTRING};
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
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::modules::stock::Item;
use crate::render::{s, si, Canvas, W_NORMAL, W_SEMI};

// 原生控件常量（windows crate 未导出）
const EM_SETMARGINS: u32 = 0x00D3;
const VK_ESCAPE: u32 = 0x1B;
const EC_LEFTMARGIN: usize = 0x0001;
const EN_CHANGE: u32 = 0x0300;
// WM_MOUSELEAVE（windows crate 此版本未导出该常量，用字面值）
const MSG_MOUSELEAVE: u32 = 0x02A3;
// 自定义消息：后台线程拉到名称后通知编辑窗口刷新（WM_APP + 1）
const MSG_NAME_READY: u32 = 0x8001;

// ───────────────────────── 尺寸 / 颜色 ─────────────────────────
const W: f32 = 460.0;
const H: f32 = 560.0;
const ROW_H: f32 = 54.0;
const SLOT_H: f32 = 46.0;
const EDIT_H: f32 = 20.0; // EDIT 只占槽上半部分，下半部分留给名称自动补全
const EDIT_DY: f32 = 3.0;
const BTN_H: f32 = 40.0;

// 垂直节奏（逻辑值）
const TITLE_Y: f32 = 22.0;
const SUB_Y: f32 = 54.0;
const HEAD_Y: f32 = 92.0;
const LIST_TOP: f32 = 120.0;
const LIST_BOT: f32 = 488.0;

// 列布局（逻辑值）：代码（下方显示自动补全名称） / 成本价 / 数量 / 删除
const PADX: f32 = 20.0;
const INDENT: f32 = 11.0; // 表头与输入文字共同的左缩进
const CODE_X: f32 = 20.0;
const CODE_W: f32 = 200.0;
const COST_X: f32 = 228.0;
const COST_W: f32 = 100.0;
const QTY_X: f32 = 336.0;
const QTY_W: f32 = 74.0;
const DEL_X: f32 = 418.0;
const DEL_W: f32 = 24.0;

const BG: u32 = 0x0D0D11;
const SLOT_BG: u32 = 0x1E1E25;
const SLOT_HOVER: u32 = 0x232330;
const TXT: u32 = 0xF0F0F5;
const TXT2: u32 = 0x9C9CA7;
const TXT3: u32 = 0x62626D;
const BTN_BG: u32 = 0x23232B;
const GHOST_BORDER: u32 = 0x2E2E38;
const RED: u32 = 0xF4554F;

thread_local! {
    static WIN: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 自选行数据（字符串态，编辑时与 EDIT 控件双向同步）：代码 / 成本价 / 数量
    static ROWS: RefCell<Vec<[String; 3]>> = const { RefCell::new(Vec::new()) };
    /// 与 ROWS 平行的自动补全名称（代码满 6 位时从行情快照识别）
    static NAMES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// 每行 3 个 EDIT 控件的句柄
    static EDITS: RefCell<Vec<[isize; 3]>> = const { RefCell::new(Vec::new()) };
    static SCROLL: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
    /// 鼠标 hover 的行（-1 表示不在列表区）
    static HOVER: std::cell::Cell<isize> = const { std::cell::Cell::new(-1) };
    static CANVAS: RefCell<Option<Canvas>> = const { RefCell::new(None) };
    static EDIT_FONT: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    /// 已发起异步名称查询的代码（去重，避免每次按键都拉行情）
    static NAME_PENDING: RefCell<std::collections::HashSet<String>> = RefCell::new(std::collections::HashSet::new());
    /// build_edits 期间抑制 EN_CHANGE（SetWindowTextW 会触发，但此刻 EDITS 尚未就绪）
    static BUILDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 打开「编辑自选与持仓」对话框（模态）。保存返回 Some(条目列表)，取消返回 None。
/// 返回前会恢复主窗口置顶。
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
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &no_border as *const _ as *const core::ffi::c_void, 4);

        WIN.with(|c| c.set(hwnd.0 as isize));
        DONE.with(|c| c.set(false));
        SCROLL.with(|c| c.set(0.0));
        HOVER.with(|c| c.set(-1));
        NAME_PENDING.with(|p| p.borrow_mut().clear());

        // 由现有 watchlist 初始化行数据；为空时给一行空模板便于直接输入
        let src = crate::modules::stock::read_items();
        ROWS.with(|r| {
            let mut rows = r.borrow_mut();
            rows.clear();
            if src.is_empty() {
                rows.push(default_row());
            } else {
                for it in &src {
                    rows.push([it.code.clone(), fmt_num(it.cost), fmt_int(it.qty)]);
                }
            }
        });
        NAMES.with(|n| {
            let mut names = n.borrow_mut();
            names.clear();
            if src.is_empty() {
                names.push(String::new());
            } else {
                for it in &src {
                    // 优先用已存名称；缺失则从当前行情快照补全
                    let nm = if it.name.is_empty() {
                        crate::modules::stock::name_from_snapshot(&it.code)
                    } else {
                        it.name.clone()
                    };
                    names.push(nm);
                }
            }
        });
        build_edits(hwnd);

        crate::app::set_island_topmost(false);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        focus_first();
        let _ = InvalidateRect(Some(hwnd), None, false);

        // ── 模态消息循环 ──
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.hwnd == hwnd && msg.message == WM_KEYDOWN && msg.wParam.0 as u32 == VK_ESCAPE {
                DONE.with(|c| c.set(true));
                let _ = DestroyWindow(hwnd);
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if DONE.with(|c| c.get()) {
                break;
            }
        }

        // 保存前同步最后一次编辑；名称识别不到的代码联网兜底补全
        sync_edits_to_rows();
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
                        let saved = names.get(i).map(|s| s.trim().to_string()).unwrap_or_default();
                        let name = if saved.is_empty() {
                            crate::modules::stock::resolve_name(&code)
                        } else {
                            saved
                        };
                        Some(Item { code, name, cost, qty })
                    })
                    .collect::<Vec<_>>()
            })
        });
        cleanup(hwnd);
        crate::app::set_island_topmost(true);
        if items.is_empty() {
            None
        } else {
            Some(items)
        }
    }
}

fn default_row() -> [String; 3] {
    [String::new(), String::new(), String::new()]
}

/// 从输入里提取 6 位纯数字代码
fn digits6(s: &str) -> Option<String> {
    let d: String = s.trim().chars().filter(|c| c.is_ascii_digit()).collect();
    if d.len() == 6 {
        Some(d)
    } else {
        None
    }
}

fn fmt_num(v: f64) -> String {
    if v.abs() < 1e-9 {
        String::new()
    } else {
        format!("{:.2}", v)
    }
}
fn fmt_int(v: f64) -> String {
    if v.abs() < 1e-9 {
        String::new()
    } else {
        let iv = v.round() as i64;
        iv.to_string()
    }
}

// ───────────────────────── 资源与原生 EDIT 管理 ─────────────────────────

unsafe fn setup_resources() {
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
}

unsafe fn free_resources() {
    let f = EDIT_FONT.with(|c| c.get());
    if f != 0 {
        let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(f as *mut core::ffi::c_void));
        EDIT_FONT.with(|c| c.set(0));
    }
}

unsafe fn create_edit(parent: HWND, x: f32, y: f32, w: f32, h: f32, hmenu: usize, text: &str) -> HWND {
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
    let _ = SendMessageW(e, EM_SETMARGINS, Some(WPARAM(EC_LEFTMARGIN)), Some(LPARAM(s(INDENT).round() as isize)));
    let _ = SetWindowTextW(e, &HSTRING::from(text));
    e
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
}

fn scroll_max() -> f32 {
    let n = ROWS.with(|r| r.borrow().len()) as f32;
    (n * ROW_H - (LIST_BOT - LIST_TOP)).max(0.0)
}

/// 行 i 的槽位物理 y（x 由列决定，y 由行 + 滚动决定）
fn slot_y(i: usize, sc: f32) -> f32 {
    s(LIST_TOP + i as f32 * ROW_H - sc)
}

/// 重建全部 EDIT：先读回旧文本，再按当前 ROWS + SCROLL 重建。行数变化（增/删）后调用。
unsafe fn build_edits(hwnd: HWND) {
    BUILDING.with(|b| b.set(true));
    sync_edits_to_rows();
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
            create_edit(hwnd, dx(CODE_X), ey, s(CODE_W), eh, 100 + i * 3 + 0, &row[0]),
            create_edit(hwnd, dx(COST_X), ey, s(COST_W), eh, 100 + i * 3 + 1, &row[1]),
            create_edit(hwnd, dx(QTY_X), ey, s(QTY_W), eh, 100 + i * 3 + 2, &row[2]),
        ];
        ed.push([
            handles[0].0 as isize,
            handles[1].0 as isize,
            handles[2].0 as isize,
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
        ];
        let widths = [s(CODE_W), s(COST_W), s(QTY_W)];
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

/// 把每个 EDIT 的文本同步回 ROWS（增删 / 保存前调用）。
fn sync_edits_to_rows() {
    unsafe {
        let ed = EDITS.with(|c| c.borrow().clone());
        ROWS.with(|r| {
            let mut rows = r.borrow_mut();
            for i in 0..rows.len().min(ed.len()) {
                for k in 0..3 {
                    let h = ed[i][k];
                    if h != 0 {
                        rows[i][k] = edit_text(HWND(h as *mut core::ffi::c_void));
                    }
                }
            }
        });
    }
}

/// 代码 EDIT 内容变化时：同步该行 + 即时识别名称（缓存/快照没有则异步补）
fn on_code_changed(idx: usize) {
    unsafe {
        let h = EDITS.with(|c| c.borrow().get(idx).map(|r| r[0]).unwrap_or(0));
        if h == 0 {
            return;
        }
        let txt = edit_text(HWND(h as *mut core::ffi::c_void));
        ROWS.with(|r| {
            if let Some(row) = r.borrow_mut().get_mut(idx) {
                row[0] = txt.clone();
            }
        });
        let Some(code) = digits6(&txt) else {
            NAMES.with(|n| {
                if let Some(slot) = n.borrow_mut().get_mut(idx) {
                    slot.clear();
                }
            });
            return;
        };
        // 即时查（持久缓存 + 行情快照）
        let nm = crate::modules::stock::name_known(&code);
        NAMES.with(|n| {
            if let Some(slot) = n.borrow_mut().get_mut(idx) {
                *slot = nm.clone();
            }
        });
        // 查不到 → 后台拉一次行情，拉到后 PostMessage 通知刷新（去重）
        if nm.is_empty() {
            let fresh = NAME_PENDING.with(|p| p.borrow_mut().insert(code.clone()));
            if fresh {
                let hwnd = WIN.with(|c| c.get());
                std::thread::spawn(move || {
                    let n = crate::modules::stock::resolve_name(&code);
                    if !n.is_empty() {
                        crate::modules::stock::remember_name(&code, &n);
                        if hwnd != 0 {
                            let _ = PostMessageW(
                                Some(HWND(hwnd as *mut core::ffi::c_void)),
                                MSG_NAME_READY,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                    }
                });
            }
        }
    }
}

/// 后台名称补全完成：把所有行的名称从缓存重查一遍并刷新（可能一次通知对应多行）
fn refresh_all_names() {
    let rows = ROWS.with(|r| r.borrow().clone());
    let names: Vec<String> = rows
        .iter()
        .map(|row| {
            digits6(&row[0])
                .map(|c| crate::modules::stock::name_known(&c))
                .unwrap_or_default()
        })
        .collect();
    NAMES.with(|n| *n.borrow_mut() = names);
}

unsafe fn edit_text(h: HWND) -> String {
    let n = GetWindowTextLengthW(h) as usize;
    let mut buf = vec![0u16; n + 1];
    let got = GetWindowTextW(h, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..got]).trim().to_string()
}

/// 当前拥有键盘焦点的 EDIT 所在行（用于聚焦槽高亮）
fn focus_row() -> Option<usize> {
    unsafe {
        let fw = GetFocus();
        if fw.0.is_null() {
            return None;
        }
        EDITS.with(|c| {
            let ed = c.borrow();
            ed.iter().position(|row| row.iter().any(|&h| h != 0 && HWND(h as *mut core::ffi::c_void) == fw))
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

/// 聚焦最后一行的代码输入框（新增条目后调用，让光标直接落到新行）
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
    destroy_edits();
    free_resources();
    WIN.with(|c| c.set(0));
    HOVER.with(|c| c.set(-1));
    let _ = hwnd;
}

// ───────────────────────── 窗口类 / 自绘 ─────────────────────────

fn ensure_class(hinst: HINSTANCE) {
    unsafe {
        let class_name = w!("WinIslandWatchEdit");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class_name,
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
    // 背景
    cv.rrect(0.0, 0.0, w, h, s(14.0), BG, 1.0);
    // 标题 + 说明
    cv.text1(s(24.0), s(TITLE_Y), w - s(48.0), s(26.0), "编辑自选与持仓", 17.0, W_SEMI, TXT, 1.0);
    cv.text1(s(24.0), s(SUB_Y), w - s(48.0), s(18.0), "输入代码自动识别名称 · 填成本价与数量后计入今日盈亏", 10.5, W_NORMAL, TXT2, 1.0);

    // 表头（与输入文字同缩进对齐）
    let cols: [(f32, f32, &str); 3] = [
        (CODE_X, CODE_W, "代码 / 自动识别名称"),
        (COST_X, COST_W, "成本价"),
        (QTY_X, QTY_W, "数量"),
    ];
    for (cx, _cw, label) in cols {
        cv.text1(s(cx + INDENT), s(HEAD_Y), s(120.0), s(16.0), label, 10.0, W_SEMI, TXT3, 1.0);
    }

    // 列表区：每行三个槽位 + 删除按钮 + 名称补全
    let sc = SCROLL.with(|c| c.get());
    let hover = HOVER.with(|c| c.get());
    let focus = focus_row().map(|r| r as isize).unwrap_or(-1);
    let n = ROWS.with(|r| r.borrow().len());
    let names = NAMES.with(|x| x.borrow().clone());
    for i in 0..n {
        let sy = slot_y(i, sc);
        // 行槽底色：聚焦 > hover > 常态
        let bg = if i as isize == focus || i as isize == hover {
            SLOT_HOVER
        } else {
            SLOT_BG
        };
        for (cx, cw, _) in cols {
            let x = s(cx);
            cv.rrect(x, sy, s(cw), s(SLOT_H), s(8.0), bg, 1.0);
            if i as isize == focus {
                cv.rrect_stroke(x, sy, s(cw), s(SLOT_H), s(8.0), accent, 0.9, s(1.5));
            }
        }
        // 自动补全名称：代码槽下半部分，小字显示
        let nm = names.get(i).map(|s| s.as_str()).unwrap_or("");
        if !nm.is_empty() {
            cv.text1(s(CODE_X + INDENT), sy + s(24.0), s(CODE_W - INDENT - 6.0), s(15.0), nm, 9.0, W_NORMAL, accent, 1.0);
        } else {
            cv.text1(s(CODE_X + INDENT), sy + s(24.0), s(CODE_W - INDENT - 6.0), s(15.0), "自动识别名称", 8.5, W_NORMAL, TXT3, 1.0);
        }
        // 删除按钮：圆形 ×，hover 行变红
        let dcx = s(DEL_X) + s(DEL_W) / 2.0;
        let dcy = sy + s(SLOT_H) / 2.0;
        let hovered = i as isize == hover;
        cv.circle(dcx, dcy, s(9.0), if hovered { 0x3A2226 } else { 0x26262E }, 1.0);
        let xc = if hovered { RED } else { TXT3 };
        cv.line(dcx - s(3.2), dcy - s(3.2), dcx + s(3.2), dcy + s(3.2), xc, 1.0, s(1.4));
        cv.line(dcx + s(3.2), dcy - s(3.2), dcx - s(3.2), dcy + s(3.2), xc, 1.0, s(1.4));
    }

    // 底部操作条
    let cyy = h - s(16.0) - s(BTN_H);
    // ＋ 添加条目（幽灵按钮）
    cv.rrect(s(20.0), cyy, s(140.0), s(BTN_H), s(10.0), BG, 1.0);
    cv.rrect_stroke(s(20.0), cyy, s(140.0), s(BTN_H), s(10.0), GHOST_BORDER, 1.0, s(1.0));
    cv.text_ex(s(20.0), cyy, s(140.0), s(BTN_H), "＋  添加条目", 12.0, W_SEMI, TXT2, 1.0, 1, 1);
    // 取消
    let cancel_x = w - s(20.0) - s(92.0) * 2.0 - s(8.0);
    cv.rrect(cancel_x, cyy, s(92.0), s(BTN_H), s(10.0), BTN_BG, 1.0);
    cv.text_ex(cancel_x, cyy, s(92.0), s(BTN_H), "取消", 12.5, W_NORMAL, TXT2, 1.0, 1, 1);
    // 保存（强调色）
    let save_x = w - s(20.0) - s(92.0);
    cv.rrect(save_x, cyy, s(92.0), s(BTN_H), s(10.0), accent, 0.95);
    cv.text_ex(save_x, cyy, s(92.0), s(BTN_H), "保存", 12.5, W_SEMI, crate::theme::tint(), 1.0, 1, 1);
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
        WM_COMMAND => {
            // EDIT 代码列内容变化 → 即时识别名称
            let id = (wparam.0 & 0xFFFF) as usize;
            let notify = ((wparam.0 >> 16) & 0xFFFF) as u32;
            if notify == EN_CHANGE && id >= 100 && !BUILDING.with(|b| b.get()) {
                let k = (id - 100) % 3;
                if k == 0 {
                    let idx = (id - 100) / 3;
                    on_code_changed(idx);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        MSG_NAME_READY => {
            refresh_all_names();
            let _ = InvalidateRect(Some(hwnd), None, false);
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
            // ＋ 添加条目
            if in_r(x, y, s(20.0), cyy, s(140.0), s(BTN_H)) {
                sync_edits_to_rows();
                ROWS.with(|r| r.borrow_mut().push(default_row()));
                NAMES.with(|n| n.borrow_mut().push(String::new()));
                SCROLL.with(|c| c.set(scroll_max()));
                build_edits(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, false);
                focus_last();
                return LRESULT(0);
            }
            // 取消
            let cancel_x = s(W) - s(20.0) - s(92.0) * 2.0 - s(8.0);
            if in_r(x, y, cancel_x, cyy, s(92.0), s(BTN_H)) {
                DONE.with(|c| c.set(true));
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            // 保存
            let save_x = s(W) - s(20.0) - s(92.0);
            if in_r(x, y, save_x, cyy, s(92.0), s(BTN_H)) {
                sync_edits_to_rows();
                DONE.with(|c| c.set(true));
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            // 列表内删除按钮（某行 ×）
            let sc = SCROLL.with(|c| c.get());
            let idx = ((y / s(1.0) - LIST_TOP + sc) / ROW_H).floor() as isize;
            let n = ROWS.with(|r| r.borrow().len()) as isize;
            if idx >= 0 && idx < n {
                let slot_cy = slot_y(idx as usize, sc) + s(SLOT_H) / 2.0;
                if x >= s(DEL_X) - s(6.0) && x <= s(DEL_X) + s(DEL_W) + s(6.0) && (y - slot_cy).abs() <= s(SLOT_H) {
                    sync_edits_to_rows();
                    ROWS.with(|r| r.borrow_mut().remove(idx as usize));
                    NAMES.with(|n| n.borrow_mut().remove(idx as usize));
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
