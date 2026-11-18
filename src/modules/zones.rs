//! 桌面分区：在桌面上绘制半透明毛玻璃分组框（壁纸之上、图标之下）。
//!
//! 纯视觉分组——不移动、不删除任何文件，桌面图标留在原位，仅被分区的毛玻璃框
//! 象征性地「收纳」为一组（参考 Fences / 桌面分区整理工具的效果）。
//!
//! v1 范围：
//! - 开机 / 启用时把每个分区创建为一个丙烯酸（Acrylic）毛玻璃子窗，挂在桌面图标
//!   宿主（SHELLDLL_DefView）的父层之下，即壁纸之上、图标之下。
//! - 分区几何与显隐全部由设置页的 config 驱动；分区窗口本身不接收鼠标
//!   （图标宿主 SysListView 铺满整个桌面、盖在分区之上，分区天然点击不到；
//!   桌面上的拖动 / 缩放 / 右键改名属于 v2，需要额外的桌面输入层）。
//!
//! 全部数据仅存本机 config.json，不联网。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 分区窗口类名（顶层子窗，随主窗线程的消息循环一起分发）
const CLASS: PCWSTR = w!("WinIslandZone");
/// 标题条高度（像素）
const TITLE_H: i32 = 26;
/// 圆角半径
const RADIUS: i32 = 14;

thread_local! {
    /// 与 config.zones 对齐的活窗口：None = 该分区当前无窗口（隐藏 / 未启用）
    static WIN: RefCell<Vec<Option<HWND>>> = const { RefCell::new(Vec::new()) };
    /// 分区窗口类是否已注册
    static CLASSED: Cell<bool> = const { Cell::new(false) };
}

/// 取某个分区在 config 中的主题色（0xRRGGBB）
fn zone_rgb(id: &str) -> u32 {
    crate::theme::THEMES
        .iter()
        .find(|(k, _, _)| *k == id)
        .map(|(_, _, c)| *c)
        .unwrap_or(0x4E8FFF)
}

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
    size: u32,
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
                    size: std::mem::size_of::<AccentPolicy>() as u32,
                };
                let _ = f(hwnd, &mut d as *mut _ as *mut c_void);
            }
        }
    }
}

fn register_class() {
    if CLASSED.with(|c| c.get()) {
        return;
    }
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        // 类背景刷用深色打底，给不支持丙烯酸的旧系统一个不刺眼的底色
        let bg = CreateSolidBrush(COLORREF(0x1E2438));
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

/// 在桌面图标之下、壁纸之上创建/摆放一个分区窗口。
fn create_zone(layer: HWND, z: &crate::config::ZoneConf, idx: usize) -> HWND {
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
            Err(_) => return HWND::default(),
        };
        if hw.0.is_null() {
            return hw;
        }
        SetWindowLongPtrW(hw, GWLP_USERDATA, idx as isize);
        set_acrylic(hw, 0x3A1C2438); // 深色丙烯酸打底：AARRGGBB
        let _ = SetWindowPos(hw, None, z.x, z.y, z.w.max(96), z.h.max(64), SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let _ = ShowWindow(hw, SW_SHOWNOACTIVATE);
        hw
    }
}

fn position_zone(hw: HWND, z: &crate::config::ZoneConf) {
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

/// 读取当前分区配置快照，并据此创建 / 更新 / 销毁分区窗口。
/// 在 app 主线程（消息循环）中调用即可。
pub fn sync() {
    register_class();
    let enabled = crate::app::with_cfg(|c| c.modules.zones);
    if !enabled {
        teardown();
        return;
    }

    // 首次启用且尚未有任何分区时，自动铺几个示例分区，方便一上来就看到效果
    if crate::app::with_cfg(|c| c.zones.is_empty()) {
        seed_if_needed();
    }
    let cfg2 = crate::app::with_cfg(|c| c.zones.clone());

    let (layer, def) = shell();
    if layer.0.is_null() {
        return; // 找不到桌面层（比如正在锁定屏幕），稍后重试
    }
    let mut front: Option<HWND> = None;

    WIN.with(|ws| {
        let mut v = ws.borrow_mut();
        // 配置里已删除（列表缩短）的分区：销毁其窗口
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
                    position_zone(hw, z);
                    front = Some(hw);
                }
                Some(hw) => {
                    // 配置里已隐藏：销毁窗口，显示时再重建
                    unsafe { let _ = DestroyWindow(hw); }
                    v[i] = None;
                }
                None if z.visible => {
                    let hw = create_zone(layer, z, i);
                    if !hw.0.is_null() {
                        v[i] = Some(hw);
                        front = Some(hw);
                    }
                }
                None => {}
            }
        }
    });

    // 把图标宿主插到最上面的分区之前，确保图标始终在分区之上
    if let Some(f) = front {
        unsafe {
            let _ = SetWindowPos(def, Some(f), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
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
            color: ["blue", "teal", "green", "purple", "pink", "orange"][count % 6].to_string(),
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

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, _wparam: WPARAM, _lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_zone(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // 由 WM_PAINT 全量重绘，避免闪烁
        WM_NCHITTEST => {
            // 分区纯视觉、不接收鼠标，点击穿透到桌面
            LRESULT(HTTRANSPARENT as isize)
        }
        _ => DefWindowProcW(hwnd, msg, _wparam, _lparam),
    }
}

/// 用 GDI 画分区外观：顶部主题色标题条 + 圆角边框 + 左上角名称。
fn paint_zone(hwnd: HWND) {
    let idx = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    let (name, color) = if idx >= 0 {
        crate::app::with_cfg(|c| {
            c.zones
                .get(idx as usize)
                .map(|z| (z.name.clone(), zone_rgb(&z.color)))
                .unwrap_or_else(|| ("分区".to_string(), 0x4E8FFF))
        })
    } else {
        ("分区".to_string(), 0x4E8FFF)
    };

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

        // 顶部标题条：主题色填充
        let bar = CreateSolidBrush(COLORREF(color));
        let rgn = CreateRectRgn(1, 1, w, TITLE_H);
        let _ = FillRgn(hdc, rgn, bar);
        let _ = DeleteObject(rgn.into());
        let _ = DeleteObject(bar.into());

        // 圆角边框
        let pen = CreatePen(PS_SOLID, 2, COLORREF(color));
        let old_pen = SelectObject(hdc, pen.into());
        let old_br = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = RoundRect(hdc, 1, 1, w - 1, h - 1, RADIUS * 2, RADIUS * 2);
        let _ = SelectObject(hdc, old_pen);
        let _ = SelectObject(hdc, old_br);
        let _ = DeleteObject(pen.into());

        // 名称文字（主题色）
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
        let _ = SetTextColor(hdc, COLORREF(color));
        let mut tr = RECT { left: 16, top: 5, right: w - 16, bottom: TITLE_H };
        let mut wide: Vec<u16> = name.encode_utf16().collect();
        let _ = DrawTextW(hdc, &mut wide, &mut tr, DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX);
        let _ = SelectObject(hdc, old_font);
        let _ = DeleteObject(font.into());

        let _ = EndPaint(hwnd, &ps);
    }
}
