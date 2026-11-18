//! 桌面分区：在桌面上绘制半透明分组框（仿 Fences），分区可完全自定义。
//!
//! 纯视觉分组——不移动、不删除任何文件，桌面图标留在原位，仅被分区的半透明框
//! 象征性地「收纳」为一组。分区窗口挂在桌面图标宿主（SHELLDLL_DefView）的父层下，
//! 渲染在壁纸之上。
//!
//! v2（可交互层，本文件当前范围）：
//! - 分区提升到图标宿主之上，可接收鼠标：
//!   * 顶部标题区 → HTCAPTION：拖动整块分区改位置；
//!   * 四边 / 四角 → HTLEFT/HTTOP/...：系统自动缩放窗口改大小；
//!   * 内部 → HTTRANSPARENT：点击穿透到桌面图标，图标仍可点；
//! - 内部用高透半透明（WS_EX_LAYERED + SetLayeredWindowAttributes）而非
//!   Acrylic blur，避免毛玻璃把下方的桌面图标糊掉；壁纸与图标透过清晰可见。
//! - 右键弹出菜单：重命名 / 换主题色 / 隐藏显示。
//! - 位置、大小、配色改动持久化到 config.json。
//!
//! 文件拖入分区的「虚拟分组」归组能力属下一阶段（需分组数据模型 + 拖放接收）。
//!
//! 全部数据仅存本机 config.json，不联网。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

/// 分区窗口类名（顶层子窗，随主窗线程的消息循环一起分发）
const CLASS: PCWSTR = w!("WinIslandZone");
/// 重命名弹窗类名
const RENAME_CLASS: PCWSTR = w!("WinIslandRename");
/// 圆角半径
const RADIUS: i32 = 12;
/// 命中边界厚度（四边/四角的缩放手柄宽度）
const EDGE: i32 = 6;
/// 顶部标题区高度（在此区域内可拖动整块分区）
const GRAB_H: i32 = 22;
/// 分区窗口整体不透明度（0-255，越小越透）
const ZONE_ALPHA: u8 = 198;

thread_local! {
    /// 与 config.zones 对齐的活窗口：None = 该分区当前无窗口（隐藏 / 未启用）
    static WIN: RefCell<Vec<Option<HWND>>> = const { RefCell::new(Vec::new()) };
    /// 分区窗口类是否已注册
    static CLASSED: Cell<bool> = const { Cell::new(false) };
    /// 重命名弹窗类是否已注册
    static REN_CLASSED: Cell<bool> = const { Cell::new(false) };
    /// 当前正在重命名的分区索引（无则 None）
    static RENAME_IDX: Cell<Option<usize>> = const { Cell::new(None) };
}

/// 取某个分区在 config 中的主题色（0xRRGGBB）
fn zone_rgb(id: &str) -> u32 {
    crate::theme::THEMES
        .iter()
        .find(|(k, _, _)| *k == id)
        .map(|(_, _, c)| *c)
        .unwrap_or(0x4E8FFF)
}

/// 分区主题色 ID 列表（右键菜单换色用）
const COLOR_IDS: [&str; 7] = ["blue", "teal", "green", "purple", "pink", "orange", "red"];

/// 找桌面图标宿主的父层（layer = 壁纸之上的那一层，是分区窗口要挂的父）与图标宿主（def）。
/// 返回 (layer, def)，找不到时均为 HWND::default()。
fn shell() -> (HWND, HWND) {
    unsafe {
        let prog = match FindWindowW(w!("Progman"), None) {
            Ok(h) => h,
            Err(_) => return (HWND::default(), HWND::default()),
        };
        if prog.0.is_null() {
            return (HWND::default(), HWND::default());
        }
        let mut def = match FindWindowExW(Some(prog), None, w!("SHELLDLL_DefView"), None) {
            Ok(h) => h,
            Err(_) => HWND::default(),
        };
        if def.0.is_null() {
            // 遍历顶层 WorkerW，找装有 SHELLDLL_DefView 的那一个
            let mut ww = match FindWindowExW(None, None, w!("WorkerW"), None) {
                Ok(h) => h,
                Err(_) => HWND::default(),
            };
            let mut guard = 0u32;
            while !ww.0.is_null() && guard < 64 {
                let inside = match FindWindowExW(Some(ww), None, w!("SHELLDLL_DefView"), None) {
                    Ok(h) => h,
                    Err(_) => HWND::default(),
                };
                if !inside.0.is_null() {
                    def = inside;
                    break;
                }
                ww = match FindWindowExW(None, Some(ww), w!("WorkerW"), None) {
                    Ok(h) => h,
                    Err(_) => HWND::default(),
                };
                guard += 1;
            }
        }
        if def.0.is_null() {
            return (HWND::default(), HWND::default());
        }
        let layer = match GetParent(def) {
            Ok(h) => h,
            Err(_) => HWND::default(),
        };
        (layer, def)
    }
}

fn register_class() {
    if CLASSED.with(|c| c.get()) {
        return;
    }
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        // 背景刷用黑色打底：配合 WS_EX_LAYERED 统一样式，内部即为半透明黑、透出壁纸/图标
        let bg = CreateSolidBrush(COLORREF(0x000000));
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: CLASS,
            hbrBackground: bg,
            style: CS_HREDRAW | CS_VREDRAW,
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
    CLASSED.with(|c| c.set(true));
}

fn register_rename_class() {
    if REN_CLASSED.with(|c| c.get()) {
        return;
    }
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(rename_wndproc),
            hInstance: hinst.into(),
            lpszClassName: RENAME_CLASS,
            hbrBackground: CreateSolidBrush(COLORREF(0x1E2438)),
            style: CS_HREDRAW | CS_VREDRAW,
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
    REN_CLASSED.with(|c| c.set(true));
}

/// 创建/摆放一个分区窗口（分区在图标之上，可交互）。
fn create_zone(layer: HWND, z: &crate::config::ZoneConf, idx: usize, parent_visible: bool) -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let hw = match CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            CLASS,
            w!(""),
            WS_POPUP,
            z.x,
            z.y,
            z.w.max(96),
            z.h.max(64),
            Some(layer),
            None,
            Some(hinst.into()),
            None,
        ) {
            Ok(h) => h,
            Err(_) => return None,
        };
        if hw.0.is_null() {
            return None;
        }
        SetWindowLongPtrW(hw, GWLP_USERDATA, idx as isize);
        // 高透半透明（不模糊）：壁纸与图标透过可见
        let _ = SetLayeredWindowAttributes(hw, COLORREF(0), ZONE_ALPHA, LWA_ALPHA);
        if parent_visible {
            let _ = ShowWindow(hw, SW_SHOWNOACTIVATE);
        } else {
            let _ = ShowWindow(hw, SW_HIDE);
        }
        Some(hw)
    }
}

/// 由 config 驱动的同步：创建 / 更新 / 销毁分区窗口。在 app 主线程调用。
pub fn sync() {
    register_class();
    register_rename_class();
    let enabled = crate::app::with_cfg(|c| c.modules.zones);
    if !enabled {
        teardown();
        return;
    }

    if crate::app::with_cfg(|c| c.zones.is_empty()) {
        seed_if_needed();
    }
    let cfg2 = crate::app::with_cfg(|c| c.zones.clone());

    let (layer, _def) = shell();
    if layer.0.is_null() {
        return;
    }

    WIN.with(|ws| {
        let mut v = ws.borrow_mut();
        if v.len() > cfg2.len() {
            for hw in v.iter_mut().skip(cfg2.len()) {
                if let Some(h) = hw.take() {
                    unsafe { let _ = DestroyWindow(h); }
                }
            }
        }
        v.resize_with(cfg2.len(), || None);

        for (i, z) in cfg2.iter().enumerate() {
            match v[i] {
                Some(hw) if z.visible => {
                    unsafe {
                        let _ = SetWindowPos(
                            hw,
                            None,
                            z.x,
                            z.y,
                            z.w.max(96),
                            z.h.max(64),
                            SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
                        );
                    }
                }
                Some(hw) => {
                    unsafe { let _ = ShowWindow(hw, SW_HIDE); }
                }
                None if z.visible => {
                    if let Some(hw) = create_zone(layer, z, i, true) {
                        v[i] = Some(hw);
                    }
                }
                None => {}
            }
        }
    });
}

/// 销毁所有分区窗口（模块关闭 / 退出）。
fn teardown() {
    WIN.with(|ws| {
        let mut v = ws.borrow_mut();
        for hw in v.iter_mut() {
            if let Some(h) = hw.take() {
                unsafe { let _ = DestroyWindow(h); }
            }
        }
    });
}

/// 首次启用且分区为空时，铺一批示例分区（进程内只做一次）。
fn seed_if_needed() {
    use std::sync::OnceLock;
    static SEEDED: OnceLock<()> = OnceLock::new();
    SEEDED.get_or_init(|| {
        unsafe {
            let sw = GetSystemMetrics(SM_CXSCREEN);
            let sh = GetSystemMetrics(SM_CYSCREEN);
            let samples: [(&str, f32, f32, i32, i32); 3] = [
                ("项目", 0.06, 0.10, 420, 150),
                ("文档", 0.06, 0.32, 420, 150),
                ("应用", 0.06, 0.54, 420, 150),
            ];
            let mut zones: Vec<crate::config::ZoneConf> = Vec::new();
            for (i, (name, fx, fy, w, h)) in samples.iter().enumerate() {
                zones.push(crate::config::ZoneConf {
                    name: name.to_string(),
                    x: (sw as f32 * fx) as i32,
                    y: (sh as f32 * fy) as i32,
                    w: *w,
                    h: *h,
                    color: ["blue", "teal", "purple"][i % 3].to_string(),
                    visible: true,
                });
            }
            crate::app::with_cfg(|c| {
                c.zones = zones;
            });
            crate::app::save_now();
        }
    });
}

/// 设置页「＋ 新建分区」：在桌面中央偏下的位置新增一个默认分区。
pub fn add_zone() {
    register_rename_class();
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    crate::app::with_cfg(|c| {
        let count = c.zones.len();
        let base_x = sw / 2 - 210;
        let base_y = sh / 2 - 80 + (count % 4) as i32 * 28;
        c.zones.push(crate::config::ZoneConf {
            name: format!("分区 {}", count + 1),
            x: base_x,
            y: base_y,
            w: 420,
            h: 150,
            color: COLOR_IDS[count % COLOR_IDS.len()].to_string(),
            visible: true,
        });
    });
    crate::app::save_now();
    sync();
}

/// 设置页「隐藏 / 显示全部」：翻转所有分区显隐。返回当前是否处于隐藏态。
pub fn toggle_all() -> bool {
    crate::app::with_cfg(|c| {
        for z in c.zones.iter_mut() {
            z.visible = !z.visible;
        }
    });
    let hidden = crate::app::with_cfg(|c| c.zones.iter().all(|z| !z.visible));
    crate::app::save_now();
    sync();
    hidden
}

/// 程序退出干净清理（托盘退出时调用，可选）。
pub fn shutdown() {
    teardown();
}

// ── 命中测试：决定分区上每个点的角色（拖动 / 缩放 / 穿透） ──

/// 把屏幕坐标转成 lParam 形式（WM_NCHITTEST 用）
fn hit_test(hwnd: HWND, screen_pt: POINT, w: i32, h: i32) -> i32 {
    // 屏幕坐标 → 客户区坐标
    let mut pt = screen_pt;
    unsafe { let _ = ScreenToClient(hwnd, &mut pt); }
    let x = pt.x;
    let y = pt.y;

    let left = x <= EDGE;
    let right = x >= w - 1 - EDGE;
    let top = y <= EDGE;
    let bottom = y >= h - 1 - EDGE;

    // 四角优先
    if left && top {
        return HTTOPLEFT as i32;
    }
    if right && top {
        return HTTOPRIGHT as i32;
    }
    if left && bottom {
        return HTBOTTOMLEFT as i32;
    }
    if right && bottom {
        return HTBOTTOMRIGHT as i32;
    }
    if left {
        return HTLEFT as i32;
    }
    if right {
        return HTRIGHT as i32;
    }
    if top {
        return HTTOP as i32;
    }
    if bottom {
        return HTBOTTOM as i32;
    }
    // 顶部标题区：拖动整块分区
    if y < GRAB_H {
        return HTCAPTION as i32;
    }
    // 内部：点击穿透到桌面图标
    HTTRANSPARENT
}

/// 交接 WM_MOVING / WM_SIZING 结束（拖动或缩放完成）后，把窗口几何写回 config 并落盘。
fn commit_rect(hwnd: HWND) {
    let idx = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if idx < 0 {
        return;
    }
    let idx = idx as usize;
    unsafe {
        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        crate::app::with_cfg(|c| {
            if let Some(z) = c.zones.get_mut(idx) {
                z.x = rc.left;
                z.y = rc.top;
                z.w = (rc.right - rc.left).max(96);
                z.h = (rc.bottom - rc.top).max(64);
            }
        });
    }
    crate::app::save_now();
}

// ── 右键菜单 ──

const MENU_RENAME: u32 = 2001;
const MENU_HIDE: u32 = 2002;
const MENU_COLOR_BASE: u32 = 2100; // +idx

fn popup_menu(hwnd: HWND, idx: usize) {
    let (_name, color_id) = crate::app::with_cfg(|c| {
        c.zones
            .get(idx)
            .map(|z| (z.name.clone(), z.color.clone()))
            .unwrap_or_else(|| ("分区".to_string(), "blue".to_string()))
    });
    let checked_color = COLOR_IDS.iter().position(|c| *c == color_id).unwrap_or(0);

    unsafe {
        let menu = CreatePopupMenu().unwrap();
        let _ = AppendMenuW(menu, MF_STRING, MENU_RENAME as usize, w!("重命名…"));
        // 配色子菜单
        let cm = CreatePopupMenu().unwrap();
        for (i, cid) in COLOR_IDS.iter().enumerate() {
            let label: Vec<u16> = match *cid {
                "blue" => "海蓝".encode_utf16().collect(),
                "teal" => "青碧".encode_utf16().collect(),
                "green" => "青草".encode_utf16().collect(),
                "purple" => "罗兰".encode_utf16().collect(),
                "pink" => "樱粉".encode_utf16().collect(),
                "orange" => "落橙".encode_utf16().collect(),
                _ => "绯红".encode_utf16().collect(),
            };
            let lz: Vec<u16> = label.iter().copied().chain(std::iter::once(0)).collect();
            let flags = if i == checked_color { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(cm, flags, (MENU_COLOR_BASE + i as u32) as usize, PCWSTR(lz.as_ptr()));
        }
        let _ = AppendMenuW(menu, MF_POPUP, cm.0 as usize, w!("配色"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, MENU_HIDE as usize, w!("隐藏"));

        let mut pos = POINT::default();
        let _ = GetCursorPos(&mut pos);
        let _ = TrackPopupMenu(menu, TPM_LEFTALIGN | TPM_RIGHTBUTTON, pos.x, pos.y, Some(0), hwnd, None);
        let _ = DestroyMenu(cm);
        let _ = DestroyMenu(menu);
    }
}

// ── 重命名弹窗（简易模态） ──

const RENAME_EDIT: i32 = 3001;
const RENAME_OK: i32 = 3002;
const RENAME_CANCEL: i32 = 3003;

unsafe extern "system" fn rename_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinst = GetModuleHandleW(None).unwrap();
            let edit_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | ES_AUTOHSCROLL as u32);
            let btn_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32);
            let _ = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("Edit"),
                w!(""),
                edit_style,
                16,
                16,
                248,
                24,
                Some(hwnd),
                Some(HMENU(RENAME_EDIT as *mut c_void)),
                Some(hinst.into()),
                None,
            );
            let _ = CreateWindowExW(
                WS_EX_APPWINDOW,
                w!("Button"),
                w!("确定"),
                btn_style,
                60,
                56,
                72,
                26,
                Some(hwnd),
                Some(HMENU(RENAME_OK as *mut c_void)),
                Some(hinst.into()),
                None,
            );
            let cancel_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0);
            let _ = CreateWindowExW(
                WS_EX_APPWINDOW,
                w!("Button"),
                w!("取消"),
                cancel_style,
                148,
                56,
                72,
                26,
                Some(hwnd),
                Some(HMENU(RENAME_CANCEL as *mut c_void)),
                Some(hinst.into()),
                None,
            );
            // 预填当前名字
            if let Some(idx) = RENAME_IDX.with(|c| c.get()) {
                if let Some(cur) = crate::app::with_cfg(|c| c.zones.get(idx).map(|z| z.name.clone())) {
                    let cur_w: Vec<u16> = cur.encode_utf16().collect();
                    if let Ok(edit) = GetDlgItem(Some(hwnd), RENAME_EDIT) {
                        let _ = SetWindowTextW(edit, PCWSTR(cur_w.as_ptr()));
                        let _ = SetFocus(Some(edit));
                    }
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 as u32) & 0xffff;
            if id as i32 == RENAME_OK {
                // 读取输入 → 写回 config → 结束
                if let Some(idx) = RENAME_IDX.with(|c| c.get()) {
                    if let Ok(edit) = GetDlgItem(Some(hwnd), RENAME_EDIT) {
                        let len = GetWindowTextLengthW(edit) as usize;
                        let mut buf = vec![0u16; len + 1];
                        let _ = GetWindowTextW(edit, &mut buf);
                        let s = String::from_utf16_lossy(&buf[..len]).trim().to_string();
                        if !s.is_empty() {
                            crate::app::with_cfg(|c| {
                                if let Some(z) = c.zones.get_mut(idx) {
                                    z.name = s;
                                }
                            });
                            crate::app::save_now();
                        }
                    }
                }
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                LRESULT(0)
            } else if id as i32 == RENAME_CANCEL {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            RENAME_IDX.with(|c| c.set(None));
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _hdc = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn open_rename(idx: usize) {
    register_rename_class();
    RENAME_IDX.with(|c| c.set(Some(idx)));
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let hw = match CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
            RENAME_CLASS,
            w!("重命名分区"),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            0,
            0,
            300,
            120,
            None,
            None,
            Some(hinst.into()),
            None,
        ) {
            Ok(h) => h,
            Err(_) => {
                RENAME_IDX.with(|c| c.set(None));
                return;
            }
        };
        // 居中
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let mut rc = RECT::default();
        let _ = GetWindowRect(hw, &mut rc);
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        let _ = SetWindowPos(hw, None, (sw - w) / 2, (sh - h) / 2, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        let _ = ShowWindow(hw, SW_SHOW);
    }
}

// ── 分区窗口过程 ──

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if idx >= 0 {
                paint_zone(hwnd, idx as usize);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // 由 WM_PAINT 全量重绘，避免闪烁
        WM_NCHITTEST => {
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if idx < 0 {
                return LRESULT(HTNOWHERE as isize);
            }
            let (w, h) = unsafe {
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                (rc.right - rc.left, rc.bottom - rc.top)
            };
            // WM_NCHITTEST 的 lParam 高位/低位是屏幕坐标
            let sx = ((lparam.0 as u32) & 0xffff) as i16 as i32;
            let sy = (((lparam.0 as u32) >> 16) & 0xffff) as i16 as i32;
            let ht = hit_test(hwnd, POINT { x: sx, y: sy }, w, h);
            LRESULT(ht as isize)
        }
        WM_MOVING | WM_SIZING => {
            // 拖动 / 缩放中：先用默认行为移动窗口，结束时再落盘
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_EXITSIZEMOVE => {
            commit_rect(hwnd);
            LRESULT(0)
        }
        WM_CONTEXTMENU => {
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if idx >= 0 {
                popup_menu(hwnd, idx as usize);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            let id = (wparam.0 as u32) & 0xffff;
            if idx >= 0 {
                if id == MENU_RENAME {
                    open_rename(idx as usize);
                } else if id == MENU_HIDE {
                    crate::app::with_cfg(|c| {
                        if let Some(z) = c.zones.get_mut(idx as usize) {
                            z.visible = false;
                        }
                    });
                    crate::app::save_now();
                    sync();
                } else if id >= MENU_COLOR_BASE && id < MENU_COLOR_BASE + COLOR_IDS.len() as u32 {
                    let cid = COLOR_IDS[(id - MENU_COLOR_BASE) as usize];
                    crate::app::with_cfg(|c| {
                        if let Some(z) = c.zones.get_mut(idx as usize) {
                            z.color = cid.to_string();
                        }
                    });
                    crate::app::save_now();
                    // 立即重绘新配色
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 用 GDI 画分区外观：1px 柔和细圆角边框 + 顶部 2px 主题色横线 + 左上角名称小字。
/// 其余区域留空，让半透明窗口透出壁纸与图标。
fn paint_zone(hwnd: HWND, idx: usize) {
    let (name, color) = crate::app::with_cfg(|c| {
        c.zones
            .get(idx)
            .map(|z| (z.name.clone(), zone_rgb(&z.color)))
            .unwrap_or_else(|| ("分区".to_string(), 0x4E8FFF))
    });

    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        if w <= 0 || h <= 0 {
            let _ = EndPaint(hwnd, &ps);
            return;
        }

        // 1px 柔和细边框（贴合半透明的浅灰蓝）
        let pen = CreatePen(PS_SOLID, 1, COLORREF(0x7E8898));
        let old_pen = SelectObject(hdc, pen.into());
        let old_br = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = RoundRect(hdc, 1, 1, w - 1, h - 1, RADIUS * 2, RADIUS * 2);
        let _ = SelectObject(hdc, old_pen);
        let _ = SelectObject(hdc, old_br);
        let _ = DeleteObject(pen.into());

        // 顶部 2px 主题色横线（分区强调色）
        let line_pen = CreatePen(PS_SOLID, 2, COLORREF(color));
        let old2 = SelectObject(hdc, line_pen.into());
        let _ = MoveToEx(hdc, 14, 9, None);
        let _ = LineTo(hdc, w - 14, 9);
        let _ = SelectObject(hdc, old2);
        let _ = DeleteObject(line_pen.into());

        // 左上角名称（主题色小字，透明背景）
        let font = CreateFontW(
            -15,
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            w!("Microsoft YaHei UI"),
        );
        let old_font = SelectObject(hdc, font.into());
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(color));
        let mut tr = RECT { left: 18, top: 16, right: w - 18, bottom: 42 };
        let mut wide: Vec<u16> = name.encode_utf16().collect();
        let _ = DrawTextW(hdc, &mut wide, &mut tr, DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX);
        let _ = SelectObject(hdc, old_font);
        let _ = DeleteObject(font.into());

        let _ = EndPaint(hwnd, &ps);
    }
}
