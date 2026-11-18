//! 桌面分区：在桌面上绘制毛玻璃分组框（仿 Fences），分区可完全自定义。
//!
//! 纯视觉分组——不移动、不删除任何文件，桌面图标留在原位，仅被分区的毛玻璃框
//! 象征性地「收纳」为一组。
//!
//! 交互采用「编辑模式」（兼顾视觉与自定义）：
//! - **平时（非编辑）**：分区挂在桌面图标宿主（SHELLDLL_DefView）的父层下、位于
//!   图标**之下**，套丙烯酸毛玻璃（Acrylic）→ 壁纸透亮可见、图标清晰可点、
//!   文件可正常拖到桌面，分区完全不干扰日常操作。
//! - **编辑模式**（设置页「编辑分区」触发）：分区临时上浮到图标**之上**并显示
//!   手柄——顶部拖动整块移动、四边四角缩放、右键重命名/换色/隐藏；退出即恢复。
//! - 位置、大小、配色改动持久化到 config.json。
//!
//! 全部数据仅存本机 config.json，不联网。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 分区窗口类名（顶层子窗，随主窗线程的消息循环一起分发）
const CLASS: PCWSTR = w!("WinIslandZone");
/// 重命名弹窗类名
const RENAME_CLASS: PCWSTR = w!("WinIslandRename");
/// 圆角半径
const RADIUS: i32 = 12;
/// 命中边界厚度（四边/四角的缩放手柄宽度）
const EDGE: i32 = 6;

thread_local! {
    /// 与 config.zones 对齐的活窗口：None = 该分区当前无窗口（隐藏 / 未启用）
    static WIN: RefCell<Vec<Option<HWND>>> = const { RefCell::new(Vec::new()) };
    /// 分区窗口类是否已注册
    static CLASSED: Cell<bool> = const { Cell::new(false) };
    /// 重命名弹窗类是否已注册
    static REN_CLASSED: Cell<bool> = const { Cell::new(false) };
    /// 当前正在重命名的分区索引（无则 None）
    static RENAME_IDX: Cell<Option<usize>> = const { Cell::new(None) };
    /// 是否处于「编辑分区」模式（分区上浮到图标之上、可拖动/缩放/右键）
    static EDITING: Cell<bool> = const { Cell::new(false) };
    /// 自绘拖动/缩放状态
    static DRAG: RefCell<Option<DragState>> = const { RefCell::new(None) };
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

// ── 丙烯酸毛玻璃：动态调用 user32!SetWindowCompositionAttribute（Win10 1803+ 可用） ──

#[repr(C)]
struct AccentPolicy {
    state: i32,
    flags: u32,
    gradient: u32,
    anim: u32,
}

#[repr(C)]
struct WcaData {
    attrib: i32,
    data: *mut c_void,
    size: usize, // SIZE_T：x64 下必须 8 字节，用 u32 会错位导致调用失败
}

const ACCENT_ENABLE_ACRYLICBLURBEHIND: i32 = 4;
const WCA_ACCENT_POLICY: i32 = 19;

type Swca = unsafe extern "system" fn(HWND, *mut c_void) -> i32;

/// 给窗口套上丙烯酸毛玻璃背景；不支持时静默退化为类背景色。
fn set_acrylic(hwnd: HWND, tint_argb: u32) {
    unsafe {
        if let Ok(m) = LoadLibraryW(w!("user32.dll")) {
            let farproc = GetProcAddress(m, s!("SetWindowCompositionAttribute"));
            if let Some(p) = farproc {
                let f: Swca = std::mem::transmute(p);
                let mut ap = AccentPolicy {
                    state: ACCENT_ENABLE_ACRYLICBLURBEHIND,
                    flags: 2,
                    gradient: tint_argb,
                    anim: 0,
                };
                let mut d = WcaData {
                    attrib: WCA_ACCENT_POLICY,
                    data: &mut ap as *mut _ as *mut c_void,
                    size: std::mem::size_of::<AccentPolicy>(),
                };
                let _ = f(hwnd, &mut d as *mut _ as *mut c_void);
            }
        }
    }
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
        // 背景刷：丙烯酸不支持时的兜底深色（正常情况下被毛玻璃覆盖）
        let bg = CreateSolidBrush(COLORREF(0x1B2030));
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

/// 创建/摆放一个分区窗口。作为桌面层的**真子窗口**（WS_CHILD），
/// 这样它和图标宿主 def 是兄弟，z 序可被精确比较——平时沉到 def 之下，编辑时浮到其上。
/// 若用 WS_POPUP，parent 参数只是 owner，owned popup 永远在 owner 的子窗口之上，
/// 无法沉到图标之下（这正是「拖入显示禁止 / 拦截桌面拖放」的根因）。
fn create_zone(layer: HWND, z: &crate::config::ZoneConf, idx: usize, parent_visible: bool) -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        // config 存屏幕坐标；WS_CHILD 坐标相对父窗口客户区，需转换
        let mut pos = POINT { x: z.x, y: z.y };
        let _ = ScreenToClient(layer, &mut pos);
        let hw = match CreateWindowExW(
            WS_EX_NOACTIVATE,
            CLASS,
            w!(""),
            WS_CHILD | WS_VISIBLE,
            pos.x,
            pos.y,
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
        // 丙烯酸毛玻璃打底：AARRGGBB，A≈12%（高透，壁纸清晰可见）
        set_acrylic(hw, 0x1F1B2336);
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

    let (layer, def) = shell();
    if layer.0.is_null() {
        return;
    }
    let editing = EDITING.with(|c| c.get());

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
                        let mut pos = POINT { x: z.x, y: z.y };
                        let _ = ScreenToClient(layer, &mut pos);
                        let _ = SetWindowPos(
                            hw,
                            None,
                            pos.x,
                            pos.y,
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

    // 层序：zone 与 def 同为 layer 的子窗口（兄弟），z 序可精确比较。
    // 编辑模式把每个分区顶到最上（浮到图标之上、可交互）；
    // 平时把每个分区沉到最底（在图标宿主之下：壁纸透出、图标清晰可点、拖放不干扰）。
    let _ = def;
    unsafe {
        WIN.with(|ws| {
            if let Ok(v) = ws.try_borrow() {
                let anchor = if editing { HWND_TOP } else { HWND_BOTTOM };
                for hw in v.iter().flatten() {
                    let _ = SetWindowPos(*hw, Some(anchor), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                }
            }
        });
    }
    // 层序变化后重绘
    WIN.with(|ws| {
        if let Ok(v) = ws.try_borrow() {
            for hw in v.iter().flatten() {
                unsafe { let _ = InvalidateRect(Some(*hw), None, true); }
            }
        }
    });
}

/// 是否处于编辑模式
#[allow(dead_code)]
pub fn is_editing() -> bool {
    EDITING.with(|c| c.get())
}

/// 进入 / 退出「编辑分区」模式。返回进入后是否处于编辑态。
pub fn toggle_edit() -> bool {
    let now = EDITING.with(|c| {
        let n = !c.get();
        c.set(n);
        n
    });
    sync();
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

/// 判断鼠标落在分区的哪个部位：四角/四边缩放，其余整块移动（自绘，不依赖系统 hit）。
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
    else { DragMode::Move }
}

/// 客户区尺寸
fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rc = RECT::default();
    unsafe { let _ = GetClientRect(hwnd, &mut rc); }
    (rc.right - rc.left, rc.bottom - rc.top)
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
        let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT_EDIT as usize, w!("退出编辑模式"));

        let mut pos = POINT::default();
        let _ = GetCursorPos(&mut pos);
        // 关键：TrackPopupMenu 要求调用线程的前台窗口属于本线程。
        // 分区是 WS_CHILD，不能设为前台，改设其根祖先（桌面层所在的顶层窗）。
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
            // 编辑模式：整块作为客户区接收鼠标（自绘拖动/缩放/右键）；
            // 平时：分区沉在图标之下返回穿透，进一步确保不拦截任何桌面交互。
            if EDITING.with(|c| c.get()) {
                LRESULT(HTCLIENT as isize)
            } else {
                LRESULT(HTTRANSPARENT as isize)
            }
        }
        WM_SETCURSOR => {
            if EDITING.with(|c| c.get()) {
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
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            if EDITING.with(|c| c.get()) {
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
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_MOUSEMOVE => {
            if EDITING.with(|c| c.get()) && DRAG.with(|d| d.borrow().is_some()) {
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
                // 新矩形是屏幕坐标，转换为父窗口（layer）客户区坐标后 MoveWindow
                let parent = unsafe { GetParent(hwnd).unwrap_or_default() };
                let mut pt = POINT { x: l, y: t };
                if !parent.0.is_null() {
                    unsafe { let _ = ScreenToClient(parent, &mut pt); }
                }
                unsafe {
                    let _ = MoveWindow(hwnd, pt.x, pt.y, r - l, b - t, false);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                return LRESULT(0);
            }
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
        WM_CONTEXTMENU => {
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if idx >= 0 && EDITING.with(|c| c.get()) {
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
/// 编辑模式下额外画主题色强调边框 + 四角手柄，提示可拖动 / 缩放。
/// 其余区域留空，让丙烯酸毛玻璃透出壁纸与图标。
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

        // 边框：编辑模式用主题色 2px 强调框；平时 1px 柔和浅灰蓝
        let (bw, bcolor) = if editing { (2, color) } else { (1, 0x7E8898) };
        let pen = CreatePen(PS_SOLID, bw, COLORREF(bcolor));
        let old_pen = SelectObject(hdc, pen.into());
        let old_br = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = RoundRect(hdc, 1, 1, w - 1, h - 1, RADIUS * 2, RADIUS * 2);
        let _ = SelectObject(hdc, old_pen);
        let _ = SelectObject(hdc, old_br);
        let _ = DeleteObject(pen.into());

        // 编辑模式：四角画实心小方块手柄
        if editing {
            let hb = CreateSolidBrush(COLORREF(color));
            let s = 8;
            for (hx, hy) in [(1, 1), (w - 1 - s, 1), (1, h - 1 - s), (w - 1 - s, h - 1 - s)] {
                let hr = RECT { left: hx, top: hy, right: hx + s, bottom: hy + s };
                let _ = FillRect(hdc, &hr, hb);
            }
            let _ = DeleteObject(hb.into());
        }

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
