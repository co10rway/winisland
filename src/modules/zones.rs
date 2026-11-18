//! 桌面分区（围栏）：仿 Fences 的桌面图标收纳面板。
//!
//! 架构（v0.12 重写为「图标托管」）：
//! - 每个分区是一块**顶层分层窗口（WS_EX_LAYERED）**，内容由
//!   `UpdateLayeredWindow` 提交一张逐像素预乘的 ARGB 位图。
//!
//!   **为什么不用 acrylic**：`SetWindowCompositionAttribute` 在 acrylic 窗口上
//!   画过的 GDI 像素**无法擦除**——GDI 写不了 alpha=0，旧像素永久留在窗口表面。
//!   表现就是改名字旧字叠新字、拖动/缩放时系统位图搬运遗留成阶梯状重影。
//!   改成「每次内容变化都整张重建位图再提交」后，位图里只可能有本次绘制的内容，
//!   物理上不可能残留；窗口移动时 DWM 搬运的是完整图层而非屏幕像素，拖动也无重影。
//!
//!   半透明由像素 alpha 直接给出（面板底 A=210），所以壁纸能透出来、图标依然清晰：
//!   GDI 画上去的像素在提交前会被提升到 alpha=255（见 `render` 第 ④ 步），
//!   避免与预乘值二次相乘而发灰。
//! - 桌面图标由本模块接管：隐藏 Explorer 的图标层（SHELLDLL_DefView），按分区配置
//!   枚举桌面项、**自绘图标 + 名称**，网格排列在面板内。这样图标才真正
//!   「在分区里」，而不是被一个空框罩住。
//! - 双击图标用 ShellExecute 打开；把文件拖到面板上即归入该分区
//!   （DragAcceptFiles + WM_DROPFILES）。
//! - 标题栏拖动移动、四边四角缩放、右上角箭头折叠 / 展开、右键菜单改名 / 换色 / 删除。
//! - 位置 / 大小 / 折叠 / 归入项持久化到 config.json。退出时恢复 Explorer 图标层。
//!
//! 全部数据仅存本机 config.json，不联网。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus};
use windows::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, SHGetFileInfoW, ShellExecuteW, HDROP, SHFILEINFOW,
    SHGFI_ICON, SHGFI_LARGEICON,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 分区窗口类名
const CLASS: PCWSTR = w!("WinIslandZone");
/// 重命名弹窗类名
const RENAME_CLASS: PCWSTR = w!("WinIslandRename");
/// 标题栏高度
const TITLE_H: i32 = 34;
/// 面板内边距
const PAD: i32 = 10;
/// 图标格宽
const CELL_W: i32 = 96;
/// 图标格高（图标 + 两行名称）
const CELL_H: i32 = 86;
/// 图标边长
const ICON_SZ: i32 = 32;
/// 圆角半径
const RADIUS: i32 = 12;
/// 缩放手柄厚度
const EDGE: i32 = 8;
/// 最小宽 / 高
const MIN_W: i32 = 160;
const MIN_H: i32 = TITLE_H + 18;
/// 面板底色（RGB）：深灰绿，与参考图一致
const PANEL_RGB: (u32, u32, u32) = (30, 38, 36);
/// 面板底色不透明度（0-255）：210 ≈ 82%，壁纸能透出来但不糊
const PANEL_A: u32 = 210;
/// 标题栏底色（RGB）：比面板更沉，接近参考图
const TITLE_RGB: (u32, u32, u32) = (20, 28, 26);
/// 分区主题色 ID 列表（右键菜单换色用）
const COLOR_IDS: [&str; 7] = ["blue", "teal", "green", "purple", "pink", "orange", "red"];

thread_local! {
    /// 与 config.zones 对齐的活窗口
    static WIN: RefCell<Vec<Option<HWND>>> = const { RefCell::new(Vec::new()) };
    static CLASSED: Cell<bool> = const { Cell::new(false) };
    static REN_CLASSED: Cell<bool> = const { Cell::new(false) };
    static RENAME_IDX: Cell<Option<usize>> = const { Cell::new(None) };
    static DRAG: RefCell<Option<DragState>> = const { RefCell::new(None) };
    static EDITING: Cell<bool> = const { Cell::new(false) };
    /// 文件名 → HICON（isize），懒加载缓存
    static ICONS: RefCell<HashMap<String, isize>> = RefCell::new(HashMap::new());
    /// 当前桌面项快照
    static ITEMS: RefCell<Vec<DeskItem>> = const { RefCell::new(Vec::new()) };
    /// 桌面项签名（用于轮询变更）
    static SIGN: RefCell<String> = RefCell::new(String::new());
}

#[derive(Clone)]
struct DeskItem {
    name: String,
    path: PathBuf,
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
    None,
}

struct DragState {
    mode: DragMode,
    start_screen: POINT,
    start_rect: RECT, // 屏幕坐标
}

// ── 小工具 ──

/// 主题色 id → 0xRRGGBB
fn zone_rgb(id: &str) -> u32 {
    crate::theme::THEMES
        .iter()
        .find(|(k, _, _)| *k == id)
        .map(|(_, _, c)| *c)
        .unwrap_or(0x4E8FFF)
}

/// RGB → GDI COLORREF（0x00BBGGRR）
fn rgb(r: u32, g: u32, b: u32) -> u32 {
    (b << 16) | (g << 8) | r
}

/// 转 NUL 结尾 UTF-16（所有 LPCWSTR 参数必须 NUL 结尾，否则读越界乱码）
fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 找桌面图标宿主（def = SHELLDLL_DefView）及其父层（layer）
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

/// 隐藏 / 显示 Explorer 桌面图标层（图标由本模块托管）
fn set_shell_icons_visible(show: bool) {
    let (_, def) = shell();
    if def.0.is_null() {
        return;
    }
    unsafe {
        let _ = ShowWindow(def, if show { SW_SHOW } else { SW_HIDE });
    }
}

// ── 分层窗口渲染（ARGB 逐像素） ──
//
// 为什么必须这样：acrylic 窗口上 GDI 画过的像素**无法擦除**——GDI 写不了
// alpha=0，所以旧像素永远留在窗口表面。这就是「改名字旧字叠新字」、
// 「拖动 / 缩放时靠系统位图搬运遗留成阶梯状残影」的物理原因。
//
// 改为 layered 窗口 + UpdateLayeredWindow：每次内容变化都**整张重建**一张
// 32bpp 预乘 ARGB 位图再提交，物理上不可能有残留；窗口移动时 DWM 搬运的是
// 完整图层而不是屏幕像素，所以拖动同样不会重影。

/// 一张离屏 32bpp（预乘 BGRA、top-down）位图及其 DC
struct Surf {
    dib: HBITMAP,
    mdc: HDC,
    old: HGDIOBJ,
    buf: *mut u32,
    w: i32,
    h: i32,
}

impl Surf {
    fn new(w: i32, h: i32) -> Option<Surf> {
        unsafe {
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // 负高 = top-down，与 GDI 坐标一致
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;
            let mut bits: *mut c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            if dib.0.is_null() || bits.is_null() {
                return None;
            }
            let mdc = CreateCompatibleDC(None);
            if mdc.0.is_null() {
                let _ = DeleteObject(dib.into());
                return None;
            }
            let old = SelectObject(mdc, dib.into());
            Some(Surf {
                dib,
                mdc,
                old,
                buf: bits as *mut u32,
                w,
                h,
            })
        }
    }

    fn pixels(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.buf, (self.w * self.h) as usize) }
    }
}

impl Drop for Surf {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mdc, self.old);
            let _ = DeleteDC(self.mdc);
            let _ = DeleteObject(self.dib.into());
        }
    }
}

/// 写一个预乘 ARGB 像素（r,g,b 未预乘）
#[inline]
fn put_px(buf: &mut [u32], i: usize, c: (u32, u32, u32), a: u32) {
    buf[i] = (a << 24)
        | ((c.0 * a / 255) << 16)
        | ((c.1 * a / 255) << 8)
        | (c.2 * a / 255);
}

/// 圆角覆盖率（0-4）。返回 4 表示完全在圆角内。只对四角像素调用。
fn corner_cover(x: i32, y: i32, w: i32, h: i32, r: i32) -> u32 {
    let (cx, cy) = if x < r && y < r {
        (r as f32, r as f32)
    } else if x >= w - r && y < r {
        ((w - r) as f32, r as f32)
    } else if x < r && y >= h - r {
        (r as f32, (h - r) as f32)
    } else if x >= w - r && y >= h - r {
        ((w - r) as f32, (h - r) as f32)
    } else {
        return 4;
    };
    let rf = r as f32;
    let mut hit = 0u32;
    for sy in 0..2 {
        for sx in 0..2 {
            let px = x as f32 + 0.25 + sx as f32 * 0.5;
            let py = y as f32 + 0.25 + sy as f32 * 0.5;
            let (dx, dy) = (px - cx, py - cy);
            if dx * dx + dy * dy <= rf * rf {
                hit += 1;
            }
        }
    }
    hit
}

// ── 类注册 ──

fn register_class() {
    if CLASSED.with(|c| c.get()) {
        return;
    }
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: CLASS,
            hbrBackground: std::mem::transmute(GetStockObject(NULL_BRUSH)),
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
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
            hbrBackground: CreateSolidBrush(COLORREF(rgb(30, 38, 36))),
            style: CS_HREDRAW | CS_VREDRAW,
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
    REN_CLASSED.with(|c| c.set(true));
}

// ── 桌面项枚举与图标 ──

fn desktop_items() -> Vec<DeskItem> {
    let mut out: Vec<DeskItem> = Vec::new();
    let mut dirs: Vec<PathBuf> = vec![crate::modules::files::desktop_dir()];
    if let Ok(pubd) = std::env::var("PUBLIC") {
        dirs.push(PathBuf::from(pubd).join("Desktop"));
    }
    for d in dirs {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.is_empty() || name.starts_with('.') {
                    continue;
                }
                if name.eq_ignore_ascii_case("desktop.ini") {
                    continue;
                }
                let p = e.path();
                out.push(DeskItem { name, path: p });
            }
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

fn refresh_items() {
    let items = desktop_items();
    let mut names: Vec<String> = items.iter().map(|i| i.name.clone()).collect();
    names.sort();
    let sig = names.join("\u{1}");
    ITEMS.with(|v| *v.borrow_mut() = items);
    SIGN.with(|s| *s.borrow_mut() = sig);
}

fn path_of(name: &str) -> Option<PathBuf> {
    ITEMS.with(|v| {
        v.borrow()
            .iter()
            .find(|i| i.name == name)
            .map(|i| i.path.clone())
    })
}

/// 取某桌面项的 HICON（懒加载并缓存）。返回 0 表示失败。
fn icon_of(name: &str) -> isize {
    if let Some(v) = ICONS.with(|m| m.borrow().get(name).copied()) {
        return v;
    }
    let path = match path_of(name) {
        Some(p) => p,
        None => return 0,
    };
    let wide = wide_nul(&path.to_string_lossy());
    let mut fi = SHFILEINFOW::default();
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut fi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    let h = if ok != 0 { fi.hIcon.0 as isize } else { 0 };
    ICONS.with(|m| m.borrow_mut().insert(name.to_string(), h));
    h
}

fn open_item(name: &str) {
    if let Some(p) = path_of(name) {
        let wide = wide_nul(&p.to_string_lossy());
        unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            );
        }
    }
}

// ── 窗口创建 ──

fn create_zone(z: &crate::config::ZoneConf, idx: usize) -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let h = if z.collapsed { TITLE_H } else { z.h.max(MIN_H) };
        let hw = match CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            CLASS,
            w!(""),
            WS_POPUP,
            z.x,
            z.y,
            z.w.max(MIN_W),
            h,
            None,
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
        DragAcceptFiles(hw, true);
        // layered 窗口必须提交过位图才可见，先渲染一次
        render(hw, idx);
        let _ = SetWindowPos(
            hw,
            Some(HWND_BOTTOM),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        let _ = ShowWindow(
            hw,
            if z.visible {
                SW_SHOWNOACTIVATE
            } else {
                SW_HIDE
            },
        );
        if idx == 0 {
            SetTimer(Some(hw), 1, 3000, None);
        }
        Some(hw)
    }
}

// ── 生命周期 ──

/// 由 config 驱动同步：创建 / 更新 / 销毁分区窗口。在 app 主线程调用。
pub fn sync() {
    register_class();
    register_rename_class();
    let enabled = crate::app::with_cfg(|c| c.modules.zones);
    if !enabled {
        set_shell_icons_visible(true);
        teardown();
        return;
    }
    refresh_items();
    // 从旧版本升级上来的分区没有 items 字段（全空）→ 清空重新按类型分类，
    // 否则用户会看到一堆空面板 + 一个塞满的「其他」。
    let stale = crate::app::with_cfg(|c| {
        !c.zones.is_empty() && c.zones.iter().all(|z| z.items.is_empty())
    });
    if stale && !ITEMS.with(|v| v.borrow().is_empty()) {
        crate::app::with_cfg(|c| c.zones.clear());
    }
    if crate::app::with_cfg(|c| c.zones.is_empty()) {
        seed_if_needed();
    }
    reconcile_items();
    let zones = crate::app::with_cfg(|c| c.zones.clone());
    // 接管桌面图标：隐藏 Explorer 图标层
    set_shell_icons_visible(false);

    WIN.with(|ws| {
        let mut v = ws.borrow_mut();
        if v.len() > zones.len() {
            for hw in v.iter_mut().skip(zones.len()) {
                if let Some(h) = hw.take() {
                    unsafe {
                        let _ = DestroyWindow(h);
                    }
                }
            }
        }
        v.resize_with(zones.len(), || None);

        for (i, z) in zones.iter().enumerate() {
            let want_h = if z.collapsed { TITLE_H } else { z.h.max(MIN_H) };
            match v[i] {
                Some(hw) => unsafe {
                    if z.visible {
                        let _ = SetWindowPos(
                            hw,
                            None,
                            z.x,
                            z.y,
                            z.w.max(MIN_W),
                            want_h,
                            SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
                        );
                        render(hw, i);
                    } else {
                        let _ = ShowWindow(hw, SW_HIDE);
                    }
                },
                None if z.visible => {
                    if let Some(hw) = create_zone(z, i) {
                        v[i] = Some(hw);
                    }
                }
                None => {}
            }
        }
    });
}

/// 重新枚举桌面项并同步（设置页 / 右键「刷新」）
pub fn refresh() {
    refresh_items();
    sync();
}

/// 兼容设置页接口（面板常驻可拖动/缩放，不做独立编辑模式）
#[allow(dead_code)]
pub fn is_editing() -> bool {
    EDITING.with(|c| c.get())
}

/// 兼容设置页接口
#[allow(dead_code)]
pub fn toggle_edit() -> bool {
    let now = EDITING.with(|c| {
        let n = !c.get();
        c.set(n);
        n
    });
    WIN.with(|ws| {
        if let Ok(v) = ws.try_borrow() {
            for (i, hw) in v.iter().enumerate() {
                if let Some(h) = hw {
                    render(*h, i);
                }
            }
        }
    });
    now
}

fn teardown() {
    WIN.with(|ws| {
        let mut v = ws.borrow_mut();
        for hw in v.iter_mut() {
            if let Some(h) = hw.take() {
                unsafe {
                    let _ = KillTimer(Some(h), 1);
                    let _ = DestroyWindow(h);
                }
            }
        }
    });
}

/// 清理失效项；把未归入任何分区的桌面项收进「其他」分区，保证图标不丢。
fn reconcile_items() {
    let names: Vec<String> = ITEMS.with(|v| v.borrow().iter().map(|i| i.name.clone()).collect());
    let nameset: HashSet<String> = names.iter().cloned().collect();
    crate::app::with_cfg(|c| {
        for z in c.zones.iter_mut() {
            z.items.retain(|n| nameset.contains(n));
        }
        let mut used: HashSet<String> = HashSet::new();
        for z in c.zones.iter() {
            for n in &z.items {
                used.insert(n.clone());
            }
        }
        let loose: Vec<String> = names
            .iter()
            .filter(|n| !used.contains(*n))
            .cloned()
            .collect();
        if loose.is_empty() {
            return;
        }
        if c.zones.is_empty() {
            c.zones.push(crate::config::ZoneConf {
                name: "其他".into(),
                x: 60,
                y: 120,
                w: 340,
                h: 210,
                color: "purple".into(),
                visible: true,
                collapsed: false,
                items: loose,
            });
        } else {
            let target = c.zones.iter().position(|z| z.name == "其他").unwrap_or(0);
            c.zones[target].items.extend(loose);
        }
    });
}

/// 首次启用：按扩展名把桌面项自动分到「文档 / 应用 / 其他」三个面板。
fn seed_if_needed() {
    {
        let items = ITEMS.with(|v| v.borrow().clone());
        let mut docs: Vec<String> = Vec::new();
        let mut apps: Vec<String> = Vec::new();
        let mut others: Vec<String> = Vec::new();
        for it in &items {
            let ext = it
                .path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let is_dir = it.path.is_dir();
            let is_doc = matches!(
                ext.as_str(),
                "docx"
                    | "doc"
                    | "xlsx"
                    | "xls"
                    | "pptx"
                    | "ppt"
                    | "pdf"
                    | "txt"
                    | "md"
                    | "csv"
                    | "rtf"
                    | "one"
            );
            if ext == "lnk" || ext == "url" || ext == "exe" {
                apps.push(it.name.clone());
            } else if is_doc || is_dir {
                docs.push(it.name.clone());
            } else {
                others.push(it.name.clone());
            }
        }

        let sw = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let sh = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        let x = (sw as f32 * 0.04) as i32;
        let w = 340;
        let cols = ((w - 2 * PAD) / CELL_W).max(1);
        let mut zones: Vec<crate::config::ZoneConf> = Vec::new();
        let mut y = (sh as f32 * 0.10) as i32;
        let groups: [(&str, Vec<String>, &str); 3] = [
            ("文档", docs, "blue"),
            ("应用", apps, "teal"),
            ("其他", others, "purple"),
        ];
        for (name, list, color) in groups {
            if list.is_empty() {
                continue;
            }
            let rows = ((list.len() as i32 + cols - 1) / cols).max(1);
            let h = (TITLE_H + PAD * 2 + rows * CELL_H).min((sh as f32 * 0.42) as i32);
            zones.push(crate::config::ZoneConf {
                name: name.to_string(),
                x,
                y,
                w,
                h,
                color: color.to_string(),
                visible: true,
                collapsed: false,
                items: list,
            });
            y += h + 18;
        }
        if zones.is_empty() {
            zones.push(crate::config::ZoneConf {
                name: "新围栏".into(),
                x,
                y: (sh as f32 * 0.12) as i32,
                w,
                h: 210,
                color: "blue".into(),
                visible: true,
                collapsed: false,
                items: Vec::new(),
            });
        }
        crate::app::with_cfg(|c| c.zones = zones);
        crate::app::save_now();
    }
}

/// 设置页「＋ 新建围栏」：在桌面新增一个空面板。
pub fn add_zone() {
    register_rename_class();
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    crate::app::with_cfg(|c| {
        let count = c.zones.len();
        c.zones.push(crate::config::ZoneConf {
            name: format!("新建围栏 {}", count + 1),
            x: sw / 2 - 170 + (count % 4) as i32 * 24,
            y: sh / 2 - 100 + (count % 4) as i32 * 24,
            w: 340,
            h: 210,
            color: ["blue", "teal", "green", "purple", "pink", "orange", "red"][count % 7]
                .to_string(),
            visible: true,
            collapsed: false,
            items: Vec::new(),
        });
    });
    crate::app::save_now();
    sync();
}

/// 设置页「隐藏 / 显示全部」
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

/// 程序退出：恢复 Explorer 图标层 + 销毁分区窗口。
pub fn shutdown() {
    teardown();
    set_shell_icons_visible(true);
}

// ── 命中测试 ──

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    (rc.right - rc.left, rc.bottom - rc.top)
}

fn hit_mode(hwnd: HWND, screen_pt: POINT, w: i32, h: i32) -> DragMode {
    let mut pt = screen_pt;
    unsafe {
        let _ = ScreenToClient(hwnd, &mut pt);
    }
    let (x, y) = (pt.x, pt.y);
    let left = x <= EDGE;
    let right = x >= w - 1 - EDGE;
    let top = y <= EDGE;
    let bottom = y >= h - 1 - EDGE;
    if left && top {
        DragMode::TopLeft
    } else if right && top {
        DragMode::TopRight
    } else if left && bottom {
        DragMode::BottomLeft
    } else if right && bottom {
        DragMode::BottomRight
    } else if left {
        DragMode::Left
    } else if right {
        DragMode::Right
    } else if top {
        DragMode::Top
    } else if bottom {
        DragMode::Bottom
    } else if y < TITLE_H {
        DragMode::Move
    } else {
        DragMode::None
    }
}

/// 标题栏右侧折叠箭头的命中框
fn in_chevron(pt: POINT, w: i32) -> bool {
    pt.x >= w - 34 && pt.x <= w - 8 && pt.y >= 4 && pt.y <= TITLE_H - 4
}

/// 内部坐标点落在第几个图标格
fn icon_at(hwnd: HWND, screen_pt: POINT) -> Option<usize> {
    let idx = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if idx < 0 {
        return None;
    }
    let idx = idx as usize;
    let (collapsed, count) = crate::app::with_cfg(|c| {
        c.zones
            .get(idx)
            .map(|z| (z.collapsed, z.items.len()))
            .unwrap_or((false, 0))
    });
    if collapsed || count == 0 {
        return None;
    }
    let mut pt = screen_pt;
    unsafe {
        let _ = ScreenToClient(hwnd, &mut pt);
    }
    let (w, _) = client_size(hwnd);
    let cols = ((w - 2 * PAD) / CELL_W).max(1);
    for i in 0..count {
        let col = i as i32 % cols;
        let row = i as i32 / cols;
        let cx = PAD + col * CELL_W;
        let cy = TITLE_H + PAD + row * CELL_H;
        if pt.x >= cx && pt.x < cx + CELL_W && pt.y >= cy && pt.y < cy + CELL_H {
            return Some(i);
        }
    }
    None
}

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
                z.w = (rc.right - rc.left).max(MIN_W);
                // 折叠态时窗口高度只是标题栏，不要把展开高度覆盖掉
                if !z.collapsed {
                    z.h = (rc.bottom - rc.top).max(MIN_H);
                }
            }
        });
    }
    crate::app::save_now();
}

/// 折叠 / 展开切换
fn toggle_collapse(idx: usize) {
    let (now_collapsed, full_h) = crate::app::with_cfg(|c| {
        c.zones
            .get(idx)
            .map(|z| (z.collapsed, z.h))
            .unwrap_or((false, 200))
    });
    let nc = !now_collapsed;
    crate::app::with_cfg(|c| {
        if let Some(z) = c.zones.get_mut(idx) {
            z.collapsed = nc;
        }
    });
    crate::app::save_now();
    let hw = WIN.with(|v| v.borrow().get(idx).copied().flatten());
    if let Some(hw) = hw {
        unsafe {
            let mut rc = RECT::default();
            let _ = GetWindowRect(hw, &mut rc);
            let w = (rc.right - rc.left).max(MIN_W);
            let nh = if nc { TITLE_H } else { full_h.max(MIN_H) };
            let _ = SetWindowPos(
                hw,
                None,
                rc.left,
                rc.top,
                w,
                nh,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            render(hw, idx);
        }
    }
}

// ── 右键菜单 ──

const MENU_RENAME: u32 = 2001;
const MENU_DELETE: u32 = 2002;
const MENU_NEW: u32 = 2003;
const MENU_HIDE_ALL: u32 = 2004;
const MENU_REFRESH: u32 = 2005;
const MENU_COLLAPSE: u32 = 2006;
const MENU_COLOR_BASE: u32 = 2100;

fn popup_menu(hwnd: HWND, idx: usize) {
    let color_id = crate::app::with_cfg(|c| {
        c.zones
            .get(idx)
            .map(|z| z.color.clone())
            .unwrap_or_else(|| "blue".to_string())
    });
    let collapsed = crate::app::with_cfg(|c| c.zones.get(idx).map(|z| z.collapsed).unwrap_or(false));
    let checked = COLOR_IDS.iter().position(|c| *c == color_id).unwrap_or(0);

    unsafe {
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return,
        };
        let cm = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => {
                let _ = DestroyMenu(menu);
                return;
            }
        };
        let _ = AppendMenuW(menu, MF_STRING, MENU_RENAME as usize, w!("重命名…"));
        for (i, cid) in COLOR_IDS.iter().enumerate() {
            let label: Vec<u16> = match *cid {
                "blue" => "海蓝",
                "teal" => "青碧",
                "green" => "青草",
                "purple" => "罗兰",
                "pink" => "樱粉",
                "orange" => "落橙",
                _ => "绯红",
            }
            .encode_utf16()
            .collect();
            let lz: Vec<u16> = label.into_iter().chain(std::iter::once(0)).collect();
            let flags = if i == checked {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            };
            let _ = AppendMenuW(
                cm,
                flags,
                (MENU_COLOR_BASE + i as u32) as usize,
                PCWSTR(lz.as_ptr()),
            );
        }
        let _ = AppendMenuW(menu, MF_POPUP, cm.0 as usize, w!("配色"));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_COLLAPSE as usize,
            if collapsed { w!("展开") } else { w!("折叠") },
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, MENU_NEW as usize, w!("新建围栏"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_HIDE_ALL as usize, w!("隐藏全部围栏"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_REFRESH as usize, w!("刷新"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, MENU_DELETE as usize, w!("删除围栏"));

        let mut pos = POINT::default();
        let _ = GetCursorPos(&mut pos);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON,
            pos.x,
            pos.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(cm);
        let _ = DestroyMenu(menu);
    }
}

// ── 重命名弹窗 ──

const RENAME_EDIT: i32 = 3001;
const RENAME_OK: i32 = 3002;
const RENAME_CANCEL: i32 = 3003;

unsafe extern "system" fn rename_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinst = GetModuleHandleW(None).unwrap();
            let edit_style =
                WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | ES_AUTOHSCROLL as u32);
            let btn_style =
                WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32);
            let _ = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("Edit"),
                w!(""),
                edit_style,
                16,
                16,
                288,
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
                80,
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
                168,
                56,
                72,
                26,
                Some(hwnd),
                Some(HMENU(RENAME_CANCEL as *mut c_void)),
                Some(hinst.into()),
                None,
            );
            if let Some(idx) = RENAME_IDX.with(|c| c.get()) {
                if let Some(cur) = crate::app::with_cfg(|c| c.zones.get(idx).map(|z| z.name.clone()))
                {
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
                            WIN.with(|ws| {
                                if let Ok(v) = ws.try_borrow() {
                                    if let Some(Some(hw)) = v.get(idx) {
                                        render(*hw, idx);
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
        WM_ERASEBKGND => LRESULT(1),
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
            w!("重命名围栏"),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            0,
            0,
            330,
            130,
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
        let _ = SetWindowPos(
            hw,
            None,
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        let _ = ShowWindow(hw, SW_SHOW);
    }
}

// ── 分区窗口过程 ──

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            // layered 窗口的内容全部由 UpdateLayeredWindow 提供，
            // 这里只需要把 dirty 标记清掉，避免 WM_PAINT 反复触发。
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            // 尺寸变化 → 整张位图重建（缩放时每帧走这里，保证边框始终贴合）
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if idx >= 0 {
                render(hwnd, idx as usize);
            }
            LRESULT(0)
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
                DragMode::Move => IDC_SIZEALL,
                _ => IDC_ARROW,
            };
            if let Ok(cur) = LoadCursorW(None, idc) {
                let _ = SetCursor(Some(cur));
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if idx < 0 {
                return LRESULT(0);
            }
            let idx = idx as usize;
            let cx = ((lparam.0 as u32) & 0xffff) as i16 as i32;
            let cy = (((lparam.0 as u32) >> 16) & 0xffff) as i16 as i32;
            let local = POINT { x: cx, y: cy };
            let mut sp = local;
            let _ = ClientToScreen(hwnd, &mut sp);
            let (w, h) = client_size(hwnd);

            // 折叠箭头
            if in_chevron(local, w) {
                toggle_collapse(idx);
                return LRESULT(0);
            }
            // 点在图标上：不启动拖动（等双击打开）
            if icon_at(hwnd, sp).is_some() {
                return LRESULT(0);
            }
            let mode = hit_mode(hwnd, sp, w, h);
            if mode == DragMode::None {
                return LRESULT(0);
            }
            let mut rc = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rc);
            DRAG.with(|d| {
                *d.borrow_mut() = Some(DragState {
                    mode,
                    start_screen: sp,
                    start_rect: rc,
                });
            });
            let _ = SetCapture(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if idx < 0 {
                return LRESULT(0);
            }
            let cx = ((lparam.0 as u32) & 0xffff) as i16 as i32;
            let cy = (((lparam.0 as u32) >> 16) & 0xffff) as i16 as i32;
            let mut sp = POINT { x: cx, y: cy };
            let _ = ClientToScreen(hwnd, &mut sp);
            let idx = idx as usize;
            if let Some(i) = icon_at(hwnd, sp) {
                let name =
                    crate::app::with_cfg(|c| c.zones.get(idx).and_then(|z| z.items.get(i).cloned()));
                if let Some(n) = name {
                    open_item(&n);
                }
            }
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
                match mode {
                    DragMode::Move => {
                        l += dx;
                        t += dy;
                        r += dx;
                        b += dy;
                    }
                    DragMode::Left => {
                        l += dx;
                        if l > r - MIN_W {
                            l = r - MIN_W;
                        }
                    }
                    DragMode::Right => {
                        r += dx;
                        if r < l + MIN_W {
                            r = l + MIN_W;
                        }
                    }
                    DragMode::Top => {
                        t += dy;
                        if t > b - MIN_H {
                            t = b - MIN_H;
                        }
                    }
                    DragMode::Bottom => {
                        b += dy;
                        if b < t + MIN_H {
                            b = t + MIN_H;
                        }
                    }
                    DragMode::TopLeft => {
                        l += dx;
                        t += dy;
                        if l > r - MIN_W {
                            l = r - MIN_W;
                        }
                        if t > b - MIN_H {
                            t = b - MIN_H;
                        }
                    }
                    DragMode::TopRight => {
                        r += dx;
                        t += dy;
                        if r < l + MIN_W {
                            r = l + MIN_W;
                        }
                        if t > b - MIN_H {
                            t = b - MIN_H;
                        }
                    }
                    DragMode::BottomLeft => {
                        l += dx;
                        b += dy;
                        if l > r - MIN_W {
                            l = r - MIN_W;
                        }
                        if b < t + MIN_H {
                            b = t + MIN_H;
                        }
                    }
                    DragMode::BottomRight => {
                        r += dx;
                        b += dy;
                        if r < l + MIN_W {
                            r = l + MIN_W;
                        }
                        if b < t + MIN_H {
                            b = t + MIN_H;
                        }
                    }
                    DragMode::None => {}
                }
                let _ = MoveWindow(hwnd, l, t, r - l, b - t, true);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONUP => {
            if DRAG.with(|d| d.borrow().is_some()) {
                let _ = ReleaseCapture();
                DRAG.with(|d| *d.borrow_mut() = None);
                commit_rect(hwnd);
            }
            LRESULT(0)
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
                let idx = idx as usize;
                if id == MENU_RENAME {
                    open_rename(idx);
                } else if id == MENU_DELETE {
                    crate::app::with_cfg(|c| {
                        if idx < c.zones.len() {
                            c.zones.remove(idx);
                        }
                    });
                    crate::app::save_now();
                    sync();
                } else if id == MENU_NEW {
                    add_zone();
                } else if id == MENU_HIDE_ALL {
                    toggle_all();
                } else if id == MENU_REFRESH {
                    refresh();
                } else if id == MENU_COLLAPSE {
                    toggle_collapse(idx);
                } else if id >= MENU_COLOR_BASE && id < MENU_COLOR_BASE + COLOR_IDS.len() as u32 {
                    let cid = COLOR_IDS[(id - MENU_COLOR_BASE) as usize];
                    crate::app::with_cfg(|c| {
                        if let Some(z) = c.zones.get_mut(idx) {
                            z.color = cid.to_string();
                        }
                    });
                    crate::app::save_now();
                    render(hwnd, idx);
                }
            }
            LRESULT(0)
        }
        WM_DROPFILES => {
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if idx >= 0 {
                let idx = idx as usize;
                let hdrop = HDROP(wparam.0 as *mut c_void);
                let count = DragQueryFileW(hdrop, u32::MAX, None);
                let mut added: Vec<String> = Vec::new();
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, None);
                    if len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; len as usize + 1];
                    DragQueryFileW(hdrop, i, Some(&mut buf));
                    let p = String::from_utf16_lossy(&buf[..len as usize]);
                    if let Some(n) = Path::new(&p)
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                    {
                        added.push(n);
                    }
                }
                DragFinish(hdrop);
                if !added.is_empty() {
                    refresh_items();
                    crate::app::with_cfg(|c| {
                        for z in c.zones.iter_mut() {
                            z.items.retain(|n| !added.contains(n));
                        }
                        if let Some(z) = c.zones.get_mut(idx) {
                            z.items.extend(added.iter().cloned());
                        }
                    });
                    crate::app::save_now();
                    sync();
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let mut names: Vec<String> =
                ITEMS.with(|v| v.borrow().iter().map(|i| i.name.clone()).collect());
            names.sort();
            let cur = names.join("\u{1}");
            let old = SIGN.with(|s| s.borrow().clone());
            if cur != old {
                refresh();
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(Some(hwnd), 1);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ── 绘制 ──

fn draw_chevron(hdc: HDC, cx: i32, cy: i32, up: bool, col: COLORREF) {
    unsafe {
        let pen = CreatePen(PS_SOLID, 2, col);
        let old = SelectObject(hdc, pen.into());
        let d = 4;
        if up {
            let _ = MoveToEx(hdc, cx - d, cy + 2, None);
            let _ = LineTo(hdc, cx, cy - 2);
            let _ = LineTo(hdc, cx + d, cy + 2);
        } else {
            let _ = MoveToEx(hdc, cx - d, cy - 2, None);
            let _ = LineTo(hdc, cx, cy + 2);
            let _ = LineTo(hdc, cx + d, cy - 2);
        }
        let _ = SelectObject(hdc, old);
        let _ = DeleteObject(pen.into());
    }
}

/// 提交一整张 ARGB 位图到 layered 窗口（位置由 SetWindowPos 负责，这里只换内容）
fn commit_layered(hwnd: HWND, surf: &Surf, w: i32, h: i32) {
    unsafe {
        let screen = GetDC(None);
        if screen.0.is_null() {
            return;
        }
        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let bf = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // pptDst 传 None → 不改变窗口位置，避免和拖动逻辑抢位置
        let _ = UpdateLayeredWindow(
            hwnd,
            Some(screen),
            None,
            Some(&size),
            Some(surf.mdc),
            Some(&src),
            COLORREF(0),
            Some(&bf),
            ULW_ALPHA,
        );
        let _ = ReleaseDC(None, screen);
    }
}

/// 渲染一个分区：每次都**从空白画布整张重建**预乘 ARGB 位图再提交。
///
/// 这是消除「显示 / 拖动重影」的关键：旧实现直接往窗口 DC 上画，
/// 而 GDI 无法把像素写回 alpha=0，旧内容永远擦不掉（改名字叠字、
/// 拖动缩放靠系统位图搬运 → 阶梯状残影）。整张重建后，位图里只可能
/// 有本次绘制的内容。
fn render(hwnd: HWND, idx: usize) {
    let (w, h) = client_size(hwnd);
    if w <= 0 || h <= 0 {
        return;
    }
    let mut surf = match Surf::new(w, h) {
        Some(s) => s,
        None => return,
    };
    let (name, color, collapsed, items) = crate::app::with_cfg(|c| {
        c.zones
            .get(idx)
            .map(|z| {
                (
                    z.name.clone(),
                    zone_rgb(&z.color),
                    z.collapsed,
                    z.items.clone(),
                )
            })
            .unwrap_or_else(|| ("围栏".to_string(), 0x4E8FFF, false, Vec::new()))
    });

    // ① 面板底色（四角 2x2 超采样，边缘不发锯齿）
    {
        let buf = surf.pixels();
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as usize;
                let cv = corner_cover(x, y, w, h, RADIUS);
                if cv == 0 {
                    buf[i] = 0;
                } else {
                    put_px(buf, i, PANEL_RGB, PANEL_A * cv / 4);
                }
            }
        }
        // ② 标题栏：不透明深色
        for y in 0..TITLE_H.min(h) {
            for x in 0..w {
                let cv = corner_cover(x, y, w, h, RADIUS);
                if cv == 0 {
                    continue;
                }
                let i = (y * w + x) as usize;
                put_px(buf, i, TITLE_RGB, 255 * cv / 4);
            }
        }
    }

    // ③ 记下底色快照：稍后据此判断哪些像素是 GDI 画上去的
    let base: Vec<u32> = surf.pixels().to_vec();
    let hdc = surf.mdc;

    unsafe {
        // 标题栏底部 1px 主题色描边
        let line_pen = CreatePen(PS_SOLID, 1, COLORREF(color));
        let old_pen = SelectObject(hdc, line_pen.into());
        let _ = MoveToEx(hdc, 0, TITLE_H - 1, None);
        let _ = LineTo(hdc, w, TITLE_H - 1);
        let _ = SelectObject(hdc, old_pen);
        let _ = DeleteObject(line_pen.into());

        // 标题文字
        let font_bold = CreateFontW(
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
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            w!("Microsoft YaHei UI"),
        );
        let old_font = SelectObject(hdc, font_bold.into());
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(0xF0F4F2));
        let mut tr = RECT {
            left: 12,
            top: 0,
            right: w - 38,
            bottom: TITLE_H,
        };
        let mut title_w = wide_nul(&name);
        let _ = DrawTextW(
            hdc,
            &mut title_w,
            &mut tr,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        // 折叠箭头
        draw_chevron(hdc, w - 21, TITLE_H / 2, !collapsed, COLORREF(0xC8D2CE));

        if !collapsed {
            let cols = ((w - 2 * PAD) / CELL_W).max(1);
            let font_small = CreateFontW(
                -12,
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
                ANTIALIASED_QUALITY,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Microsoft YaHei UI"),
            );
            let old_small = SelectObject(hdc, font_small.into());
            let _ = SetTextColor(hdc, COLORREF(0xE6ECE9));

            for (i, it) in items.iter().enumerate() {
                let col = i as i32 % cols;
                let row = i as i32 / cols;
                let cx = PAD + col * CELL_W;
                let cy = TITLE_H + PAD + row * CELL_H;
                if cy + CELL_H > h + 4 {
                    break;
                }
                let ic = icon_of(it);
                if ic != 0 {
                    let _ = DrawIconEx(
                        hdc,
                        cx + (CELL_W - ICON_SZ) / 2,
                        cy,
                        HICON(ic as *mut c_void),
                        ICON_SZ,
                        ICON_SZ,
                        0,
                        None,
                        DI_NORMAL,
                    );
                }
                let mut nr = RECT {
                    left: cx + 3,
                    top: cy + ICON_SZ + 4,
                    right: cx + CELL_W - 3,
                    bottom: cy + CELL_H,
                };
                let mut nw = wide_nul(it);
                let _ = DrawTextW(
                    hdc,
                    &mut nw,
                    &mut nr,
                    DT_CENTER | DT_WORDBREAK | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
            }
            let _ = SelectObject(hdc, old_small);
            let _ = DeleteObject(font_small.into());

            if items.is_empty() {
                let _ = SetTextColor(hdc, COLORREF(0x9BA8A4));
                let mut er = RECT {
                    left: PAD,
                    top: TITLE_H + PAD,
                    right: w - PAD,
                    bottom: h - PAD,
                };
                let mut ew = wide_nul("把图标拖到这里");
                let _ = DrawTextW(
                    hdc,
                    &mut ew,
                    &mut er,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
            }
        }

        let _ = SelectObject(hdc, old_font);
        let _ = DeleteObject(font_bold.into());
    }

    // ④ GDI 画上去的像素 alpha 提到 255。
    //    面板底色是半透明且已预乘，而 GDI 写入的是未预乘实色 ——
    //    若不提 alpha，图标和文字会被再乘一次 PANEL_A 而发灰。
    {
        let buf = surf.pixels();
        for i in 0..(w * h) as usize {
            if buf[i] != base[i] {
                buf[i] = 0xFF00_0000 | (buf[i] & 0x00FF_FFFF);
            }
        }
    }

    commit_layered(hwnd, &surf, w, h);
}
