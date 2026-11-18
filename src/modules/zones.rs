//! 桌面分区：在桌面上绘制「镂空描边框」分组框（仿 Fences），分区可完全自定义。
//!
//! 核心架构（v0.11 重写，彻底放弃 acrylic / layered / z 序控制）：
//! - 分区窗口用 `SetWindowRgn` 裁成「边框环 + 顶部标题条」——**窗口中间根本不属于
//!   窗口区域**（镂空）。因此：壁纸与桌面图标天然透出（不再有黑色遮挡）、鼠标事件
//!   自然穿透到桌面（文件可正常拖放、图标可点，不再出现 🚫 禁止光标）。
//! - 交互常驻（无需编辑模式上浮下沉）：标题条拖动整块、四边四角缩放、右键改名换色隐藏。
//!   因为只有边框/标题条是窗口，中间镂空区域永远不拦截桌面。
//! - 自绘拖动/缩放（不依赖系统位图拉伸，边框不会错乱）。
//! - 位置、大小、配色持久化到 config.json。
//!
//! 全部数据仅存本机 config.json，不联网。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 分区窗口类名
const CLASS: PCWSTR = w!("WinIslandZone");
/// 重命名弹窗类名
const RENAME_CLASS: PCWSTR = w!("WinIslandRename");
/// 边框厚度（窗口区域 = 边框环 + 标题条）
const BORDER: i32 = 3;
/// 顶部标题条高度（拖动区 + 显示名称）
const TITLE_H: i32 = 30;
/// 命中边界厚度（四边/四角的缩放手柄宽度）
const EDGE: i32 = 8;

thread_local! {
    /// 与 config.zones 对齐的活窗口：None = 该分区当前无窗口（隐藏 / 未启用）
    static WIN: RefCell<Vec<Option<HWND>>> = const { RefCell::new(Vec::new()) };
    /// 分区窗口类是否已注册
    static CLASSED: Cell<bool> = const { Cell::new(false) };
    /// 重命名弹窗类是否已注册
    static REN_CLASSED: Cell<bool> = const { Cell::new(false) };
    /// 当前正在重命名的分区索引（无则 None）
    static RENAME_IDX: Cell<Option<usize>> = const { Cell::new(None) };
    /// 自绘拖动/缩放状态
    static DRAG: RefCell<Option<DragState>> = const { RefCell::new(None) };
    /// 是否处于「编辑分区」模式（仅作设置页按钮兼容；交互已常驻，仅控制手柄高亮显示）
    static EDITING: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    Move,
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

struct DragState {
    mode: DragMode,
    start_screen: POINT,
    start_rect: RECT, // 屏幕坐标
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

/// 把一段 Rust 字符串转成 Win32 需要的、以 NUL 结尾的 UTF-16 缓冲。
/// 关键：SetWindowTextW / CreateWindowExW 等 LPCWSTR 参数必须 NUL 结尾，
/// 直接用 encode_utf16().collect() 不带结尾 0 会导致读越界 → 中文乱码。
fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn register_class() {
    if CLASSED.with(|c| c.get()) {
        return;
    }
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        // 用 NULL_BRUSH：窗口区域（边框+标题条）由 WM_PAINT 自绘，不需要默认填充
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: CLASS,
            hbrBackground: std::mem::transmute(GetStockObject(NULL_BRUSH)),
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

/// 给分区窗口设置镂空区域：只保留「边框环 + 顶部标题条」。
/// 中间区域不属于窗口 → 壁纸/图标透出、鼠标穿透桌面（拖放不再 🚫）。
fn apply_zone_region(hwnd: HWND, w: i32, h: i32) {
    if w <= BORDER * 2 || h <= TITLE_H + BORDER {
        return;
    }
    unsafe {
        // 外框（整块）
        let outer = CreateRectRgn(0, 0, w, h);
        // 内框（要挖空的部分）：跳过标题条与四周边框
        let inner = CreateRectRgn(BORDER, TITLE_H, w - BORDER, h - BORDER);
        // 边框环 = 外框 - 内框
        let frame = CreateRectRgn(0, 0, 0, 0);
        CombineRgn(Some(frame), Some(outer), Some(inner), RGN_DIFF);
        // 并把顶部标题条并回来（标题条是拖动区 + 名称显示区，需要可交互）
        let title = CreateRectRgn(0, 0, w, TITLE_H);
        CombineRgn(Some(frame), Some(frame), Some(title), RGN_OR);
        // SetWindowRgn 接管 frame 的所有权，不要 DeleteObject(frame)
        SetWindowRgn(hwnd, Some(frame), true);
        let _ = DeleteObject(outer.into());
        let _ = DeleteObject(inner.into());
        let _ = DeleteObject(title.into());
    }
}

/// 创建/摆放一个分区窗口。用 WS_POPUP 挂在桌面层（owner），配合镂空 region：
/// 边框与标题条浮在图标之上可见可交互，中间镂空不遮挡、不拦截桌面。
fn create_zone(layer: HWND, z: &crate::config::ZoneConf, idx: usize, parent_visible: bool) -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let hw = match CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
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
        apply_zone_region(hw, z.w.max(96), z.h.max(64));
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
                    apply_zone_region(hw, z.w.max(96), z.h.max(64));
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

/// 是否处于编辑模式（兼容设置页接口；交互已常驻，仅控制手柄高亮）
#[allow(dead_code)]
pub fn is_editing() -> bool {
    EDITING.with(|c| c.get())
}

/// 进入 / 退出「编辑分区」模式（仅切换四角手柄高亮，交互本身常驻可用）
pub fn toggle_edit() -> bool {
    let now = EDITING.with(|c| {
        let n = !c.get();
        c.set(n);
        n
    });
    // 重绘所有分区以反映手柄高亮
    WIN.with(|ws| {
        if let Ok(v) = ws.try_borrow() {
            for hw in v.iter().flatten() {
                unsafe { let _ = InvalidateRect(Some(*hw), None, true); }
            }
        }
    });
    now
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

/// 设置页「隐藏 / 显示全部」：翻转所有分区显隐。
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

/// 程序退出干净清理。
pub fn shutdown() {
    teardown();
}

// ── 命中测试：判断鼠标落在分区的哪个部位 ──

/// 屏幕坐标 → 命中部位。中间镂空区域收不到消息，这里只处理边框与标题条。
fn hit_mode(hwnd: HWND, screen_pt: POINT, w: i32, h: i32) -> DragMode {
    let mut pt = screen_pt;
    unsafe { let _ = ScreenToClient(hwnd, &mut pt); }
    let (x, y) = (pt.x, pt.y);

    let left = x <= EDGE;
    let right = x >= w - 1 - EDGE;
    let top = y <= EDGE;
    let bottom = y >= h - 1 - EDGE;

    if left && top { DragMode::TopLeft }
    else if right && top { DragMode::TopRight }
    else if left && bottom { DragMode::BottomLeft }
    else if right && bottom { DragMode::BottomRight }
    else if left { DragMode::Left }
    else if right { DragMode::Right }
    else if top { DragMode::Top }
    else if bottom { DragMode::Bottom }
    else { DragMode::Move } // 标题条内部：整块移动
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rc = RECT::default();
    unsafe { let _ = GetClientRect(hwnd, &mut rc); }
    (rc.right - rc.left, rc.bottom - rc.top)
}

/// 拖动 / 缩放结束后，把窗口几何写回 config 并落盘。
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
const MENU_EXIT_EDIT: u32 = 2003;
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
        let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT_EDIT as usize, w!("退出编辑"));

        let mut pos = POINT::default();
        let _ = GetCursorPos(&mut pos);
        let root = GetAncestor(hwnd, GA_ROOT);
        if !root.0.is_null() {
            let _ = SetForegroundWindow(root);
        }
        let _ = TrackPopupMenu(menu, TPM_LEFTALIGN | TPM_RIGHTBUTTON, pos.x, pos.y, Some(0), hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(cm);
        let _ = DestroyMenu(menu);
    }
}

// ── 重命名弹窗 ──

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
            // 预填当前名字（wide_nul 保证 NUL 结尾，修复中文乱码）
            if let Some(idx) = RENAME_IDX.with(|c| c.get()) {
                if let Some(cur) = crate::app::with_cfg(|c| c.zones.get(idx).map(|z| z.name.clone())) {
                    let cur_w = wide_nul(&cur);
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
                if let Some(idx) = RENAME_IDX.with(|c| c.get()) {
                    if let Ok(edit) = GetDlgItem(Some(hwnd), RENAME_EDIT) {
                        let len = GetWindowTextLengthW(edit) as usize;
                        let mut buf = vec![0u16; len + 1];
                        let _ = GetWindowTextW(edit, &mut buf);
                        let s = String::from_utf16_lossy(&buf[..len]).trim().to_string();
                        if !s.is_empty() {
                            crate::app::with_cfg(|c| {
                                if let Some(z) = c.zones.get_mut(idx) {
                                    z.name = s.clone();
                                }
                            });
                            crate::app::save_now();
                            // 立即重绘对应分区名称
                            WIN.with(|ws| {
                                if let Ok(v) = ws.try_borrow() {
                                    if let Some(Some(hw)) = v.get(idx) {
                                        unsafe { let _ = InvalidateRect(Some(*hw), None, true); }
                                    }
                                }
                            });
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
        WM_ERASEBKGND => LRESULT(1), // 由 WM_PAINT 全量自绘
        WM_NCHITTEST => {
            // 中间镂空区域不属于窗口，收不到此消息；这里只对边框/标题条返回客户区，
            // 交给自绘拖动/缩放。返回 HTCLIENT 让鼠标消息进入。
            LRESULT(HTCLIENT as isize)
        }
        WM_SETCURSOR => {
            let (w, h) = client_size(hwnd);
            let mut sp = POINT::default();
            let _ = GetCursorPos(&mut sp);
            let mode = hit_mode(hwnd, sp, w, h);
            let idc = match mode {
                DragMode::Left | DragMode::Right => IDC_SIZEWE,
                DragMode::Top | DragMode::Bottom => IDC_SIZENS,
                DragMode::TopLeft | DragMode::BottomRight => IDC_SIZENWSE,
                DragMode::TopRight | DragMode::BottomLeft => IDC_SIZENESW,
                _ => IDC_SIZEALL,
            };
            if let Ok(cur) = LoadCursorW(None, idc) {
                let _ = SetCursor(Some(cur));
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let (w, h) = client_size(hwnd);
            let cx = ((lparam.0 as u32) & 0xffff) as i16 as i32;
            let cy = (((lparam.0 as u32) >> 16) & 0xffff) as i16 as i32;
            let mut sp = POINT { x: cx, y: cy };
            let _ = ClientToScreen(hwnd, &mut sp);
            let mode = hit_mode(hwnd, sp, w, h);
            let mut rc = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rc);
            DRAG.with(|d| {
                *d.borrow_mut() = Some(DragState { mode, start_screen: sp, start_rect: rc });
            });
            let _ = SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if DRAG.with(|d| d.borrow().is_some()) {
                let cx = ((lparam.0 as u32) & 0xffff) as i16 as i32;
                let cy = (((lparam.0 as u32) >> 16) & 0xffff) as i16 as i32;
                let mut sp = POINT { x: cx, y: cy };
                let _ = ClientToScreen(hwnd, &mut sp);
                let (st, sr, mode) = DRAG.with(|d| {
                    let b = d.borrow();
                    let s = b.as_ref().unwrap();
                    (s.start_screen, s.start_rect, s.mode)
                });
                let dx = sp.x - st.x;
                let dy = sp.y - st.y;
                let (mut l, mut t, mut r, mut b) = (sr.left, sr.top, sr.right, sr.bottom);
                let (minw, minh) = (120, 80);
                match mode {
                    DragMode::Move => { l += dx; t += dy; r += dx; b += dy; }
                    DragMode::Left => { l += dx; if l > r - minw { l = r - minw; } }
                    DragMode::Right => { r += dx; if r < l + minw { r = l + minw; } }
                    DragMode::Top => { t += dy; if t > b - minh { t = b - minh; } }
                    DragMode::Bottom => { b += dy; if b < t + minh { b = t + minh; } }
                    DragMode::TopLeft => { l += dx; t += dy; if l > r - minw { l = r - minw; } if t > b - minh { t = b - minh; } }
                    DragMode::TopRight => { r += dx; t += dy; if r < l + minw { r = l + minw; } if t > b - minh { t = b - minh; } }
                    DragMode::BottomLeft => { l += dx; b += dy; if l > r - minw { l = r - minw; } if b < t + minh { b = t + minh; } }
                    DragMode::BottomRight => { r += dx; b += dy; if r < l + minw { r = l + minw; } if b < t + minh { b = t + minh; } }
                }
                unsafe {
                    let _ = MoveWindow(hwnd, l, t, r - l, b - t, false);
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_SIZE => {
            // 尺寸变化后重建镂空区域，边框始终跟随窗口
            let (w, h) = client_size(hwnd);
            apply_zone_region(hwnd, w, h);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONUP => {
            if DRAG.with(|d| d.borrow().is_some()) {
                let _ = unsafe { ReleaseCapture() };
                DRAG.with(|d| *d.borrow_mut() = None);
                commit_rect(hwnd);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_RBUTTONUP => {
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
                } else if id == MENU_EXIT_EDIT {
                    EDITING.with(|c| c.set(false));
                    let _ = InvalidateRect(Some(hwnd), None, true);
                } else if id >= MENU_COLOR_BASE && id < MENU_COLOR_BASE + COLOR_IDS.len() as u32 {
                    let cid = COLOR_IDS[(id - MENU_COLOR_BASE) as usize];
                    crate::app::with_cfg(|c| {
                        if let Some(z) = c.zones.get_mut(idx as usize) {
                            z.color = cid.to_string();
                        }
                    });
                    crate::app::save_now();
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 用 GDI 画分区外观：**只画边框环 + 顶部标题条**（中间镂空，由 region 保证透明）。
/// 标题条：主题色打底 + 白色名称；边框：主题色细线；编辑模式额外画四角手柄。
fn paint_zone(hwnd: HWND, idx: usize) {
    let (name, color) = crate::app::with_cfg(|c| {
        c.zones
            .get(idx)
            .map(|z| (z.name.clone(), zone_rgb(&z.color)))
            .unwrap_or_else(|| ("分区".to_string(), 0x4E8FFF))
    });
    let editing = EDITING.with(|c| c.get());

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

        // 顶部标题条：主题色填充（这是唯一不透明的实心区，也是拖动区）
        let bar_brush = CreateSolidBrush(COLORREF(color));
        let bar_rect = RECT { left: 0, top: 0, right: w, bottom: TITLE_H };
        let _ = FillRect(hdc, &bar_rect, bar_brush);
        let _ = DeleteObject(bar_brush.into());

        // 边框：主题色细线（region 已裁成环，这里描边即只显示边框）
        let pen = CreatePen(PS_SOLID, 2, COLORREF(color));
        let old_pen = SelectObject(hdc, pen.into());
        let old_br = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = Rectangle(hdc, 0, 0, w, h);
        let _ = SelectObject(hdc, old_pen);
        let _ = SelectObject(hdc, old_br);
        let _ = DeleteObject(pen.into());

        // 标题条上的名称（白色小字，透明背景）
        let font = CreateFontW(
            -14,
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
        let _ = SetTextColor(hdc, COLORREF(0xFFFFFF));
        let mut tr = RECT { left: 12, top: 4, right: w - 12, bottom: TITLE_H - 2 };
        let mut wide: Vec<u16> = name.encode_utf16().collect();
        let _ = DrawTextW(hdc, &mut wide, &mut tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
        let _ = SelectObject(hdc, old_font);
        let _ = DeleteObject(font.into());

        // 编辑模式：四角画实心小方块手柄（提示可缩放）
        if editing {
            let hb = CreateSolidBrush(COLORREF(0xFFFFFF));
            let s = 8;
            for (hx, hy) in [(0, 0), (w - s, 0), (0, h - s), (w - s, h - s)] {
                let hr = RECT { left: hx, top: hy, right: hx + s, bottom: hy + s };
                let _ = FillRect(hdc, &hr, hb);
            }
            let _ = DeleteObject(hb.into());
        }

        let _ = EndPaint(hwnd, &ps);
    }
}
