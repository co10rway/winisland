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
//! - 分区窗口尺寸随图标数量**自适应扩展**（增高→加列），保证所有图标都画得下、
//!   不再有「塞进面板但超出窗口高度而被截断」的遗漏。
//! - 双击图标用 ShellExecute 打开；**按住面板内图标拖到另一个分区**可移动归属
//!   （带跟随光标的浮窗预览 + 落点分区高亮）；把文件从资源管理器拖到面板上
//!   即归入该分区（DragAcceptFiles + WM_DROPFILES）。
//! - 标题栏拖动移动、四边四角缩放、右上角箭头折叠 / 展开、右键菜单改名 / 换色 / 删除。
//! - 位置 / 大小 / 折叠 / 归入项持久化到 config.json。退出时恢复 Explorer 图标层。
//!
//! 全部数据仅存本机 config.json，不联网。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, IContextMenu, IShellFolder, ILCombine,
    SHBindToParent, SHChangeNotifyDeregister, SHChangeNotifyEntry, SHChangeNotifyRegister,
    SHGetDesktopFolder, SHGetNameFromIDList, SHGetSpecialFolderLocation, SHGetFileInfoW,
    SHParseDisplayName, ShellExecuteExW, ShellExecuteW,
    CMINVOKECOMMANDINFO, CMF_NORMAL, CSIDL_DESKTOP, HDROP, SHCNE_ALLEVENTS, SHCNRF_InterruptLevel,
    SHCNRF_ShellLevel, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_PIDL,
    SEE_MASK_IDLIST, SHELLEXECUTEINFOW, SHCONTF_FOLDERS, SHCONTF_NONFOLDERS, SIGDN_FILESYSPATH,
    SIGDN_NORMALDISPLAY,
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
/// 标题栏底色（RGB）：比面板更沉，接近参考图
const TITLE_RGB: (u32, u32, u32) = (20, 28, 26);
/// 透明度三档（配置存的是实际 alpha）
const ALPHA_STEPS: [(&str, u32); 3] = [("更透", 110), ("标准", 150), ("更实", 205)];
/// 分区主题色 ID 列表（右键菜单换色用）
const COLOR_IDS: [&str; 7] = ["blue", "teal", "green", "purple", "pink", "orange", "red"];
/// 拖拽跟随浮窗类名
const GHOST_CLASS: PCWSTR = w!("WinIslandZoneGhost");
/// 拖拽浮窗尺寸
const GHOST_W: i32 = 100;
const GHOST_H: i32 = 100;

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
    static SIGN: RefCell<String> = const { RefCell::new(String::new()) };
    /// 正在拖拽的面板内图标（跨分区移动归属）
    static IDRAG: RefCell<Option<ItemDrag>> = const { RefCell::new(None) };
    /// 全局鼠标钩子捕获的「从分区图标起拖」状态。
    /// 分区窗口是否收到鼠标消息不可靠（分层窗口 + owner 关系 + 捕获），
    /// 钩子能保证收到每一次物理按下/松开，用它兜底把图标拖到灵动岛的入站走通。
    static HOOK_DRAG: RefCell<Option<HookDrag>> = const { RefCell::new(None) };
    /// 钩子拖拽当前是否处于「悬停在灵动岛上」——用于只在状态翻转时投递
    /// 进入/离开消息，避免每次鼠标移动都骚扰主线程。
    static HOOK_HOVER: Cell<bool> = const { Cell::new(false) };
    /// 拖拽跟随浮窗句柄（0 = 未创建）
    static GHOST_HW: Cell<isize> = const { Cell::new(0) };
    static GHOST_CLASSED: Cell<bool> = const { Cell::new(false) };
    /// 拖拽悬停的目标分区（高亮描边）；None = 不在任何分区上
    static GTARGET: Cell<Option<usize>> = const { Cell::new(None) };
    /// 每个分区的竖向滚动行偏移（与 config.zones 同序）
    static SCROLL: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
    /// 滚轮全局钩子句柄（0 = 未安装）
    static HOOK: Cell<isize> = const { Cell::new(0) };
    /// 滚轮增量累加器：触控板一次滚动往往不足一格（120），累加后再翻行
    static WACC: Cell<i32> = const { Cell::new(0) };
    /// Shell 变更通知（桌面增删改）注册到的消息号，0 = 未注册
    static CHG_MSG: Cell<u32> = const { Cell::new(0) };
    /// SHChangeNotifyRegister 返回的注册 id，0 = 未注册
    static CHG_REG: Cell<u32> = const { Cell::new(0) };
    /// 变更通知当前挂在哪个窗口上（窗口重建后需要重新挂）
    static CHG_HWND: Cell<isize> = const { Cell::new(0) };
    /// 右键菜单预热是否已做过（避免每次都付出外壳扩展加载时间）
    static WARMED: Cell<bool> = const { Cell::new(false) };
}

/// 一次面板内图标拖拽：来源分区、拖的项、光标位置、是否真的移动过
struct ItemDrag {
    from: usize,
    name: String,
    last: POINT,
    moved: bool,
}

/// 全局钩子口径的拖拽：按下时命中分区图标则记录起点与项名，
/// 松开时若移动过且落点在主面板矩形内 → 把该文件推入文件中转站。
struct HookDrag {
    /// 按下时的屏幕坐标（判断是否真的拖动过）
    start: POINT,
    /// 被拖的项名（用于从 ITEMS 取真实路径）
    name: String,
}

#[derive(Clone)]
struct DeskItem {
    name: String,
    path: PathBuf,
    /// Shell 虚拟项（此电脑/回收站/网络等）的 PIDL 字节副本；物理文件为 None。
    /// 虚拟项磁盘上没有真实路径，path 只是用于去重/分类的伪路径，
    /// 图标/打开/右键菜单都按 pidl 走 Shell API。
    pidl: Option<Vec<u8>>,
}

/// 计算一个（可能多段的）绝对 PIDL 的总字节长度，用于复制保存。
/// PIDL 由若干 SHITEMID 首尾相接，每段前 2 字节是 cb（含自身），cb=0 为终止符。
unsafe fn pidl_len(pidl: *const ITEMIDLIST) -> usize {
    let mut p = pidl as *const u8;
    let mut total = 0usize;
    loop {
        let cb = u16::from_ne_bytes(std::ptr::read_unaligned(p as *const [u8; 2])) as usize;
        if cb == 0 {
            // 终止 SHITEMID 占 2 字节
            return total + 2;
        }
        total += cb;
        p = p.add(cb);
    }
}

/// 把 PIDL 复制成可长期持有的字节（枚举器返回的 PIDL 在下次 Next 前失效）。
unsafe fn clone_pidl(pidl: *const ITEMIDLIST) -> Vec<u8> {
    let n = pidl_len(pidl);
    std::slice::from_raw_parts(pidl as *const u8, n).to_vec()
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

/// 当前面板不透明度（右键菜单三档可调，范围夹到 80-230 保证文字可读）
fn panel_alpha() -> u32 {
    crate::app::with_cfg(|c| c.zone_alpha).clamp(80, 230)
}

/// 标题栏不透明度：比面板实一些，但同样保留壁纸透视
fn title_alpha() -> u32 {
    (panel_alpha() + 75).min(250)
}

/// 转 NUL 结尾 UTF-16（所有 LPCWSTR 参数必须 NUL 结尾，否则读越界乱码）
fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 读取 Shell 返回的 NUL 结尾 UTF-16 字符串（调用方负责 CoTaskMemFree）
unsafe fn pwstr_string(p: windows::core::PWSTR) -> String {
    if p.0.is_null() {
        return String::new();
    }
    let mut n = 0usize;
    while *p.0.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p.0, n))
}

/// 把分区的 owner 设为桌面图标宿主 SHELLDLL_DefView。
///
/// Win+D 不是最小化窗口，而是把桌面窗口提到 z 序最前；分区这种无 owner 的
/// 顶层窗口就被盖住。Windows 规则是「owned 窗口永远位于 owner 之上」，
/// 所以把 owner 设成 DefView 后，桌面被提到前面时分区会随之上来，
/// 无需任何轮询、置顶切换或反复重绘（那会把桌面刷得不停闪）。
/// 分区仍不是 topmost，普通窗口正常压在它上面。
unsafe fn own_by_desktop(hw: HWND) {
    let (_, def) = shell();
    if !def.0.is_null() {
        let _ = SetWindowLongPtrW(hw, GWLP_HWNDPARENT, def.0 as isize);
    }
}

/// 拖拽坐标统一取系统光标位置。
///
/// 不能用 lParam 的 client 坐标 + ClientToScreen 反推：捕获鼠标、窗口被
/// MoveWindow 挪动、以及多显示器不同 DPI 时，反推出来的屏幕坐标会和真实
/// 光标有偏差，表现就是「鼠标在这里，拖动的文件/浮窗却偏在别处」。
/// GetCursorPos 是唯一的权威屏幕坐标源。
unsafe fn cursor_screen() -> POINT {
    let mut p = POINT::default();
    if GetCursorPos(&mut p).is_err() {
        // 极端情况退回 lParam 无法在此函数内处理，返回 0 让调用方忽略本次移动
        p = POINT { x: 0, y: 0 };
    }
    p
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
        let mut def = FindWindowExW(Some(prog), None, w!("SHELLDLL_DefView"), None)
            .unwrap_or_default();
        if def.0.is_null() {
            let mut ww = FindWindowExW(None, None, w!("WorkerW"), None).unwrap_or_default();
            let mut guard = 0u32;
            while !ww.0.is_null() && guard < 64 {
                let inside = FindWindowExW(Some(ww), None, w!("SHELLDLL_DefView"), None)
                    .unwrap_or_default();
                if !inside.0.is_null() {
                    def = inside;
                    break;
                }
                ww = FindWindowExW(None, Some(ww), w!("WorkerW"), None).unwrap_or_default();
                guard += 1;
            }
        }
        if def.0.is_null() {
            return (HWND::default(), HWND::default());
        }
        let layer = GetParent(def).unwrap_or_default();
        (layer, def)
    }
}

/// Explorer 桌面图标列表窗口（SysListView32）。
///
/// 关键：**绝不能隐藏 SHELLDLL_DefView 本身**——Win10/11 的壁纸由 DefView 自己绘制，
/// 隐藏它会把壁纸一起隐藏，整块桌面变成纯黑（实测已确认，v0.17.3 曾因兜底分支
/// 误隐藏 DefView 导致用户「运行后屏幕黑了」）。图标实际画在 DefView 的子窗口
/// SysListView32 里，只隐藏这一层，壁纸才能留着。
///
/// 因此这里**找不到就返回 NULL**，由调用方保持不动，宁可与真实图标并存，
/// 也不允许退化成隐藏 DefView。
fn desktop_icon_view() -> HWND {
    let (_, def) = shell();
    if def.0.is_null() {
        return HWND::default();
    }
    unsafe {
        // Win11 不同 Build 的层级不完全一样，直接子窗口找不到时向下递归几层
        find_child_by_class(def, "SysListView32", 0)
    }
}

/// 深度优先查找指定类名的子窗口（限深限宽，避免在 Explorer 窗口树上失控）。
unsafe fn find_child_by_class(parent: HWND, name: &str, depth: u32) -> HWND {
    if depth > 4 {
        return HWND::default();
    }
    let mut cur = GetWindow(parent, GW_CHILD).unwrap_or_default();
    let mut guard = 0u32;
    while !cur.0.is_null() && guard < 128 {
        let mut buf = [0u16; 128];
        let n = GetClassNameW(cur, &mut buf);
        if n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == name {
            return cur;
        }
        let deeper = find_child_by_class(cur, name, depth + 1);
        if !deeper.0.is_null() {
            return deeper;
        }
        cur = GetWindow(cur, GW_HWNDNEXT).unwrap_or_default();
        guard += 1;
    }
    HWND::default()
}

/// 隐藏 / 显示 Explorer 桌面图标（图标由本模块托管，壁纸保持可见）
fn set_shell_icons_visible(show: bool) {
    let h = desktop_icon_view();
    let (_, def) = shell();
    unsafe {
        if !def.0.is_null() {
            // 防御性修复：历史版本可能误把 DefView 隐藏过，导致桌面纯黑。
            // 每次都先把壁纸层拉回来（幂等：已可见时 SW_SHOW 是空操作）。
            // 这样即使用户的桌面此刻是黑的，启动一次本版本也会自动恢复。
            let _ = ShowWindow(def, SW_SHOW);
        }
    }
    if h.0.is_null() || h == def {
        // 没找到图标列表窗口：保持原样，绝不动 DefView（否则壁纸一起消失）
        crate::app::diag_log(&format!(
            "ICON_HIDE skip show={} reason=no_SysListView32 def_null={}",
            show,
            def.0.is_null()
        ));
        return;
    }
    unsafe {
        let _ = ShowWindow(h, if show { SW_SHOW } else { SW_HIDE });
        let vis = IsWindowVisible(h).as_bool();
        crate::app::diag_log(&format!(
            "ICON_HIDE show={} still_visible={} zones={}",
            show,
            vis,
            WIN.with(|v| v.borrow().iter().filter(|x| x.is_some()).count())
        ));
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
mod zones_surf;
use zones_surf::{Surf, blend_px, corner_cover, put_px};

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
            hIcon: crate::modules::tray::app_icon(),
            hbrBackground: std::mem::transmute::<
                windows::Win32::Graphics::Gdi::HGDIOBJ,
                windows::Win32::Graphics::Gdi::HBRUSH,
            >(GetStockObject(NULL_BRUSH)),
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
            hIcon: crate::modules::tray::app_icon(),
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
    // 同名去重（用户桌面与公共桌面常有同名快捷方式）：先入者（用户桌面）优先
    let mut seen: HashSet<String> = HashSet::new();
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
                // Office 打开文档时生成的「所有者锁文件」（~$xxx.docx / ~$xxx.xlsx），
                // 以及其它隐藏文件：资源管理器默认不显示，这里也一并不显示。
                if name.starts_with("~$") {
                    continue;
                }
                {
                    use std::os::windows::fs::MetadataExt;
                    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
                    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
                    match e.metadata() {
                        Ok(md) => {
                            let attrs = md.file_attributes();
                            if attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0 {
                                continue;
                            }
                        }
                        Err(_) => continue,
                    }
                }
                if !seen.insert(name.clone()) {
                    continue;
                }
                let p = e.path();
                out.push(DeskItem { name, path: p, pidl: None });
            }
        }
    }

    // Shell 桌面命名空间虚拟项：此电脑 / 回收站 / 网络 / 控制面板等，
    // 磁盘上没有真实文件，只能通过 IShellFolder::EnumObjects 枚举。
    // 物理项已在上面收录，这里只补名字去重的、确实不在物理集合里的虚拟项。
    unsafe {
        // 桌面绝对 PIDL：EnumObjects 返回的是相对桌面的子 PIDL，
        // 而 SHGetFileInfoW(SHGFI_PIDL)/ShellExecuteEx/SHBindToParent 都要绝对 PIDL，
        // 所以这里一次合成绝对 PIDL 保存，下游统一按绝对 PIDL 处理。
        let desktop_abs = SHGetSpecialFolderLocation(None, CSIDL_DESKTOP as i32).ok();
        if let Ok(desktop) = SHGetDesktopFolder() {
            let mut enum_idl: Option<windows::Win32::UI::Shell::IEnumIDList> = None;
            let hr = desktop.EnumObjects(
                HWND::default(),
                (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0) as u32,
                &mut enum_idl,
            );
            if hr.is_ok() {
                if let Some(en) = enum_idl {
                    loop {
                        let mut one: [*mut ITEMIDLIST; 1] = [std::ptr::null_mut()];
                        let mut got = 0u32;
                        if en.Next(&mut one, Some(&mut got)).is_err() || got == 0 {
                            break;
                        }
                        let child = one[0];
                        if child.is_null() {
                            break;
                        }
                        // 跳过物理项：能拿到真实文件系统路径的说明是普通文件/快捷方式，
                        // 已由上面的 read_dir 收录。Shell 的显示名可能去掉 .lnk 扩展名，
                        // 与文件名不一致会导致去重失效、图标重复，故按「有无真实路径」区分。
                        let is_physical = match SHGetNameFromIDList(child, SIGDN_FILESYSPATH) {
                            Ok(p) => {
                                let s = pwstr_string(p);
                                CoTaskMemFree(Some(p.0 as *const c_void));
                                !s.is_empty()
                            }
                            Err(_) => false,
                        };
                        if !is_physical {
                            // 虚拟项：显示名（本地化）此电脑 / 回收站 / …
                            if let Ok(namep) = SHGetNameFromIDList(child, SIGDN_NORMALDISPLAY) {
                                let name = pwstr_string(namep);
                                CoTaskMemFree(Some(namep.0 as *const c_void));
                                if !name.is_empty() && seen.insert(name.clone()) {
                                    // 合成绝对 PIDL 并复制保存
                                    let abs = match desktop_abs {
                                        Some(d) => ILCombine(Some(d), Some(child)),
                                        None => std::ptr::null_mut(),
                                    };
                                    if !abs.is_null() {
                                        let bytes = clone_pidl(abs);
                                        CoTaskMemFree(Some(abs as *const c_void));
                                        // 伪路径仅用于去重/分类显示，不指向真实文件
                                        let pseudo = PathBuf::from(format!("shell://{}", name));
                                        out.push(DeskItem { name, path: pseudo, pidl: Some(bytes) });
                                    }
                                }
                            }
                        }
                        CoTaskMemFree(Some(child as *const c_void));
                    }
                }
            }
            if let Some(d) = desktop_abs {
                CoTaskMemFree(Some(d as *const c_void));
            }
        }
    }

    out.sort_by_key(|a| a.name.to_lowercase());
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

/// 取某桌面项的 HICON（懒加载并缓存）。返回 0 表示失败。
/// 虚拟项用保存的 PIDL + SHGFI_PIDL 取图标；物理文件走路径。
fn icon_of(name: &str) -> isize {
    if let Some(v) = ICONS.with(|m| m.borrow().get(name).copied()) {
        return v;
    }
    let item = ITEMS.with(|v| {
        v.borrow()
            .iter()
            .find(|i| i.name == name)
            .cloned()
    });
    let Some(item) = item else { return 0 };
    let mut fi = SHFILEINFOW::default();
    let ok = unsafe {
        if let Some(pidl) = &item.pidl {
            // 虚拟项：pszPath 传 PIDL 指针，必须带 SHGFI_PIDL
            SHGetFileInfoW(
                PCWSTR(pidl.as_ptr() as *const u16),
                FILE_FLAGS_AND_ATTRIBUTES(0),
                Some(&mut fi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON | SHGFI_PIDL,
            )
        } else {
            let wide = wide_nul(&item.path.to_string_lossy());
            SHGetFileInfoW(
                PCWSTR(wide.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(0),
                Some(&mut fi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON,
            )
        }
    };
    let h = if ok != 0 { fi.hIcon.0 as isize } else { 0 };
    ICONS.with(|m| m.borrow_mut().insert(name.to_string(), h));
    h
}

fn open_item(name: &str) {
    let item = ITEMS.with(|v| {
        v.borrow()
            .iter()
            .find(|i| i.name == name)
            .cloned()
    });
    let Some(item) = item else { return };
    unsafe {
        if let Some(abs_bytes) = &item.pidl {
            // 虚拟项：直接把保存的绝对 PIDL 交给 ShellExecuteEx 的 lpIDList
            let mut abs = abs_bytes.clone();
            let mut sei = SHELLEXECUTEINFOW {
                cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
                fMask: SEE_MASK_IDLIST,
                hwnd: HWND::default(),
                lpVerb: w!("open"),
                lpFile: PCWSTR::null(),
                lpParameters: PCWSTR::null(),
                lpDirectory: PCWSTR::null(),
                nShow: SW_SHOWNORMAL.0,
                hInstApp: std::mem::zeroed(),
                lpIDList: abs.as_mut_ptr() as *mut c_void,
                lpClass: PCWSTR::null(),
                hkeyClass: std::mem::zeroed(),
                dwHotKey: 0,
                Anonymous: std::mem::zeroed(),
                hProcess: std::mem::zeroed(),
            };
            let _ = ShellExecuteExW(&mut sei);
        } else {
            let wide = wide_nul(&item.path.to_string_lossy());
            let _ = ShellExecuteW(
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
        // 挂到桌面 DefView 名下：显示桌面时随桌面一起浮上来，无需轮询
        own_by_desktop(hw);
        DragAcceptFiles(hw, true);
        // layered 窗口必须提交过位图才可见，先渲染一次
        render(hw, idx);
        let _ = ShowWindow(
            hw,
            if z.visible {
                SW_SHOWNOACTIVATE
            } else {
                SW_HIDE
            },
        );
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
    // 旧版分类（文档/应用/其他，或全空面板）一次性升级到完整分类；
    // 升级后置 zones_migrated，之后不再重置用户已调整过的分区。
    let stale = crate::app::with_cfg(|c| {
        !c.zones.is_empty()
            && !c.zones_migrated
            && c.zones.iter().all(|z| {
                z.items.is_empty()
                    || matches!(
                        z.name.as_str(),
                        "文档" | "应用" | "其他" | "文件夹"
                    )
            })
    });
    if stale && !ITEMS.with(|v| v.borrow().is_empty()) {
        crate::app::with_cfg(|c| {
            c.zones.clear();
            c.zones_migrated = true;
        });
    }
    if crate::app::with_cfg(|c| c.zones.is_empty()) {
        seed_if_needed();
    }
    reconcile_items();
    fit_zones();
    crate::app::save_now();
    let zones = crate::app::with_cfg(|c| c.zones.clone());
    // 只在真有条目可见的分区时才接管图标（全部隐藏时把图标还给桌面）
    let any_visible = zones.iter().any(|z| z.visible && !z.items.is_empty());
    set_shell_icons_visible(!any_visible);
    // 内容超出可视区时靠滚轮翻页（分区不抢焦点，需要全局滚轮钩子）
    install_wheel_hook();
    SCROLL.with(|s| s.borrow_mut().resize(zones.len(), 0));

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
                    // Explorer 重启会重建桌面层，旧句柄随之失效；这里补建，
                    // 否则 sync() 只会往死句柄上 SetWindowPos，分区永远回不来。
                    if !IsWindow(Some(hw)).as_bool() {
                        v[i] = None;
                        if z.visible {
                            v[i] = create_zone(z, i);
                        }
                        continue;
                    }
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

    // 定时兜底 + 桌面变更通知挂在**第一个可用分区窗口**上。
    // 旧实现固定挂在 0 号分区：0 号一旦被隐藏/删除，轮询和通知就全停了。
    WIN.with(|ws| {
        let v = ws.borrow();
        let mut anchor: Option<HWND> = None;
        for hw in v.iter().flatten() {
            unsafe {
                let _ = KillTimer(Some(*hw), 1);
                let _ = KillTimer(Some(*hw), 2);
            }
            if anchor.is_none() {
                anchor = Some(*hw);
            }
        }
        match anchor {
            Some(a) => {
                // 1.2 秒兜底轮询：变更通知万一没投递（Explorer 重启 / 通知被过滤），
                // 最多 1.2 秒内也会自动同步；正常情况是通知先到（240ms 内）
                unsafe {
                    SetTimer(Some(a), 1, 1200, None);
                }
                install_change_notify(a);
            }
            None => uninstall_change_notify(),
        }
    });
    prewarm_context_menu();
    // 分区增删 / 显隐 / 位置刚变完，让灵动岛立刻重判遮挡
    crate::app::occ_poke();
}

/// 低频兜底：只在「配置要显示、窗口却真没了/被最小化」时动一下。
///
/// 「显示桌面」本身已由 owner=DefView 在窗口创建时解决（owned 窗口永远在
/// owner 之上），这里**不做** z 序比较、置顶切换和周期性重绘——上一版那种
/// 250ms 看门狗会让桌面不停刷新，体验很差。这里只处理 Explorer 重启导致
/// 句柄失效（owner 被销毁时系统会自动销毁 owned 窗口）等真正异常。
fn restore_hidden_zones() {
    let wants: Vec<bool> = crate::app::with_cfg(|c| {
        c.zones.iter().map(|z| z.visible).collect()
    });
    let mut dead = false;
    WIN.with(|ws| {
        let v = ws.borrow();
        for (i, hw) in v.iter().enumerate() {
            let Some(hw) = *hw else { continue };
            if !wants.get(i).copied().unwrap_or(false) {
                continue; // 用户主动隐藏的分区，保持隐藏
            }
            unsafe {
                if !IsWindow(Some(hw)).as_bool() {
                    dead = true;
                    continue;
                }
                if IsIconic(hw).as_bool() || !IsWindowVisible(hw).as_bool() {
                    // SW_RESTORE 同时解除最小化；不激活，避免抢走当前窗口焦点
                    let _ = ShowWindow(hw, SW_RESTORE);
                    render(hw, i);
                }
            }
        }
    });
    if dead {
        sync();
    }
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
    uninstall_wheel_hook();
    uninstall_change_notify();
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

/// 某个桌面项的分类（按扩展名，文件夹单独一类）
fn item_cat(it: &DeskItem) -> &'static str {
    // 系统虚拟项（此电脑/回收站等）归「应用」
    if it.pidl.is_some() {
        return "应用";
    }
    let ext = it
        .path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    classify(&ext, it.path.is_dir())
}

/// 清理失效项；把未归入任何分区的桌面项**按类型自动收进对应围栏**。
///
/// 这是「实时归档」的落地环节：桌面上新建 / 复制 / 下载的文件，经 Shell 变更
/// 通知（或兜底轮询）发现后，会按扩展名落到同名围栏（图片→「图片」、新建的
/// 快捷方式→「应用」…）；没有对应围栏的，统一进「其他」，保证任何图标都不会
/// 因为没归属而消失。
fn reconcile_items() {
    let names: Vec<String> = ITEMS.with(|v| v.borrow().iter().map(|i| i.name.clone()).collect());
    let nameset: HashSet<String> = names.iter().cloned().collect();
    // 名称 → 分类，供新文件自动归档
    let cats: HashMap<String, &'static str> = ITEMS.with(|v| {
        v.borrow()
            .iter()
            .map(|i| (i.name.clone(), item_cat(i)))
            .collect()
    });
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
        if names.iter().all(|n| used.contains(n)) {
            return;
        }
        // 按分类归堆：有同名围栏就进去，否则攒到「其他」
        let mut by_cat: Vec<(&'static str, Vec<String>)> = Vec::new();
        for n in names.iter().filter(|n| !used.contains(*n)) {
            let cat = cats.get(n).copied().unwrap_or("其他");
            match by_cat.iter_mut().find(|(c, _)| *c == cat) {
                Some((_, v)) => v.push(n.clone()),
                None => by_cat.push((cat, vec![n.clone()])),
            }
        }
        let mut to_other: Vec<String> = Vec::new();
        for (cat, list) in by_cat {
            match c.zones.iter().position(|z| z.name == cat) {
                Some(pi) => c.zones[pi].items.extend(list),
                None => to_other.extend(list),
            }
        }
        if to_other.is_empty() {
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
                items: to_other,
            });
        } else {
            // 有「其他」围栏就并进去；用户删掉了就挑最后一个围栏收着，绝不丢图标
            let target = c
                .zones
                .iter()
                .position(|z| z.name == "其他")
                .unwrap_or(c.zones.len() - 1);
            c.zones[target].items.extend(to_other);
        }
    });
}

/// 扩展名 → 分类名（首次自动分区用，对齐参考产品的完整类别）
fn classify(ext: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "文件夹";
    }
    match ext {
        // 应用与快捷方式
        "lnk" | "url" | "exe" | "msi" | "appx" | "msix" => "应用",
        // 文档
        "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "pdf" | "txt" | "md" | "csv"
        | "rtf" | "one" | "odt" | "ods" | "odp" | "epub" | "mobi" | "azw3" | "xmind" => "文档",
        // 图片
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tif" | "tiff"
        | "heic" | "psd" | "ai" | "xd" | "fig" | "sketch" | "raw" | "cr2" | "nef" => "图片",
        // 视频
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "rmvb" | "rm" | "3gp" => "视频",
        // 音频
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "ape" | "opus" | "mid" => "音乐",
        // 压缩包 / 镜像
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "iso" | "cab" => "压缩包",
        // 代码 / 脚本
        "rs" | "py" | "js" | "ts" | "jsx" | "tsx" | "java" | "c" | "h" | "cpp" | "hpp"
        | "go" | "rb" | "php" | "html" | "css" | "scss" | "json" | "xml" | "yml" | "yaml"
        | "toml" | "sh" | "bat" | "cmd" | "ps1" | "sql" | "ipynb" | "lua" | "swift" | "kt"
        | "dart" => "代码",
        _ => "其他",
    }
}

/// 首次启用：按扩展名把桌面项自动分到完整类别分区，两列排布。
fn seed_if_needed() {
    let items = ITEMS.with(|v| v.borrow().clone());
    let mut groups: Vec<(&'static str, Vec<String>)> = [
        ("应用", Vec::new()),
        ("文档", Vec::new()),
        ("图片", Vec::new()),
        ("视频", Vec::new()),
        ("音乐", Vec::new()),
        ("压缩包", Vec::new()),
        ("代码", Vec::new()),
        ("文件夹", Vec::new()),
        ("其他", Vec::new()),
    ]
    .into_iter()
    .collect();
    for it in &items {
        let cat = item_cat(it); // 虚拟项归「应用」，物理项按扩展名
        if let Some(g) = groups.iter_mut().find(|(n, _)| *n == cat) {
            g.1.push(it.name.clone());
        }
    }

    let sw = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let sh = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let w = 340;
    let gap = 18;
    let cols0 = ((w - 2 * PAD) / CELL_W).max(1);
    let color_of = |name: &str| -> &'static str {
        match name {
            "应用" => "teal",
            "文档" => "blue",
            "图片" => "pink",
            "视频" => "purple",
            "音乐" => "orange",
            "压缩包" => "green",
            "代码" => "red",
            "文件夹" => "green",
            _ => "blue",
        }
    };

    // 先建内容（空类别不建面板），再 3 列网格排布。
    // 每块最多显示 3 行图标，更多的靠滚轮翻页 —— 否则 9 块分区会把壁纸铺满。
    let chrome = TITLE_H + PAD * 2;
    let mut zones: Vec<crate::config::ZoneConf> = Vec::new();
    for (name, list) in groups {
        if list.is_empty() {
            continue;
        }
        let rows = ((list.len() as i32 + cols0 - 1) / cols0).max(1);
        let h = chrome + rows.min(3) * CELL_H;
        zones.push(crate::config::ZoneConf {
            name: name.to_string(),
            x: 0,
            y: 0,
            w,
            h,
            color: color_of(name).to_string(),
            visible: true,
            collapsed: false,
            items: list,
        });
    }
    let bx = (sw as f32 * 0.04) as i32;
    let by = (sh as f32 * 0.07) as i32;
    let per_row = 3usize;
    let mut y = by;
    let mut i = 0usize;
    while i < zones.len() {
        let row_len = (zones.len() - i).min(per_row);
        let row_h = zones[i..i + row_len].iter().map(|z| z.h).max().unwrap_or(200);
        for j in 0..row_len {
            zones[i + j].x = bx + j as i32 * (w + gap);
            zones[i + j].y = y;
        }
        y += row_h + gap;
        i += row_len;
    }
    if zones.is_empty() {
        zones.push(crate::config::ZoneConf {
            name: "新围栏".into(),
            x: (sw as f32 * 0.04) as i32,
            y: (sh as f32 * 0.12) as i32,
            w,
            h: 210,
            color: "blue".into(),
            visible: true,
            collapsed: false,
            items: Vec::new(),
        });
    }
    crate::app::with_cfg(|c| {
        c.zones = zones;
        c.zones_migrated = true;
    });
    crate::app::save_now();
}

/// 保证分区窗口完整落在屏幕内、不小于最小尺寸。
///
/// 不再自动撑高：旧做法会把面板养到屏幕 88% 高、两列铺满桌面 →
/// 壁纸几乎被全覆盖（用户反馈「看不到壁纸」）。现在内容超出可视区
/// 改用**滚轮翻页**（见 `scroll_zone`），面板保持用户设定的紧凑尺寸。
fn fit_zones() {
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    // 桌面工作区（不含任务栏），避免分区压到任务栏上
    let mut wa = RECT::default();
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut wa as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
    };
    let (left, top, right, bottom) = if ok && wa.right > wa.left && wa.bottom > wa.top {
        (wa.left, wa.top, wa.right, wa.bottom)
    } else {
        (0, 0, sw, sh)
    };
    crate::app::with_cfg(|c| {
        // 上限：高度不超过屏高 55%、宽度不超过 8 列图标。
        // 超出部分靠滚轮翻页（0.13.0 曾自动撑高 / 加宽，旧配置里可能存着超大尺寸）
        let max_h = ((sh as f32 * 0.55) as i32).max(MIN_H);
        let max_w = (8 * CELL_W + 2 * PAD).max(MIN_W);
        for z in c.zones.iter_mut() {
            z.w = z.w.max(MIN_W);
            if !z.collapsed {
                z.h = z.h.max(MIN_H);
            }
            if z.h > max_h && !z.collapsed {
                z.h = max_h;
            }
            if z.w > max_w {
                z.w = max_w;
            }
            if z.x + z.w > right {
                z.x = (right - z.w).max(left);
            }
            if z.y + z.h > bottom {
                z.y = (bottom - z.h).max(top);
            }
            if z.x < left {
                z.x = left;
            }
            if z.y < top {
                z.y = top;
            }
        }
    });
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
    let (w, h) = client_size(hwnd);
    let cols = ((w - 2 * PAD) / CELL_W).max(1);
    // 必须和 render() 的排布逐行一致。scroll_info 的 first 是「行号」，
    // 滚动后第一个可见项的索引是 first*cols，不是 first。
    // 旧代码从 first 开始且把 (i-first) 当成列/行，滚动后点第 1 列图标
    // 会命中一个完全看不见的项 —— 表现就是「鼠标在这里，拖的却是别的文件」。
    let (first, _rows_vis, _max_first) = scroll_info(idx, w, h);
    for i in first * cols as usize..count {
        let row_all = i as i32 / cols;
        if row_all < first as i32 {
            continue;
        }
        let col = i as i32 % cols;
        let row = row_all - first as i32;
        let cx = PAD + col * CELL_W;
        let cy = TITLE_H + PAD + row * CELL_H;
        if cy + CELL_H > h + 4 {
            break;
        }
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
            // 折叠/展开改了高度：让灵动岛立刻重判是否被这块分区压住
            crate::app::occ_poke();
        }
    }
}

// ── 右键菜单 ──

const MENU_RENAME: u32 = 2001;
const MENU_DELETE: u32 = 2002;
const MENU_NEW: u32 = 2003;
const MENU_HIDE_ALL: u32 = 2004;
const MENU_COLLAPSE: u32 = 2006;
const MENU_COLOR_BASE: u32 = 2100;
const MENU_ALPHA_BASE: u32 = 2200;

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

        // 透明度三档（壁纸透视程度）
        let cur_alpha = panel_alpha();
        if let Ok(am) = CreatePopupMenu() {
            let nearest = ALPHA_STEPS
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, v))| (*v as i32 - cur_alpha as i32).abs())
                .map(|(i, _)| i)
                .unwrap_or(1);
            for (i, (label, _)) in ALPHA_STEPS.iter().enumerate() {
                let lz = wide_nul(label);
                let flags = if i == nearest {
                    MF_STRING | MF_CHECKED
                } else {
                    MF_STRING
                };
                let _ = AppendMenuW(
                    am,
                    flags,
                    (MENU_ALPHA_BASE + i as u32) as usize,
                    PCWSTR(lz.as_ptr()),
                );
            }
            let _ = AppendMenuW(menu, MF_POPUP, am.0 as usize, w!("透明度"));
        }
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_COLLAPSE as usize,
            if collapsed { w!("展开") } else { w!("折叠") },
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, MENU_NEW as usize, w!("新建围栏"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_HIDE_ALL as usize, w!("隐藏全部围栏"));
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

// ── 单个文件的系统右键菜单 ──

/// 弹出某个桌面项的 **Explorer 同款右键菜单**（打开 / 打开方式 / 压缩 / 属性 …）。
///
/// 用 Shell 的 `IContextMenu` 而不是自建菜单：这样第三方外壳扩展（7-Zip、Git、
/// 各种云盘同步）的菜单项也能原样出现，和资源管理器里右键完全一致。
/// 成功返回 true；失败（解析路径失败等）返回 false，调用方回退到围栏菜单。
fn file_menu(hwnd: HWND, name: &str) -> bool {
    let item = ITEMS.with(|v| {
        v.borrow().iter().find(|i| i.name == name).cloned()
    });
    let Some(item) = item else { return false };
    unsafe {
        // 虚拟项已有绝对 PIDL；物理项从路径解析绝对 PIDL。
        // 两者都交给 SHBindToParent 拆出 child + 父文件夹，走同一条 IContextMenu 链路。
        let owned_abs: Vec<u8> = if let Some(bytes) = &item.pidl {
            bytes.clone()
        } else {
            let wide = wide_nul(&item.path.to_string_lossy());
            let mut abs: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut abs, 0, None).is_err()
                || abs.is_null()
            {
                return false;
            }
            let copied = clone_pidl(abs);
            CoTaskMemFree(Some(abs as *const c_void));
            copied
        };

        let abs_ptr = owned_abs.as_ptr() as *const ITEMIDLIST;
        let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
        let folder = match SHBindToParent::<IShellFolder>(abs_ptr as *mut ITEMIDLIST, Some(&mut child)) {
            Ok(f) => f,
            Err(_) => return false,
        };
        // child 指向 owned_abs 内部，owned_abs 需活到函数末尾
        let apidl: [*const ITEMIDLIST; 1] = [child as *const ITEMIDLIST];
        let cm = match folder.GetUIObjectOf::<IContextMenu>(hwnd, &apidl, None) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return false,
        };
        let hr = cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL);
        let mut invoked = false;
        if hr.is_ok() {
            let mut pos = POINT::default();
            let _ = GetCursorPos(&mut pos);
            let _ = SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_RETURNCMD | TPM_RIGHTBUTTON,
                pos.x,
                pos.y,
                Some(0),
                hwnd,
                None,
            );
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let id = cmd.0 as u32;
            if id > 0 {
                // 菜单命令 id 从 1 开始，InvokeCommand 要传 id-1 的 MAKEINTRESOURCE
                let ici = CMINVOKECOMMANDINFO {
                    cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                    fMask: 0,
                    hwnd,
                    lpVerb: PCSTR((id - 1) as usize as *const u8),
                    lpParameters: PCSTR::null(),
                    lpDirectory: PCSTR::null(),
                    nShow: SW_SHOWNORMAL.0,
                    dwHotKey: 0,
                    hIcon: HANDLE::default(),
                };
                if cm.InvokeCommand(&ici).is_ok() {
                    invoked = true;
                }
            }
        }
        let _ = DestroyMenu(menu);
        // 菜单里可能做了重命名 / 删除 / 移动，刷新一次
        if invoked {
            refresh();
        }
        hr.is_ok()
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

// ── 面板内图标拖拽（跨分区移动） ──

unsafe extern "system" fn ghost_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn register_ghost_class() {
    if GHOST_CLASSED.with(|c| c.get()) {
        return;
    }
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(ghost_wndproc),
            hInstance: hinst.into(),
            lpszClassName: GHOST_CLASS,
            hbrBackground: std::mem::transmute::<
                windows::Win32::Graphics::Gdi::HGDIOBJ,
                windows::Win32::Graphics::Gdi::HBRUSH,
            >(GetStockObject(NULL_BRUSH)),
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
    GHOST_CLASSED.with(|c| c.set(true));
}

/// 懒创建拖拽跟随浮窗（透明点击穿透，不会挡住落点判定）
fn ensure_ghost() -> Option<HWND> {
    register_ghost_class();
    let g = GHOST_HW.with(|c| c.get());
    if g != 0 {
        return Some(HWND(g as *mut c_void));
    }
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let hw = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT
                | WS_EX_NOACTIVATE,
            GHOST_CLASS,
            w!(""),
            WS_POPUP,
            0,
            0,
            GHOST_W,
            GHOST_H,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .ok()?;
        if hw.0.is_null() {
            return None;
        }
        GHOST_HW.with(|c| c.set(hw.0 as isize));
        Some(hw)
    }
}

fn hide_ghost() {
    let g = GHOST_HW.with(|c| c.get());
    if g != 0 {
        unsafe {
            let _ = ShowWindow(HWND(g as *mut c_void), SW_HIDE);
        }
    }
}

/// 跟随光标的拖拽预览浮窗：小图标 + 名称。每次移动都整张重建 ARGB
/// 并用 UpdateLayeredWindow 的 pptDst 直接挪到光标旁（不调 SetWindowPos）。
fn render_ghost(name: &str, at: POINT) {
    let Some(hw) = ensure_ghost() else { return };
    let Some(mut surf) = Surf::new(GHOST_W, GHOST_H) else { return };
    let (w, h) = (GHOST_W, GHOST_H);
    {
        let buf = surf.pixels();
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as usize;
                let cv = corner_cover(x, y, w, h, 10);
                if cv == 0 {
                    buf[i] = 0;
                } else {
                    put_px(buf, i, TITLE_RGB, 235 * cv / 4);
                }
            }
        }
    }
    let base: Vec<u32> = surf.pixels().to_vec();
    let hdc = surf.mdc;
    unsafe {
        let font = CreateFontW(
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
            crate::render::ui_font_pcwstr(),
        );
        let old_font = SelectObject(hdc, font.into());
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(0xE6ECE9));
        let ic = icon_of(name);
        if ic != 0 {
            let _ = DrawIconEx(
                hdc,
                (w - 40) / 2,
                10,
                HICON(ic as *mut c_void),
                40,
                40,
                0,
                None,
                DI_NORMAL,
            );
        }
        let mut nr = RECT {
            left: 4,
            top: 56,
            right: w - 4,
            bottom: h - 4,
        };
        let mut nw = wide_nul(name);
        let _ = DrawTextW(
            hdc,
            &mut nw,
            &mut nr,
            DT_CENTER | DT_WORDBREAK | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        let _ = SelectObject(hdc, old_font);
        let _ = DeleteObject(font.into());
        let buf = surf.pixels();
        for i in 0..(w * h) as usize {
            if buf[i] != base[i] {
                buf[i] = 0xFF00_0000 | (buf[i] & 0x00FF_FFFF);
            }
        }
    }
    unsafe {
        let screen = GetDC(None);
        if screen.0.is_null() {
            return;
        }
        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        // 位置与内容分开提交：SetWindowPos 负责跟光标，UpdateLayeredWindow
        // 只换位图（pptDst=null）。之前把位置塞进 pptDst，窗口尚未显示时
        // 这条位置更新在部分系统版本上不生效，浮窗就停在初始 (0,0) 或
        // 旧位置，看起来跟鼠标完全不对齐。
        let _ = ShowWindow(hw, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(
            hw,
            None,
            at.x - (w - 40) / 2 - 20, // 图标绘制于 (30,10) 40x40，中心压在光标
            at.y - 10 - 20,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        let bf = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            hw,
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

/// 屏幕坐标处命中的分区窗口索引（拖拽落点判定）。
/// 浮窗带 WS_EX_TRANSPARENT，WindowFromPoint 会自动跳过它。
fn window_zone_at(pt: POINT) -> Option<usize> {
    // 几何判定（遍历分区窗口矩形），不用 WindowFromPoint。
    // 拖拽时鼠标被 SetCapture 捕获到源分区窗口，WindowFromPoint 会返回捕获
    // 它的那个窗口（即源分区），导致「拖到别的分区」被误判成「还在源分区」，
    // 跨分区移动、落点判定全部失效。几何判定不受捕获影响。
    let mut hit = None;
    WIN.with(|ws| {
        for (i, hw) in ws.borrow().iter().enumerate() {
            let Some(hw) = *hw else { continue };
            if !unsafe { IsWindowVisible(hw).as_bool() } {
                continue;
            }
            let mut rc = RECT::default();
            if unsafe { GetWindowRect(hw, &mut rc) }.is_err() {
                continue;
            }
            if pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom {
                hit = Some(i);
                break;
            }
        }
    });
    hit
}

/// 屏幕坐标是否落在主面板（灵动岛）窗口矩形内。
///
/// 不能用 WindowFromPoint + 类名：拖拽图标时鼠标被 SetCapture 捕获到分区窗口，
/// 捕获期间 WindowFromPoint 会返回捕获窗口本身，而不是光标下的主面板，
/// 于是落点判定永远失败（这正是"拖到灵动岛没反应"的根因）。
/// 直接按主面板 HWND 取窗口矩形做几何判定，不受鼠标捕获和透明分层影响。
/// 屏幕坐标点命中的分区图标项名（几何判定，遍历所有分区窗口）。
/// 全局钩子用它判断「按下处是不是分区里的某个图标」——不依赖分区窗口是否
/// 收到鼠标消息，也不受 SetCapture 影响。
/// 把单个文件路径投递给主面板的入站通道（复用 OLE 拖放用的 WM_DROP_FILES）。
/// 为什么不直接在钩子里调 push_file_external：钩子回调有超时限制，
/// 重绘/写盘放在里面会拖慢全局鼠标；投递到主线程做，最稳且复用已验证路径。
fn post_drop_file(path: String) -> bool {
    unsafe {
        let Ok(hw) = FindWindowW(w!("WinIsland"), None) else {
            return false;
        };
        if hw.0.is_null() {
            return false;
        }
        let boxed = Box::new(vec![path]);
        let ptr = Box::into_raw(boxed) as isize;
        if PostMessageW(Some(hw), crate::ui::WM_DROP_FILES, WPARAM(0), LPARAM(ptr)).is_err() {
            // 投递失败必须自己回收，否则泄漏
            drop(Box::from_raw(ptr as *mut Vec<String>));
            return false;
        }
        true
    }
}

fn zone_icon_at_screen(pt: POINT) -> Option<String> {
    let mut hit: Option<String> = None;
    WIN.with(|ws| {
        for (i, hw) in ws.borrow().iter().enumerate() {
            let Some(hw) = *hw else { continue };
            unsafe {
                let mut rc = RECT::default();
                if GetWindowRect(hw, &mut rc).is_err() {
                    continue;
                }
                if pt.x < rc.left || pt.x >= rc.right || pt.y < rc.top || pt.y >= rc.bottom {
                    continue;
                }
            }
            if let Some(k) = icon_at(hw, pt) {
                let name = crate::app::with_cfg(|c| {
                    c.zones.get(i).and_then(|z| z.items.get(k).cloned())
                });
                if let Some(n) = name {
                    hit = Some(n);
                    break;
                }
            }
        }
    });
    hit
}

fn window_is_main_panel(pt: POINT) -> bool {
    unsafe {
        let Ok(hw) = FindWindowW(w!("WinIsland"), None) else {
            return false;
        };
        if hw.0.is_null() {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(hw, &mut r).is_err() {
            return false;
        }
        // 放几像素容差，顶部小黑条/圆角边缘也能命中
        const T: i32 = 6;
        pt.x >= r.left - T && pt.x <= r.right + T && pt.y >= r.top - T && pt.y <= r.bottom + T
    }
}

fn zone_hwnd(i: usize) -> Option<HWND> {
    WIN.with(|v| v.borrow().get(i).copied().flatten())
}

// ── 滚轮滚动（内容超出可视区时翻页，避免把面板撑到满屏挡住壁纸） ──

fn scroll_of(idx: usize) -> i32 {
    SCROLL.with(|v| v.borrow().get(idx).copied().unwrap_or(0))
}

fn set_scroll(idx: usize, v: i32) {
    SCROLL.with(|s| {
        let mut b = s.borrow_mut();
        if b.len() <= idx {
            b.resize(idx + 1, 0);
        }
        b[idx] = v;
    });
}

/// 某分区的滚动状态：(起始行, 可见行数, 最大起始行)
fn scroll_info(idx: usize, w: i32, h: i32) -> (usize, usize, usize) {
    let count = crate::app::with_cfg(|c| c.zones.get(idx).map(|z| z.items.len()).unwrap_or(0));
    let cols = (((w - 2 * PAD) / CELL_W).max(1)) as usize;
    let rows_vis = (((h - TITLE_H - 2 * PAD) / CELL_H).max(1)) as usize;
    let total = (count + cols.max(1) - 1) / cols.max(1);
    let max_first = total.saturating_sub(rows_vis);
    let first = (scroll_of(idx).max(0) as usize).min(max_first);
    (first, rows_vis, max_first)
}

/// 滚动一个分区：notches > 0 = 滚轮向前（内容向上翻，起始行减小）
fn scroll_zone(idx: usize, notches: i32) {
    let Some(hw) = zone_hwnd(idx) else { return };
    let (w, h) = client_size(hw);
    let (_first, _vis, max_first) = scroll_info(idx, w, h);
    let cur = scroll_of(idx).max(0).min(max_first as i32);
    let ns = (cur - notches).clamp(0, max_first as i32);
    if ns != cur {
        set_scroll(idx, ns);
        render(hw, idx);
    }
}

/// 全局滚轮钩子：分区是 `WS_EX_NOACTIVATE`（不抢焦点），收不到 WM_MOUSEWHEEL，
/// 用低级鼠标钩子当光标落在分区上时把滚轮转给该分区；分区不可滚时放行。
unsafe extern "system" fn wheel_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 分区图标 → 灵动岛（文件中转站）的全局兜底。
    //
    // 为什么用全局钩子而不是分区窗口的 WM_LBUTTONDOWN/UP：
    // 实测 drag.log 里分区窗口的 ZONE_DOWN/UP/DROP 一次都没触发，说明按下事件
    // 并没有可靠地到达分区窗口的消息过程（分层窗口 + owner=DefView + 分层
    // 命中测试都可能影响）。低级鼠标钩子由系统在鼠标驱动层回调，不经过任何
    // 窗口的命中测试，能保证收到每一次物理按下/松开，用它兜底最稳。
    //
    // 钩子里只做「记录起点 / 判定落点」，真正的入站走 PostMessage 到主线程，
    // 避免在钩子回调里做重绘、写盘等重活拖慢全局鼠标响应。
    if code >= 0 && lparam.0 != 0 {
        let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        match wparam.0 as u32 {
            WM_LBUTTONDOWN => {
                HOOK_HOVER.with(|c| c.set(false));
                // 按下处若是分区里的某个图标，记下项名与起点，等松手判定
                if let Some(name) = zone_icon_at_screen(ms.pt) {
                    crate::app::diag_log(&format!(
                        "HK_DOWN name={} at=({},{})",
                        name, ms.pt.x, ms.pt.y
                    ));
                    HOOK_DRAG.with(|d| {
                        *d.borrow_mut() = Some(HookDrag {
                            start: ms.pt,
                            name,
                        })
                    });
                } else {
                    HOOK_DRAG.with(|d| *d.borrow_mut() = None);
                }
            }
            WM_MOUSEMOVE => {
                // 拖着分区图标经过灵动岛时，驱动与 OLE 拖放相同的「悬停提示态」：
                // 面板自动展开并显示「松开鼠标，暂存到中转站」。
                // 只在状态翻转时投递一次，避免每条鼠标移动消息都打扰主线程。
                if HOOK_DRAG.with(|d| d.borrow().is_some()) {
                    let inside = window_is_main_panel(ms.pt);
                    let was = HOOK_HOVER.with(|c| c.get());
                    if inside != was {
                        HOOK_HOVER.with(|c| c.set(inside));
                        crate::ui::post_drag_hover(inside);
                    }
                }
            }
            WM_LBUTTONUP => {
                // 松手：只有「真的拖动过」且落点在灵动岛内，才推入中转站
                let pending = HOOK_DRAG.with(|d| d.borrow_mut().take());
                let was_hover = HOOK_HOVER.with(|c| c.replace(false));
                if let Some(h) = pending {
                    let dx = (ms.pt.x - h.start.x).abs();
                    let dy = (ms.pt.y - h.start.y).abs();
                    let moved = dx > 6 || dy > 6;
                    let in_panel = window_is_main_panel(ms.pt);
                    crate::app::diag_log(&format!(
                        "HK_UP name={} at=({},{}) moved={} inPanel={}",
                        h.name, ms.pt.x, ms.pt.y, moved, in_panel
                    ));
                    if moved && in_panel {
                        // 只推物理文件；虚拟项（此电脑/回收站）没有磁盘路径
                        let path = ITEMS.with(|v| {
                            v.borrow()
                                .iter()
                                .find(|i| i.name == h.name)
                                .filter(|i| i.pidl.is_none())
                                .map(|i| i.path.clone())
                        });
                        if let Some(p) = path {
                            if p.exists() {
                                crate::app::diag_log(&format!(
                                    "HK_DROP name={} at=({},{}) path={}",
                                    h.name, ms.pt.x, ms.pt.y,
                                    p.to_string_lossy()
                                ));
                                // 入站会顺带清掉提示态（WM_DROP_FILES 里 set_drag_hover(false)）
                                if post_drop_file(p.to_string_lossy().to_string()) {
                                    return CallNextHookEx(None, code, wparam, lparam);
                                }
                                crate::ui::post_drag_hover(false);
                            }
                        }
                        // 落在灵动岛上但没推成（虚拟项/路径失效）：撤掉提示态，
                        // 否则面板会一直停在「松开鼠标暂存」的引导界面。
                        if was_hover || in_panel {
                            crate::ui::post_drag_hover(false);
                        }
                    } else if was_hover {
                        // 没落在灵动岛（或没真正拖动）：退出提示态
                        crate::ui::post_drag_hover(false);
                    }
                } else if was_hover {
                    crate::ui::post_drag_hover(false);
                }
            }
            _ => {}
        }
    }
    if code >= 0 && wparam.0 as u32 == WM_MOUSEWHEEL && lparam.0 != 0 {
        let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        // 只吞「光标真正落在分区上」的滚轮。分区是贴桌面浮层，永远在普通应用
        // 窗口**之下**；几何判定 window_zone_at 只看矩形、不关心谁在上面，一旦
        // 普通窗口与分区矩形重叠，用户在应用里滚轮就会被这里 LRESULT(1) 吞掉，
        // 表现为「开启分区后滚轮失效」。普通滚动不受 SetCapture 影响，用
        // WindowFromPoint 取光标下真正在最顶层的窗口，只有它确属分区才转投；
        // 否则原样放行，让下面的应用正常收到滚轮。
        if let Some(idx) = zone_at_topmost(ms.pt) {
            if let Some(hw) = zone_hwnd(idx) {
                let (w, h) = client_size(hw);
                let (_f, _v, max_first) = scroll_info(idx, w, h);
                if max_first > 0 {
                    // WM_MOUSEWHEEL 的 lParam 必须带屏幕光标坐标；传 0 会让
                    // 依赖它的命中测试永远落到屏幕左上角。
                    let xy = (((ms.pt.y as u32 & 0xffff) << 16) | (ms.pt.x as u32 & 0xffff)) as isize;
                    let _ = PostMessageW(
                        Some(hw),
                        WM_MOUSEWHEEL,
                        WPARAM(((ms.mouseData >> 16) as usize) << 16),
                        LPARAM(xy),
                    );
                    return LRESULT(1);
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// 光标点是否为某个分区窗口（且它是该点真正的最顶层可见窗口）。
///
/// 供滚轮钩子判定用：普通滚动时没有 `SetCapture`，`WindowFromPoint` 会准确返回
/// 光标下最顶层的窗口。只有它确实是本应用的分区窗口，才认为「滚轮应交给分区」；
/// 若是应用窗口（分区被压在下面），返回 None，滚轮照常放行给应用。
fn zone_at_topmost(pt: POINT) -> Option<usize> {
    let top = unsafe { WindowFromPoint(pt) };
    if top.0.is_null() {
        return None;
    }
    WIN.with(|ws| {
        let v = ws.borrow();
        for (i, hw) in v.iter().enumerate() {
            let Some(hw) = *hw else { continue };
            if hw == top {
                return Some(i);
            }
        }
        None
    })
}

fn install_wheel_hook() {
    if HOOK.with(|c| c.get()) != 0 {
        return;
    }
    unsafe {
        if let Ok(h) = SetWindowsHookExW(WH_MOUSE_LL, Some(wheel_hook), None, 0) {
            HOOK.with(|c| c.set(h.0 as isize));
        }
    }
}

fn uninstall_wheel_hook() {
    let h = HOOK.with(|c| c.replace(0));
    if h != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(h as *mut c_void));
        }
    }
}

// ── 桌面变更实时监测（Shell 变更通知） ──

/// 注册 Shell 变更通知：桌面（含公共桌面）有任何增删改名，Explorer 会直接给
/// 这个窗口发一条消息 —— 比定时轮询快得多，也不需要后台线程扫目录。
///
/// 用 `SHCNRF_InterruptLevel`（同步发送）时回调必须**极快**，否则会卡住
/// Explorer 的通知线程，所以窗口过程里只起一个 240ms 去抖定时器，真正的
/// 重新枚举放到定时器里做（多次变更合并成一次）。
fn install_change_notify(hwnd: HWND) {
    if CHG_HWND.with(|c| c.get()) == hwnd.0 as isize && CHG_REG.with(|c| c.get()) != 0 {
        return;
    }
    uninstall_change_notify();
    unsafe {
        let mut msg = CHG_MSG.with(|c| c.get());
        if msg == 0 {
            msg = RegisterWindowMessageW(w!("SHChangeNotify"));
            if msg == 0 {
                return;
            }
            CHG_MSG.with(|c| c.set(msg));
        }
        let pidl = match SHGetSpecialFolderLocation(None, CSIDL_DESKTOP as i32) {
            Ok(p) if !p.is_null() => p,
            _ => return,
        };
        let entry = SHChangeNotifyEntry {
            pidl,
            fRecursive: true.into(),
        };
        let id = SHChangeNotifyRegister(
            hwnd,
            SHCNRF_InterruptLevel | SHCNRF_ShellLevel,
            SHCNE_ALLEVENTS.0 as i32,
            msg,
            1,
            &entry,
        );
        CoTaskMemFree(Some(pidl as *const c_void));
        if id != 0 {
            CHG_REG.with(|c| c.set(id));
            CHG_HWND.with(|c| c.set(hwnd.0 as isize));
        }
    }
}

fn uninstall_change_notify() {
    let id = CHG_REG.with(|c| c.replace(0));
    CHG_HWND.with(|c| c.set(0));
    if id != 0 {
        unsafe {
            let _ = SHChangeNotifyDeregister(id);
        }
    }
}

/// 右键菜单预热：Shell 的 `IContextMenu` 第一次创建时要加载并初始化所有已注册的
/// 外壳扩展（7-Zip、云盘同步、Git…），这一下往往要几百毫秒到数秒，表现就是
/// 右键后光标转圈。这里在**后台 STA 线程**里先空跑一遍（建菜单→销毁菜单），
/// 把扩展 DLL 在进程里加载好，之后真正右键就快了。
///
/// 线程跑完就 park 住不退出：外壳扩展的 COM 对象/线程可能仍被引用，
/// 让线程活着比让公寓消失安全。整个过程不碰 UI。
fn prewarm_context_menu() {
    if WARMED.with(|c| c.replace(true)) {
        return;
    }
    // 注意：ITEMS 是 thread_local，新线程里读到的是空的，必须在当前线程取出路径
    // 预热要拿真实路径解析 PIDL，优先取物理文件项（跳过虚拟项）
    let Some(first) = ITEMS.with(|v| {
        v.borrow()
            .iter()
            .find(|i| i.pidl.is_none())
            .cloned()
    }) else {
        return;
    };
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let wide = wide_nul(&first.path.to_string_lossy());
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        if SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None).is_ok()
            && !pidl.is_null()
        {
            let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
            if let Ok(folder) = SHBindToParent::<IShellFolder>(pidl, Some(&mut child)) {
                let apidl: [*const ITEMIDLIST; 1] = [child as *const ITEMIDLIST];
                // 传 0 当 hwnd：只是为了实例化扩展，不显示菜单
                if let Ok(cm) = folder.GetUIObjectOf::<IContextMenu>(HWND::default(), &apidl, None) {
                    if let Ok(menu) = CreatePopupMenu() {
                        let _ = cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL);
                        let _ = DestroyMenu(menu);
                    }
                    drop(cm);
                }
            }
            CoTaskMemFree(Some(pidl as *const c_void));
        }
        // 保持这个 STA 存活（不退出，就不触发公寓销毁 / DLL 卸载）
        loop {
            std::thread::park();
        }
    });
}

// ── 分区窗口过程 ──

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 桌面变更通知（消息号是运行时注册的，只能在这里动态比对）。
    // 必须立刻返回：这条消息由 Explorer 同步发送，卡住会拖慢整个外壳。
    // 只起一个去抖定时器，240ms 内的多次变更合并成一次同步。
    let chg = CHG_MSG.with(|c| c.get());
    if chg != 0 && msg == chg {
        SetTimer(Some(hwnd), 2, 240, None);
        return LRESULT(0);
    }
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
            // 分区属于桌面：任何原因被最小化都立刻恢复，不等看门狗
            if wparam.0 as u32 == SIZE_MINIMIZED {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            // 尺寸变化 → 整张位图重建（缩放时每帧走这里，保证边框始终贴合）
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if idx >= 0 {
                render(hwnd, idx as usize);
            }
            LRESULT(0)
        }
        WM_SYSCOMMAND => {
            // 拦截系统「最小化」：Win+M / Shell 的 MinimizeAll 会发这条消息，
            // 分区是桌面组件，不该跟普通窗口一起缩进任务栏。
            if (wparam.0 & 0xFFF0) == SC_MINIMIZE as usize {
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
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
            let sp = cursor_screen();
            let mut local = sp;
            let _ = ScreenToClient(hwnd, &mut local);
            let (w, h) = client_size(hwnd);

            // 折叠箭头
            if in_chevron(local, w) {
                toggle_collapse(idx);
                return LRESULT(0);
            }
            // 点在图标上：启动跨分区图标拖拽（未拖动前松手 = 无操作，双击仍会触发打开）
            let hit_icon = icon_at(hwnd, sp);
            crate::app::diag_log(&format!("ZONE_DOWN at=({},{}) hit_icon={:?}", sp.x, sp.y, hit_icon));
            if let Some(i) = hit_icon {
                let name =
                    crate::app::with_cfg(|c| c.zones.get(idx).and_then(|z| z.items.get(i).cloned()));
                if let Some(n) = name {
                    IDRAG.with(|d| {
                        *d.borrow_mut() = Some(ItemDrag {
                            from: idx,
                            name: n,
                            last: sp,
                            moved: false,
                        });
                    });
                    let _ = SetCapture(hwnd);
                }
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
            let sp = cursor_screen();
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
            // 面板内图标拖拽中：跟随浮窗 + 落点分区高亮
            if IDRAG.with(|d| d.borrow().is_some()) {
                let sp = cursor_screen();
                let (name, from, moved) = IDRAG.with(|d| {
                    let mut b = d.borrow_mut();
                    match b.as_mut() {
                        Some(s) => {
                            if !s.moved {
                                let dx = sp.x - s.last.x;
                                let dy = sp.y - s.last.y;
                                if dx * dx + dy * dy > 25 {
                                    s.moved = true;
                                }
                            }
                            s.last = sp;
                            (s.name.clone(), s.from, s.moved)
                        }
                        None => (String::new(), 0usize, false),
                    }
                });
                if moved {
                    render_ghost(&name, sp);
                    // 落点高亮（拖回来源分区不高亮）
                    let t = window_zone_at(sp).filter(|&t| t != from);
                    let prev = GTARGET.with(|c| c.replace(t));
                    if t != prev {
                        if let Some(p) = prev {
                            if let Some(hw) = zone_hwnd(p) {
                                render(hw, p);
                            }
                        }
                        if let Some(t2) = t {
                            if let Some(hw) = zone_hwnd(t2) {
                                render(hw, t2);
                            }
                        }
                    }
                    if let Ok(cur) = LoadCursorW(None, IDC_SIZEALL) {
                        let _ = SetCursor(Some(cur));
                    }
                }
                return LRESULT(0);
            }
            if DRAG.with(|d| d.borrow().is_some()) {
                let sp = cursor_screen();
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
                // 分区正被拖动/缩放：立刻重判遮挡，靠到灵动岛跟前时能及时收起
                crate::app::occ_poke();
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONUP => {
            // 面板内图标拖拽收尾：落到其它分区 → 移动归属
            let had_idrag = IDRAG.with(|d| d.borrow().is_some());
            let up_pt = cursor_screen();
            crate::app::diag_log(&format!("ZONE_UP at=({},{}) had_idrag={}", up_pt.x, up_pt.y, had_idrag));
            let mut idrag = IDRAG.with(|d| d.borrow_mut().take());
            if let Some(s) = idrag.as_mut() {
                // 落点必须用「松手瞬间」的光标位置，不能沿用最后一次 MOUSEMOVE：
                // 快速拖动时最后一条 MOUSEMOVE 可能离松手点有几十像素，
                // 导致图标落到相邻分区，看起来像鼠标和实际落点对不上。
                let p = cursor_screen();
                if p.x != 0 || p.y != 0 {
                    s.last = p;
                }
            }
            if let Some(s) = idrag {
                let _ = ReleaseCapture();
                hide_ghost();
                let prev = GTARGET.with(|c| c.take());
                if let Some(p) = prev {
                    if let Some(hw) = zone_hwnd(p) {
                        render(hw, p);
                    }
                }
                if s.moved {
                    // 诊断：记录每次分区图标拖拽的落点判定过程（写入 %APPDATA%\WinIsland\drag.log）
                    unsafe {
                        let hit = WindowFromPoint(s.last);
                        let mut cbuf = [0u16; 64];
                        let cn = GetClassNameW(hit, &mut cbuf);
                        let cname = String::from_utf16_lossy(&cbuf[..cn.max(0) as usize]);
                        let main_rect = FindWindowW(w!("WinIsland"), None)
                            .ok()
                            .and_then(|h| {
                                let mut r = RECT::default();
                                if GetWindowRect(h, &mut r).is_ok() {
                                    Some(format!("({},{})-({},{})", r.left, r.top, r.right, r.bottom))
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_else(|| "no-hwnd".into());
                        crate::app::diag_log(&format!(
                            "ZONE_DROP name={} at=({},{}) WindowFromPoint={} mainRect={} inMain={}",
                            s.name, s.last.x, s.last.y, cname, main_rect,
                            window_is_main_panel(s.last)
                        ));
                    }
                    if let Some(t) = window_zone_at(s.last) {
                        if t != s.from {
                            let done = crate::app::with_cfg(|c| {
                                let Some(from) = c.zones.get_mut(s.from) else {
                                    return false;
                                };
                                let Some(pos) = from.items.iter().position(|n| *n == s.name) else {
                                    return false;
                                };
                                from.items.remove(pos);
                                match c.zones.get_mut(t) {
                                    Some(to) => {
                                        if !to.items.contains(&s.name) {
                                            to.items.push(s.name.clone());
                                        }
                                    }
                                    None => {
                                        // 目标分区刚好被删：放回来源分区，不丢文件
                                        if let Some(f) = c.zones.get_mut(s.from) {
                                            f.items.push(s.name.clone());
                                        }
                                    }
                                }
                                true
                            });
                            if done {
                                crate::app::save_now();
                                sync();
                            }
                        }
                    } else if window_is_main_panel(s.last) {
                        // 落到主面板（灵动岛）：把拖的物理文件推入文件中转站。
                        // 分区图标拖拽是自绘的 IDRAG，不走 OLE 拖放那套，
                        // 所以必须在这里手动交接，否则落点在主面板上什么都不会发生。
                        // 只有真实磁盘文件能暂存；虚拟项（此电脑/回收站）没有磁盘路径。
                        let item = ITEMS.with(|v| {
                            v.borrow().iter().find(|i| i.name == s.name).cloned()
                        });
                        match item {
                            None => crate::app::diag_log("ZONE_DROP 未找到 ITEMS 项"),
                            Some(item) => {
                                let ok = item.pidl.is_none() && item.path.exists();
                                crate::app::diag_log(&format!(
                                    "ZONE_DROP item path={} pidl={} exists={} ok={}",
                                    item.path.to_string_lossy(), item.pidl.is_some(),
                                    item.path.exists(), ok
                                ));
                                if ok {
                                    crate::app::push_file_external(
                                        item.path.to_string_lossy().to_string(),
                                    );
                                    crate::app::focus_filehub();
                                }
                            }
                        }
                    }
                }
                return LRESULT(0);
            }
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
                let idx = idx as usize;
                let sp = cursor_screen();
                // 右键点在具体图标上 → 该项的系统菜单；空白处 → 围栏菜单
                if let Some(i) = icon_at(hwnd, sp) {
                    let name = crate::app::with_cfg(|c| {
                        c.zones.get(idx).and_then(|z| z.items.get(i).cloned())
                    });
                    if let Some(n) = name {
                        if file_menu(hwnd, &n) {
                            return LRESULT(0);
                        }
                    }
                }
                popup_menu(hwnd, idx);
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
                } else if id == MENU_COLLAPSE {
                    toggle_collapse(idx);
                } else if id >= MENU_ALPHA_BASE && id < MENU_ALPHA_BASE + ALPHA_STEPS.len() as u32 {
                    let v = ALPHA_STEPS[(id - MENU_ALPHA_BASE) as usize].1;
                    crate::app::with_cfg(|c| c.zone_alpha = v);
                    crate::app::save_now();
                    // 透明度是全局的，所有分区一起重绘
                    WIN.with(|ws| {
                        if let Ok(v) = ws.try_borrow() {
                            for (i, hw) in v.iter().enumerate() {
                                if let Some(h) = hw {
                                    render(*h, i);
                                }
                            }
                        }
                    });
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
        WM_MOUSEWHEEL => {
            // 滚轮翻页（由全局钩子把滚轮转进来：分区不抢焦点）
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if idx >= 0 {
                let delta = (((wparam.0 >> 16) & 0xffff) as u16 as i16) as i32;
                let acc = WACC.with(|c| {
                    let v = c.get() + delta;
                    c.set(v % WHEEL_DELTA as i32);
                    v
                });
                let notches = acc / WHEEL_DELTA as i32;
                if notches != 0 {
                    scroll_zone(idx as usize, notches);
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
            // 定时器 2：桌面变更去抖到期 → 重新枚举，**签名真的变了**才重建
            // （Shell 通知也会因非桌面目录触发，避免无谓的重绘和写盘）
            if wparam.0 == 2 {
                let _ = KillTimer(Some(hwnd), 2);
                let before = SIGN.with(|s| s.borrow().clone());
                refresh_items();
                let after = SIGN.with(|s| s.borrow().clone());
                if before != after {
                    sync();
                }
                return LRESULT(0);
            }
            let mut names: Vec<String> =
                ITEMS.with(|v| v.borrow().iter().map(|i| i.name.clone()).collect());
            names.sort();
            let cur = names.join("\u{1}");
            let old = SIGN.with(|s| s.borrow().clone());
            if cur != old {
                refresh();
            } else {
                // Explorer 重启 / 用户手动「显示桌面图标」后，重新接管图标层
                let any_visible = crate::app::with_cfg(|c| {
                    c.zones.iter().any(|z| z.visible && !z.items.is_empty())
                });
                if any_visible {
                    let h = desktop_icon_view();
                    if !h.0.is_null() && IsWindowVisible(h).as_bool() {
                        set_shell_icons_visible(false);
                    }
                }
            }
            // 「显示桌面」(Win+D) 会把顶层窗口最小化/隐藏。分区是顶层窗口，
            // 因此一起消失。每个兜底 tick 检查并恢复「配置要显示却被系统隐藏」的分区。
            restore_hidden_zones();
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            // 捕获被抢占（如弹出菜单）：中止面板内图标拖拽，避免状态残留
            IDRAG.with(|d| *d.borrow_mut() = None);
            hide_ghost();
            GTARGET.with(|c| c.take());
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
                    put_px(buf, i, PANEL_RGB, panel_alpha() * cv / 4);
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
                put_px(buf, i, TITLE_RGB, title_alpha() * cv / 4);
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
            crate::render::ui_font_pcwstr(),
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
                crate::render::ui_font_pcwstr(),
            );
            let old_small = SelectObject(hdc, font_small.into());
            let _ = SetTextColor(hdc, COLORREF(0xE6ECE9));

            let (first, _vis, _max_first) = scroll_info(idx, w, h);
            for (i, it) in items.iter().enumerate() {
                let row_all = i as i32 / cols;
                if row_all < first as i32 {
                    continue;
                }
                let col = i as i32 % cols;
                let row = row_all - first as i32;
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

        // 拖拽悬停高亮：本分区是当前图标拖拽的落点时画主题色描边
        if GTARGET.with(|c| c.get()) == Some(idx) {
            let hl = CreatePen(PS_SOLID, 3, COLORREF(color));
            let old_hl = SelectObject(hdc, hl.into());
            let old_br = SelectObject(hdc, GetStockObject(NULL_BRUSH));
            let _ = Rectangle(hdc, 2, 2, w - 2, h - 2);
            let _ = SelectObject(hdc, old_hl);
            let _ = SelectObject(hdc, old_br);
            let _ = DeleteObject(hl.into());
            let buf = surf.pixels();
            for i in 0..(w * h) as usize {
                if buf[i] != base[i] {
                    buf[i] = 0xFF00_0000 | (buf[i] & 0x00FF_FFFF);
                }
            }
        }
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

    // ⑤ 内容超出可视区 → 右侧画滚动指示条（滚轮可翻页）
    {
        let (first, vis, max_first) = scroll_info(idx, w, h);
        if max_first > 0 {
            let buf = surf.pixels();
            let total = (max_first + vis) as f32;
            let x0 = w - 7;
            let y0 = TITLE_H + PAD - 2;
            let y1 = h - PAD;
            for y in y0..y1 {
                for x in x0..x0 + 3 {
                    blend_px(buf, (y * w + x) as usize, (255, 255, 255), 30);
                }
            }
            let track = (y1 - y0) as f32;
            let th = (track * vis as f32 / total).max(20.0);
            let ty = y0 as f32 + (track - th) * (first as f32 / max_first as f32);
            for y in ty as i32..(ty + th) as i32 {
                if y < y0 || y >= y1 {
                    continue;
                }
                for x in x0..x0 + 3 {
                    blend_px(buf, (y * w + x) as usize, (255, 255, 255), 130);
                }
            }
        }
    }

    commit_layered(hwnd, &surf, w, h);
}
